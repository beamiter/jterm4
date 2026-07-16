//! ai_panel — provider-neutral right-side chat sidebar.
//!
//! Layout: vertical column = [header row | conversation scroll | status label |
//! input row]. The input is a multi-line TextView so users can paste shell
//! errors without losing newlines; Ctrl+Enter sends, plain Enter inserts a
//! newline (matches Slack / Linear conventions).
//!
//! Networking runs on a worker `std::thread`; the response (or error) hops
//! back to the GTK main loop via `glib::MainContext::channel`. While a
//! request is in flight the send button is desensitised and the status label
//! shows "…".

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Box as GBox, Button, EventControllerKey, Label, Orientation, ScrolledWindow, TextBuffer,
    TextTag, TextView, WrapMode,
};

use crate::ai::{self, BlockContext, Role, Turn};
use crate::config::Config;

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
    input_buffer: TextBuffer,
    send_btn: Button,
    status_label: Label,
    block_context: Rc<RefCell<Option<BlockContext>>>,
    config: Rc<std::cell::RefCell<Config>>,
}

impl AiPanel {
    /// Build the panel widget tree. The returned root is hidden / shown by
    /// `UiState::toggle_ai_panel` — visibility is not owned here.
    pub(crate) fn build(config: Rc<std::cell::RefCell<Config>>) -> Self {
        let header = GBox::new(Orientation::Horizontal, 6);
        header.add_css_class("ai-panel-header");
        let title = Label::new(Some("AI"));
        title.set_halign(gtk4::Align::Start);
        title.set_hexpand(true);
        title.add_css_class("heading");
        let clear_btn = Button::with_label("Clear");
        clear_btn.set_focus_on_click(false);
        clear_btn.add_css_class("flat");
        header.append(&title);
        header.append(&clear_btn);

        // Conversation transcript (read-only TextView, monospace-ish for code blocks).
        let convo_buffer = TextBuffer::new(None);
        // Pre-register tags so apply_tag finds them by name later.
        let tag_table = convo_buffer.tag_table();
        // 700 = Pango bold; using the integer avoids depending on pango::Weight's
        // ABI conversion (gtk-rs 0.11 dropped IntoGlib for plain enum literals).
        let user_tag = TextTag::builder()
            .name("role-user")
            .foreground("#7bbfea")
            .weight(700)
            .build();
        let asst_tag = TextTag::builder()
            .name("role-asst")
            .foreground("#84bf96")
            .weight(700)
            .build();
        let err_tag = TextTag::builder()
            .name("role-err")
            .foreground("#ed1941")
            .weight(700)
            .build();
        tag_table.add(&user_tag);
        tag_table.add(&asst_tag);
        tag_table.add(&err_tag);

        let convo_view = TextView::with_buffer(&convo_buffer);
        convo_view.set_editable(false);
        convo_view.set_cursor_visible(false);
        convo_view.set_wrap_mode(WrapMode::WordChar);
        convo_view.set_monospace(false);
        convo_view.set_top_margin(6);
        convo_view.set_bottom_margin(6);
        convo_view.set_left_margin(8);
        convo_view.set_right_margin(8);

        let convo_scroll = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .vexpand(true)
            .child(&convo_view)
            .build();

        // Status / busy line — single label, replaces its own text.
        let status_label = Label::new(None);
        status_label.set_halign(gtk4::Align::Start);
        status_label.add_css_class("dim-label");
        status_label.set_margin_start(8);
        status_label.set_margin_end(8);

        // Input row — TextView for multi-line + Send button.
        let input_buffer = TextBuffer::new(None);
        let input_view = TextView::with_buffer(&input_buffer);
        input_view.set_wrap_mode(WrapMode::WordChar);
        input_view.set_top_margin(4);
        input_view.set_bottom_margin(4);
        input_view.set_left_margin(6);
        input_view.set_right_margin(6);
        input_view.set_accepts_tab(false);
        let input_scroll = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .min_content_height(60)
            .max_content_height(160)
            .child(&input_view)
            .build();
        input_scroll.add_css_class("ai-panel-input");

        let send_btn = Button::with_label("Send");
        send_btn.set_focus_on_click(false);
        send_btn.add_css_class("suggested-action");
        send_btn.set_tooltip_text(Some("Send (Ctrl+Enter)"));

        let input_row = GBox::new(Orientation::Horizontal, 6);
        input_row.append(&input_scroll);
        input_row.append(&send_btn);
        input_row.set_margin_start(4);
        input_row.set_margin_end(4);
        input_row.set_margin_bottom(4);

        let root = GBox::new(Orientation::Vertical, 4);
        root.add_css_class("ai-panel");
        root.set_hexpand(false);
        root.set_vexpand(true);
        root.set_margin_start(4);
        root.set_margin_end(4);
        root.set_margin_top(4);
        root.append(&header);
        root.append(&convo_scroll);
        root.append(&status_label);
        root.append(&input_row);

        let panel = AiPanel {
            root,
            conversation: Rc::new(RefCell::new(ConversationState::default())),
            convo_buffer: convo_buffer.clone(),
            convo_view,
            convo_scroll: convo_scroll.clone(),
            input_buffer: input_buffer.clone(),
            send_btn: send_btn.clone(),
            status_label: status_label.clone(),
            block_context: Rc::new(RefCell::new(None)),
            config,
        };

        // Wire Clear: drop history + clear the transcript buffer.
        {
            let p = panel.clone();
            clear_btn.connect_clicked(move |_| {
                p.conversation.borrow_mut().clear();
                *p.block_context.borrow_mut() = None;
                p.convo_buffer.set_text("");
                p.status_label.set_text("");
                p.send_btn.set_sensitive(true);
            });
        }

        // Wire Send button.
        {
            let p = panel.clone();
            send_btn.connect_clicked(move |_| {
                p.send_from_input(None);
            });
        }

        // Ctrl+Enter in input → send. Plain Enter is left to TextView's
        // default (inserts a newline).
        {
            let key = EventControllerKey::new();
            let p = panel.clone();
            key.connect_key_pressed(move |_ctrl, keyval, _code, state| {
                let is_enter =
                    keyval == gtk4::gdk::Key::Return || keyval == gtk4::gdk::Key::KP_Enter;
                let ctrl_held = state.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
                if is_enter && ctrl_held {
                    p.send_from_input(None);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            input_view.add_controller(key);
        }

        panel
    }

    /// Append a labelled message to the visible transcript and scroll to end.
    /// Pure UI — does NOT mutate `history` (the API-facing transcript).
    fn append_visible(&self, label: &str, role_tag: &str, body: &str) {
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
        let s = self.convo_buffer.iter_at_offset(label_start_off);
        let e = self.convo_buffer.iter_at_offset(label_end_off);
        self.convo_buffer.apply_tag_by_name(role_tag, &s, &e);
        // Schedule a scroll-to-end on the next idle so the view has actually
        // re-laid out with the new content.
        let convo_view = self.convo_view.clone();
        let convo_buffer = self.convo_buffer.clone();
        glib::idle_add_local_once(move || {
            let mut end = convo_buffer.end_iter();
            convo_view.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
        });
        // Also force the ScrolledWindow's vadjustment to its upper bound —
        // TextView::scroll_to_iter doesn't always propagate through the
        // wrapping ScrolledWindow under GTK4 (the inner scroll snaps the
        // text but the outer Adjustment lags one frame).
        let adj = self.convo_scroll.vadjustment();
        adj.set_value(adj.upper());
    }

    /// Convenience: pop the input box's full content (or use `override_text`)
    /// and fire a request. No-op if both sources are empty or a request is
    /// already in flight.
    fn send_from_input(&self, override_text: Option<String>) {
        if self.conversation.borrow().is_busy() {
            return;
        }
        let text = override_text.unwrap_or_else(|| {
            let s = self.input_buffer.start_iter();
            let e = self.input_buffer.end_iter();
            self.input_buffer.text(&s, &e, false).to_string()
        });
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
        self.status_label
            .set_text("Generating a reviewable command…");
    }

    pub(crate) fn command_generation_review_required(&self, command: &str) {
        self.status_label
            .set_text("Review the generated command before inserting it.");
        self.append_visible("Assistant", "role-asst", command);
    }

    pub(crate) fn command_generation_inserted(&self) {
        self.status_label
            .set_text("Inserted in the terminal for review; it was not run.");
    }

    pub(crate) fn command_generation_failed(&self, error: &str) {
        self.status_label.set_text(error);
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

        self.send_btn.set_sensitive(false);
        self.status_label
            .set_text(&format!("Thinking… ({provider_label})"));

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
                        p.send_btn.set_sensitive(true);
                        p.status_label.set_text("");
                        p.append_visible("Assistant", "role-asst", &text);
                    }
                    Err(e) => {
                        if !p.conversation.borrow_mut().complete_error(request_epoch) {
                            return glib::ControlFlow::Break;
                        }
                        p.send_btn.set_sensitive(true);
                        let msg = format!("Error: {e}");
                        p.status_label.set_text(&msg);
                        p.append_visible("Assistant", "role-err", &msg);
                    }
                }
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                if !p.conversation.borrow_mut().complete_error(request_epoch) {
                    return glib::ControlFlow::Break;
                }
                p.send_btn.set_sensitive(true);
                p.status_label.set_text("Error: worker thread disconnected");
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
}
