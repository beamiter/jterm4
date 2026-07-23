//! Unified fuzzy command palette over actions, persistent history, workflows
//! and the review-first AI command entry.

use adw::prelude::*;
use gtk4::gdk::{Key, ModifierType};
use gtk4::{
    Box as GBox, Button, EventControllerKey, Label, ListBox, Orientation, ScrolledWindow,
    SearchEntry, Spinner,
};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::{Rc, Weak};

use super::command_review::{CommandReviewCard, CommandReviewSpec, ReviewPresentation};
use super::UiState;
use crate::block_view::TermView;
use crate::palette::{Accept, Entry, PaletteMode, Query};

/// One live review-first natural-language command suggestion. It is kept
/// separate from the multi-turn Shell Agent state machine, but intentionally
/// uses the same Block-pinned review surface and interaction vocabulary.
pub(crate) struct CommandSuggestionHandle {
    runtime: Rc<CommandSuggestionRuntime>,
}

impl CommandSuggestionHandle {
    pub(crate) fn shutdown(&self) {
        self.runtime.shutdown();
    }
}

struct CommandSuggestionRuntime {
    target: Rc<TermView>,
    config: Rc<RefCell<crate::config::Config>>,
    slot: Weak<RefCell<Option<CommandSuggestionHandle>>>,
    card: gtk4::Widget,
    review_box: GBox,
    status: Label,
    spinner: Spinner,
    stop: Button,
    retry: Button,
    request: String,
    cwd: String,
    shell: String,
    block_context: Option<crate::ai::BlockContext>,
    cancellation: RefCell<Option<crate::ai::AiCancellationToken>>,
    busy: Cell<bool>,
    alive: Cell<bool>,
}

impl CommandSuggestionRuntime {
    fn set_status(&self, message: &str, active: bool, error: bool) {
        self.status.set_text(message);
        if error {
            self.status.add_css_class("error");
        } else {
            self.status.remove_css_class("error");
        }
        if active {
            self.spinner.start();
        } else {
            self.spinner.stop();
        }
        self.stop.set_visible(active);
        self.stop.set_sensitive(active);
        self.retry.set_visible(!active && error && self.alive.get());
        self.retry
            .set_sensitive(!active && error && self.alive.get());
    }

    fn clear_review(&self) {
        while let Some(child) = self.review_box.first_child() {
            self.review_box.remove(&child);
        }
        self.review_box.set_visible(false);
    }

    fn stop_current_request(&self) {
        if !self.alive.get() || !self.busy.get() {
            return;
        }
        if let Some(cancellation) = self.cancellation.borrow().as_ref() {
            cancellation.cancel();
            self.stop.set_sensitive(false);
            self.set_status("Stopping this suggestion request…", true, false);
        }
    }

    fn request_model(runtime: Rc<Self>) {
        if !runtime.alive.get() || runtime.busy.get() {
            return;
        }
        let client = match crate::ai::AiClient::from_config(&runtime.config.borrow()) {
            Ok(client) => client,
            Err(error) => {
                runtime.set_status(&error.to_string(), false, true);
                return;
            }
        };
        runtime.clear_review();
        runtime.busy.set(true);
        let provider = client.display_name();
        runtime.set_status(
            &format!("Drafting with {provider} for this Block pane…"),
            true,
            false,
        );
        let cancellation = crate::ai::AiCancellationToken::new();
        *runtime.cancellation.borrow_mut() = Some(cancellation.clone());
        let request = runtime.request.clone();
        let cwd = runtime.cwd.clone();
        let shell = runtime.shell.clone();
        let block_context = runtime.block_context.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = crate::ai::nl_to_command_with_context_blocking_cancellable(
                &client,
                &request,
                &cwd,
                &shell,
                std::env::consts::OS,
                block_context.as_ref(),
                &cancellation,
            );
            let _ = tx.send(result);
        });

        let rx = RefCell::new(rx);
        gtk4::glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            if !runtime.alive.get() {
                return gtk4::glib::ControlFlow::Break;
            }
            match rx.borrow().try_recv() {
                Ok(Ok(command)) => {
                    runtime.cancellation.borrow_mut().take();
                    runtime.busy.set(false);
                    runtime.set_status(
                        "Review the proposal below. Nothing has been inserted or run.",
                        false,
                        false,
                    );
                    Self::render_proposal(&runtime, command, &provider);
                    gtk4::glib::ControlFlow::Break
                }
                Ok(Err(error)) => {
                    runtime.cancellation.borrow_mut().take();
                    runtime.busy.set(false);
                    let message = if matches!(error, crate::ai::AiError::Cancelled) {
                        "Suggestion request stopped. Retry when ready.".to_string()
                    } else {
                        format!("Command suggestion failed: {error}")
                    };
                    runtime.set_status(&message, false, true);
                    gtk4::glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => gtk4::glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    runtime.cancellation.borrow_mut().take();
                    runtime.busy.set(false);
                    runtime.set_status(
                        "Command suggestion worker disconnected. Retry when ready.",
                        false,
                        true,
                    );
                    gtk4::glib::ControlFlow::Break
                }
            }
        });
    }

    fn render_proposal(runtime: &Rc<Self>, command: String, provider: &str) {
        runtime.clear_review();
        runtime.review_box.set_visible(true);
        let review = CommandReviewCard::new(CommandReviewSpec {
            presentation: ReviewPresentation::Embedded,
            compact: runtime.config.borrow().block_compact,
            icon: "\u{f0eb}", // nf-fa-lightbulb_o
            title: "Command proposal".to_string(),
            badge: provider.to_string(),
            description: format!("Generated for: {}", compact_one_line(&runtime.request, 140)),
            command,
            primary_label: "Insert for review".to_string(),
            primary_executes: false,
            auxiliary_label: None,
            secondary_label: Some("Regenerate".to_string()),
            close_button: false,
        });
        runtime.review_box.append(&review.root);

        if let Some(regenerate) = review.secondary.as_ref() {
            let weak = Rc::downgrade(runtime);
            regenerate.connect_clicked(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    Self::request_model(runtime);
                }
            });
        }
        {
            let weak = Rc::downgrade(runtime);
            let entry = review.entry.clone();
            let feedback = review.feedback.clone();
            review.primary.connect_clicked(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    Self::insert_for_review(runtime, &entry, &feedback);
                }
            });
        }
        {
            let weak = Rc::downgrade(runtime);
            let feedback = review.feedback.clone();
            review.entry.connect_activate(move |entry| {
                if let Some(runtime) = weak.upgrade() {
                    Self::insert_for_review(runtime, entry, &feedback);
                }
            });
        }
        review.focus();
    }

    fn insert_for_review(runtime: Rc<Self>, entry: &gtk4::Entry, feedback: &Label) {
        let command = match crate::review_input::validate(&entry.text()) {
            Ok(command) => command.to_string(),
            Err(error) => {
                feedback.set_text(&format!("Cannot insert: {error}"));
                feedback.add_css_class("error");
                feedback.set_visible(true);
                return;
            }
        };
        let prompt_status = runtime.target.command_prompt_status();
        if !prompt_status.is_ready() {
            feedback.set_text(prompt_status.blocked_message());
            feedback.add_css_class("error");
            feedback.set_visible(true);
            return;
        }
        runtime.target.grab_focus();
        runtime.target.write_input(command.as_bytes());
        Self::dismiss(runtime);
    }

    fn dismiss(runtime: Rc<Self>) {
        if let Some(slot) = runtime.slot.upgrade() {
            let is_current = slot
                .borrow()
                .as_ref()
                .is_some_and(|handle| Rc::ptr_eq(&handle.runtime, &runtime));
            if is_current {
                slot.borrow_mut().take();
            }
        }
        runtime.shutdown();
    }

    fn shutdown(&self) {
        if !self.alive.replace(false) {
            return;
        }
        if let Some(cancellation) = self.cancellation.borrow_mut().take() {
            cancellation.cancel();
            if !cancellation.wait_for_inactive(std::time::Duration::from_millis(500)) {
                log::warn!("Timed out waiting for the command suggestion request to stop");
            }
        }
        self.busy.set(false);
        self.spinner.stop();
        self.target.remove_inline_notice(&self.card);
    }
}

fn compact_one_line(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

fn clear_rows(list: &ListBox) {
    while let Some(row) = list.row_at_index(0) {
        list.remove(&row);
    }
}

fn render_rows(list: &ListBox, entries: &[Entry]) {
    clear_rows(list);
    for entry in entries {
        let row = adw::ActionRow::builder()
            .title(&entry.label)
            .activatable(true)
            .build();
        if let Some(subtitle) = entry.sublabel.as_deref() {
            row.set_subtitle(subtitle);
        }
        if let Some(right) = entry.right.as_deref() {
            let label = Label::new(Some(right));
            label.add_css_class("dim-label");
            row.add_suffix(&label);
        }
        list.append(&row);
    }
    if let Some(first) = list.row_at_index(0) {
        list.select_row(Some(&first));
    }
}

impl UiState {
    /// The single UI boundary for review-only shell insertion. Every caller
    /// gets the same control-character rejection and visible failure instead
    /// of silently turning a multi-line value into submitted shell input.
    pub(crate) fn insert_review_text(&self, pane: &crate::ui::PaneLeaf, text: &str) -> bool {
        match pane.write_review_input(text) {
            Ok(()) => {
                pane.grab_focus();
                true
            }
            Err(error) => {
                log::warn!("refusing unsafe review-only shell input: {error}");
                let dialog = adw::AlertDialog::new(
                    Some("Command was not inserted"),
                    Some(&format!(
                        "This review-only action cannot insert the command because {error}."
                    )),
                );
                dialog.add_response("ok", "OK");
                dialog.set_default_response(Some("ok"));
                dialog.present(Some(&self.window));
                false
            }
        }
    }

    fn gather_palette_entries(&self, raw_query: &str) -> Vec<Entry> {
        let workflows = crate::workflows::load_all();
        let history_path = {
            let config = self.config.borrow();
            config
                .command_history_enabled
                .then(|| config.command_history_path.clone())
                .flatten()
        };
        crate::palette::gather(
            &Query::parse(raw_query, PaletteMode::All),
            &self.keybinding_map.borrow(),
            history_path.as_deref().map(Path::new),
            &workflows,
            100,
        )
    }

    fn accept_palette_entry(&self, entry: Entry) {
        match entry.accept {
            Accept::Action(action) => self.execute_action(action),
            Accept::TypeCommand(command) => {
                if command.is_empty() {
                    return;
                }
                if let Some(pane) = self.current_pane_leaf() {
                    self.insert_review_text(&pane, &command);
                }
            }
            Accept::AskAi(request) => {
                if request.is_empty() {
                    return;
                }
                self.generate_command_for_review(request);
            }
            Accept::RunWorkflow(path) => {
                let Some(workflow) = crate::workflows::load_all()
                    .into_iter()
                    .find(|workflow| workflow.source_path == path)
                else {
                    log::warn!("workflow disappeared before activation: {}", path.display());
                    return;
                };
                let Some(pane) = self.current_pane_leaf() else {
                    return;
                };
                if workflow.args.is_empty() {
                    self.insert_review_text(&pane, &workflow.command);
                } else {
                    self.show_workflow_args_dialog(workflow, pane);
                }
            }
        }
    }

    fn generate_command_for_review(&self, request: String) {
        match crate::ai::AiClient::from_config(&self.config.borrow()) {
            Ok(_) => {}
            Err(error) => {
                self.show_ai_error(&error.to_string());
                return;
            }
        }
        let Some(target) = self.current_term_view() else {
            self.show_ai_error(
                "AI command suggestions require an active Block pane. Switch this pane to Block mode and try again.",
            );
            return;
        };
        if let Some(previous) = self.command_suggestion.borrow_mut().take() {
            previous.shutdown();
        }

        let compact = self.config.borrow().block_compact;
        let cwd = match target.cwd() {
            cwd if !cwd.is_empty() => cwd,
            _ => ".".to_string(),
        };
        let shell = self
            .shell_argv
            .borrow()
            .first()
            .cloned()
            .unwrap_or_else(|| "sh".to_string());
        let block_context = target.selected_block_context(80);

        let outer = GBox::new(Orientation::Vertical, 0);
        outer.add_css_class("block-finished");
        outer.add_css_class("block-assistant");
        outer.add_css_class("command-suggestion");
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
        header.set_margin_start(if compact { 8 } else { 12 });
        header.set_margin_end(if compact { 6 } else { 8 });
        header.set_margin_top(if compact { 3 } else { 6 });
        header.set_margin_bottom(if compact { 1 } else { 2 });
        let icon = Label::new(Some("\u{f0eb}")); // nf-fa-lightbulb_o
        icon.add_css_class("assistant-card-icon");
        header.append(&icon);
        let title = Label::new(Some("AI command suggestion"));
        title.add_css_class("assistant-card-title");
        title.set_xalign(0.0);
        header.append(&title);
        let binding = Label::new(Some(&format!("{cwd} · review only")));
        binding.add_css_class("assistant-card-badge");
        binding.set_hexpand(true);
        binding.set_halign(gtk4::Align::End);
        binding.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
        binding.set_tooltip_text(Some(&cwd));
        header.append(&binding);
        let close = Button::with_label("\u{2715}");
        close.add_css_class("flat");
        close.set_focusable(false);
        close.set_tooltip_text(Some("Stop and dismiss this suggestion (Esc)"));
        header.append(&close);
        outer.append(&header);

        let body = GBox::new(Orientation::Vertical, 7);
        body.set_margin_start(if compact { 8 } else { 12 });
        body.set_margin_end(if compact { 8 } else { 12 });
        body.set_margin_top(2);
        body.set_margin_bottom(if compact { 7 } else { 11 });
        let request_label = Label::new(Some(&format!(
            "Request: {}",
            compact_one_line(&request, 180)
        )));
        request_label.add_css_class("command-review-description");
        request_label.set_xalign(0.0);
        request_label.set_wrap(true);
        request_label.set_selectable(true);
        body.append(&request_label);
        if let Some(context) = block_context.as_ref() {
            let context_label = Label::new(Some(&format!(
                "Selected Block context · exit {}{} · {}",
                context.exit_code,
                if context.truncated {
                    " · output truncated"
                } else {
                    ""
                },
                compact_one_line(&context.cmd, 72)
            )));
            context_label.add_css_class("assistant-context-chip");
            context_label.set_xalign(0.0);
            context_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
            context_label.set_tooltip_text(Some(
                "Attached as bounded, untrusted command/output context for this request",
            ));
            body.append(&context_label);
        }

        let spinner = Spinner::new();
        let status = Label::new(Some("Preparing command suggestion…"));
        status.add_css_class("assistant-status");
        status.set_xalign(0.0);
        status.set_wrap(true);
        status.set_hexpand(true);
        status.set_accessible_role(gtk4::AccessibleRole::Status);
        let retry = Button::with_label("Retry");
        retry.set_visible(false);
        let stop = Button::with_label("Stop");
        stop.add_css_class("destructive-action");
        stop.set_visible(false);
        let status_row = GBox::new(Orientation::Horizontal, 7);
        status_row.add_css_class("assistant-status-row");
        status_row.append(&spinner);
        status_row.append(&status);
        status_row.append(&retry);
        status_row.append(&stop);
        body.append(&status_row);
        let review_box = GBox::new(Orientation::Vertical, 0);
        review_box.set_visible(false);
        body.append(&review_box);
        let hint = Label::new(Some(
            "Enter uses the labelled primary action · generated commands never run automatically",
        ));
        hint.add_css_class("agent-input-hint");
        hint.set_xalign(0.0);
        hint.set_wrap(true);
        body.append(&hint);
        outer.append(&body);

        let card: gtk4::Widget = outer.clone().upcast();
        let runtime = Rc::new(CommandSuggestionRuntime {
            target: target.clone(),
            config: self.config.clone(),
            slot: Rc::downgrade(&self.command_suggestion),
            card: card.clone(),
            review_box,
            status,
            spinner,
            stop: stop.clone(),
            retry: retry.clone(),
            request,
            cwd,
            shell,
            block_context,
            cancellation: RefCell::new(None),
            busy: Cell::new(false),
            alive: Cell::new(true),
        });

        {
            let weak = Rc::downgrade(&runtime);
            close.connect_clicked(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    CommandSuggestionRuntime::dismiss(runtime);
                }
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            stop.connect_clicked(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    runtime.stop_current_request();
                }
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            retry.connect_clicked(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    CommandSuggestionRuntime::request_model(runtime);
                }
            });
        }
        {
            let weak = Rc::downgrade(&runtime);
            let keys = EventControllerKey::new();
            keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == Key::Escape {
                    if let Some(runtime) = weak.upgrade() {
                        CommandSuggestionRuntime::dismiss(runtime);
                    }
                    gtk4::glib::Propagation::Stop
                } else {
                    gtk4::glib::Propagation::Proceed
                }
            });
            outer.add_controller(keys);
        }
        {
            let weak = Rc::downgrade(&runtime);
            target.connect_exited(move |_| {
                if let Some(runtime) = weak.upgrade() {
                    CommandSuggestionRuntime::dismiss(runtime);
                }
            });
        }

        *self.command_suggestion.borrow_mut() = Some(CommandSuggestionHandle {
            runtime: runtime.clone(),
        });
        target.insert_inline_notice(&card);
        CommandSuggestionRuntime::request_model(runtime);
    }

    pub(crate) fn toggle_unified_command_palette(&self) {
        // `force_close` emits `closed` synchronously; release this borrow first
        // so its callback can clear the slot without a RefCell re-entry panic.
        let dialog_to_close = self.command_palette_dialog.borrow_mut().take();
        if let Some(dialog) = dialog_to_close {
            dialog.force_close();
            return;
        }

        let dialog = adw::Dialog::builder()
            .title("Command Palette")
            .content_width(620)
            .content_height(520)
            .build();
        let header = adw::HeaderBar::new();
        let filter = SearchEntry::new();
        filter.set_placeholder_text(Some(
            "Search all · > actions · @ history · : workflows · ? ask AI",
        ));
        filter.set_hexpand(true);

        let list = ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::Single);
        list.add_css_class("boxed-list");
        list.set_margin_start(12);
        list.set_margin_end(12);
        list.set_margin_bottom(12);
        let entries = Rc::new(RefCell::new(self.gather_palette_entries("")));
        render_rows(&list, &entries.borrow());

        let scrolled = ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&list)
            .build();
        let body = gtk4::Box::new(Orientation::Vertical, 0);
        filter.set_margin_start(12);
        filter.set_margin_end(12);
        filter.set_margin_top(8);
        filter.set_margin_bottom(8);
        body.append(&filter);
        body.append(&scrolled);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&body));
        dialog.set_child(Some(&toolbar));

        {
            let ui = self.clone();
            let list = list.clone();
            let entries = entries.clone();
            filter.connect_search_changed(move |entry| {
                let next = ui.gather_palette_entries(&entry.text());
                render_rows(&list, &next);
                *entries.borrow_mut() = next;
            });
        }

        let activate = {
            let ui = self.clone();
            let entries = entries.clone();
            let dialog = dialog.clone();
            Rc::new(move |index: usize| {
                let Some(entry) = entries.borrow().get(index).cloned() else {
                    return;
                };
                dialog.force_close();
                ui.accept_palette_entry(entry);
            })
        };
        {
            let activate = activate.clone();
            list.connect_row_activated(move |_, row| activate(row.index() as usize));
        }

        let keys = EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        {
            let list = list.clone();
            let dialog = dialog.clone();
            let dialog_slot = self.command_palette_dialog.clone();
            let activate = activate.clone();
            keys.connect_key_pressed(move |_, key, _, state| {
                if key == Key::Escape
                    || (matches!(key, Key::P | Key::p)
                        && state.contains(ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK))
                {
                    dialog_slot.borrow_mut().take();
                    dialog.force_close();
                    return true.into();
                }
                if matches!(key, Key::Return | Key::KP_Enter) {
                    if let Some(row) = list.selected_row() {
                        activate(row.index() as usize);
                    }
                    return true.into();
                }
                if matches!(key, Key::Down | Key::Up) {
                    let current = list.selected_row().map(|row| row.index()).unwrap_or(0);
                    let next = if key == Key::Down {
                        current.saturating_add(1)
                    } else {
                        current.saturating_sub(1)
                    };
                    if let Some(row) = list.row_at_index(next) {
                        list.select_row(Some(&row));
                    }
                    return true.into();
                }
                false.into()
            });
        }
        dialog.add_controller(keys);

        let slot = self.command_palette_dialog.clone();
        dialog.connect_closed(move |_| *slot.borrow_mut() = None);
        *self.command_palette_dialog.borrow_mut() = Some(dialog.clone());
        dialog.present(Some(&self.window));
        filter.grab_focus();
    }
}
