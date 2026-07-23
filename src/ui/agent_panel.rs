//! Native GTK shell Agent UI. The model can only propose commands; every
//! command remains editable and requires an explicit per-command approval.
//!
//! The session renders as an inline card in the bound Block pane's
//! conversation, pinned directly above the live prompt: activity, proposals,
//! approval, and the instruction composer all live in the block flow.
//! Configuration-type content (identity, provider chips, the correction
//! toggle) stays in a small settings dialog opened from the card header.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk4::{
    Box as GBox, Button, Entry, Image, Label, Orientation, ProgressBar, Spinner, Switch,
};
use libadwaita as adw;

use super::UiState;
use crate::agent::{AgentSession, AgentState, ModelOutcome, ProposalId};
use crate::block_view::TermView;

/// Bind the next completed foreground block to the approved proposal.
///
/// Approval is only possible while the pinned Block prompt is idle and empty,
/// after the preceding block has already been finalized. The next foreground
/// completion is therefore the approved command even when VTE's best-effort
/// command capture is stale or reflects a shell line-editor redraw.
fn take_pending_for_finished_block<T>(
    pending: &mut Option<(T, String)>,
    captured_command: &str,
) -> Option<T> {
    let (value, approved_command) = pending.take()?;
    if captured_command.trim() != approved_command.trim() {
        // Do not log either command: command text can contain sensitive data.
        log::debug!("Agent command completed with a differing VTE command capture");
    }
    Some(value)
}

/// The one live Shell Agent session, stored in `UiState::agent_session`.
/// Closing it cancels the session and removes its inline card.
pub(crate) struct AgentHandle {
    runtime: Rc<AgentRuntime>,
}

impl AgentHandle {
    pub(crate) fn shutdown(&self) {
        self.runtime.shutdown();
    }
}

struct AgentRuntime {
    session: RefCell<AgentSession>,
    target: Rc<TermView>,
    config: Rc<RefCell<crate::config::Config>>,
    shell: String,
    block_context: RefCell<Option<crate::ai::BlockContext>>,
    /// The inline card widget inserted into the target pane's block list.
    card: gtk4::Widget,
    input: Entry,
    send: Button,
    stop_request: Button,
    retry_request: Button,
    context_clear: Button,
    context_card: GBox,
    status: Label,
    status_spinner: Spinner,
    turn_progress: ProgressBar,
    turn_label: Label,
    proposal_box: GBox,
    pending_command: RefCell<Option<(ProposalId, String)>>,
    request_cancellation: RefCell<Option<crate::ai::AiCancellationToken>>,
    busy: Cell<bool>,
    alive: Cell<bool>,
}

impl AgentRuntime {
    /// Add one conversation message as its own block in the pane's block flow,
    /// directly above the pinned Agent card. Messages are ordinary blocks in
    /// the conversation: they stay in place as history and survive the session
    /// card being closed.
    fn append(&self, speaker: &str, body: &str) {
        let compact = self.config.borrow().block_compact;
        let message = build_agent_message_block(speaker, body, compact);
        self.target.insert_inline_notice(&message);
        // Keep the session card below the newest message, pinned above the
        // live prompt. (On the intro message this also performs the card's
        // initial insertion.)
        self.target.insert_inline_notice(&self.card);
    }

    fn clear_proposal(&self) {
        while let Some(child) = self.proposal_box.first_child() {
            self.proposal_box.remove(&child);
        }
        self.proposal_box.set_visible(false);
    }

    fn set_status(&self, message: &str, active: bool) {
        self.status.set_text(message);
        if active {
            self.status_spinner.start();
        } else {
            self.status_spinner.stop();
        }
        let session = self.session.borrow();
        let used = session.turns_used();
        let max = session.max_turns();
        self.turn_label.set_text(&format!("{used} / {max} turns"));
        self.turn_progress
            .set_fraction(f64::from(used) / f64::from(max.max(1)));
    }

    fn sync_controls(&self) {
        let session = self.session.borrow();
        let ready = self.alive.get() && !self.busy.get() && session.state() == AgentState::Ready;
        self.input.set_sensitive(ready);
        self.send
            .set_sensitive(ready && !self.input.text().trim().is_empty());
        self.stop_request
            .set_visible(self.alive.get() && self.busy.get());
        self.stop_request
            .set_sensitive(self.alive.get() && self.busy.get());
        self.retry_request
            .set_visible(self.alive.get() && !self.busy.get() && session.can_retry_model());
        self.retry_request
            .set_sensitive(self.retry_request.is_visible());
        self.context_clear.set_sensitive(
            self.alive.get() && !self.busy.get() && session.state() == AgentState::Ready,
        );
    }

    fn render_session_state(&self, ready_status: Option<&str>) {
        self.sync_controls();
        if self.busy.get() {
            if let Some(message) = ready_status {
                self.set_status(message, true);
            }
            return;
        }
        let state = self.session.borrow().state();
        match state {
            AgentState::Ready => {
                self.set_status(
                    ready_status.unwrap_or("Ready for the next instruction"),
                    false,
                );
                self.input.grab_focus();
            }
            AgentState::AwaitingApproval { proposal_id } => self.set_status(
                &format!("Proposal #{} is waiting for review", proposal_id.get()),
                false,
            ),
            AgentState::AwaitingObservation { .. } => {
                self.set_status("Running the approved command…", true)
            }
            AgentState::Completed => self.set_status("Task completed", false),
            AgentState::Cancelled => self.set_status("Agent cancelled", false),
            AgentState::TurnLimitReached => self.set_status(
                "Turn limit reached. Start a new Agent session to continue.",
                false,
            ),
            AgentState::AwaitingModel => {
                self.set_status(ready_status.unwrap_or("Waiting for the model…"), true)
            }
        }
    }

    fn stop_current_request(&self) {
        if !self.busy.get() || !self.alive.get() {
            return;
        }
        if let Some(cancellation) = self.request_cancellation.borrow().as_ref() {
            cancellation.cancel();
            self.stop_request.set_sensitive(false);
            self.set_status("Stopping the current model request…", true);
        }
    }

    fn retry_model(runtime: Rc<Self>) {
        if runtime.busy.get() || !runtime.alive.get() {
            return;
        }
        let retry_result = runtime.session.borrow_mut().retry_model();
        match retry_result {
            Ok(()) => Self::request_model(runtime),
            Err(error) => {
                runtime.render_session_state(Some(&error.to_string()));
            }
        }
    }

    fn detach_block_context(&self) {
        if self.busy.get() || self.session.borrow().state() != AgentState::Ready {
            return;
        }
        if self.block_context.borrow_mut().take().is_some() {
            self.context_card.set_visible(false);
            self.append(
                "Agent",
                "Selected Block context detached. Session activity is still retained.",
            );
            self.render_session_state(None);
        }
    }

    fn submit(runtime: Rc<Self>) {
        if runtime.busy.get() || !runtime.alive.get() {
            return;
        }
        let text = runtime.input.text().trim().to_string();
        if text.is_empty() {
            return;
        }
        let submit_result = runtime.session.borrow_mut().submit_user(text.clone());
        if let Err(error) = submit_result {
            runtime.render_session_state(Some(&error.to_string()));
            return;
        }
        runtime.input.set_text("");
        runtime.append("You", &text);
        Self::request_model(runtime);
    }

    fn request_model(runtime: Rc<Self>) {
        if !runtime.alive.get()
            || runtime.busy.get()
            || runtime.session.borrow().state() != AgentState::AwaitingModel
        {
            runtime.render_session_state(None);
            return;
        }

        let client = match crate::ai::AiClient::from_config(&runtime.config.borrow()) {
            Ok(client) => client,
            Err(error) => {
                let message = error.to_string();
                let _ = runtime.session.borrow_mut().model_failed(&message);
                runtime.append("Error", &message);
                runtime.render_session_state(Some(&message));
                return;
            }
        };
        let cwd = runtime.target.cwd();
        let system = crate::ai::build_agent_system_prompt();
        let prompt = crate::ai::agent_user_prompt(
            &runtime.session.borrow().build_user_prompt(),
            if cwd.is_empty() { "." } else { &cwd },
            &runtime.shell,
            std::env::consts::OS,
            runtime.block_context.borrow().as_ref(),
        );
        let session_cancellation = runtime.session.borrow().cancellation_token();
        let request_cancellation = crate::ai::AiCancellationToken::new();
        *runtime.request_cancellation.borrow_mut() = Some(request_cancellation.clone());

        runtime.busy.set(true);
        runtime.sync_controls();
        let (next_turn, max_turns) = {
            let session = runtime.session.borrow();
            (session.turns_used() + 1, session.max_turns())
        };
        runtime.set_status(
            &format!(
                "Thinking with {} · turn {}/{}",
                client.display_name(),
                next_turn,
                max_turns
            ),
            true,
        );

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if session_cancellation.is_cancelled() {
                return;
            }
            let result = client.send_turns_blocking_cancellable(
                Some(&system),
                &[crate::ai::Turn {
                    role: crate::ai::Role::User,
                    text: prompt,
                }],
                &request_cancellation,
            );
            if !session_cancellation.is_cancelled() {
                let _ = tx.send(result);
            }
        });

        let rx = RefCell::new(rx);
        gtk4::glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            if !runtime.alive.get() {
                return gtk4::glib::ControlFlow::Break;
            }
            match rx.borrow().try_recv() {
                Ok(Ok(reply)) => {
                    runtime.request_cancellation.borrow_mut().take();
                    runtime.busy.set(false);
                    let outcome = runtime.session.borrow_mut().accept_model_reply(&reply);
                    match outcome {
                        Ok(ModelOutcome::Proposal {
                            id,
                            command,
                            danger,
                        }) => Self::render_proposal(&runtime, id, command, danger),
                        Ok(ModelOutcome::Said(message)) => {
                            runtime.append("Agent", &message);
                            runtime.render_session_state(None);
                        }
                        Ok(ModelOutcome::Completed(message)) => {
                            runtime.append("Agent", &message);
                            runtime.render_session_state(None);
                        }
                        Err(error) => {
                            let message = error.to_string();
                            runtime.append("Protocol error", &message);
                            runtime.render_session_state(Some(&message));
                        }
                    }
                    gtk4::glib::ControlFlow::Break
                }
                Ok(Err(error)) => {
                    runtime.request_cancellation.borrow_mut().take();
                    runtime.busy.set(false);
                    let stopped = matches!(error, crate::ai::AiError::Cancelled);
                    let message = if stopped {
                        "Model request stopped. Retry it or revise the instruction.".to_string()
                    } else {
                        error.to_string()
                    };
                    let _ = runtime.session.borrow_mut().model_failed(&message);
                    runtime.append(if stopped { "Stopped" } else { "Error" }, &message);
                    runtime.render_session_state(Some(&message));
                    gtk4::glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => gtk4::glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    runtime.request_cancellation.borrow_mut().take();
                    runtime.busy.set(false);
                    let message = "Agent worker disconnected.";
                    let _ = runtime.session.borrow_mut().model_failed(message);
                    runtime.append("Error", message);
                    runtime.render_session_state(Some(message));
                    gtk4::glib::ControlFlow::Break
                }
            }
        });
    }

    fn render_proposal(
        runtime: &Rc<Self>,
        id: ProposalId,
        command: String,
        danger: Option<&'static str>,
    ) {
        runtime.clear_proposal();
        runtime.proposal_box.set_visible(true);

        let heading = if let Some(reason) = danger {
            format!("Potentially destructive: {reason}")
        } else {
            "Proposed command — edit if needed · Enter approves & runs".to_string()
        };
        let warning = Label::new(Some(&heading));
        warning.set_xalign(0.0);
        warning.set_wrap(true);
        if danger.is_some() {
            warning.add_css_class("error");
        }

        let command_entry = Entry::new();
        command_entry.set_text(&command);
        command_entry.set_hexpand(true);
        command_entry.set_tooltip_text(Some("This exact text will run only after approval"));
        command_entry
            .update_property(&[gtk4::accessible::Property::Label("Proposed shell command")]);

        let approve = Button::with_label("Approve & Run");
        sync_proposal_risk(&warning, &approve, &command);
        let reject = Button::with_label("Reject");
        let copy = Button::with_label("Copy");
        copy.set_tooltip_text(Some("Copy the proposed command without running it"));
        let buttons = GBox::new(Orientation::Horizontal, 6);
        buttons.set_halign(gtk4::Align::End);
        buttons.append(&copy);
        buttons.append(&reject);
        buttons.append(&approve);

        runtime.proposal_box.append(&warning);
        runtime.proposal_box.append(&command_entry);
        runtime.proposal_box.append(&buttons);
        runtime.render_session_state(None);
        command_entry.grab_focus();

        {
            let warning = warning.clone();
            let approve = approve.clone();
            command_entry.connect_changed(move |entry| {
                sync_proposal_risk(&warning, &approve, &entry.text());
            });
        }

        let weak = Rc::downgrade(runtime);
        let entry_for_approve = command_entry.clone();
        approve.connect_clicked(move |_| {
            if let Some(runtime) = weak.upgrade() {
                Self::approve(runtime, id, entry_for_approve.text().to_string());
            }
        });
        // Enter in the command entry approves & runs; dangerous commands
        // still go through the extra confirmation dialog in `approve`.
        let weak = Rc::downgrade(runtime);
        command_entry.connect_activate(move |entry| {
            if let Some(runtime) = weak.upgrade() {
                Self::approve(runtime, id, entry.text().to_string());
            }
        });
        let weak = Rc::downgrade(runtime);
        let entry_for_copy = command_entry.clone();
        copy.connect_clicked(move |_| {
            if let Some(runtime) = weak.upgrade() {
                let command = entry_for_copy.text();
                runtime.card.clipboard().set_text(&command);
                runtime.set_status("Command copied; nothing was run.", false);
            }
        });
        let weak = Rc::downgrade(runtime);
        reject.connect_clicked(move |_| {
            if let Some(runtime) = weak.upgrade() {
                Self::reject(runtime, id);
            }
        });
    }

    fn approve(runtime: Rc<Self>, id: ProposalId, command: String) {
        let command = match crate::review_input::validate(&command) {
            Ok(command) => command.to_string(),
            Err(error) => {
                runtime.set_status(&format!("Cannot approve: {error}"), false);
                return;
            }
        };
        if let Some(reason) = crate::agent::is_dangerous(&command) {
            Self::confirm_dangerous_approval(runtime, id, command, reason);
            return;
        }
        Self::approve_validated(runtime, id, command);
    }

    fn confirm_dangerous_approval(
        runtime: Rc<Self>,
        id: ProposalId,
        command: String,
        reason: &'static str,
    ) {
        let dialog = adw::AlertDialog::new(
            Some("Run a potentially destructive command?"),
            Some(&format!(
                "{reason}. Verify the exact command below before continuing."
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("run", "Run Command")]);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("run", adw::ResponseAppearance::Destructive);
        let preview = Label::new(Some(&command));
        preview.set_selectable(true);
        preview.set_wrap(true);
        preview.set_xalign(0.0);
        preview.add_css_class("agent-danger-command");
        dialog.set_extra_child(Some(&preview));
        let weak = Rc::downgrade(&runtime);
        dialog.connect_response(None, move |_, response| {
            if response == "run" {
                if let Some(runtime) = weak.upgrade() {
                    Self::approve_validated(runtime, id, command.clone());
                }
            }
        });
        dialog.present(Some(&runtime.proposal_box));
    }

    fn approve_validated(runtime: Rc<Self>, id: ProposalId, command: String) {
        if !runtime.target.can_accept_agent_command() {
            let message =
                "The target prompt is busy or already contains input. Clear it and approve again.";
            runtime.set_status(message, false);
            runtime.append("Safety check", message);
            return;
        }
        let approval_result = runtime.session.borrow_mut().edit_and_approve(id, command);
        let approved = match approval_result {
            Ok(approved) => approved,
            Err(error) => {
                runtime.set_status(&error.to_string(), false);
                return;
            }
        };
        runtime.clear_proposal();
        // No visible "approved" message: the approved command runs immediately
        // and its real finished block lands in the conversation right here.
        *runtime.pending_command.borrow_mut() =
            Some((approved.proposal_id, approved.command.clone()));
        runtime.render_session_state(None);
        runtime.target.grab_focus();
        runtime.target.submit_command(&approved.command);
    }

    fn reject(runtime: Rc<Self>, id: ProposalId) {
        let result = runtime.session.borrow_mut().reject(id);
        match result {
            Ok(()) => {
                runtime.clear_proposal();
                runtime.append("You", "Rejected proposal; ask for another approach.");
                Self::request_model(runtime);
            }
            Err(error) => runtime.render_session_state(Some(&error.to_string())),
        }
    }

    fn observe(runtime: Rc<Self>, command: String, exit_code: i32, output: String) {
        let id = {
            let mut pending = runtime.pending_command.borrow_mut();
            take_pending_for_finished_block(&mut pending, &command)
        };
        let Some(id) = id else {
            return;
        };
        let observation_result = runtime.session.borrow_mut().observe(id, exit_code, &output);
        if let Err(error) = observation_result {
            runtime.render_session_state(Some(&error.to_string()));
            return;
        }
        // The command's own finished block already shows the result in the
        // conversation; only the session (model context) records it here.
        Self::request_model(runtime);
    }

    fn cancel(&self) {
        if !self.alive.replace(false) {
            return;
        }
        if let Some(cancellation) = self.request_cancellation.borrow_mut().take() {
            cancellation.cancel();
            if !cancellation.wait_for_inactive(std::time::Duration::from_millis(500)) {
                log::warn!("Timed out waiting for the Agent request to shut down");
            }
        }
        self.session.borrow_mut().cancel();
        self.pending_command.borrow_mut().take();
        self.busy.set(false);
        self.clear_proposal();
        self.render_session_state(None);
    }

    /// Cancel the session and remove its inline card. Idempotent.
    fn shutdown(&self) {
        self.cancel();
        self.target.remove_inline_notice(&self.card);
    }
}

/// Build one Shell Agent conversation message styled like a finished block:
/// a header row identifying the Shell Agent dialogue and the speaker, then the
/// message body. It is inserted as an inline notice, so it never joins block
/// history or virtualization.
fn build_agent_message_block(speaker: &str, body: &str, compact: bool) -> gtk4::Widget {
    let error_speaker = matches!(
        speaker,
        "Error" | "Protocol error" | "Stopped" | "Safety check"
    );

    let outer = GBox::new(Orientation::Vertical, 0);
    outer.add_css_class("block-finished");
    outer.add_css_class("block-agent");
    outer.set_hexpand(true);
    outer.set_vexpand(false);
    if compact {
        outer.add_css_class("block-compact");
        outer.set_margin_top(1);
        outer.set_margin_bottom(1);
        outer.set_margin_start(4);
        outer.set_margin_end(4);
    } else {
        outer.set_margin_top(4);
        outer.set_margin_bottom(4);
        outer.set_margin_start(8);
        outer.set_margin_end(8);
    }

    let header = GBox::new(Orientation::Horizontal, 8);
    header.add_css_class("block-header");
    if compact {
        header.set_margin_start(8);
        header.set_margin_end(6);
        header.set_margin_top(3);
        header.set_margin_bottom(1);
    } else {
        header.set_margin_start(12);
        header.set_margin_end(8);
        header.set_margin_top(6);
        header.set_margin_bottom(2);
    }
    let icon = Label::new(Some(if speaker == "You" {
        "\u{f007}" // nf-fa-user
    } else {
        "\u{f544}" // nf-fa-robot
    }));
    icon.add_css_class("agent-card-icon");
    header.append(&icon);
    let title = Label::new(Some("Shell Agent"));
    title.add_css_class("agent-card-title");
    title.set_xalign(0.0);
    header.append(&title);
    let speaker_chip = Label::new(Some(speaker));
    speaker_chip.add_css_class("agent-chip");
    if error_speaker {
        speaker_chip.add_css_class("agent-msg-error");
    }
    speaker_chip.set_halign(gtk4::Align::Start);
    speaker_chip.set_hexpand(true);
    header.append(&speaker_chip);
    outer.append(&header);

    let body_label = Label::new(Some(body));
    body_label.add_css_class("agent-msg-body");
    body_label.set_xalign(0.0);
    body_label.set_wrap(true);
    body_label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    body_label.set_selectable(true);
    body_label.set_margin_start(if compact { 8 } else { 12 });
    body_label.set_margin_end(if compact { 8 } else { 12 });
    body_label.set_margin_top(2);
    body_label.set_margin_bottom(if compact { 6 } else { 10 });
    outer.append(&body_label);

    outer.upcast()
}

fn sync_proposal_risk(warning: &Label, approve: &Button, command: &str) {
    if let Some(reason) = crate::agent::is_dangerous(command) {
        warning.set_text(&format!("Potentially destructive: {reason}"));
        warning.add_css_class("error");
        approve.remove_css_class("suggested-action");
        approve.add_css_class("destructive-action");
        approve.set_tooltip_text(Some("A second confirmation is required"));
    } else {
        warning.set_text("Proposed command — edit if needed · Enter approves & runs");
        warning.remove_css_class("error");
        approve.remove_css_class("destructive-action");
        approve.add_css_class("suggested-action");
        approve.set_tooltip_text(Some("Run this exact command after approval"));
    }
}

fn agent_block_context_label(context: &crate::ai::BlockContext) -> String {
    let truncation = if context.truncated {
        " · output truncated"
    } else {
        ""
    };
    format!(
        "Selected Block · exit {}{truncation} · {}",
        context.exit_code,
        compact_one_line(&context.cmd, 56)
    )
}

fn agent_block_context_tooltip(context: &crate::ai::BlockContext) -> String {
    let cwd = context.cwd.as_deref().unwrap_or("unknown cwd");
    format!(
        "Attached as untrusted context\nexit: {}\noutput: {}\ncwd: {cwd}\ncommand: {}",
        context.exit_code,
        if context.truncated {
            "truncated"
        } else {
            "complete"
        },
        compact_one_line(&context.cmd, 160)
    )
}

fn compact_one_line(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else if preview.is_empty() {
        "(empty command)".to_string()
    } else {
        preview
    }
}

/// Configuration-type Shell Agent content lives in this small dialog, opened
/// from the inline card's header: identity, provider/shell chips, and the AI
/// command-correction toggle. Session activity never renders here.
fn show_agent_settings_dialog(ui: &UiState, cwd: &str, shell: &str) {
    let (provider, model, correction_enabled) = {
        let config = ui.config.borrow();
        (
            config.ai_provider.clone(),
            config.ai_model.clone(),
            config.command_correction_enabled,
        )
    };

    let dialog = adw::Dialog::builder()
        .title("Shell Agent settings")
        .content_width(620)
        .build();
    let header = adw::HeaderBar::new();

    let overview = GBox::new(Orientation::Vertical, 8);
    overview.add_css_class("agent-overview");
    let identity_row = GBox::new(Orientation::Horizontal, 10);
    let agent_icon = Image::from_icon_name("system-run-symbolic");
    agent_icon.set_pixel_size(32);
    agent_icon.add_css_class("agent-icon");
    let identity_copy = GBox::new(Orientation::Vertical, 2);
    identity_copy.set_hexpand(true);
    let title = Label::new(Some("Approval-gated shell assistant"));
    title.set_xalign(0.0);
    title.add_css_class("title-3");
    let target_label = Label::new(Some(&format!("Bound to Block pane · {cwd}")));
    target_label.set_xalign(0.0);
    target_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    target_label.set_tooltip_text(Some(cwd));
    target_label.add_css_class("dim-label");
    identity_copy.append(&title);
    identity_copy.append(&target_label);
    identity_row.append(&agent_icon);
    identity_row.append(&identity_copy);
    overview.append(&identity_row);

    let chips = GBox::new(Orientation::Horizontal, 6);
    let provider_chip = Label::new(Some(&format!("{provider} · {model}")));
    provider_chip.set_hexpand(true);
    // Keep the pill hugging its text; hexpand alone stretches the
    // background into a long empty capsule.
    provider_chip.set_halign(gtk4::Align::Start);
    provider_chip.set_max_width_chars(44);
    provider_chip.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    provider_chip.set_tooltip_text(Some(&format!("{provider} · {model}")));
    provider_chip.add_css_class("agent-chip");
    let shell_chip = Label::new(Some(&format!("shell: {shell}")));
    shell_chip.set_max_width_chars(26);
    shell_chip.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
    shell_chip.set_tooltip_text(Some(shell));
    shell_chip.add_css_class("agent-chip");
    let safety_chip = Label::new(Some("Review required"));
    safety_chip.add_css_class("agent-chip");
    safety_chip.add_css_class("agent-safety-chip");
    chips.append(&provider_chip);
    chips.append(&shell_chip);
    chips.append(&safety_chip);
    overview.append(&chips);

    let correction_row = GBox::new(Orientation::Horizontal, 12);
    correction_row.add_css_class("agent-setting-card");
    let correction_copy = GBox::new(Orientation::Vertical, 2);
    correction_copy.set_hexpand(true);
    let correction_title = Label::new(Some("AI command correction"));
    correction_title.set_xalign(0.0);
    correction_title.add_css_class("heading");
    let correction_hint = Label::new(Some(
        "After typo-like Block failures, offer an editable correction; never run it automatically.",
    ));
    correction_hint.set_xalign(0.0);
    correction_hint.set_wrap(true);
    correction_hint.add_css_class("dim-label");
    correction_copy.append(&correction_title);
    correction_copy.append(&correction_hint);
    let correction_switch = Switch::builder()
        .active(correction_enabled)
        .valign(gtk4::Align::Center)
        .build();
    correction_switch.set_tooltip_text(Some("Enable review-first command correction"));
    correction_row.append(&correction_copy);
    correction_row.append(&correction_switch);

    let ui_for_correction = ui.clone();
    correction_switch.connect_active_notify(move |toggle| {
        ui_for_correction
            .config
            .borrow_mut()
            .command_correction_enabled = toggle.is_active();
        ui_for_correction.persist_config();
    });

    let body = GBox::new(Orientation::Vertical, 10);
    body.add_css_class("agent-dashboard");
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.set_margin_top(10);
    body.set_margin_bottom(12);
    body.append(&overview);
    body.append(&correction_row);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_css_class("agent-surface");
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&body));
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(&ui.window));
}

impl UiState {
    /// Keep the visible top-bar Agent control aligned with both configuration
    /// availability and the lifetime of the active Agent session.
    pub(crate) fn sync_agent_toggle(&self) {
        let available = {
            let config = self.config.borrow();
            config.ai_enabled && config.agent_enabled
        };
        self.agent_toggle.set_sensitive(available);

        if !available {
            // Take the handle out of the slot before shutdown so anything
            // observing the slot already sees the session as closed.
            let session = self.agent_session.borrow_mut().take();
            if let Some(session) = session {
                session.shutdown();
            }
            self.agent_toggle.set_active(false);
        } else {
            self.agent_toggle
                .set_active(self.agent_session.borrow().is_some());
        }
    }

    pub(crate) fn toggle_agent_panel(&self) {
        // Toggle off: close the active inline session.
        let existing = self.agent_session.borrow_mut().take();
        if let Some(session) = existing {
            session.shutdown();
            self.agent_toggle.set_active(false);
            return;
        }
        let config = self.config.borrow();
        if !config.ai_enabled || !config.agent_enabled {
            drop(config);
            self.agent_toggle.set_active(false);
            self.show_ai_error("Agent mode is disabled in Settings or safe mode.");
            return;
        }
        let max_turns = config.agent_max_turns;
        let compact = config.block_compact;
        drop(config);
        let Some(target) = self.current_term_view() else {
            self.agent_toggle.set_active(false);
            self.show_ai_error("Agent mode requires an active Block pane.");
            return;
        };
        let block_context = target.selected_block_context(80);
        let cwd = target.cwd();
        let cwd = if cwd.is_empty() { ".".to_string() } else { cwd };
        let shell = self
            .shell_argv
            .borrow()
            .first()
            .cloned()
            .unwrap_or_else(|| "sh".to_string());

        // ── Inline agent card, styled like a block ────────────────────────
        let outer = GBox::new(Orientation::Vertical, 0);
        outer.add_css_class("block-finished");
        outer.add_css_class("block-agent");
        outer.set_hexpand(true);
        outer.set_vexpand(false);
        if compact {
            outer.add_css_class("block-compact");
            outer.set_margin_top(1);
            outer.set_margin_bottom(1);
            outer.set_margin_start(4);
            outer.set_margin_end(4);
        } else {
            outer.set_margin_top(4);
            outer.set_margin_bottom(4);
            outer.set_margin_start(8);
            outer.set_margin_end(8);
        }

        let header = GBox::new(Orientation::Horizontal, 8);
        header.add_css_class("block-header");
        if compact {
            header.set_margin_start(8);
            header.set_margin_end(6);
            header.set_margin_top(3);
            header.set_margin_bottom(1);
        } else {
            header.set_margin_start(12);
            header.set_margin_end(8);
            header.set_margin_top(6);
            header.set_margin_bottom(2);
        }
        let icon = Label::new(Some("\u{f544}")); // nf-fa-robot
        icon.add_css_class("agent-card-icon");
        header.append(&icon);
        let title = Label::new(Some("Shell Agent"));
        title.add_css_class("agent-card-title");
        title.set_xalign(0.0);
        header.append(&title);
        let binding_label = Label::new(Some(&format!(
            "{cwd} · review required · every command needs approval"
        )));
        binding_label.add_css_class("agent-card-binding");
        binding_label.set_hexpand(true);
        binding_label.set_halign(gtk4::Align::End);
        binding_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
        binding_label.set_tooltip_text(Some(&cwd));
        header.append(&binding_label);
        let settings_btn = Button::with_label("\u{f013}"); // nf-fa-cog
        settings_btn.add_css_class("flat");
        settings_btn.set_focusable(false);
        settings_btn.set_tooltip_text(Some("Shell Agent settings"));
        header.append(&settings_btn);
        let close_btn = Button::with_label("\u{2715}");
        close_btn.add_css_class("flat");
        close_btn.set_focusable(false);
        close_btn.set_tooltip_text(Some("Cancel Agent and close this card"));
        header.append(&close_btn);
        outer.append(&header);

        let context_card = GBox::new(Orientation::Horizontal, 8);
        context_card.add_css_class("agent-context-card");
        let context_label = Label::new(
            block_context
                .as_ref()
                .map(agent_block_context_label)
                .as_deref(),
        );
        context_label.set_xalign(0.0);
        context_label.set_hexpand(true);
        context_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
        context_label.set_tooltip_text(
            block_context
                .as_ref()
                .map(agent_block_context_tooltip)
                .as_deref(),
        );
        let context_clear = Button::from_icon_name("window-close-symbolic");
        context_clear.add_css_class("flat");
        context_clear.set_tooltip_text(Some("Detach selected Block context"));
        context_card.append(&context_label);
        context_card.append(&context_clear);
        context_card.set_visible(block_context.is_some());

        let status = Label::new(Some("Ready for the next instruction"));
        status.set_xalign(0.0);
        status.set_wrap(true);
        status.set_hexpand(true);
        status.add_css_class("agent-status");
        status.set_accessible_role(gtk4::AccessibleRole::Status);
        let status_spinner = Spinner::new();
        status_spinner.set_spinning(false);
        let turn_label = Label::new(Some(&format!("0 / {max_turns} turns")));
        turn_label.add_css_class("dim-label");
        let retry_request = Button::with_label("Retry");
        retry_request.set_visible(false);
        retry_request.set_tooltip_text(Some(
            "Retry the failed model turn without duplicating input",
        ));
        let stop_request = Button::with_label("Stop");
        stop_request.set_visible(false);
        stop_request.add_css_class("destructive-action");
        stop_request.set_tooltip_text(Some("Stop this model request and keep the Agent session"));
        let turn_progress = ProgressBar::new();
        turn_progress.set_hexpand(true);
        turn_progress.set_fraction(0.0);
        let status_top = GBox::new(Orientation::Horizontal, 8);
        status_top.append(&status_spinner);
        status_top.append(&status);
        status_top.append(&retry_request);
        status_top.append(&stop_request);
        status_top.append(&turn_label);
        let status_card = GBox::new(Orientation::Vertical, 6);
        status_card.add_css_class("agent-status-card");
        status_card.append(&status_top);
        status_card.append(&turn_progress);

        let proposal_box = GBox::new(Orientation::Vertical, 8);
        proposal_box.add_css_class("card");
        proposal_box.add_css_class("agent-proposal-card");
        proposal_box.set_visible(false);

        let input = Entry::new();
        input.set_hexpand(true);
        input.set_placeholder_text(Some(if block_context.is_some() {
            "Ask about the selected Block or describe a task…"
        } else {
            "Describe a task for this pane…"
        }));
        input.add_css_class("agent-input");
        input.update_property(&[gtk4::accessible::Property::Label("Shell Agent instruction")]);
        let send = Button::with_label("Send");
        send.set_sensitive(false);
        send.add_css_class("suggested-action");
        send.add_css_class("agent-send");
        let input_row = GBox::new(Orientation::Horizontal, 6);
        input_row.append(&input);
        input_row.append(&send);
        let input_hint = Label::new(Some(
            "Enter sends · every proposed command stays editable and requires approval",
        ));
        input_hint.set_xalign(0.0);
        input_hint.add_css_class("dim-label");
        input_hint.add_css_class("agent-input-hint");
        let composer = GBox::new(Orientation::Vertical, 6);
        composer.add_css_class("agent-composer");
        composer.append(&input_row);
        composer.append(&input_hint);

        let body = GBox::new(Orientation::Vertical, 8);
        body.set_margin_start(if compact { 8 } else { 12 });
        body.set_margin_end(if compact { 8 } else { 12 });
        body.set_margin_top(2);
        body.set_margin_bottom(if compact { 6 } else { 10 });
        body.append(&context_card);
        body.append(&proposal_box);
        body.append(&status_card);
        body.append(&composer);
        outer.append(&body);

        let card: gtk4::Widget = outer.clone().upcast();
        let runtime = Rc::new(AgentRuntime {
            session: RefCell::new(AgentSession::new(max_turns)),
            target: target.clone(),
            config: self.config.clone(),
            shell: shell.clone(),
            block_context: RefCell::new(block_context.clone()),
            card: card.clone(),
            input: input.clone(),
            send: send.clone(),
            stop_request: stop_request.clone(),
            retry_request: retry_request.clone(),
            context_clear: context_clear.clone(),
            context_card: context_card.clone(),
            status,
            status_spinner,
            turn_progress,
            turn_label,
            proposal_box,
            pending_command: RefCell::new(None),
            request_cancellation: RefCell::new(None),
            busy: Cell::new(false),
            alive: Cell::new(true),
        });
        let intro = if block_context.is_some() {
            "Bound to this Block pane with the selected finished Block attached as untrusted context. I can propose commands, but cannot run one without your explicit approval."
        } else {
            "Bound to this Block pane. I can propose commands, but cannot run one without your explicit approval."
        };
        runtime.append("Agent", intro);
        runtime.render_session_state(None);

        // Close this specific session: clear the UiState slot only when it
        // still holds this runtime (the pane's exited callback outlives the
        // session and must never tear down a newer one).
        let close_session = {
            let slot = self.agent_session.clone();
            let toggle = self.agent_toggle.clone();
            let weak = Rc::downgrade(&runtime);
            Rc::new(move || {
                let Some(runtime) = weak.upgrade() else {
                    return;
                };
                let is_current = slot
                    .borrow()
                    .as_ref()
                    .is_some_and(|session| Rc::ptr_eq(&session.runtime, &runtime));
                if is_current {
                    slot.borrow_mut().take();
                    toggle.set_active(false);
                }
                runtime.shutdown();
            })
        };

        let ui_for_settings = self.clone();
        let cwd_for_settings = cwd.clone();
        let shell_for_settings = shell.clone();
        settings_btn.connect_clicked(move |_| {
            show_agent_settings_dialog(&ui_for_settings, &cwd_for_settings, &shell_for_settings);
        });
        {
            let close_session = close_session.clone();
            close_btn.connect_clicked(move |_| close_session());
        }
        let weak: Weak<AgentRuntime> = Rc::downgrade(&runtime);
        target.connect_block_finished(move |command, exit_code, output| {
            if let Some(runtime) = weak.upgrade() {
                // The freshly finished block was inserted below this card;
                // re-pin the card so it stays directly above the live prompt.
                runtime.target.insert_inline_notice(&runtime.card);
                AgentRuntime::observe(runtime, command, exit_code, output);
            }
        });
        {
            let close_session = close_session.clone();
            target.connect_exited(move |_| close_session());
        }
        let weak = Rc::downgrade(&runtime);
        send.connect_clicked(move |_| {
            if let Some(runtime) = weak.upgrade() {
                AgentRuntime::submit(runtime);
            }
        });
        let weak = Rc::downgrade(&runtime);
        input.connect_activate(move |_| {
            if let Some(runtime) = weak.upgrade() {
                AgentRuntime::submit(runtime);
            }
        });
        let weak = Rc::downgrade(&runtime);
        input.connect_changed(move |_| {
            if let Some(runtime) = weak.upgrade() {
                runtime.sync_controls();
            }
        });
        let weak = Rc::downgrade(&runtime);
        stop_request.connect_clicked(move |_| {
            if let Some(runtime) = weak.upgrade() {
                runtime.stop_current_request();
            }
        });
        let weak = Rc::downgrade(&runtime);
        retry_request.connect_clicked(move |_| {
            if let Some(runtime) = weak.upgrade() {
                AgentRuntime::retry_model(runtime);
            }
        });
        let weak = Rc::downgrade(&runtime);
        context_clear.connect_clicked(move |_| {
            if let Some(runtime) = weak.upgrade() {
                runtime.detach_block_context();
            }
        });

        *self.agent_session.borrow_mut() = Some(AgentHandle { runtime });
        self.agent_toggle.set_active(true);
        target.insert_inline_notice(&card);
        input.grab_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::take_pending_for_finished_block;

    #[test]
    fn finished_block_consumes_approval_even_when_vte_capture_differs() {
        let mut pending = Some((7_u64, "cat monitor_xilem_bar.sh".to_string()));

        assert_eq!(take_pending_for_finished_block(&mut pending, "ls"), Some(7));
        assert!(pending.is_none());
    }

    #[test]
    fn finished_block_without_an_approval_is_ignored() {
        let mut pending: Option<(u64, String)> = None;

        assert_eq!(take_pending_for_finished_block(&mut pending, "ls"), None);
    }
}
