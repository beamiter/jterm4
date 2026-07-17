//! ai_panel — provider-neutral right-side chat sidebar.
//!
//! Layout: vertical column = [header row | conversation scroll | status label |
//! input row]. The input is a multi-line TextView so users can paste shell
//! errors without losing newlines. Enter and Ctrl+Enter send; Shift+Enter
//! inserts a newline. Input-method events are offered to TextView before the
//! send shortcut so confirming an IME candidate never submits the message.
//!
//! Networking runs on a worker `std::thread`; a bounded main-loop timer polls
//! its channel for the response or error. While a request is in flight the
//! send button is desensitised and the status row shows provider activity.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Box as GBox, Button, EventControllerKey, Label, Orientation, Overlay, ScrolledWindow, Spinner,
    TextBuffer, TextTag, TextView, WrapMode,
};

use crate::ai::{self, BlockContext, Role, Turn};
use crate::config::Config;

type PersistenceCallback = Rc<dyn Fn()>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ComposerKeyAction {
    Send,
    Newline,
    Proceed,
}

fn classify_composer_key(
    keyval: gtk4::gdk::Key,
    state: gtk4::gdk::ModifierType,
) -> ComposerKeyAction {
    let is_enter = matches!(keyval, gtk4::gdk::Key::Return | gtk4::gdk::Key::KP_Enter);
    if !is_enter {
        return ComposerKeyAction::Proceed;
    }
    if state.contains(gtk4::gdk::ModifierType::SHIFT_MASK) {
        ComposerKeyAction::Newline
    } else if state.intersects(
        gtk4::gdk::ModifierType::ALT_MASK
            | gtk4::gdk::ModifierType::SUPER_MASK
            | gtk4::gdk::ModifierType::HYPER_MASK
            | gtk4::gdk::ModifierType::META_MASK,
    ) {
        ComposerKeyAction::Proceed
    } else {
        ComposerKeyAction::Send
    }
}

fn conversation_snapshot_for_persistence(
    history: &[Turn],
    block_context: Option<&BlockContext>,
    redact: bool,
) -> Option<ai::ConversationSnapshot> {
    let mut history = history.to_vec();
    let mut block_context = block_context.cloned();

    // A user may enable redaction after restoring a conversation that was
    // originally saved with the opt-out setting. Scrub a persistence-only
    // copy so the next snapshot upgrades every retained field without
    // rewriting the visible transcript or provider history mid-conversation.
    if redact {
        for turn in &mut history {
            turn.text = crate::redact::redact_secrets(&turn.text);
        }
        if let Some(context) = block_context.as_mut() {
            context.cmd = crate::redact::redact_secrets(&context.cmd);
            context.output = crate::redact::redact_secrets(&context.output);
            context.cwd = context
                .cwd
                .take()
                .map(|cwd| crate::redact::redact_secrets(&cwd));
        }
    }

    ai::ConversationSnapshot::from_completed_history(&history, block_context.as_ref())
}

/// Provider-facing transcript plus a monotonically increasing request token.
/// Clearing a conversation invalidates the active token, so a response that
/// was already in flight can never repopulate the cleared transcript or append
/// an assistant turn without its matching user turn.
#[derive(Default)]
struct ConversationState {
    history: Vec<Turn>,
    epoch: u64,
    active_epoch: Option<u64>,
}

impl ConversationState {
    fn is_busy(&self) -> bool {
        self.active_epoch.is_some()
    }

    fn begin(&mut self, user_text: String) -> (u64, Vec<Turn>) {
        self.epoch = self.epoch.wrapping_add(1);
        let epoch = self.epoch;
        self.history.push(Turn {
            role: Role::User,
            text: user_text,
        });
        self.active_epoch = Some(epoch);
        (epoch, self.history.clone())
    }

    fn clear(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.active_epoch = None;
        self.history.clear();
    }

    fn restore(&mut self, history: Vec<Turn>) {
        self.epoch = self.epoch.wrapping_add(1);
        self.active_epoch = None;
        self.history = history;
    }

    fn complete_success(&mut self, epoch: u64, text: String) -> bool {
        if self.active_epoch != Some(epoch) {
            return false;
        }
        self.active_epoch = None;
        self.history.push(Turn {
            role: Role::Assistant,
            text,
        });
        true
    }

    fn complete_error(&mut self, epoch: u64) -> bool {
        if self.active_epoch != Some(epoch) {
            return false;
        }
        self.active_epoch = None;
        if self
            .history
            .last()
            .is_some_and(|turn| turn.role == Role::User)
        {
            self.history.pop();
        }
        true
    }
}

/// All widgets + state the AI panel needs to drive itself.
#[derive(Clone)]
pub(crate) struct AiPanel {
    /// Root box returned from `build`. Add this as the end child of the
    /// outer Paned in main.rs.
    pub(crate) root: GBox,
    conversation: Rc<RefCell<ConversationState>>,
    convo_buffer: TextBuffer,
    convo_view: TextView,
    convo_scroll: ScrolledWindow,
    empty_state: GBox,
    input_buffer: TextBuffer,
    input_view: TextView,
    input_placeholder: Label,
    send_btn: Button,
    status_row: GBox,
    status_spinner: Spinner,
    status_label: Label,
    header_meta: Label,
    close_btn: Button,
    block_context: Rc<RefCell<Option<BlockContext>>>,
    persistence_callback: Rc<RefCell<Option<PersistenceCallback>>>,
    config: Rc<std::cell::RefCell<Config>>,
}

impl AiPanel {
    /// Build the panel widget tree. The returned root is hidden / shown by
    /// `UiState::toggle_ai_panel` — visibility is not owned here.
    pub(crate) fn build(config: Rc<std::cell::RefCell<Config>>) -> Self {
        let header = GBox::new(Orientation::Horizontal, 6);
        header.add_css_class("ai-panel-header");
        let title = Label::new(Some("AI Assistant"));
        title.set_halign(gtk4::Align::Start);
        title.add_css_class("ai-panel-title");
        let header_meta = Label::new(None);
        header_meta.set_halign(gtk4::Align::Start);
        header_meta.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        header_meta.add_css_class("ai-panel-subtitle");
        let header_text = GBox::new(Orientation::Vertical, 0);
        header_text.append(&title);
        header_text.append(&header_meta);
        header_text.set_hexpand(true);
        let clear_btn = Button::with_label("New chat");
        clear_btn.set_tooltip_text(Some(
            "Start a new chat; an in-flight response will be ignored",
        ));
        clear_btn.set_focus_on_click(false);
        clear_btn.add_css_class("flat");
        let close_btn = Button::from_icon_name("window-close-symbolic");
        close_btn.set_tooltip_text(Some("Close AI panel"));
        close_btn.set_focus_on_click(false);
        close_btn.add_css_class("flat");
        header.append(&header_text);
        header.append(&clear_btn);
        header.append(&close_btn);

        // Conversation transcript remains a single selectable TextView so a
        // copied range can span multiple messages. Role labels carry the
        // semantic distinction without relying on theme-specific hard-coded
        // blue/green body colors.
        let convo_buffer = TextBuffer::new(None);
        let tag_table = convo_buffer.tag_table();
        // 700 = Pango bold; using the integer avoids depending on
        // pango::Weight's ABI conversion.
        let user_tag = TextTag::builder().name("role-user").weight(700).build();
        let asst_tag = TextTag::builder().name("role-asst").weight(700).build();
        let err_tag = TextTag::builder()
            .name("role-err")
            .foreground("#e01b24")
            .weight(700)
            .build();
        tag_table.add(&user_tag);
        tag_table.add(&asst_tag);
        tag_table.add(&err_tag);

        let convo_view = TextView::with_buffer(&convo_buffer);
        convo_view.set_editable(false);
        convo_view.set_cursor_visible(false);
        convo_view.set_focusable(true);
        convo_view.set_wrap_mode(WrapMode::WordChar);
        convo_view.set_monospace(false);
        convo_view.set_top_margin(12);
        convo_view.set_bottom_margin(12);
        convo_view.set_left_margin(12);
        convo_view.set_right_margin(12);
        convo_view.add_css_class("ai-transcript");
        convo_view.set_accessible_role(gtk4::AccessibleRole::Log);
        convo_view.update_property(&[
            gtk4::accessible::Property::Label("AI conversation"),
            gtk4::accessible::Property::ReadOnly(true),
        ]);

        let convo_scroll = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .vexpand(true)
            .child(&convo_view)
            .build();

        let empty_title = Label::new(Some("Ask about this terminal"));
        empty_title.add_css_class("ai-empty-title");
        let empty_hint = Label::new(Some(
            "Ask about a command, error, or output.\nYou can also select a finished block and press Ctrl+Shift+Q.",
        ));
        empty_hint.set_justify(gtk4::Justification::Center);
        empty_hint.set_wrap(true);
        let empty_safety = Label::new(Some("Review generated commands before running them."));
        empty_safety.add_css_class("dim-label");
        empty_safety.set_wrap(true);
        empty_safety.set_justify(gtk4::Justification::Center);
        let empty_state = GBox::new(Orientation::Vertical, 8);
        empty_state.add_css_class("ai-empty-state");
        empty_state.set_halign(gtk4::Align::Center);
        empty_state.set_valign(gtk4::Align::Center);
        empty_state.set_can_target(false);
        empty_state.append(&empty_title);
        empty_state.append(&empty_hint);
        empty_state.append(&empty_safety);
        let transcript_overlay = Overlay::new();
        transcript_overlay.set_child(Some(&convo_scroll));
        transcript_overlay.add_overlay(&empty_state);
        transcript_overlay.set_vexpand(true);

        // A compact status row is hidden when idle and exposes activity with a
        // spinner instead of changing the Send button label back and forth.
        let status_spinner = Spinner::new();
        status_spinner.set_visible(false);
        let status_label = Label::new(None);
        status_label.set_halign(gtk4::Align::Start);
        status_label.set_hexpand(true);
        status_label.set_wrap(true);
        status_label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        status_label.set_selectable(true);
        let status_row = GBox::new(Orientation::Horizontal, 6);
        status_row.add_css_class("ai-panel-status-row");
        status_row.append(&status_spinner);
        status_row.append(&status_label);
        status_row.set_visible(false);
        status_row.set_accessible_role(gtk4::AccessibleRole::Status);

        // Composer: a growing TextView, overlay placeholder, explicit shortcut
        // hint, and a bottom-aligned action button.
        let input_buffer = TextBuffer::new(None);
        let input_view = TextView::with_buffer(&input_buffer);
        input_view.set_wrap_mode(WrapMode::WordChar);
        input_view.set_top_margin(7);
        input_view.set_bottom_margin(7);
        input_view.set_left_margin(8);
        input_view.set_right_margin(8);
        input_view.set_accepts_tab(false);
        input_view.update_property(&[
            gtk4::accessible::Property::Label("AI message"),
            gtk4::accessible::Property::Placeholder("Ask about commands, errors, or output"),
            gtk4::accessible::Property::MultiLine(true),
            gtk4::accessible::Property::KeyShortcuts("Enter; Control+Enter; Shift+Enter"),
        ]);
        let input_scroll = ScrolledWindow::builder()
            .hexpand(true)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .min_content_height(72)
            .max_content_height(180)
            .child(&input_view)
            .build();
        input_scroll.add_css_class("ai-panel-input");

        let input_placeholder = Label::new(Some("Ask about commands, errors, or output…"));
        input_placeholder.set_halign(gtk4::Align::Start);
        input_placeholder.set_valign(gtk4::Align::Start);
        input_placeholder.set_can_target(false);
        input_placeholder.add_css_class("ai-input-placeholder");
        let input_overlay = Overlay::new();
        input_overlay.set_child(Some(&input_scroll));
        input_overlay.add_overlay(&input_placeholder);

        let send_btn = Button::with_label("Send");
        send_btn.set_focus_on_click(false);
        send_btn.set_sensitive(false);
        send_btn.set_valign(gtk4::Align::Center);
        send_btn.add_css_class("suggested-action");
        send_btn.add_css_class("ai-send-button");
        send_btn.set_tooltip_text(Some("Send (Enter / Ctrl+Enter) · New line (Shift+Enter)"));

        let input_hint = Label::new(Some("Enter to send · Shift+Enter for new line"));
        input_hint.set_halign(gtk4::Align::Start);
        input_hint.set_hexpand(true);
        input_hint.add_css_class("ai-input-hint");
        let composer_actions = GBox::new(Orientation::Horizontal, 8);
        composer_actions.append(&input_hint);
        composer_actions.append(&send_btn);
        let composer = GBox::new(Orientation::Vertical, 6);
        composer.add_css_class("ai-panel-composer");
        composer.append(&input_overlay);
        composer.append(&composer_actions);

        let root = GBox::new(Orientation::Vertical, 0);
        root.add_css_class("ai-panel");
        root.set_hexpand(false);
        root.set_vexpand(true);
        root.append(&header);
        root.append(&transcript_overlay);
        root.append(&status_row);
        root.append(&composer);

        let panel = AiPanel {
            root,
            conversation: Rc::new(RefCell::new(ConversationState::default())),
            convo_buffer: convo_buffer.clone(),
            convo_view,
            convo_scroll: convo_scroll.clone(),
            empty_state,
            input_buffer: input_buffer.clone(),
            input_view: input_view.clone(),
            input_placeholder,
            send_btn: send_btn.clone(),
            status_row,
            status_spinner,
            status_label: status_label.clone(),
            header_meta,
            close_btn,
            block_context: Rc::new(RefCell::new(None)),
            persistence_callback: Rc::new(RefCell::new(None)),
            config,
        };
        panel.refresh_config_display();

        // Wire Clear: drop history + clear the transcript buffer.
        {
            let p = panel.clone();
            clear_btn.connect_clicked(move |_| {
                let interrupted = p.conversation.borrow().is_busy();
                p.conversation.borrow_mut().clear();
                *p.block_context.borrow_mut() = None;
                p.convo_buffer.set_text("");
                p.sync_empty_state();
                if interrupted {
                    p.set_info_status("New chat started; the previous response will be ignored.");
                } else {
                    p.clear_status();
                }
                p.sync_composer_state();
                p.publish_persisted_conversation();
            });
        }

        // Wire Send button.
        {
            let p = panel.clone();
            send_btn.connect_clicked(move |_| {
                p.send_from_input(None);
            });
        }

        {
            let p = panel.clone();
            input_buffer.connect_changed(move |_| p.sync_composer_state());
        }

        // Offer Enter to the TextView IM context first. If an active input
        // method consumes it to confirm a candidate, Stop prevents accidental
        // submission. Otherwise Enter sends and Shift+Enter reaches TextView's
        // native newline path (Shift wins even when Ctrl is also held).
        {
            let key = EventControllerKey::new();
            key.set_propagation_phase(gtk4::PropagationPhase::Capture);
            let p = panel.clone();
            let input_for_key = input_view.clone();
            key.connect_key_pressed(move |controller, keyval, _code, state| {
                let action = classify_composer_key(keyval, state);
                if action != ComposerKeyAction::Proceed {
                    if let Some(event) = controller.current_event() {
                        if input_for_key.im_context_filter_keypress(&event) {
                            return glib::Propagation::Stop;
                        }
                    }
                }
                match action {
                    ComposerKeyAction::Send => {
                        p.send_from_input(None);
                        glib::Propagation::Stop
                    }
                    ComposerKeyAction::Newline | ComposerKeyAction::Proceed => {
                        glib::Propagation::Proceed
                    }
                }
            });
            input_view.add_controller(key);
        }

        panel.sync_empty_state();
        panel.sync_composer_state();
        panel
    }

    pub(crate) fn refresh_config_display(&self) {
        let config = self.config.borrow();
        let provider = match config.ai_provider.as_str() {
            "openai-compatible" => "OpenAI-compatible",
            "ollama" => "Ollama",
            _ => "Anthropic",
        };
        let summary = if config.ai_model.trim().is_empty() {
            provider.to_string()
        } else {
            format!("{provider} · {}", config.ai_model.trim())
        };
        self.header_meta.set_text(&summary);
        self.header_meta.set_tooltip_text(Some(&summary));
    }

    pub(crate) fn connect_close_requested(&self, callback: impl Fn() + 'static) {
        self.close_btn.connect_clicked(move |_| callback());
    }

    pub(crate) fn set_persistence_callback(&self, callback: impl Fn() + 'static) {
        *self.persistence_callback.borrow_mut() = Some(Rc::new(callback));
    }

    /// Restore only after `load_tabs_state` has claimed this window's snapshot.
    /// Safe mode and `--no-restore` never call this method.
    pub(crate) fn restore_persisted_conversation(&self) {
        let Some(snapshot) = crate::state::get_ai_conversation_snapshot() else {
            return;
        };
        let (turns, context) = snapshot.into_parts();
        let has_context = context.is_some();
        self.conversation.borrow_mut().restore(turns.clone());
        *self.block_context.borrow_mut() = context;
        self.convo_buffer.set_text("");
        for turn in turns {
            match turn.role {
                Role::User => self.insert_visible("You", "role-user", &turn.text),
                Role::Assistant => self.insert_visible("Assistant", "role-asst", &turn.text),
            }
        }
        self.sync_empty_state();
        self.set_info_status(if has_context {
            "Conversation and selected-block context restored for this window."
        } else {
            "Conversation restored for this window."
        });
        self.scroll_transcript_to_end();
        self.sync_composer_state();
    }

    pub(crate) fn focus_input(&self) {
        self.input_view.grab_focus();
    }

    pub(crate) fn input_has_focus(&self) -> bool {
        self.input_view.has_focus()
    }

    fn sync_empty_state(&self) {
        self.empty_state
            .set_visible(self.convo_buffer.char_count() == 0);
    }

    fn input_text(&self) -> String {
        let start = self.input_buffer.start_iter();
        let end = self.input_buffer.end_iter();
        self.input_buffer.text(&start, &end, false).to_string()
    }

    fn sync_composer_state(&self) {
        let text = self.input_text();
        self.input_placeholder.set_visible(text.is_empty());
        self.send_btn
            .set_sensitive(!self.conversation.borrow().is_busy() && !text.trim().is_empty());
    }

    fn clear_status(&self) {
        self.root
            .update_state(&[gtk4::accessible::State::Busy(false)]);
        self.status_spinner.stop();
        self.status_spinner.set_visible(false);
        self.status_label.set_text("");
        self.status_row.remove_css_class("error");
        self.status_row.set_visible(false);
    }

    fn set_busy_status(&self, message: &str) {
        self.root
            .update_state(&[gtk4::accessible::State::Busy(true)]);
        self.status_row.remove_css_class("error");
        self.status_label.set_text(message);
        self.status_spinner.set_visible(true);
        self.status_spinner.start();
        self.status_row.set_visible(true);
    }

    fn set_info_status(&self, message: &str) {
        self.root
            .update_state(&[gtk4::accessible::State::Busy(false)]);
        self.status_spinner.stop();
        self.status_spinner.set_visible(false);
        self.status_row.remove_css_class("error");
        self.status_label.set_text(message);
        self.status_row.set_visible(!message.is_empty());
    }

    fn set_error_status(&self, message: &str) {
        self.root
            .update_state(&[gtk4::accessible::State::Busy(false)]);
        self.status_spinner.stop();
        self.status_spinner.set_visible(false);
        self.status_row.add_css_class("error");
        self.status_label.set_text(message);
        self.status_row.set_visible(true);
    }

    /// Update the per-window snapshot. The persistence implementation plugs
    /// into this method; keeping the call at every stable state transition
    /// makes Clear and stale-response invalidation share the same boundary.
    fn publish_persisted_conversation(&self) {
        let state = self.conversation.borrow();
        let context = self.block_context.borrow();
        let snapshot = conversation_snapshot_for_persistence(
            &state.history,
            context.as_ref(),
            self.config.borrow().ai_redact_secrets,
        );
        let changed = crate::state::get_ai_conversation_snapshot() != snapshot;
        crate::state::set_ai_conversation_snapshot(snapshot);
        if changed {
            if let Some(callback) = self.persistence_callback.borrow().as_ref().cloned() {
                callback();
            }
        }
    }

    pub(crate) fn refresh_persisted_privacy(&self) {
        if self.config.borrow().ai_redact_secrets {
            self.publish_persisted_conversation();
        }
    }

    /// Copy a mouse/keyboard selection from the focused composer or transcript.
    ///
    /// The application owns Ctrl+Shift+C at the window capture phase for
    /// terminal copying, so either TextView needs priority before the active
    /// terminal fallback.
    pub(crate) fn copy_focused_selection(&self) -> bool {
        if self.status_label.has_focus() {
            if let Some((start, end)) = self.status_label.selection_bounds() {
                let lower = start.min(end).max(0) as usize;
                let upper = start.max(end).max(0) as usize;
                let selected: String = self
                    .status_label
                    .text()
                    .chars()
                    .skip(lower)
                    .take(upper.saturating_sub(lower))
                    .collect();
                if !selected.is_empty() {
                    self.status_label.clipboard().set_text(&selected);
                }
            }
            return true;
        }
        let (view, buffer) = if self.input_view.has_focus() {
            (&self.input_view, &self.input_buffer)
        } else if self.convo_view.has_focus() {
            (&self.convo_view, &self.convo_buffer)
        } else {
            return false;
        };
        let Some((start, end)) = buffer.selection_bounds() else {
            // The shortcut still belongs to the focused AI text widget; do
            // not unexpectedly copy a stale terminal selection instead.
            return true;
        };
        let text = buffer.text(&start, &end, false);
        if text.is_empty() {
            return true;
        }
        view.clipboard().set_text(&text);
        true
    }

    pub(crate) fn paste_into_composer_if_focused(&self) -> bool {
        if !self.input_view.has_focus() {
            return false;
        }
        self.input_buffer
            .paste_clipboard(&self.input_view.clipboard(), None, true);
        true
    }

    fn insert_visible(&self, label: &str, role_tag: &str, body: &str) {
        let mut end = self.convo_buffer.end_iter();
        if self.convo_buffer.char_count() > 0 {
            self.convo_buffer.insert(&mut end, "\n\n");
        }
        let label_start_off = end.offset();
        self.convo_buffer.insert(&mut end, label);
        let label_end_off = end.offset();
        self.convo_buffer.insert(&mut end, "\n");
        self.convo_buffer.insert(&mut end, body);
        // Re-fetch iters via offsets (insert invalidates the prior `end`).
        let start = self.convo_buffer.iter_at_offset(label_start_off);
        let end = self.convo_buffer.iter_at_offset(label_end_off);
        self.convo_buffer.apply_tag_by_name(role_tag, &start, &end);
    }

    fn scroll_transcript_to_end(&self) {
        let convo_view = self.convo_view.clone();
        let convo_buffer = self.convo_buffer.clone();
        let adjustment = self.convo_scroll.vadjustment();
        glib::idle_add_local_once(move || {
            let mut end = convo_buffer.end_iter();
            convo_view.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
            adjustment.set_value(adjustment.upper());
        });
    }

    /// Append a labelled message to the visible transcript and scroll to end.
    /// Pure UI — does NOT mutate `history` (the API-facing transcript).
    fn append_visible(&self, label: &str, role_tag: &str, body: &str) {
        let adjustment = self.convo_scroll.vadjustment();
        let was_empty = self.convo_buffer.char_count() == 0;
        let was_near_bottom =
            adjustment.value() + adjustment.page_size() >= adjustment.upper() - 32.0;
        self.insert_visible(label, role_tag, body);
        self.sync_empty_state();
        // Keep a reader's position when they have scrolled up to copy an older
        // answer. New/near-bottom conversations continue following replies.
        if was_empty || was_near_bottom {
            self.scroll_transcript_to_end();
        }
    }

    /// Convenience: pop the input box's full content (or use `override_text`)
    /// and fire a request. No-op if both sources are empty or a request is
    /// already in flight.
    fn send_from_input(&self, override_text: Option<String>) {
        if self.conversation.borrow().is_busy() {
            return;
        }
        let text = override_text.unwrap_or_else(|| self.input_text());
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let user_text = trimmed.to_string();
        self.input_buffer.set_text("");
        self.send_with_context(user_text, None);
    }

    /// Public entry point used by the action dispatcher (ToggleAiPanel sister
    /// action `AskAiAboutSelectedBlock`). Posts the question, attaches the
    /// block as system context.
    pub(crate) fn ask_about_block(&self, ctx: BlockContext) {
        if self.conversation.borrow().is_busy() {
            // Don't queue — quietly drop the second click. The status label
            // already says "Thinking…", which is signal enough.
            return;
        }
        let prompt = if ctx.exit_code == 0 {
            "Explain what this command does and what its output means."
        } else {
            "This command failed. Diagnose the error and suggest a fix."
        };
        self.send_with_context(prompt.to_string(), Some(ctx));
    }

    pub(crate) fn command_generation_started(&self, request: &str) {
        self.append_visible("You", "role-user", &format!("Generate command: {request}"));
        self.set_busy_status("Generating a reviewable command…");
    }

    pub(crate) fn command_generation_review_required(&self, command: &str) {
        self.set_info_status("Review the generated command before inserting it.");
        self.append_visible("Assistant", "role-asst", command);
    }

    pub(crate) fn command_generation_inserted(&self) {
        self.set_info_status("Inserted in the terminal for review; it was not run.");
    }

    pub(crate) fn command_generation_failed(&self, error: &str) {
        self.set_error_status(error);
        self.append_visible("Assistant", "role-err", error);
    }

    /// Core send path. Appends to the visible transcript + the API history,
    /// spawns a worker thread, posts the result back via glib channel.
    fn send_with_context(&self, user_text: String, ctx: Option<BlockContext>) {
        // Redact secrets before anything else — the visible transcript shows
        // exactly what gets sent, so a leaked AWS key in the input box stays
        // visible nowhere from this point on. Opt-out via `ai_redact_secrets`.
        let (user_text, ctx) = {
            let cfg = self.config.borrow();
            if cfg.ai_redact_secrets {
                let user_text = crate::redact::redact_secrets(&user_text);
                let ctx = ctx.map(|c| BlockContext {
                    cmd: crate::redact::redact_secrets(&c.cmd),
                    output: crate::redact::redact_secrets(&c.output),
                    cwd: c.cwd,
                    exit_code: c.exit_code,
                });
                (user_text, ctx)
            } else {
                (user_text, ctx)
            }
        };

        // A selected block seeds the conversation's system context. Retain it
        // for follow-up turns until Clear, otherwise the second question would
        // send the role history but silently lose the command/output being
        // discussed.
        if let Some(context) = ctx.as_ref() {
            *self.block_context.borrow_mut() = Some(context.clone());
        }
        let effective_context = ctx.clone().or_else(|| self.block_context.borrow().clone());

        // Show what we sent.
        let visible_user = match &ctx {
            Some(c) => format!("{user_text}\n[context: `{}`, exit {}]", c.cmd, c.exit_code),
            None => user_text.clone(),
        };
        self.append_visible("You", "role-user", &visible_user);
        // History always holds the raw user message — the block context goes
        // into the system prompt, not the user turn.
        let (request_epoch, history) = self.conversation.borrow_mut().begin(user_text);
        let client = ai::AiClient::from_config(&self.config.borrow());
        let provider_label = client
            .as_ref()
            .map(ai::AiClient::display_name)
            .unwrap_or_else(|_| "AI unavailable".to_string());
        let system = ai::build_system_prompt(effective_context.as_ref());

        self.sync_composer_state();
        self.set_busy_status(&format!("Thinking… ({provider_label})"));

        // mpsc + polling timeout matches the pattern in pty.rs:346 (gtk-rs 0.11
        // dropped MainContext::channel; the codebase polls a std::sync::mpsc
        // from a glib timer instead). 50ms is well below human-perceptible
        // latency and far cheaper than an API call's hundreds-of-ms cost.
        let (tx, rx) = std::sync::mpsc::channel::<Result<String, ai::AiError>>();
        std::thread::spawn(move || {
            let res =
                client.and_then(|client| client.send_turns_blocking(system.as_deref(), &history));
            let _ = tx.send(res);
        });

        let p = self.clone();
        let rx_cell = std::cell::RefCell::new(rx);
        glib::timeout_add_local(std::time::Duration::from_millis(50), move || match rx_cell
            .borrow()
            .try_recv()
        {
            Ok(res) => {
                match res {
                    Ok(text) => {
                        if !p
                            .conversation
                            .borrow_mut()
                            .complete_success(request_epoch, text.clone())
                        {
                            return glib::ControlFlow::Break;
                        }
                        p.clear_status();
                        p.append_visible("Assistant", "role-asst", &text);
                        p.sync_composer_state();
                        p.publish_persisted_conversation();
                    }
                    Err(e) => {
                        if !p.conversation.borrow_mut().complete_error(request_epoch) {
                            return glib::ControlFlow::Break;
                        }
                        let msg = format!("Error: {e}");
                        p.set_error_status(&msg);
                        p.append_visible("Assistant", "role-err", &msg);
                        p.sync_composer_state();
                    }
                }
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                if !p.conversation.borrow_mut().complete_error(request_epoch) {
                    return glib::ControlFlow::Break;
                }
                let msg = "Error: worker thread disconnected";
                p.set_error_status(msg);
                p.append_visible("Assistant", "role-err", msg);
                p.sync_composer_state();
                glib::ControlFlow::Break
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_invalidates_in_flight_reply_and_prevents_assistant_only_history() {
        let mut state = ConversationState::default();
        let (epoch, sent) = state.begin("first".into());
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].role, Role::User);

        state.clear();
        assert!(!state.complete_success(epoch, "stale".into()));
        assert!(state.history.is_empty());
        assert!(!state.is_busy());
    }

    #[test]
    fn success_alternates_roles_and_error_rolls_back_only_its_user_turn() {
        let mut state = ConversationState::default();
        let (first, _) = state.begin("one".into());
        assert!(state.complete_success(first, "answer one".into()));
        let (second, sent) = state.begin("two".into());
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0].role, Role::User);
        assert_eq!(sent[1].role, Role::Assistant);
        assert_eq!(sent[2].role, Role::User);
        assert!(state.complete_error(second));
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.history[1].role, Role::Assistant);
    }

    #[test]
    fn composer_enter_shortcuts_send_or_insert_newline_as_documented() {
        use gtk4::gdk::{Key, ModifierType};

        let cases = [
            (Key::Return, ModifierType::empty(), ComposerKeyAction::Send),
            (
                Key::Return,
                ModifierType::CONTROL_MASK,
                ComposerKeyAction::Send,
            ),
            (
                Key::KP_Enter,
                ModifierType::CONTROL_MASK | ModifierType::LOCK_MASK,
                ComposerKeyAction::Send,
            ),
            (
                Key::Return,
                ModifierType::SHIFT_MASK,
                ComposerKeyAction::Newline,
            ),
            (
                Key::KP_Enter,
                ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK,
                ComposerKeyAction::Newline,
            ),
            (
                Key::Return,
                ModifierType::ALT_MASK,
                ComposerKeyAction::Proceed,
            ),
            (Key::a, ModifierType::empty(), ComposerKeyAction::Proceed),
        ];

        for (key, modifiers, expected) in cases {
            assert_eq!(classify_composer_key(key, modifiers), expected);
        }
    }

    #[test]
    fn restoring_history_invalidates_an_in_flight_epoch() {
        let mut state = ConversationState::default();
        let (stale_epoch, _) = state.begin("stale".into());
        state.restore(vec![
            Turn {
                role: Role::User,
                text: "restored question".into(),
            },
            Turn {
                role: Role::Assistant,
                text: "restored answer".into(),
            },
        ]);

        assert!(!state.complete_success(stale_epoch, "late answer".into()));
        assert!(!state.is_busy());
        assert_eq!(state.history.len(), 2);
    }

    #[test]
    fn enabling_redaction_scrubs_every_persisted_conversation_field() {
        const SECRET: &str = "AKIAIOSFODNN7EXAMPLE";
        let history = vec![
            Turn {
                role: Role::User,
                text: format!("question {SECRET}"),
            },
            Turn {
                role: Role::Assistant,
                text: format!("answer {SECRET}"),
            },
        ];
        let context = BlockContext {
            cmd: format!("echo {SECRET}"),
            output: format!("result {SECRET}"),
            cwd: Some(format!("/tmp/{SECRET}")),
            exit_code: 0,
        };

        let snapshot =
            conversation_snapshot_for_persistence(&history, Some(&context), true).unwrap();
        for turn in snapshot.turns() {
            assert!(!turn.text.contains(SECRET));
        }
        let context = snapshot.block_context().unwrap();
        assert!(!context.cmd.contains(SECRET));
        assert!(!context.output.contains(SECRET));
        assert!(!context.cwd.as_deref().unwrap().contains(SECRET));
    }
}
