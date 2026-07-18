//! Native GTK shell Agent UI. The model can only propose commands; every
//! command remains editable and requires an explicit per-command approval.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk4::{
    Box as GBox, Button, Entry, Label, Orientation, ScrolledWindow, TextBuffer, TextView, WrapMode,
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

struct AgentRuntime {
    session: RefCell<AgentSession>,
    target: Rc<TermView>,
    config: Rc<RefCell<crate::config::Config>>,
    shell: String,
    transcript: TextBuffer,
    transcript_view: TextView,
    input: Entry,
    send: Button,
    cancel: Button,
    status: Label,
    proposal_box: GBox,
    pending_command: RefCell<Option<(ProposalId, String)>>,
    busy: Cell<bool>,
    alive: Cell<bool>,
}

impl AgentRuntime {
    fn append(&self, speaker: &str, body: &str) {
        let mut end = self.transcript.end_iter();
        if self.transcript.char_count() > 0 {
            self.transcript.insert(&mut end, "\n\n");
        }
        self.transcript
            .insert(&mut end, &format!("{speaker}\n{body}"));
        let view = self.transcript_view.clone();
        let buffer = self.transcript.clone();
        gtk4::glib::idle_add_local_once(move || {
            let mut end = buffer.end_iter();
            view.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
        });
    }

    fn clear_proposal(&self) {
        while let Some(child) = self.proposal_box.first_child() {
            self.proposal_box.remove(&child);
        }
        self.proposal_box.set_visible(false);
    }

    fn set_ready(&self) {
        let ready = self.alive.get()
            && !self.busy.get()
            && self.session.borrow().state() == AgentState::Ready;
        self.input.set_sensitive(ready);
        self.send.set_sensitive(ready);
        if ready {
            self.status.set_text(&format!(
                "Ready · turn {}/{}",
                self.session.borrow().turns_used(),
                self.session.borrow().max_turns()
            ));
            self.input.grab_focus();
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
        if let Err(error) = runtime.session.borrow_mut().submit_user(text.clone()) {
            runtime.status.set_text(&error.to_string());
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
            runtime.set_ready();
            return;
        }

        let client = match crate::ai::AiClient::from_config(&runtime.config.borrow()) {
            Ok(client) => client,
            Err(error) => {
                let message = error.to_string();
                let _ = runtime.session.borrow_mut().model_failed(&message);
                runtime.append("Error", &message);
                runtime.status.set_text(&message);
                runtime.set_ready();
                return;
            }
        };
        let cwd = runtime.target.cwd();
        let system = crate::ai::build_agent_system_prompt(
            if cwd.is_empty() { "." } else { &cwd },
            &runtime.shell,
            std::env::consts::OS,
        );
        let prompt = runtime.session.borrow().build_user_prompt();
        let cancellation = runtime.session.borrow().cancellation_token();

        runtime.busy.set(true);
        runtime.input.set_sensitive(false);
        runtime.send.set_sensitive(false);
        runtime.status.set_text(&format!(
            "Thinking… ({}, turn {}/{})",
            client.display_name(),
            runtime.session.borrow().turns_used() + 1,
            runtime.session.borrow().max_turns()
        ));

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if cancellation.is_cancelled() {
                return;
            }
            let result = client.send_turns_blocking(
                Some(&system),
                &[crate::ai::Turn {
                    role: crate::ai::Role::User,
                    text: prompt,
                }],
            );
            if !cancellation.is_cancelled() {
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
                            runtime.set_ready();
                        }
                        Ok(ModelOutcome::Completed(message)) => {
                            runtime.append("Agent", &message);
                            runtime.status.set_text("Completed");
                            runtime.input.set_sensitive(false);
                            runtime.send.set_sensitive(false);
                        }
                        Err(error) => {
                            let message = error.to_string();
                            runtime.append("Protocol error", &message);
                            runtime.status.set_text(&message);
                            runtime.set_ready();
                        }
                    }
                    gtk4::glib::ControlFlow::Break
                }
                Ok(Err(error)) => {
                    runtime.busy.set(false);
                    let message = error.to_string();
                    let _ = runtime.session.borrow_mut().model_failed(&message);
                    runtime.append("Error", &message);
                    runtime.status.set_text(&message);
                    runtime.set_ready();
                    gtk4::glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => gtk4::glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    runtime.busy.set(false);
                    let message = "Agent worker disconnected.";
                    let _ = runtime.session.borrow_mut().model_failed(message);
                    runtime.append("Error", message);
                    runtime.status.set_text(message);
                    runtime.set_ready();
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
            "Proposed command — edit before approval if needed".to_string()
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

        let approve = Button::with_label("Approve & Run");
        approve.add_css_class(if danger.is_some() {
            "destructive-action"
        } else {
            "suggested-action"
        });
        let reject = Button::with_label("Reject");
        let buttons = GBox::new(Orientation::Horizontal, 6);
        buttons.set_halign(gtk4::Align::End);
        buttons.append(&reject);
        buttons.append(&approve);

        runtime.proposal_box.append(&warning);
        runtime.proposal_box.append(&command_entry);
        runtime.proposal_box.append(&buttons);
        runtime
            .status
            .set_text(&format!("Proposal #{} requires your approval", id.get()));

        let weak = Rc::downgrade(runtime);
        let entry_for_approve = command_entry.clone();
        approve.connect_clicked(move |_| {
            if let Some(runtime) = weak.upgrade() {
                Self::approve(runtime, id, entry_for_approve.text().to_string());
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
        if !runtime.target.can_accept_agent_command() {
            let message =
                "The target prompt is busy or already contains input. Clear it and approve again.";
            runtime.status.set_text(message);
            runtime.append("Safety check", message);
            return;
        }
        let approved = match runtime.session.borrow_mut().edit_and_approve(id, command) {
            Ok(approved) => approved,
            Err(error) => {
                runtime.status.set_text(&error.to_string());
                return;
            }
        };
        runtime.clear_proposal();
        runtime.append("Approved", &format!("$ {}", approved.command));
        runtime.status.set_text("Running approved command…");
        *runtime.pending_command.borrow_mut() =
            Some((approved.proposal_id, approved.command.clone()));
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
            Err(error) => runtime.status.set_text(&error.to_string()),
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
        if let Err(error) = runtime.session.borrow_mut().observe(id, exit_code, &output) {
            runtime.status.set_text(&error.to_string());
            return;
        }
        let output = if output.trim().is_empty() {
            "(no output)".to_string()
        } else {
            output
        };
        runtime.append("Command result", &format!("exit {exit_code}\n{output}"));
        Self::request_model(runtime);
    }

    fn cancel(&self) {
        if !self.alive.replace(false) {
            return;
        }
        self.session.borrow_mut().cancel();
        self.pending_command.borrow_mut().take();
        self.busy.set(false);
        self.clear_proposal();
        self.input.set_sensitive(false);
        self.send.set_sensitive(false);
        self.cancel.set_sensitive(false);
        self.status.set_text("Agent cancelled");
    }
}

impl UiState {
    /// Keep the visible top-bar Agent control aligned with both configuration
    /// availability and the lifetime of the active Agent dialog.
    pub(crate) fn sync_agent_toggle(&self) {
        let available = {
            let config = self.config.borrow();
            config.ai_enabled && config.agent_enabled
        };
        self.agent_toggle.set_sensitive(available);

        if !available {
            // Drop the RefCell borrow before force_close because `closed`
            // clears the same slot synchronously.
            let dialog_to_close = self.agent_dialog.borrow_mut().take();
            if let Some(dialog) = dialog_to_close {
                dialog.force_close();
            }
            self.agent_toggle.set_active(false);
        } else {
            self.agent_toggle
                .set_active(self.agent_dialog.borrow().is_some());
        }
    }

    pub(crate) fn toggle_agent_panel(&self) {
        // Drop the RefCell borrow before `force_close`: libadwaita emits
        // `closed` synchronously and that callback clears the same slot.
        let dialog_to_close = self.agent_dialog.borrow_mut().take();
        if let Some(dialog) = dialog_to_close {
            dialog.force_close();
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
        drop(config);
        let Some(target) = self.current_term_view() else {
            self.agent_toggle.set_active(false);
            self.show_ai_error("Agent mode requires an active Block pane.");
            return;
        };

        let dialog = adw::Dialog::builder()
            .title("Shell Agent")
            .content_width(720)
            .content_height(640)
            .build();
        let header = adw::HeaderBar::new();
        let cancel = Button::with_label("Cancel Agent");
        cancel.add_css_class("destructive-action");
        header.pack_end(&cancel);

        let transcript = TextBuffer::new(None);
        let transcript_view = TextView::with_buffer(&transcript);
        transcript_view.set_editable(false);
        transcript_view.set_cursor_visible(false);
        transcript_view.set_wrap_mode(WrapMode::WordChar);
        transcript_view.set_monospace(true);
        transcript_view.set_left_margin(10);
        transcript_view.set_right_margin(10);
        transcript_view.set_top_margin(10);
        transcript_view.set_bottom_margin(10);
        let transcript_scroll = ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&transcript_view)
            .build();

        let status = Label::new(Some("Ready"));
        status.set_xalign(0.0);
        status.set_wrap(true);
        status.add_css_class("dim-label");
        let proposal_box = GBox::new(Orientation::Vertical, 6);
        proposal_box.add_css_class("card");
        proposal_box.set_margin_start(8);
        proposal_box.set_margin_end(8);
        proposal_box.set_margin_top(4);
        proposal_box.set_margin_bottom(4);
        proposal_box.set_visible(false);

        let input = Entry::new();
        input.set_hexpand(true);
        input.set_placeholder_text(Some(
            "Describe the task; each command will require approval",
        ));
        let send = Button::with_label("Send");
        send.add_css_class("suggested-action");
        let input_row = GBox::new(Orientation::Horizontal, 6);
        input_row.append(&input);
        input_row.append(&send);

        let body = GBox::new(Orientation::Vertical, 6);
        body.set_margin_start(8);
        body.set_margin_end(8);
        body.set_margin_bottom(8);
        body.append(&transcript_scroll);
        body.append(&proposal_box);
        body.append(&status);
        body.append(&input_row);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&body));
        dialog.set_child(Some(&toolbar));

        let runtime = Rc::new(AgentRuntime {
            session: RefCell::new(AgentSession::new(max_turns)),
            target: target.clone(),
            config: self.config.clone(),
            shell: self
                .shell_argv
                .borrow()
                .first()
                .cloned()
                .unwrap_or_else(|| "sh".to_string()),
            transcript,
            transcript_view,
            input: input.clone(),
            send: send.clone(),
            cancel: cancel.clone(),
            status,
            proposal_box,
            pending_command: RefCell::new(None),
            busy: Cell::new(false),
            alive: Cell::new(true),
        });
        runtime.append(
            "Agent",
            "Bound to this Block pane. I can propose commands, but cannot run one without your explicit approval.",
        );
        runtime.set_ready();

        let weak: Weak<AgentRuntime> = Rc::downgrade(&runtime);
        target.connect_block_finished(move |command, exit_code, output| {
            if let Some(runtime) = weak.upgrade() {
                AgentRuntime::observe(runtime, command, exit_code, output);
            }
        });
        let weak = Rc::downgrade(&runtime);
        target.connect_exited(move |_| {
            if let Some(runtime) = weak.upgrade() {
                runtime.cancel();
                runtime
                    .status
                    .set_text("Target pane exited; Agent cancelled");
            }
        });
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
        cancel.connect_clicked(move |_| {
            if let Some(runtime) = weak.upgrade() {
                runtime.cancel();
            }
        });

        let slot = self.agent_dialog.clone();
        let agent_toggle = self.agent_toggle.clone();
        let weak = Rc::downgrade(&runtime);
        unsafe {
            dialog.set_data::<Rc<AgentRuntime>>("jterm4-agent-runtime", runtime.clone());
        }
        dialog.connect_closed(move |closed_dialog| {
            if let Some(runtime) = weak.upgrade() {
                runtime.cancel();
            }
            unsafe {
                let _ = closed_dialog.steal_data::<Rc<AgentRuntime>>("jterm4-agent-runtime");
            }
            *slot.borrow_mut() = None;
            agent_toggle.set_active(false);
        });
        *self.agent_dialog.borrow_mut() = Some(dialog.clone());
        self.agent_toggle.set_active(true);
        dialog.present(Some(&self.window));
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
