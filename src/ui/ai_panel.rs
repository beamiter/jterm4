//! Provider-neutral multi-chat sidebar.
//!
//! GTK keeps one transcript and composer alive while `ChatStore` owns a
//! bounded collection of chats. Enter and Ctrl+Enter send, Shift+Enter inserts
//! a newline, and IME candidate confirmation gets first refusal. Requests are
//! completed against `(chat_id, epoch)`, so switching chats never redirects a
//! background reply into the visible conversation.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{
    Box as GBox, Button, Entry, EventControllerKey, Image, Label, ListBox, ListBoxRow, MenuButton,
    Orientation, Overlay, Popover, ScrolledWindow, SearchEntry, Spinner, Stack, TextBuffer,
    TextTag, TextView, WrapMode,
};
use libadwaita as adw;

use super::ai_chat_store::{ChatStatus, ChatStore, ChatStoreError, ChatSummary, RequestToken};
use crate::ai::{self, BlockContext, Role};
use crate::config::Config;

const CHAT_PAGE: &str = "chat";
const CHAT_LIBRARY_PAGE: &str = "library";

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

#[derive(Clone)]
pub(crate) struct AiPanel {
    pub(crate) root: GBox,
    store: Rc<RefCell<ChatStore>>,
    content_stack: Stack,
    library_btn: Button,
    new_chat_btn: Button,
    chat_title: Label,
    header_meta: Label,
    actions_popover: Popover,
    rename_chat_btn: Button,
    archive_chat_btn: Button,
    delete_chat_btn: Button,
    close_btn: Button,
    chat_search: SearchEntry,
    chat_list: ListBox,
    chat_row_ids: Rc<RefCell<Vec<Option<u64>>>>,
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
    persistence_callback: Rc<RefCell<Option<PersistenceCallback>>>,
    draft_persist_epoch: Rc<Cell<u64>>,
    config: Rc<RefCell<Config>>,
}

impl AiPanel {
    pub(crate) fn build(config: Rc<RefCell<Config>>) -> Self {
        let header = GBox::new(Orientation::Horizontal, 6);
        header.add_css_class("ai-panel-header");

        let library_btn = Button::with_label("Chats");
        library_btn.set_tooltip_text(Some("Browse saved and archived chats"));
        library_btn.set_focus_on_click(false);
        library_btn.add_css_class("flat");
        library_btn.add_css_class("ai-chat-header-button");

        let chat_title = Label::new(Some("New chat"));
        chat_title.set_halign(gtk4::Align::Start);
        chat_title.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        chat_title.add_css_class("ai-panel-title");
        let header_meta = Label::new(None);
        header_meta.set_halign(gtk4::Align::Start);
        header_meta.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        header_meta.add_css_class("ai-panel-subtitle");
        let header_text = GBox::new(Orientation::Vertical, 0);
        header_text.append(&chat_title);
        header_text.append(&header_meta);
        header_text.set_hexpand(true);

        let new_chat_btn = Button::with_label("New chat");
        new_chat_btn.set_tooltip_text(Some("Create a new chat and keep existing chats"));
        new_chat_btn.set_focus_on_click(false);
        new_chat_btn.add_css_class("flat");
        new_chat_btn.add_css_class("ai-chat-header-button");

        let rename_chat_btn = menu_button("Rename");
        let archive_chat_btn = menu_button("Archive");
        let delete_chat_btn = menu_button("Delete");
        delete_chat_btn.add_css_class("destructive-action");
        let actions_box = GBox::new(Orientation::Vertical, 0);
        actions_box.add_css_class("menu");
        actions_box.append(&rename_chat_btn);
        actions_box.append(&archive_chat_btn);
        actions_box.append(&delete_chat_btn);
        let actions_popover = Popover::new();
        actions_popover.set_has_arrow(false);
        actions_popover.set_child(Some(&actions_box));
        let actions_btn = MenuButton::new();
        actions_btn.set_icon_name("view-more-symbolic");
        actions_btn.set_tooltip_text(Some("Chat actions"));
        actions_btn.set_popover(Some(&actions_popover));
        actions_btn.add_css_class("flat");
        actions_btn.add_css_class("ai-chat-header-button");

        let close_btn = Button::from_icon_name("window-close-symbolic");
        close_btn.set_tooltip_text(Some("Close AI panel"));
        close_btn.set_focus_on_click(false);
        close_btn.add_css_class("flat");
        close_btn.add_css_class("ai-chat-header-button");

        header.append(&library_btn);
        header.append(&header_text);
        header.append(&new_chat_btn);
        header.append(&actions_btn);
        header.append(&close_btn);

        let convo_buffer = TextBuffer::new(None);
        let tag_table = convo_buffer.tag_table();
        tag_table.add(&TextTag::builder().name("role-user").weight(700).build());
        tag_table.add(&TextTag::builder().name("role-asst").weight(700).build());
        tag_table.add(
            &TextTag::builder()
                .name("role-err")
                .foreground("#e01b24")
                .weight(700)
                .build(),
        );

        let convo_view = TextView::with_buffer(&convo_buffer);
        convo_view.set_editable(false);
        convo_view.set_cursor_visible(false);
        convo_view.set_focusable(true);
        convo_view.set_wrap_mode(WrapMode::WordChar);
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

        let chat_page = GBox::new(Orientation::Vertical, 0);
        chat_page.append(&transcript_overlay);
        chat_page.append(&status_row);
        chat_page.append(&composer);

        let library_heading = Label::new(Some("Chats"));
        library_heading.set_halign(gtk4::Align::Start);
        library_heading.add_css_class("heading");
        let library_hint = Label::new(Some("Select a chat, including archived conversations."));
        library_hint.set_halign(gtk4::Align::Start);
        library_hint.set_wrap(true);
        library_hint.add_css_class("dim-label");
        let chat_search = SearchEntry::new();
        chat_search.set_placeholder_text(Some("Search chats…"));
        chat_search.add_css_class("ai-chat-search");
        let library_toolbar = GBox::new(Orientation::Vertical, 6);
        library_toolbar.add_css_class("ai-chat-library-toolbar");
        library_toolbar.append(&library_heading);
        library_toolbar.append(&library_hint);
        library_toolbar.append(&chat_search);

        let chat_list = ListBox::new();
        chat_list.set_selection_mode(gtk4::SelectionMode::Single);
        chat_list.add_css_class("ai-chat-list");
        let library_scroll = ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .vexpand(true)
            .child(&chat_list)
            .build();
        let library_page = GBox::new(Orientation::Vertical, 0);
        library_page.add_css_class("ai-chat-library");
        library_page.append(&library_toolbar);
        library_page.append(&library_scroll);

        let content_stack = Stack::new();
        content_stack.set_vexpand(true);
        content_stack.add_named(&chat_page, Some(CHAT_PAGE));
        content_stack.add_named(&library_page, Some(CHAT_LIBRARY_PAGE));
        content_stack.set_visible_child_name(CHAT_PAGE);

        let root = GBox::new(Orientation::Vertical, 0);
        root.add_css_class("ai-panel");
        root.set_hexpand(false);
        root.set_vexpand(true);
        root.append(&header);
        root.append(&content_stack);

        let panel = Self {
            root,
            store: Rc::new(RefCell::new(ChatStore::default())),
            content_stack,
            library_btn: library_btn.clone(),
            new_chat_btn: new_chat_btn.clone(),
            chat_title,
            header_meta,
            actions_popover: actions_popover.clone(),
            rename_chat_btn: rename_chat_btn.clone(),
            archive_chat_btn: archive_chat_btn.clone(),
            delete_chat_btn: delete_chat_btn.clone(),
            close_btn,
            chat_search: chat_search.clone(),
            chat_list: chat_list.clone(),
            chat_row_ids: Rc::new(RefCell::new(Vec::new())),
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
            persistence_callback: Rc::new(RefCell::new(None)),
            draft_persist_epoch: Rc::new(Cell::new(0)),
            config,
        };

        {
            let p = panel.clone();
            library_btn.connect_clicked(move |_| p.toggle_chat_library());
        }
        {
            let p = panel.clone();
            new_chat_btn.connect_clicked(move |_| p.create_new_chat());
        }
        {
            let p = panel.clone();
            rename_chat_btn.connect_clicked(move |_| {
                p.actions_popover.popdown();
                p.show_rename_chat_dialog();
            });
        }
        {
            let p = panel.clone();
            archive_chat_btn.connect_clicked(move |_| {
                p.actions_popover.popdown();
                p.toggle_archive_active_chat();
            });
        }
        {
            let p = panel.clone();
            delete_chat_btn.connect_clicked(move |_| {
                p.actions_popover.popdown();
                p.show_delete_chat_dialog();
            });
        }
        {
            let p = panel.clone();
            chat_search.connect_search_changed(move |_| p.refresh_chat_library());
        }
        {
            let p = panel.clone();
            chat_search.connect_activate(move |_| {
                let id = p.chat_row_ids.borrow().iter().flatten().next().copied();
                if let Some(id) = id {
                    p.select_chat(id);
                }
            });
        }
        {
            let p = panel.clone();
            chat_list.connect_row_activated(move |_, row| {
                let index = row.index();
                if index < 0 {
                    return;
                }
                let id = p
                    .chat_row_ids
                    .borrow()
                    .get(index as usize)
                    .copied()
                    .flatten();
                if let Some(id) = id {
                    p.select_chat(id);
                }
            });
        }
        {
            let p = panel.clone();
            send_btn.connect_clicked(move |_| p.send_from_input(None));
        }
        {
            let p = panel.clone();
            input_buffer.connect_changed(move |_| {
                let changed = p.store.borrow_mut().set_active_draft(p.input_text());
                p.sync_composer_state();
                if changed {
                    p.schedule_draft_persistence();
                }
            });
        }
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
        {
            let key = EventControllerKey::new();
            let p = panel.clone();
            key.connect_key_pressed(move |_, keyval, _, _| {
                if keyval == gtk4::gdk::Key::Escape {
                    p.show_chat_page();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
            library_page.add_controller(key);
        }

        panel.render_active_chat();
        panel
    }

    pub(crate) fn refresh_config_display(&self) {
        let config = self.config.borrow();
        let provider = match config.ai_provider.as_str() {
            "openai-compatible" => "OpenAI-compatible",
            "ollama" => "Ollama",
            _ => "Anthropic",
        };
        let provider_model = if config.ai_model.trim().is_empty() {
            provider.to_string()
        } else {
            format!("{provider} · {}", config.ai_model.trim())
        };
        drop(config);
        let summary = if self.store.borrow().active_archived() {
            format!("Archived · {provider_model}")
        } else {
            provider_model
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

    pub(crate) fn restore_persisted_conversation(&self) {
        let Some(snapshot) = crate::state::get_ai_conversation_snapshot() else {
            return;
        };
        *self.store.borrow_mut() = ChatStore::restore(snapshot);
        let count = self.store.borrow().len();
        self.store.borrow_mut().set_active_info(format!(
            "Restored {count} chat{} for this window.",
            plural(count)
        ));
        self.render_active_chat();
    }

    pub(crate) fn focus_input(&self) {
        self.show_chat_page();
        if self.store.borrow().active_archived() {
            self.convo_view.grab_focus();
        } else {
            self.input_view.grab_focus();
        }
    }

    pub(crate) fn handles_enter_key(&self) -> bool {
        self.input_view.has_focus()
            || self.chat_search_has_focus()
            || self.chat_list.has_focus()
            || self.chat_list.focus_child().is_some()
    }

    fn toggle_chat_library(&self) {
        if self.content_stack.visible_child_name().as_deref() == Some(CHAT_LIBRARY_PAGE) {
            self.show_chat_page();
        } else {
            self.show_chat_library();
        }
    }

    fn show_chat_library(&self) {
        self.refresh_chat_library();
        self.content_stack.set_visible_child_name(CHAT_LIBRARY_PAGE);
        self.library_btn.set_label("Back");
        self.chat_search.grab_focus();
    }

    fn show_chat_page(&self) {
        self.content_stack.set_visible_child_name(CHAT_PAGE);
        self.library_btn.set_label("Chats");
        if self.store.borrow().active_archived() {
            self.convo_view.grab_focus();
        } else {
            self.input_view.grab_focus();
        }
    }

    fn create_new_chat(&self) {
        let result = self.store.borrow_mut().new_chat();
        match result {
            Ok(_) => {
                self.store.borrow_mut().clear_active_status();
                self.render_active_chat();
                self.publish_persisted_conversation();
                self.show_chat_page();
            }
            Err(ChatStoreError::LimitReached) => {
                self.show_chat_page();
                self.show_error_status(
                    "Chat limit reached. Delete a chat before creating another one.",
                );
            }
            Err(_) => {}
        }
    }

    fn select_chat(&self, id: u64) {
        let selected = self.store.borrow_mut().select_chat(id);
        if selected {
            self.render_active_chat();
            self.publish_persisted_conversation();
        }
        self.show_chat_page();
    }

    fn toggle_archive_active_chat(&self) {
        let result = self.store.borrow_mut().toggle_archive_active();
        match result {
            Ok(_) => {
                self.render_active_chat();
                self.publish_persisted_conversation();
                self.show_chat_page();
            }
            Err(_) => self.show_error_status("Could not update this chat's archive state."),
        }
    }

    fn show_rename_chat_dialog(&self) {
        let current = self.store.borrow().active_title().to_string();
        let dialog = adw::AlertDialog::new(Some("Rename chat"), None);
        dialog.add_responses(&[("cancel", "Cancel"), ("rename", "Rename")]);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        let entry = Entry::new();
        entry.set_text(&current);
        entry.set_activates_default(true);
        dialog.set_extra_child(Some(&entry));
        let p = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "rename" && p.store.borrow_mut().rename_active(&entry.text()) {
                p.refresh_chat_chrome();
                p.refresh_chat_library();
                p.publish_persisted_conversation();
            }
        });
        dialog.present(Some(&self.root));
    }

    fn show_delete_chat_dialog(&self) {
        let title = self.store.borrow().active_title().to_string();
        let dialog = adw::AlertDialog::new(
            Some("Delete this chat?"),
            Some(&format!(
                "“{title}” and its saved messages will be permanently removed."
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        let p = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "delete" {
                return;
            }
            p.store.borrow_mut().delete_active();
            p.render_active_chat();
            p.publish_persisted_conversation();
            p.show_chat_page();
        });
        dialog.present(Some(&self.root));
    }

    fn refresh_chat_chrome(&self) {
        let (title, archived, at_capacity) = {
            let store = self.store.borrow();
            (
                store.active_title().to_string(),
                store.active_archived(),
                store.at_capacity(),
            )
        };
        self.chat_title.set_text(&title);
        self.chat_title.set_tooltip_text(Some(&title));
        self.archive_chat_btn
            .set_label(if archived { "Unarchive" } else { "Archive" });
        self.new_chat_btn.set_sensitive(!at_capacity);
        self.new_chat_btn.set_tooltip_text(Some(if at_capacity {
            "Chat limit reached; delete a chat before creating another"
        } else {
            "Create a new chat and keep existing chats"
        }));
        self.refresh_config_display();
    }

    fn refresh_chat_library(&self) {
        while let Some(row) = self.chat_list.row_at_index(0) {
            self.chat_list.remove(&row);
        }
        self.chat_row_ids.borrow_mut().clear();

        let query = self.chat_search.text().trim().to_lowercase();
        let summaries: Vec<_> = self
            .store
            .borrow()
            .summaries()
            .into_iter()
            .filter(|summary| {
                query.is_empty()
                    || summary.title.to_lowercase().contains(&query)
                    || summary.preview.to_lowercase().contains(&query)
            })
            .collect();
        let active: Vec<_> = summaries
            .iter()
            .filter(|summary| !summary.archived)
            .cloned()
            .collect();
        let archived: Vec<_> = summaries
            .iter()
            .filter(|summary| summary.archived)
            .cloned()
            .collect();

        if !active.is_empty() {
            self.append_chat_section("Chats");
            for summary in &active {
                self.append_chat_row(summary);
            }
        }
        if !archived.is_empty() {
            self.append_chat_section("Archived");
            for summary in &archived {
                self.append_chat_row(summary);
            }
        }
        if active.is_empty() && archived.is_empty() {
            let row = ListBoxRow::new();
            row.set_activatable(false);
            row.set_selectable(false);
            let label = Label::new(Some("No matching chats"));
            label.add_css_class("ai-chat-empty");
            row.set_child(Some(&label));
            self.chat_list.append(&row);
            self.chat_row_ids.borrow_mut().push(None);
        }
    }

    fn append_chat_section(&self, title: &str) {
        let row = ListBoxRow::new();
        row.set_activatable(false);
        row.set_selectable(false);
        let label = Label::new(Some(title));
        label.set_halign(gtk4::Align::Start);
        label.add_css_class("ai-chat-section");
        row.set_child(Some(&label));
        self.chat_list.append(&row);
        self.chat_row_ids.borrow_mut().push(None);
    }

    fn append_chat_row(&self, summary: &ChatSummary) {
        let subtitle = if summary.busy {
            "Thinking…".to_string()
        } else if summary.unread {
            format!("New response · {}", summary.preview)
        } else if summary.history_truncated {
            format!("Some local content omitted · {}", summary.preview)
        } else {
            summary.preview.clone()
        };
        let row = adw::ActionRow::builder()
            .title(&summary.title)
            .subtitle(&subtitle)
            .activatable(true)
            .build();
        row.add_css_class("ai-chat-row");
        if summary.active {
            row.add_css_class("active");
        }
        if summary.archived {
            row.add_css_class("archived");
            let icon = Image::from_icon_name("folder-symbolic");
            icon.set_tooltip_text(Some("Archived"));
            row.add_suffix(&icon);
        }
        if summary.unread {
            row.add_css_class("unread");
            let badge = Label::new(Some("New"));
            badge.add_css_class("accent");
            row.add_suffix(&badge);
        }
        if summary.busy {
            let spinner = Spinner::new();
            spinner.start();
            row.add_suffix(&spinner);
        }
        self.chat_list.append(&row);
        self.chat_row_ids.borrow_mut().push(Some(summary.id));
        if summary.active {
            self.chat_list.select_row(Some(&row));
        }
    }

    fn render_active_chat(&self) {
        let (history, draft) = {
            let store = self.store.borrow();
            (
                store.active_history().to_vec(),
                store.active_draft().to_string(),
            )
        };
        self.convo_buffer.set_text("");
        for turn in history {
            match turn.role {
                Role::User => self.insert_visible("You", "role-user", &turn.text),
                Role::Assistant => self.insert_visible("Assistant", "role-asst", &turn.text),
            }
        }
        self.input_buffer.set_text(&draft);
        self.sync_empty_state();
        self.sync_composer_state();
        self.sync_active_status();
        self.refresh_chat_chrome();
        self.refresh_chat_library();
        self.scroll_transcript_to_end();
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
        let (busy, archived) = {
            let store = self.store.borrow();
            (store.is_active_busy(), store.active_archived())
        };
        self.input_view.set_editable(!archived);
        self.input_placeholder.set_text(if archived {
            "Unarchive this chat to continue"
        } else {
            "Ask about commands, errors, or output…"
        });
        self.input_placeholder.set_visible(text.is_empty());
        self.send_btn
            .set_sensitive(!archived && !busy && !text.trim().is_empty());
    }

    fn sync_active_status(&self) {
        let (status, archived, truncated, has_context) = {
            let store = self.store.borrow();
            (
                store.active_status().clone(),
                store.active_archived(),
                store.active_history_truncated(),
                store.active_context().is_some(),
            )
        };
        match status {
            ChatStatus::Thinking(message) => self.show_busy_status(&message),
            ChatStatus::Info(message) => self.show_info_status(&message),
            ChatStatus::Error(message) => self.show_error_status(&message),
            ChatStatus::Idle if archived => {
                self.show_info_status("Archived chat · Unarchive it to continue.")
            }
            ChatStatus::Idle if truncated => self.show_info_status(
                "Some older local chat content was omitted to stay within storage limits.",
            ),
            ChatStatus::Idle if has_context => {
                self.show_info_status("Selected Block context is attached to this chat.")
            }
            ChatStatus::Idle => self.clear_status_widgets(),
        }
    }

    fn clear_status_widgets(&self) {
        self.root
            .update_state(&[gtk4::accessible::State::Busy(false)]);
        self.status_spinner.stop();
        self.status_spinner.set_visible(false);
        self.status_label.set_text("");
        self.status_row.remove_css_class("error");
        self.status_row.set_visible(false);
    }

    fn show_busy_status(&self, message: &str) {
        self.root
            .update_state(&[gtk4::accessible::State::Busy(true)]);
        self.status_row.remove_css_class("error");
        self.status_label.set_text(message);
        self.status_spinner.set_visible(true);
        self.status_spinner.start();
        self.status_row.set_visible(true);
    }

    fn show_info_status(&self, message: &str) {
        self.root
            .update_state(&[gtk4::accessible::State::Busy(false)]);
        self.status_spinner.stop();
        self.status_spinner.set_visible(false);
        self.status_row.remove_css_class("error");
        self.status_label.set_text(message);
        self.status_row.set_visible(!message.is_empty());
    }

    fn show_error_status(&self, message: &str) {
        self.root
            .update_state(&[gtk4::accessible::State::Busy(false)]);
        self.status_spinner.stop();
        self.status_spinner.set_visible(false);
        self.status_row.add_css_class("error");
        self.status_label.set_text(message);
        self.status_row.set_visible(true);
    }

    fn publish_persisted_conversation(&self) {
        let redact = self.config.borrow().ai_redact_secrets;
        let result = self.store.borrow_mut().snapshot_for_persistence(redact);
        let (snapshot, truncation_changed) = match result {
            Ok(result) => result,
            Err(error) => {
                log::error!("Could not safely build AI chat snapshot: {error:?}");
                self.show_error_status("Chat changes could not be saved safely.");
                return;
            }
        };
        if truncation_changed {
            self.sync_active_status();
            self.refresh_chat_library();
        }
        let snapshot = Some(snapshot);
        let changed = crate::state::get_ai_conversation_snapshot() != snapshot;
        crate::state::set_ai_conversation_snapshot(snapshot);
        if changed {
            let callback = self.persistence_callback.borrow().as_ref().cloned();
            if let Some(callback) = callback {
                callback();
            }
        }
        self.sync_persisted_truncation();
    }

    /// Window-state compaction has an additional whole-workspace budget. Pull
    /// its durable truncation markers back into the live chat library after a
    /// successful save without discarding history still available in memory.
    pub(crate) fn sync_persisted_truncation(&self) {
        let Some(snapshot) = crate::state::get_ai_conversation_snapshot() else {
            return;
        };
        let changed = self.store.borrow_mut().sync_truncation_markers(&snapshot);
        if changed {
            self.sync_active_status();
            self.refresh_chat_library();
        }
    }

    fn schedule_draft_persistence(&self) {
        let epoch = self.draft_persist_epoch.get().wrapping_add(1);
        self.draft_persist_epoch.set(epoch);
        let p = self.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
            if p.draft_persist_epoch.get() == epoch {
                p.publish_persisted_conversation();
            }
        });
    }

    pub(crate) fn flush_persisted_conversation(&self) {
        self.draft_persist_epoch
            .set(self.draft_persist_epoch.get().wrapping_add(1));
        self.publish_persisted_conversation();
    }

    pub(crate) fn refresh_persisted_privacy(&self) {
        if self.config.borrow().ai_redact_secrets {
            self.publish_persisted_conversation();
        }
    }

    pub(crate) fn copy_focused_selection(&self) -> bool {
        if self.chat_search_has_focus() {
            if let Some(text) = self.chat_search_text_delegate() {
                text.emit_copy_clipboard();
            }
            return true;
        }
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
            return true;
        };
        let text = buffer.text(&start, &end, false);
        if !text.is_empty() {
            view.clipboard().set_text(&text);
        }
        true
    }

    pub(crate) fn paste_into_composer_if_focused(&self) -> bool {
        if self.chat_search_has_focus() {
            if let Some(text) = self.chat_search_text_delegate() {
                text.emit_paste_clipboard();
            }
            return true;
        }
        if !self.input_view.has_focus() || self.store.borrow().active_archived() {
            return false;
        }
        self.input_buffer
            .paste_clipboard(&self.input_view.clipboard(), None, true);
        true
    }

    fn chat_search_has_focus(&self) -> bool {
        self.chat_search.has_focus()
            || self
                .chat_search_text_delegate()
                .is_some_and(|text| text.has_focus())
    }

    fn chat_search_text_delegate(&self) -> Option<gtk4::Text> {
        self.chat_search.delegate()?.downcast::<gtk4::Text>().ok()
    }

    fn insert_visible(&self, label: &str, role_tag: &str, body: &str) {
        let mut end = self.convo_buffer.end_iter();
        if self.convo_buffer.char_count() > 0 {
            self.convo_buffer.insert(&mut end, "\n\n");
        }
        let label_start = end.offset();
        self.convo_buffer.insert(&mut end, label);
        let label_end = end.offset();
        self.convo_buffer.insert(&mut end, "\n");
        self.convo_buffer.insert(&mut end, body);
        let start = self.convo_buffer.iter_at_offset(label_start);
        let end = self.convo_buffer.iter_at_offset(label_end);
        self.convo_buffer.apply_tag_by_name(role_tag, &start, &end);
    }

    fn scroll_transcript_to_end(&self) {
        let view = self.convo_view.clone();
        let buffer = self.convo_buffer.clone();
        let adjustment = self.convo_scroll.vadjustment();
        glib::idle_add_local_once(move || {
            let mut end = buffer.end_iter();
            view.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
            adjustment.set_value(adjustment.upper());
        });
    }

    fn append_visible(&self, label: &str, role_tag: &str, body: &str) {
        let adjustment = self.convo_scroll.vadjustment();
        let was_empty = self.convo_buffer.char_count() == 0;
        let was_near_bottom =
            adjustment.value() + adjustment.page_size() >= adjustment.upper() - 32.0;
        self.insert_visible(label, role_tag, body);
        self.sync_empty_state();
        if was_empty || was_near_bottom {
            self.scroll_transcript_to_end();
        }
    }

    fn send_from_input(&self, override_text: Option<String>) {
        let (busy, archived) = {
            let store = self.store.borrow();
            (store.is_active_busy(), store.active_archived())
        };
        if busy || archived {
            return;
        }
        let text = override_text.unwrap_or_else(|| self.input_text());
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let user_text = trimmed.to_string();
        self.input_buffer.set_text("");
        self.send_with_context(user_text, None, true);
    }

    pub(crate) fn ask_about_block(&self, ctx: BlockContext) {
        if self.store.borrow().active_archived() {
            if self.store.borrow_mut().new_chat().is_err() {
                self.show_error_status(
                    "Unarchive this chat or delete another chat before asking about a Block.",
                );
                return;
            }
            self.render_active_chat();
            self.publish_persisted_conversation();
        }
        if self.store.borrow().is_active_busy() {
            return;
        }
        let prompt = if ctx.exit_code == 0 {
            "Explain what this command does and what its output means."
        } else {
            "This command failed. Diagnose the error and suggest a fix."
        };
        self.show_chat_page();
        self.send_with_context(prompt.to_string(), Some(ctx), false);
    }

    pub(crate) fn command_generation_started(&self, request: &str) -> u64 {
        let id = self.store.borrow().active_id();
        self.append_visible("You", "role-user", &format!("Generate command: {request}"));
        self.show_busy_status("Generating a reviewable command…");
        id
    }

    pub(crate) fn command_generation_review_required(&self, chat_id: u64, command: &str) {
        if chat_id == self.store.borrow().active_id() {
            self.show_info_status("Review the generated command before inserting it.");
            self.append_visible("Assistant", "role-asst", command);
        }
    }

    pub(crate) fn command_generation_inserted(&self, chat_id: u64) {
        if chat_id == self.store.borrow().active_id() {
            self.show_info_status("Inserted in the terminal for review; it was not run.");
        }
    }

    pub(crate) fn command_generation_failed(&self, chat_id: u64, error: &str) {
        if chat_id == self.store.borrow().active_id() {
            self.show_error_status(error);
            self.append_visible("Assistant", "role-err", error);
        }
    }

    fn send_with_context(
        &self,
        user_text: String,
        context: Option<BlockContext>,
        restore_pending_as_draft: bool,
    ) {
        let (user_text, context) = {
            let config = self.config.borrow();
            if config.ai_redact_secrets {
                let user_text = crate::redact::redact_secrets(&user_text);
                let context = context.map(|context| BlockContext {
                    cmd: crate::redact::redact_secrets(&context.cmd),
                    output: crate::redact::redact_secrets(&context.output),
                    cwd: context.cwd.map(|cwd| crate::redact::redact_secrets(&cwd)),
                    exit_code: context.exit_code,
                });
                (user_text, context)
            } else {
                (user_text, context)
            }
        };

        let client = ai::AiClient::from_config(&self.config.borrow());
        let provider_label = client
            .as_ref()
            .map(ai::AiClient::display_name)
            .unwrap_or_else(|_| "AI unavailable".to_string());
        let thinking = format!("Thinking… ({provider_label})");
        let visible_user = match context.as_ref() {
            Some(context) => format!(
                "{user_text}\n[context: `{}`, exit {}]",
                context.cmd, context.exit_code
            ),
            None => user_text.clone(),
        };
        let start = match self.store.borrow_mut().begin_turn(
            user_text,
            context,
            thinking,
            restore_pending_as_draft,
        ) {
            Ok(start) => start,
            Err(ChatStoreError::Archived) => {
                self.show_info_status("Unarchive this chat before sending a message.");
                return;
            }
            Err(ChatStoreError::Busy | ChatStoreError::EmptyMessage) => return,
            Err(ChatStoreError::LimitReached) => return,
            Err(ChatStoreError::SnapshotInvalid) => return,
        };

        self.append_visible("You", "role-user", &visible_user);
        self.sync_composer_state();
        self.sync_active_status();
        self.refresh_chat_chrome();
        self.refresh_chat_library();
        let system = ai::build_system_prompt(start.effective_context.as_ref());
        let history = start.history;
        let token = start.token;

        let (tx, rx) = std::sync::mpsc::channel::<Result<String, ai::AiError>>();
        std::thread::spawn(move || {
            let result =
                client.and_then(|client| client.send_turns_blocking(system.as_deref(), &history));
            let _ = tx.send(result);
        });

        let p = self.clone();
        let rx = RefCell::new(rx);
        glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            match rx.borrow().try_recv() {
                Ok(Ok(text)) => {
                    let owner_active = p.store.borrow_mut().complete_success(token, text.clone());
                    let Some(owner_active) = owner_active else {
                        return glib::ControlFlow::Break;
                    };
                    if owner_active {
                        p.append_visible("Assistant", "role-asst", &text);
                        p.sync_active_status();
                        p.sync_composer_state();
                        p.refresh_chat_chrome();
                    }
                    p.refresh_chat_library();
                    p.publish_persisted_conversation();
                    glib::ControlFlow::Break
                }
                Ok(Err(error)) => {
                    p.finish_request_error(token, format!("Error: {error}"));
                    glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    p.finish_request_error(token, "Error: worker thread disconnected".to_string());
                    glib::ControlFlow::Break
                }
            }
        });
    }

    fn finish_request_error(&self, token: RequestToken, message: String) {
        let owner_active = self
            .store
            .borrow_mut()
            .complete_error(token, message.clone());
        let Some(owner_active) = owner_active else {
            return;
        };
        if owner_active {
            let draft = self.store.borrow().active_draft().to_string();
            self.input_buffer.set_text(&draft);
            self.show_error_status(&message);
            self.append_visible("Assistant", "role-err", &message);
            self.sync_composer_state();
            self.refresh_chat_chrome();
        }
        self.refresh_chat_library();
        self.publish_persisted_conversation();
    }
}

fn menu_button(label: &str) -> Button {
    let button = Button::with_label(label);
    button.set_has_frame(false);
    button.set_halign(gtk4::Align::Fill);
    if let Some(child) = button.child() {
        child.set_halign(gtk4::Align::Start);
    }
    button.add_css_class("flat");
    button
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
