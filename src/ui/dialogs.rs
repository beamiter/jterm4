//! dialogs — UiState methods extracted from ui (mechanical split, no logic changes)
use adw::prelude::*;
use gtk4::gdk::Key;
use gtk4::gdk::ModifierType;
use gtk4::glib;
use gtk4::pango::FontDescription;
use gtk4::{Adjustment, Label, ListBox, Orientation, Scale, ScrolledWindow};
use gtk4::{EventControllerKey, GestureClick, SearchEntry};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::Format;
use vte4::Terminal;
use vte4::TerminalExt;

use super::*;
use crate::keybindings::Action;
use crate::terminal::open_uri;

impl UiState {
    pub(crate) async fn confirm_close_with_processes(
        window: &adw::ApplicationWindow,
        heading: &str,
        close_label: &str,
        process_info: &str,
    ) -> bool {
        let dialog = adw::MessageDialog::builder()
            .heading(heading)
            .body(format!(
                "The following foreground process(es) are still running:\n\n{}\n\nClosing will terminate them.",
                process_info
            ))
            .transient_for(window)
            .modal(true)
            .build();

        dialog.add_response("cancel", "Cancel");
        dialog.add_response("close", close_label);
        dialog.set_response_appearance("close", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        dialog.choose_future().await == "close"
    }

    pub(crate) async fn confirm_close_tab_with_process(
        window: &adw::ApplicationWindow,
        process_info: &str,
    ) -> bool {
        Self::confirm_close_with_processes(
            window,
            "Close tab with running process?",
            "Close Tab",
            process_info,
        )
        .await
    }

    pub(crate) fn toggle_sidebar(&self) {
        self.set_sidebar_visible(!self.sidebar.is_visible(), true);
    }

    /// Apply sidebar visibility and optionally persist the user's choice.
    pub(crate) fn set_sidebar_visible(&self, visible: bool, persist: bool) {
        self.sidebar.set_visible(visible);
        if persist {
            self.config.borrow_mut().sidebar_visible = visible;
            self.persist_config();
        }
    }

    pub(crate) fn toggle_command_palette(&self) {
        let dialog_to_close = self.command_palette_dialog.borrow_mut().take();
        if let Some(dialog) = dialog_to_close {
            dialog.force_close();
            return;
        }

        let bound_actions = self.keybinding_map.borrow().all_bound_actions();
        // Include non-keyboard actions at end
        let extra_hints: &[(&str, &str)] = &[
            ("Double-click tab", "Rename tab"),
            ("Ctrl+Click link", "Open hyperlink"),
        ];

        let dialog = adw::Dialog::builder()
            .title("Command Palette")
            .content_width(480)
            .content_height(480)
            .build();

        let header_bar = adw::HeaderBar::new();
        let filter_entry = SearchEntry::new();
        filter_entry.set_placeholder_text(Some("Search commands..."));
        filter_entry.set_hexpand(true);

        let list_box = ListBox::new();
        list_box.set_selection_mode(gtk4::SelectionMode::Single);
        list_box.add_css_class("boxed-list");
        list_box.set_margin_start(12);
        list_box.set_margin_end(12);
        list_box.set_margin_bottom(12);

        // Store action data for filtering and execution
        let actions_data: Rc<Vec<(Option<Action>, String, String)>> = Rc::new(
            bound_actions
                .iter()
                .map(|(action, binding)| {
                    (Some(*action), action.name().to_string(), binding.clone())
                })
                .chain(
                    extra_hints
                        .iter()
                        .map(|(shortcut, desc)| (None, desc.to_string(), shortcut.to_string())),
                )
                .collect(),
        );

        for (_, description, binding) in actions_data.iter() {
            let row = adw::ActionRow::builder()
                .title(description.as_str())
                .activatable(true)
                .build();
            if !binding.is_empty() {
                let key_label = Label::new(Some(binding));
                key_label.add_css_class("dim-label");
                row.add_suffix(&key_label);
            }
            list_box.append(&row);
        }

        // Select the first row by default
        if let Some(first_row) = list_box.row_at_index(0) {
            list_box.select_row(Some(&first_row));
        }

        let scrolled = ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&list_box)
            .build();

        let search_box = gtk4::Box::new(Orientation::Vertical, 0);
        filter_entry.set_margin_start(12);
        filter_entry.set_margin_end(12);
        filter_entry.set_margin_top(8);
        filter_entry.set_margin_bottom(8);
        search_box.append(&filter_entry);
        search_box.append(&scrolled);

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header_bar);
        toolbar_view.set_content(Some(&search_box));
        dialog.set_child(Some(&toolbar_view));

        // Filter rows based on search text
        let list_box_for_filter = list_box.clone();
        let actions_data_for_filter = actions_data.clone();
        filter_entry.connect_search_changed(move |entry| {
            let query = entry.text().to_string().to_lowercase();
            let mut first_visible: Option<gtk4::ListBoxRow> = None;
            for (idx, (_, desc, binding)) in actions_data_for_filter.iter().enumerate() {
                if let Some(row) = list_box_for_filter.row_at_index(idx as i32) {
                    let visible = query.is_empty()
                        || desc.to_lowercase().contains(&query)
                        || binding.to_lowercase().contains(&query);
                    row.set_visible(visible);
                    if visible && first_visible.is_none() {
                        first_visible = Some(row);
                    }
                }
            }
            // Select first visible row
            if let Some(row) = first_visible {
                list_box_for_filter.select_row(Some(&row));
            }
        });

        // Execute action on row activation (double-click or Enter via row activate)
        let ui_for_activate = self.clone();
        let actions_data_for_activate = actions_data.clone();
        let dialog_for_activate = dialog.clone();
        list_box.connect_row_activated(move |_, row| {
            let idx = row.index() as usize;
            if let Some((Some(action), _, _)) = actions_data_for_activate.get(idx) {
                let action = *action;
                dialog_for_activate.force_close();
                ui_for_activate.execute_action(action);
            }
        });

        // Key controller: Escape to close, Enter to execute selected, up/down to navigate
        let key_controller = EventControllerKey::new();
        key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let dialog_ref = self.command_palette_dialog.clone();
        let ui_for_key = self.clone();
        let list_box_for_key = list_box.clone();
        let actions_data_for_key = actions_data.clone();
        let dialog_for_key = dialog.clone();
        key_controller.connect_key_pressed(move |_, keyval, _, state| {
            if keyval == Key::Escape
                || (matches!(keyval, Key::P | Key::p)
                    && state.contains(ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK))
            {
                let dialog_to_close = dialog_ref.borrow_mut().take();
                if let Some(d) = dialog_to_close {
                    d.force_close();
                }
                return true.into();
            }
            if matches!(keyval, Key::Return | Key::KP_Enter) {
                if let Some(row) = list_box_for_key.selected_row() {
                    let idx = row.index() as usize;
                    if let Some((Some(action), _, _)) = actions_data_for_key.get(idx) {
                        let action = *action;
                        dialog_for_key.force_close();
                        ui_for_key.execute_action(action);
                    }
                }
                return true.into();
            }
            // Up/Down arrow navigate the list while keeping focus on the search entry
            if keyval == Key::Down {
                let current = list_box_for_key
                    .selected_row()
                    .map(|r| r.index())
                    .unwrap_or(-1);
                let mut next = current + 1;
                while let Some(row) = list_box_for_key.row_at_index(next) {
                    if row.is_visible() {
                        list_box_for_key.select_row(Some(&row));
                        break;
                    }
                    next += 1;
                }
                return true.into();
            }
            if keyval == Key::Up {
                let current = list_box_for_key
                    .selected_row()
                    .map(|r| r.index())
                    .unwrap_or(0);
                let mut prev = current - 1;
                while prev >= 0 {
                    if let Some(row) = list_box_for_key.row_at_index(prev) {
                        if row.is_visible() {
                            list_box_for_key.select_row(Some(&row));
                            break;
                        }
                    }
                    prev -= 1;
                }
                return true.into();
            }
            false.into()
        });
        dialog.add_controller(key_controller);

        // Clear tracking when dialog is closed
        let dialog_ref = self.command_palette_dialog.clone();
        dialog.connect_closed(move |_| {
            *dialog_ref.borrow_mut() = None;
        });

        *self.command_palette_dialog.borrow_mut() = Some(dialog.clone());
        dialog.present(Some(&self.window));
        filter_entry.grab_focus();
    }

    /// Fuzzy picker over `config.remote_hosts`. Enter / click connects.
    pub(crate) fn show_remote_picker(&self) {
        // Toggle: a second invocation closes an open picker.
        let dialog_to_close = self.remote_picker_dialog.borrow_mut().take();
        if let Some(dialog) = dialog_to_close {
            dialog.force_close();
            return;
        }

        let hosts: Rc<Vec<crate::config::RemoteHost>> =
            Rc::new(self.config.borrow().remote_hosts.clone());
        if hosts.is_empty() {
            log::warn!("[remote] no remote_hosts configured; nothing to pick");
            return;
        }

        let dialog = adw::Dialog::builder()
            .title("Connect to Remote Host")
            .content_width(480)
            .content_height(480)
            .build();

        let header_bar = adw::HeaderBar::new();
        let filter_entry = SearchEntry::new();
        filter_entry.set_placeholder_text(Some("Search hosts..."));
        filter_entry.set_hexpand(true);

        let list_box = ListBox::new();
        list_box.set_selection_mode(gtk4::SelectionMode::Single);
        list_box.add_css_class("boxed-list");
        list_box.set_margin_start(12);
        list_box.set_margin_end(12);
        list_box.set_margin_bottom(12);

        // Searchable haystack per row: "name user@host".
        let haystacks: Rc<Vec<String>> = Rc::new(
            hosts
                .iter()
                .map(|h| {
                    let target = match &h.user {
                        Some(u) => format!("{u}@{}", h.host),
                        None => h.host.clone(),
                    };
                    format!("{} {}", h.name, target).to_lowercase()
                })
                .collect(),
        );

        for h in hosts.iter() {
            let target = match &h.user {
                Some(u) => format!("{u}@{}", h.host),
                None => h.host.clone(),
            };
            let row = adw::ActionRow::builder()
                .title(h.name.as_str())
                .subtitle(target.as_str())
                .activatable(true)
                .build();
            list_box.append(&row);
        }
        if let Some(first_row) = list_box.row_at_index(0) {
            list_box.select_row(Some(&first_row));
        }

        let scrolled = ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&list_box)
            .build();

        let search_box = gtk4::Box::new(Orientation::Vertical, 0);
        filter_entry.set_margin_start(12);
        filter_entry.set_margin_end(12);
        filter_entry.set_margin_top(8);
        filter_entry.set_margin_bottom(8);
        search_box.append(&filter_entry);
        search_box.append(&scrolled);

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header_bar);
        toolbar_view.set_content(Some(&search_box));
        dialog.set_child(Some(&toolbar_view));

        // Substring filter over the haystack.
        let list_box_for_filter = list_box.clone();
        let haystacks_for_filter = haystacks.clone();
        filter_entry.connect_search_changed(move |entry| {
            let query = entry.text().to_string().to_lowercase();
            let mut first_visible: Option<gtk4::ListBoxRow> = None;
            for (idx, hay) in haystacks_for_filter.iter().enumerate() {
                if let Some(row) = list_box_for_filter.row_at_index(idx as i32) {
                    let visible = query.is_empty() || hay.contains(&query);
                    row.set_visible(visible);
                    if visible && first_visible.is_none() {
                        first_visible = Some(row);
                    }
                }
            }
            if let Some(row) = first_visible {
                list_box_for_filter.select_row(Some(&row));
            }
        });

        let connect = {
            let ui = self.clone();
            let hosts = hosts.clone();
            move |idx: usize| {
                if let Some(h) = hosts.get(idx) {
                    ui.connect_remote(h);
                }
            }
        };

        let connect_for_activate = connect.clone();
        let dialog_for_activate = dialog.clone();
        list_box.connect_row_activated(move |_, row| {
            let idx = row.index() as usize;
            dialog_for_activate.force_close();
            connect_for_activate(idx);
        });

        let key_controller = EventControllerKey::new();
        key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let dialog_ref = self.remote_picker_dialog.clone();
        let list_box_for_key = list_box.clone();
        let dialog_for_key = dialog.clone();
        let connect_for_key = connect.clone();
        key_controller.connect_key_pressed(move |_, keyval, _, _state| {
            if keyval == Key::Escape {
                let dialog_to_close = dialog_ref.borrow_mut().take();
                if let Some(d) = dialog_to_close {
                    d.force_close();
                }
                return true.into();
            }
            if matches!(keyval, Key::Return | Key::KP_Enter) {
                if let Some(row) = list_box_for_key.selected_row() {
                    let idx = row.index() as usize;
                    dialog_for_key.force_close();
                    connect_for_key(idx);
                }
                return true.into();
            }
            if keyval == Key::Down {
                let current = list_box_for_key
                    .selected_row()
                    .map(|r| r.index())
                    .unwrap_or(-1);
                let mut next = current + 1;
                while let Some(row) = list_box_for_key.row_at_index(next) {
                    if row.is_visible() {
                        list_box_for_key.select_row(Some(&row));
                        break;
                    }
                    next += 1;
                }
                return true.into();
            }
            if keyval == Key::Up {
                let current = list_box_for_key
                    .selected_row()
                    .map(|r| r.index())
                    .unwrap_or(0);
                let mut prev = current - 1;
                while prev >= 0 {
                    if let Some(row) = list_box_for_key.row_at_index(prev) {
                        if row.is_visible() {
                            list_box_for_key.select_row(Some(&row));
                            break;
                        }
                    }
                    prev -= 1;
                }
                return true.into();
            }
            false.into()
        });
        dialog.add_controller(key_controller);

        let dialog_ref = self.remote_picker_dialog.clone();
        dialog.connect_closed(move |_| {
            *dialog_ref.borrow_mut() = None;
        });

        *self.remote_picker_dialog.borrow_mut() = Some(dialog.clone());
        dialog.present(Some(&self.window));
        filter_entry.grab_focus();
    }

    /// Palette over the active Block tab plus the lightweight cross-session
    /// history index. Enter inserts into either backend without auto-running.
    pub(crate) fn show_history_palette(&self) {
        let dialog_to_close = self.history_palette_dialog.borrow_mut().take();
        if let Some(dialog) = dialog_to_close {
            dialog.force_close();
            return;
        }

        let Some(pane) = self.current_pane_leaf() else {
            log::debug!("[history] no active terminal pane");
            return;
        };
        let mut history = pane
            .block_view()
            .map(|view| view.command_history())
            .unwrap_or_default();
        let mut seen: std::collections::HashSet<String> = history.iter().cloned().collect();
        {
            let config = self.config.borrow();
            if config.command_history_enabled {
                if let Some(path) = config.command_history_path.as_deref() {
                    for record in crate::command_history::read_recent(
                        std::path::Path::new(path),
                        config.command_history_max_entries as usize,
                    )
                    .unwrap_or_default()
                    {
                        if seen.insert(record.command.clone()) {
                            history.push(record.command);
                        }
                    }
                }
            }
        }
        let history: Rc<Vec<String>> = Rc::new(history);
        if history.is_empty() {
            log::debug!("[history] no finished commands to show");
            return;
        }

        let dialog = adw::Dialog::builder()
            .title("Command History")
            .content_width(560)
            .content_height(480)
            .build();

        let header_bar = adw::HeaderBar::new();
        let filter_entry = SearchEntry::new();
        filter_entry.set_placeholder_text(Some("Filter history…"));
        filter_entry.set_hexpand(true);

        let list_box = ListBox::new();
        list_box.set_selection_mode(gtk4::SelectionMode::Single);
        list_box.add_css_class("boxed-list");
        list_box.set_margin_start(12);
        list_box.set_margin_end(12);
        list_box.set_margin_bottom(12);

        // Lowercase haystack mirrors the displayed list for substring filter.
        let haystacks: Rc<Vec<String>> =
            Rc::new(history.iter().map(|c| c.to_lowercase()).collect());

        for cmd in history.iter() {
            // Long commands wrap inside the row; keep the first line as the
            // title so the palette stays scannable.
            let first_line = cmd.lines().next().unwrap_or(cmd);
            let row = adw::ActionRow::builder()
                .title(first_line)
                .activatable(true)
                .build();
            list_box.append(&row);
        }
        if let Some(first_row) = list_box.row_at_index(0) {
            list_box.select_row(Some(&first_row));
        }

        let scrolled = ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&list_box)
            .build();

        let search_box = gtk4::Box::new(Orientation::Vertical, 0);
        filter_entry.set_margin_start(12);
        filter_entry.set_margin_end(12);
        filter_entry.set_margin_top(8);
        filter_entry.set_margin_bottom(8);
        search_box.append(&filter_entry);
        search_box.append(&scrolled);

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header_bar);
        toolbar_view.set_content(Some(&search_box));
        dialog.set_child(Some(&toolbar_view));

        let list_box_for_filter = list_box.clone();
        let haystacks_for_filter = haystacks.clone();
        filter_entry.connect_search_changed(move |entry| {
            let query = entry.text().to_string().to_lowercase();
            let mut first_visible: Option<gtk4::ListBoxRow> = None;
            for (idx, hay) in haystacks_for_filter.iter().enumerate() {
                if let Some(row) = list_box_for_filter.row_at_index(idx as i32) {
                    let visible = query.is_empty() || hay.contains(&query);
                    row.set_visible(visible);
                    if visible && first_visible.is_none() {
                        first_visible = Some(row);
                    }
                }
            }
            if let Some(row) = first_visible {
                list_box_for_filter.select_row(Some(&row));
            }
        });

        // Paste the selected command into the live VTE. Does NOT append a
        // trailing newline — user reviews/edits, then presses Enter — which
        // matches how bash's reverse-i-search behaves.
        let paste = {
            let history = history.clone();
            let pane = pane.clone();
            let ui = self.clone();
            move |idx: usize| {
                if let Some(cmd) = history.get(idx) {
                    ui.insert_review_text(&pane, cmd);
                }
            }
        };

        let paste_for_activate = paste.clone();
        let dialog_for_activate = dialog.clone();
        list_box.connect_row_activated(move |_, row| {
            let idx = row.index() as usize;
            dialog_for_activate.force_close();
            paste_for_activate(idx);
        });

        let key_controller = EventControllerKey::new();
        key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let dialog_ref = self.history_palette_dialog.clone();
        let list_box_for_key = list_box.clone();
        let dialog_for_key = dialog.clone();
        let paste_for_key = paste.clone();
        key_controller.connect_key_pressed(move |_, keyval, _, state| {
            // Escape, or the same chord that opened the palette, closes it.
            if keyval == Key::Escape
                || (matches!(keyval, Key::H | Key::h)
                    && state.contains(ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK))
            {
                let dialog_to_close = dialog_ref.borrow_mut().take();
                if let Some(d) = dialog_to_close {
                    d.force_close();
                }
                return true.into();
            }
            if matches!(keyval, Key::Return | Key::KP_Enter) {
                if let Some(row) = list_box_for_key.selected_row() {
                    let idx = row.index() as usize;
                    dialog_for_key.force_close();
                    paste_for_key(idx);
                }
                return true.into();
            }
            if keyval == Key::Down {
                let current = list_box_for_key
                    .selected_row()
                    .map(|r| r.index())
                    .unwrap_or(-1);
                let mut next = current + 1;
                while let Some(row) = list_box_for_key.row_at_index(next) {
                    if row.is_visible() {
                        list_box_for_key.select_row(Some(&row));
                        break;
                    }
                    next += 1;
                }
                return true.into();
            }
            if keyval == Key::Up {
                let current = list_box_for_key
                    .selected_row()
                    .map(|r| r.index())
                    .unwrap_or(0);
                let mut prev = current - 1;
                while prev >= 0 {
                    if let Some(row) = list_box_for_key.row_at_index(prev) {
                        if row.is_visible() {
                            list_box_for_key.select_row(Some(&row));
                            break;
                        }
                    }
                    prev -= 1;
                }
                return true.into();
            }
            false.into()
        });
        dialog.add_controller(key_controller);

        let dialog_ref = self.history_palette_dialog.clone();
        dialog.connect_closed(move |_| {
            *dialog_ref.borrow_mut() = None;
        });

        *self.history_palette_dialog.borrow_mut() = Some(dialog.clone());
        dialog.present(Some(&self.window));
        filter_entry.grab_focus();
    }

    /// Cross-block ripgrep palette. Search-as-you-type over every finished
    /// block's command line + cached ANSI-stripped output; each hit gets a
    /// flat row (cmd preview as title, "Lnn: snippet" as subtitle). Enter
    /// scrolls the target block into view and lights its VTE search
    /// highlighter on the chord-shifted hit so the user can step further
    /// with the existing find-next chord.
    ///
    /// Default mode is case-insensitive substring; ".*" toggle switches to
    /// regex. Hit count is capped at 500 to keep the palette responsive on
    /// massive scrollbacks (`cargo build` etc.).
    pub(crate) fn show_cross_block_search(&self) {
        let dialog_to_close = self.cross_block_search_dialog.borrow_mut().take();
        if let Some(dialog) = dialog_to_close {
            dialog.force_close();
            return;
        }

        let Some(term_view) = self.current_term_view() else {
            log::debug!("[xsearch] no active block-mode tab");
            return;
        };

        let dialog = adw::Dialog::builder()
            .title("Search Blocks (ripgrep)")
            .content_width(720)
            .content_height(520)
            .build();

        let header_bar = adw::HeaderBar::new();
        let regex_toggle = gtk4::ToggleButton::builder()
            .label(".*")
            .tooltip_text("Treat the query as a regular expression")
            .build();
        header_bar.pack_end(&regex_toggle);

        let filter_entry = SearchEntry::new();
        filter_entry.set_placeholder_text(Some("Search across blocks…"));
        filter_entry.set_hexpand(true);

        let list_box = ListBox::new();
        list_box.set_selection_mode(gtk4::SelectionMode::Single);
        list_box.add_css_class("boxed-list");
        list_box.set_margin_start(12);
        list_box.set_margin_end(12);
        list_box.set_margin_bottom(12);

        let status_label = Label::new(None);
        status_label.add_css_class("dim-label");
        status_label.set_xalign(0.0);
        status_label.set_margin_start(12);
        status_label.set_margin_end(12);
        status_label.set_margin_bottom(6);

        let scrolled = ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&list_box)
            .build();

        let search_box = gtk4::Box::new(Orientation::Vertical, 0);
        filter_entry.set_margin_start(12);
        filter_entry.set_margin_end(12);
        filter_entry.set_margin_top(8);
        filter_entry.set_margin_bottom(8);
        search_box.append(&filter_entry);
        search_box.append(&status_label);
        search_box.append(&scrolled);

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header_bar);
        toolbar_view.set_content(Some(&search_box));
        dialog.set_child(Some(&toolbar_view));

        // Hits live in a RefCell so both the live-filter closure and the
        // activation closure see the current pass; rebuilt on every
        // keystroke / regex-toggle change.
        let hits: Rc<RefCell<Vec<crate::block_view::CrossBlockHit>>> =
            Rc::new(RefCell::new(Vec::new()));

        let rebuild = {
            let term_view = term_view.clone();
            let list_box = list_box.clone();
            let hits = hits.clone();
            let status_label = status_label.clone();
            let filter_entry = filter_entry.clone();
            let regex_toggle = regex_toggle.clone();
            Rc::new(move || {
                let query = filter_entry.text().to_string();
                let is_regex = regex_toggle.is_active();

                while let Some(child) = list_box.first_child() {
                    list_box.remove(&child);
                }
                if query.is_empty() {
                    hits.borrow_mut().clear();
                    status_label.set_text("Type to search across blocks.");
                    return;
                }

                match term_view.cross_block_search(&query, is_regex, 500) {
                    Ok(results) => {
                        let total = results.len();
                        if total == 0 {
                            status_label.set_text("No matches.");
                        } else if total == 500 {
                            status_label.set_text("500 matches (capped) — refine your query.");
                        } else {
                            status_label.set_text(&format!("{total} matches"));
                        }
                        for hit in results.iter() {
                            let surface = if hit.is_output { "out" } else { "cmd" };
                            let subtitle = format!(
                                "{surface} L{}: {}",
                                hit.line_no,
                                glib::markup_escape_text(&hit.line_text)
                            );
                            let row = adw::ActionRow::builder()
                                .title(glib::markup_escape_text(&hit.cmd_preview).as_str())
                                .subtitle(&subtitle)
                                .activatable(true)
                                .build();
                            list_box.append(&row);
                        }
                        *hits.borrow_mut() = results;
                        if let Some(first_row) = list_box.row_at_index(0) {
                            list_box.select_row(Some(&first_row));
                        }
                    }
                    Err(e) => {
                        hits.borrow_mut().clear();
                        status_label.set_text(&format!("Bad regex: {e}"));
                    }
                }
            })
        };

        // Initial state.
        status_label.set_text("Type to search across blocks.");

        let rebuild_for_change = rebuild.clone();
        filter_entry.connect_search_changed(move |_| {
            rebuild_for_change();
        });

        let rebuild_for_toggle = rebuild.clone();
        regex_toggle.connect_toggled(move |_| {
            rebuild_for_toggle();
        });

        // Jump-to-hit: scroll the target block into view AND turn on its
        // per-VTE search highlight at the matching hit. Closes the palette
        // so the user lands on the block they picked.
        let jump = {
            let term_view = term_view.clone();
            let hits = hits.clone();
            let filter_entry = filter_entry.clone();
            let regex_toggle = regex_toggle.clone();
            move |idx: usize| {
                let pattern = filter_entry.text().to_string();
                let is_regex = regex_toggle.is_active();
                let hit = match hits.borrow().get(idx) {
                    Some(h) => h.clone(),
                    None => return,
                };
                if term_view.scroll_to_block_id(hit.block_id) {
                    term_view.focus_match_in_block(hit.block_id, &pattern, is_regex, hit.is_output);
                }
            }
        };

        let jump_for_activate = jump.clone();
        let dialog_for_activate = dialog.clone();
        list_box.connect_row_activated(move |_, row| {
            let idx = row.index() as usize;
            dialog_for_activate.force_close();
            jump_for_activate(idx);
        });

        let key_controller = EventControllerKey::new();
        key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let dialog_ref = self.cross_block_search_dialog.clone();
        let list_box_for_key = list_box.clone();
        let dialog_for_key = dialog.clone();
        let jump_for_key = jump.clone();
        key_controller.connect_key_pressed(move |_, keyval, _, state| {
            if keyval == Key::Escape
                || (matches!(keyval, Key::G | Key::g)
                    && state.contains(ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK))
            {
                let dialog_to_close = dialog_ref.borrow_mut().take();
                if let Some(d) = dialog_to_close {
                    d.force_close();
                }
                return true.into();
            }
            if matches!(keyval, Key::Return | Key::KP_Enter) {
                if let Some(row) = list_box_for_key.selected_row() {
                    let idx = row.index() as usize;
                    dialog_for_key.force_close();
                    jump_for_key(idx);
                }
                return true.into();
            }
            if keyval == Key::Down {
                let current = list_box_for_key
                    .selected_row()
                    .map(|r| r.index())
                    .unwrap_or(-1);
                let mut next = current + 1;
                while let Some(row) = list_box_for_key.row_at_index(next) {
                    if row.is_visible() {
                        list_box_for_key.select_row(Some(&row));
                        break;
                    }
                    next += 1;
                }
                return true.into();
            }
            if keyval == Key::Up {
                let current = list_box_for_key
                    .selected_row()
                    .map(|r| r.index())
                    .unwrap_or(0);
                let mut prev = current - 1;
                while prev >= 0 {
                    if let Some(row) = list_box_for_key.row_at_index(prev) {
                        if row.is_visible() {
                            list_box_for_key.select_row(Some(&row));
                            break;
                        }
                    }
                    prev -= 1;
                }
                return true.into();
            }
            false.into()
        });
        dialog.add_controller(key_controller);

        let dialog_ref = self.cross_block_search_dialog.clone();
        dialog.connect_closed(move |_| {
            *dialog_ref.borrow_mut() = None;
        });

        *self.cross_block_search_dialog.borrow_mut() = Some(dialog.clone());
        dialog.present(Some(&self.window));
        filter_entry.grab_focus();
    }

    pub(crate) fn toggle_debug_dashboard(&self) {
        let dialog_to_close = self.debug_dashboard_dialog.borrow_mut().take();
        if let Some(dialog) = dialog_to_close {
            dialog.force_close();
            return;
        }

        let dialog = adw::Dialog::builder()
            .title("Debug Dashboard")
            .content_width(480)
            .content_height(560)
            .build();

        let header_bar = adw::HeaderBar::new();
        let refresh_btn = gtk4::Button::from_icon_name("view-refresh-symbolic");
        refresh_btn.set_tooltip_text(Some("Refresh"));
        header_bar.pack_start(&refresh_btn);

        let content = gtk4::Box::new(Orientation::Vertical, 18);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_margin_top(12);
        content.set_margin_bottom(12);

        // Populate application/session diagnostics plus the active Block
        // backend's PTY/viewport snapshot. The app-level sections remain useful
        // in conventional VTE mode instead of presenting an empty dashboard.
        let ui_for_populate = self.clone();
        let populate = Rc::new(move |content: &gtk4::Box| {
            while let Some(child) = content.first_child() {
                content.remove(&child);
            }
            let tab_count = ui_for_populate.notebook.n_pages();
            let total_panes: usize = (0..tab_count)
                .filter_map(|index| ui_for_populate.notebook.nth_page(Some(index)))
                .map(|page| {
                    PaneNode::from_widget(&page)
                        .map(|node| node.leaves().len())
                        .unwrap_or(1)
                })
                .sum();
            let active_page = ui_for_populate.notebook.current_page();
            let active_widget =
                active_page.and_then(|index| ui_for_populate.notebook.nth_page(Some(index)));
            let active_title = active_widget
                .as_ref()
                .and_then(|page| crate::state::tab_label_text(&ui_for_populate.notebook, page))
                .unwrap_or_default();
            let active_panes = active_widget
                .as_ref()
                .and_then(PaneNode::from_widget)
                .map(|node| node.leaves().len())
                .unwrap_or(0);
            let config = ui_for_populate.config.borrow();
            let mut sections = vec![
                (
                    "Session".to_string(),
                    vec![
                        ("Tabs".to_string(), tab_count.to_string()),
                        ("Total panes".to_string(), total_panes.to_string()),
                        ("Active tab".to_string(), active_title),
                        ("Panes in active tab".to_string(), active_panes.to_string()),
                        (
                            "Zoomed".to_string(),
                            ui_for_populate.zoom_state.borrow().is_some().to_string(),
                        ),
                    ],
                ),
                (
                    "Appearance".to_string(),
                    vec![
                        ("Theme".to_string(), config.theme_name.clone()),
                        ("Font".to_string(), config.font_desc.clone()),
                        (
                            "Font scale".to_string(),
                            format!("{:.3}", ui_for_populate.font_scale.get()),
                        ),
                        (
                            "Opacity".to_string(),
                            format!("{:.2}", ui_for_populate.window_opacity.get()),
                        ),
                        (
                            "Terminal mode".to_string(),
                            match &config.terminal_mode {
                                crate::config::TerminalMode::Block => "block",
                                crate::config::TerminalMode::Vte => "vte",
                            }
                            .to_string(),
                        ),
                        (
                            "Scrollback".to_string(),
                            config.terminal_scrollback_lines.to_string(),
                        ),
                    ],
                ),
                (
                    "Config".to_string(),
                    vec![
                        (
                            "Keybindings".to_string(),
                            ui_for_populate
                                .keybinding_map
                                .borrow()
                                .bindings
                                .len()
                                .to_string(),
                        ),
                        (
                            "Remote hosts".to_string(),
                            config.remote_hosts.len().to_string(),
                        ),
                        (
                            "Startup commands".to_string(),
                            config.startup_commands.clone().unwrap_or_default(),
                        ),
                    ],
                ),
            ];
            drop(config);
            if let Some(term_view) = ui_for_populate.current_term_view() {
                sections.extend(
                    term_view
                        .debug_info()
                        .into_iter()
                        .map(|(section, rows)| (format!("Block · {section}"), rows)),
                );
            } else {
                sections.push((
                    "Backend".to_string(),
                    vec![(
                        "Block diagnostics".to_string(),
                        "not available for a VTE pane".to_string(),
                    )],
                ));
            };
            for (section, rows) in sections {
                let group = adw::PreferencesGroup::new();
                group.set_title(&section);
                for (key, value) in rows {
                    let row = adw::ActionRow::builder().title(key.as_str()).build();
                    let value_label = Label::new(Some(&value));
                    value_label.add_css_class("dim-label");
                    value_label.set_selectable(true);
                    value_label.set_xalign(1.0);
                    row.add_suffix(&value_label);
                    group.add(&row);
                }
                content.append(&group);
            }
        });
        populate(&content);

        let content_for_refresh = content.clone();
        let populate_for_refresh = populate.clone();
        refresh_btn.connect_clicked(move |_| {
            populate_for_refresh(&content_for_refresh);
        });

        let scrolled = ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&content)
            .build();

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header_bar);
        toolbar_view.set_content(Some(&scrolled));
        dialog.set_child(Some(&toolbar_view));

        // Escape or F12 closes the dashboard.
        let key_controller = EventControllerKey::new();
        key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let dialog_ref = self.debug_dashboard_dialog.clone();
        key_controller.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == Key::Escape || keyval == Key::F12 {
                let dialog_to_close = dialog_ref.borrow_mut().take();
                if let Some(d) = dialog_to_close {
                    d.force_close();
                }
                return true.into();
            }
            false.into()
        });
        dialog.add_controller(key_controller);

        let dialog_ref = self.debug_dashboard_dialog.clone();
        dialog.connect_closed(move |_| {
            *dialog_ref.borrow_mut() = None;
        });

        *self.debug_dashboard_dialog.borrow_mut() = Some(dialog.clone());
        dialog.present(Some(&self.window));
    }

    pub(crate) fn toggle_settings_panel(&self) {
        let dialog_to_close = self.settings_dialog.borrow_mut().take();
        if let Some(dialog) = dialog_to_close {
            dialog.force_close();
            return;
        }

        let dialog = adw::PreferencesDialog::new();
        dialog.set_title("Settings");

        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();
        group.set_title("Appearance");

        let config = self.config.borrow();

        // --- Theme ---
        let theme_names: Vec<String> = self
            .available_themes
            .iter()
            .map(|t| t.name.clone())
            .collect();
        let theme_model =
            gtk4::StringList::new(&theme_names.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let theme_row = adw::ComboRow::builder()
            .title("Theme")
            .model(&theme_model)
            .build();
        let current_theme_idx = self
            .available_themes
            .iter()
            .position(|t| t.name == config.theme_name)
            .unwrap_or(0);
        theme_row.set_selected(current_theme_idx as u32);
        group.add(&theme_row);

        // --- Font (monospace fonts from Pango) ---
        let pango_ctx = self.window.pango_context();
        let families = pango_ctx.list_families();
        let mut mono_fonts: Vec<String> = families
            .iter()
            .filter(|f| f.is_monospace())
            .map(|f| f.name().to_string())
            .collect();
        mono_fonts.sort_by_key(|a| a.to_lowercase());

        let current_font_desc = FontDescription::from_string(&config.font_desc);
        let current_family = current_font_desc
            .family()
            .map(|f| f.to_string())
            .unwrap_or_default();

        let font_model =
            gtk4::StringList::new(&mono_fonts.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let font_row = adw::ComboRow::builder()
            .title("Font")
            .model(&font_model)
            .build();
        let current_font_idx = mono_fonts
            .iter()
            .position(|f| f == &current_family)
            .unwrap_or(0);
        font_row.set_selected(current_font_idx as u32);
        group.add(&font_row);

        // --- Font Size ---
        let current_size = current_font_desc.size() as f64 / gtk4::pango::SCALE as f64;
        let font_size_adj = Adjustment::new(current_size, 6.0, 72.0, 1.0, 4.0, 0.0);
        let font_size_row = adw::SpinRow::new(Some(&font_size_adj), 1.0, 0);
        font_size_row.set_title("Font Size");
        group.add(&font_size_row);

        // --- Font Scale ---
        let font_scale_adj = Adjustment::new(self.font_scale.get(), 0.1, 10.0, 0.025, 0.1, 0.0);
        let font_scale_row = adw::SpinRow::new(Some(&font_scale_adj), 0.025, 3);
        font_scale_row.set_title("Font Scale");
        group.add(&font_scale_row);

        // --- Opacity ---
        let opacity_row = adw::ActionRow::builder().title("Opacity").build();
        let opacity_scale = Scale::with_range(Orientation::Horizontal, 0.01, 1.0, 0.025);
        opacity_scale.set_value(self.window_opacity.get());
        opacity_scale.set_hexpand(true);
        opacity_row.add_suffix(&opacity_scale);
        group.add(&opacity_row);

        // --- Scrollback ---
        let scrollback_adj = Adjustment::new(
            config.terminal_scrollback_lines as f64,
            0.0,
            1_000_000.0,
            100.0,
            1000.0,
            0.0,
        );
        let scrollback_row = adw::SpinRow::new(Some(&scrollback_adj), 100.0, 0);
        scrollback_row.set_title("Scrollback Lines");
        group.add(&scrollback_row);

        let block_compact_row = adw::SwitchRow::builder()
            .title("Compact Block Spacing")
            .subtitle("Use the denser jterm1/Warp-style layout for new Block panes")
            .active(config.block_compact)
            .build();
        group.add(&block_compact_row);

        let terminal_group = adw::PreferencesGroup::new();
        terminal_group.set_title("Terminal & Blocks");
        let terminal_mode_model = gtk4::StringList::new(&["Block", "VTE compatibility"]);
        let terminal_mode_row = adw::ComboRow::builder()
            .title("Terminal Backend")
            .subtitle("Applies to new local tabs; splits beside Block use VTE")
            .model(&terminal_mode_model)
            .selected(match config.terminal_mode {
                crate::config::TerminalMode::Block => 0,
                crate::config::TerminalMode::Vte => 1,
            })
            .build();
        let safe_mode = std::env::var_os("JTERM4_SAFE_MODE").is_some();
        terminal_mode_row.set_sensitive(!safe_mode);
        terminal_group.add(&terminal_mode_row);

        let command_history_row = adw::SwitchRow::builder()
            .title("Command History Index")
            .subtitle("Store commands, cwd and status; never terminal output")
            .active(config.command_history_enabled)
            .build();
        command_history_row.set_sensitive(!safe_mode);
        terminal_group.add(&command_history_row);

        let privacy_group = adw::PreferencesGroup::new();
        privacy_group.set_title("Features & Privacy");
        let notifications_row = adw::SwitchRow::builder()
            .title("Long-command Notifications")
            .active(config.notify_long_blocks)
            .build();
        notifications_row.set_sensitive(!safe_mode);
        privacy_group.add(&notifications_row);

        let remote_clipboard_row = adw::SwitchRow::builder()
            .title("Allow OSC 52 Clipboard Writes")
            .subtitle("Enable only for trusted local and remote programs")
            .active(config.allow_remote_clipboard_write)
            .build();
        remote_clipboard_row.set_sensitive(!safe_mode);
        privacy_group.add(&remote_clipboard_row);

        let ai_group = adw::PreferencesGroup::new();
        ai_group.set_title("AI & Agent");
        ai_group.set_description(Some(
            "Environment variables take priority. Keys entered here are stored in a private ai.key file, never in config.toml",
        ));
        let ai_enabled_row = adw::SwitchRow::builder()
            .title("Enable AI Features")
            .active(config.ai_enabled)
            .build();
        ai_enabled_row.set_sensitive(!safe_mode);
        ai_group.add(&ai_enabled_row);

        let agent_enabled_row = adw::SwitchRow::builder()
            .title("Enable Approval-gated Agent")
            .subtitle("Every proposed command remains editable and requires approval")
            .active(config.agent_enabled)
            .build();
        agent_enabled_row.set_sensitive(!safe_mode && config.ai_enabled);
        ai_group.add(&agent_enabled_row);

        let correction_enabled_row = adw::SwitchRow::builder()
            .title("Correct Mistyped Block Commands")
            .subtitle(
                "Offer an editable correction after typo-like failures; never run automatically",
            )
            .active(config.command_correction_enabled)
            .build();
        correction_enabled_row.set_sensitive(!safe_mode && config.ai_enabled);
        ai_group.add(&correction_enabled_row);

        let provider_model = gtk4::StringList::new(&["Anthropic", "OpenAI-compatible", "Ollama"]);
        let provider_row = adw::ComboRow::builder()
            .title("Provider")
            .model(&provider_model)
            .selected(match config.ai_provider.as_str() {
                "openai-compatible" => 1,
                "ollama" => 2,
                _ => 0,
            })
            .build();
        provider_row.set_sensitive(!safe_mode && config.ai_enabled);
        ai_group.add(&provider_row);

        let model_row = adw::EntryRow::new();
        model_row.set_title("Model");
        model_row.set_text(&config.ai_model);
        model_row.set_sensitive(!safe_mode && config.ai_enabled);
        ai_group.add(&model_row);

        let base_url_row = adw::EntryRow::new();
        base_url_row.set_title("Base URL");
        base_url_row.set_text(&config.ai_base_url);
        base_url_row.set_sensitive(!safe_mode && config.ai_enabled);
        ai_group.add(&base_url_row);

        let api_key_row = adw::PasswordEntryRow::builder()
            .title("API Key — enter a new value and press Apply")
            .show_apply_button(true)
            .build();
        api_key_row.set_sensitive(!safe_mode && config.ai_enabled);
        ai_group.add(&api_key_row);

        let max_tokens_adj = Adjustment::new(
            config.ai_max_tokens as f64,
            64.0,
            32_768.0,
            64.0,
            512.0,
            0.0,
        );
        let max_tokens_row = adw::SpinRow::new(Some(&max_tokens_adj), 64.0, 0);
        max_tokens_row.set_title("Maximum Response Tokens");
        max_tokens_row.set_sensitive(!safe_mode && config.ai_enabled);
        ai_group.add(&max_tokens_row);

        let agent_turns_adj =
            Adjustment::new(config.agent_max_turns as f64, 1.0, 100.0, 1.0, 5.0, 0.0);
        let agent_turns_row = adw::SpinRow::new(Some(&agent_turns_adj), 1.0, 0);
        agent_turns_row.set_title("Agent Turn Limit");
        agent_turns_row.set_sensitive(!safe_mode && config.ai_enabled && config.agent_enabled);
        ai_group.add(&agent_turns_row);

        let redact_row = adw::SwitchRow::builder()
            .title("Redact Common Secrets")
            .subtitle("Apply before terminal context is sent to a provider")
            .active(config.ai_redact_secrets)
            .build();
        redact_row.set_sensitive(!safe_mode && config.ai_enabled);
        ai_group.add(&redact_row);

        page.add(&group);
        page.add(&terminal_group);
        page.add(&privacy_group);
        page.add(&ai_group);
        dialog.add(&page);

        drop(config);

        // --- Signal: Theme ---
        let ui = self.clone();
        let themes = self.available_themes.clone();
        theme_row.connect_notify_local(Some("selected"), move |row, _| {
            let idx = row.selected() as usize;
            if let Some(theme) = themes.get(idx) {
                ui.apply_theme(theme);
                ui.persist_config();
            }
        });

        // --- Signal: Font ---
        let ui = self.clone();
        let mono_fonts_clone = mono_fonts.clone();
        let font_size_row_clone = font_size_row.clone();
        font_row.connect_notify_local(Some("selected"), move |row, _| {
            let idx = row.selected() as usize;
            if let Some(family) = mono_fonts_clone.get(idx) {
                let size = font_size_row_clone.value() as i32;
                let new_desc = format!("{} {}", family, size);
                ui.config.borrow_mut().font_desc = new_desc;
                ui.apply_font_all();
                ui.persist_config();
            }
        });

        // --- Signal: Font Size ---
        let ui = self.clone();
        let mono_fonts_clone2 = mono_fonts;
        let font_row_clone = font_row.clone();
        font_size_row.connect_notify_local(Some("value"), move |row, _| {
            let idx = font_row_clone.selected() as usize;
            let family = mono_fonts_clone2
                .get(idx)
                .map(|s| s.as_str())
                .unwrap_or("Monospace");
            let size = row.value() as i32;
            let new_desc = format!("{} {}", family, size);
            ui.config.borrow_mut().font_desc = new_desc;
            ui.apply_font_all();
            ui.persist_config();
        });

        // --- Signal: Font Scale ---
        let ui = self.clone();
        font_scale_row.connect_notify_local(Some("value"), move |row, _| {
            let new_scale = row.value();
            ui.set_font_scale_all(new_scale);
            ui.config.borrow_mut().default_font_scale = new_scale;
            ui.persist_config();
        });

        // --- Signal: Opacity ---
        let ui = self.clone();
        opacity_scale.connect_value_changed(move |scale| {
            let val = scale.value();
            ui.window_opacity.set(val);
            ui.window.set_opacity(val);
            ui.config.borrow_mut().window_opacity = val;
            ui.persist_config();
        });

        // --- Signal: Scrollback ---
        let ui = self.clone();
        scrollback_row.connect_notify_local(Some("value"), move |row, _| {
            let val = row.value() as u32;
            ui.config.borrow_mut().terminal_scrollback_lines = val;
            ui.apply_scrollback_all();
            ui.persist_config();
        });

        let ui = self.clone();
        block_compact_row.connect_active_notify(move |row| {
            ui.config.borrow_mut().block_compact = row.is_active();
            ui.sync_block_configs();
            ui.persist_config();
        });

        let ui = self.clone();
        terminal_mode_row.connect_selected_notify(move |row| {
            ui.config.borrow_mut().terminal_mode = if row.selected() == 0 {
                crate::config::TerminalMode::Block
            } else {
                crate::config::TerminalMode::Vte
            };
            ui.persist_config();
        });

        let ui = self.clone();
        command_history_row.connect_active_notify(move |row| {
            let enabled = row.is_active();
            let mut config = ui.config.borrow_mut();
            config.command_history_enabled = enabled;
            if enabled && config.command_history_path.is_none() {
                config.command_history_path = Some(crate::config::default_command_history_path());
            }
            drop(config);
            ui.sync_block_configs();
            ui.persist_config();
        });

        let ui = self.clone();
        notifications_row.connect_active_notify(move |row| {
            ui.config.borrow_mut().notify_long_blocks = row.is_active();
            ui.sync_block_configs();
            ui.persist_config();
        });

        let ui = self.clone();
        remote_clipboard_row.connect_active_notify(move |row| {
            ui.config.borrow_mut().allow_remote_clipboard_write = row.is_active();
            ui.sync_block_configs();
            ui.persist_config();
        });

        let dependent_rows: Vec<gtk4::Widget> = vec![
            agent_enabled_row.clone().upcast(),
            correction_enabled_row.clone().upcast(),
            provider_row.clone().upcast(),
            model_row.clone().upcast(),
            base_url_row.clone().upcast(),
            api_key_row.clone().upcast(),
            max_tokens_row.clone().upcast(),
            redact_row.clone().upcast(),
        ];
        let agent_turns_for_ai = agent_turns_row.clone();
        let agent_enabled_for_ai = agent_enabled_row.clone();
        let ui = self.clone();
        ai_enabled_row.connect_active_notify(move |row| {
            let enabled = row.is_active();
            ui.config.borrow_mut().ai_enabled = enabled;
            for dependent in &dependent_rows {
                dependent.set_sensitive(enabled);
            }
            agent_turns_for_ai.set_sensitive(enabled && agent_enabled_for_ai.is_active());
            ui.sync_agent_toggle();
            ui.persist_config();
        });

        let turns_for_agent = agent_turns_row.clone();
        let ui = self.clone();
        agent_enabled_row.connect_active_notify(move |row| {
            let enabled = row.is_active();
            ui.config.borrow_mut().agent_enabled = enabled;
            turns_for_agent.set_sensitive(enabled);
            ui.sync_agent_toggle();
            ui.persist_config();
        });

        let ui = self.clone();
        correction_enabled_row.connect_active_notify(move |row| {
            ui.config.borrow_mut().command_correction_enabled = row.is_active();
            ui.persist_config();
        });

        // `Editable::set_text()` may emit `changed` for the intermediate empty
        // value while it replaces the old contents.  Provider changes update
        // these two rows programmatically, so suppress their ordinary edit
        // handlers until the matching Config fields have been replaced as one
        // coherent set.  Otherwise the intermediate empty model is validated
        // and produces a spurious "Settings were not saved" error.
        let syncing_ai_defaults = Rc::new(Cell::new(false));
        let model_for_provider = model_row.clone();
        let base_for_provider = base_url_row.clone();
        let syncing_for_provider = syncing_ai_defaults.clone();
        let ui = self.clone();
        provider_row.connect_selected_notify(move |row| {
            let provider = match row.selected() {
                1 => crate::ai::Provider::OpenAiCompatible,
                2 => crate::ai::Provider::Ollama,
                _ => crate::ai::Provider::Anthropic,
            };
            syncing_for_provider.set(true);
            model_for_provider.set_text(provider.default_model());
            base_for_provider.set_text(provider.default_base_url());
            let mut config = ui.config.borrow_mut();
            config.ai_provider = provider.as_config_value().to_string();
            config.ai_model = provider.default_model().to_string();
            config.ai_base_url = provider.default_base_url().to_string();
            drop(config);
            syncing_for_provider.set(false);
            ui.persist_config();
        });

        let syncing_for_model = syncing_ai_defaults.clone();
        let ui = self.clone();
        model_row.connect_changed(move |row| {
            if syncing_for_model.get() {
                return;
            }
            ui.config.borrow_mut().ai_model = row.text().to_string();
            ui.persist_config();
        });

        let syncing_for_base = syncing_ai_defaults;
        let ui = self.clone();
        base_url_row.connect_changed(move |row| {
            if syncing_for_base.get() {
                return;
            }
            ui.config.borrow_mut().ai_base_url = row.text().to_string();
            ui.persist_config();
        });

        let ui = self.clone();
        api_key_row.connect_apply(move |row| {
            let path = ui
                .config
                .borrow()
                .ai_api_key_file
                .clone()
                .unwrap_or_else(crate::config::default_ai_api_key_path);
            if let Err(error) = crate::ai::write_api_key_file(&path, row.text().as_str()) {
                ui.show_config_error("API Key was not saved", &error.to_string());
                return;
            }
            row.set_text("");
            row.set_title("API Key stored — enter a new value to replace it");
            ui.config.borrow_mut().ai_api_key_file = Some(path);
            ui.ai_panel.refresh_config_display();
            ui.persist_config();
        });

        let ui = self.clone();
        max_tokens_row.connect_value_notify(move |row| {
            ui.config.borrow_mut().ai_max_tokens = row.value() as u32;
            ui.persist_config();
        });

        let ui = self.clone();
        agent_turns_row.connect_value_notify(move |row| {
            ui.config.borrow_mut().agent_max_turns = row.value() as u32;
            ui.persist_config();
        });

        let ui = self.clone();
        redact_row.connect_active_notify(move |row| {
            ui.config.borrow_mut().ai_redact_secrets = row.is_active();
            ui.ai_panel.refresh_persisted_privacy();
            ui.persist_config();
        });

        // Key controller: Ctrl+Shift+O to close
        let key_controller = EventControllerKey::new();
        key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let dialog_ref = self.settings_dialog.clone();
        key_controller.connect_key_pressed(move |_, keyval, _, state| {
            if matches!(keyval, Key::O | Key::o)
                && state.contains(ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK)
            {
                let dialog_to_close = dialog_ref.borrow_mut().take();
                if let Some(d) = dialog_to_close {
                    d.force_close();
                }
                return true.into();
            }
            false.into()
        });
        dialog.add_controller(key_controller);

        let dialog_ref = self.settings_dialog.clone();
        dialog.connect_closed(move |_| {
            *dialog_ref.borrow_mut() = None;
        });

        *self.settings_dialog.borrow_mut() = Some(dialog.clone());
        dialog.present(Some(&self.window));
    }

    /// Set up right-click context menu for a terminal.
    pub(crate) fn setup_context_menu(&self, terminal: &Terminal) {
        let right_click = GestureClick::new();
        right_click.set_button(3); // Right mouse button

        let ui = self.clone();
        let term = terminal.clone();
        right_click.connect_pressed(move |gesture, _n_press, x, y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);

            // Plain Popover + Buttons: the GAction-based PopoverMenu dispatch does
            // not fire in this GTK build, so direct connect_clicked closures are used.
            let remote_hosts = ui.config.borrow().remote_hosts.clone();
            let link_uri: Option<String> = term.check_match_at(x, y).0.map(|s| s.to_string());

            let popover = gtk4::Popover::new();
            popover.set_parent(&term);
            popover.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.set_has_arrow(false);

            let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            vbox.add_css_class("menu");

            let make_item = |label: &str| -> gtk4::Button {
                let btn = gtk4::Button::with_label(label);
                btn.set_has_frame(false);
                btn.set_halign(gtk4::Align::Fill);
                if let Some(child) = btn.child() {
                    child.set_halign(gtk4::Align::Start);
                }
                btn.add_css_class("flat");
                btn
            };

            // Copy
            {
                let item = make_item("Copy");
                let popover_c = popover.clone();
                let term_copy = term.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    term_copy.copy_clipboard_format(Format::Text);
                });
                vbox.append(&item);
            }

            // Paste
            {
                let item = make_item("Paste");
                let popover_c = popover.clone();
                let ui_paste = ui.clone();
                let term_paste_target = term.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    // In block mode the visible VTE is not attached to the shell
                    // PTY.  Use the same view-aware path as Ctrl+Shift+V so
                    // multiline and bracketed paste reach the real session. Focus
                    // the clicked surface first so VTE split panes keep their
                    // existing "paste into this pane" behavior.
                    term_paste_target.grab_focus();
                    ui_paste.execute_action(Action::Paste);
                });
                vbox.append(&item);
            }

            // Split Right
            {
                let item = make_item("Split Right");
                let popover_c = popover.clone();
                let ui_split_h = ui.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    ui_split_h.split_current(Orientation::Horizontal);
                });
                vbox.append(&item);
            }

            // Split Down
            {
                let item = make_item("Split Down");
                let popover_c = popover.clone();
                let ui_split_v = ui.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    ui_split_v.split_current(Orientation::Vertical);
                });
                vbox.append(&item);
            }

            // New Tab
            {
                let item = make_item("New Tab");
                let popover_c = popover.clone();
                let ui_new_tab = ui.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    ui_new_tab.execute_action(Action::NewTab);
                });
                vbox.append(&item);
            }

            // Close Pane
            {
                let item = make_item("Close Pane");
                let popover_c = popover.clone();
                let ui_close = ui.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    ui_close.execute_action(Action::ClosePaneOrTab);
                });
                vbox.append(&item);
            }

            // Remote connect items
            for h in remote_hosts.iter() {
                let item = make_item(&format!("Connect: {}", h.name));
                let popover_c = popover.clone();
                let ui_remote = ui.clone();
                let host = h.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    ui_remote.connect_remote(&host);
                });
                vbox.append(&item);
            }

            // Open Link (only when a hyperlink is under the cursor)
            if let Some(uri) = link_uri {
                let item = make_item("Open Link");
                let popover_c = popover.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    open_uri(&uri);
                });
                vbox.append(&item);
            }

            popover.set_child(Some(&vbox));

            popover.connect_closed(move |p| {
                p.unparent();
            });

            popover.popup();
        });

        terminal.add_controller(right_click);
    }

    /// Workflows palette — fuzzy-filterable list of saved command
    /// templates from `~/.config/jterm4/workflows/`. Enter on a row
    /// either writes the command directly (no args) or opens an
    /// args-entry dialog. Same toggle-to-close model as the other
    /// palettes: re-pressing Ctrl+Shift+M with the palette open closes it.
    pub(crate) fn show_workflows_palette(&self) {
        let dialog_to_close = self.workflows_palette_dialog.borrow_mut().take();
        if let Some(dialog) = dialog_to_close {
            dialog.force_close();
            return;
        }

        let Some(pane) = self.current_pane_leaf() else {
            log::debug!("[workflows] no active terminal pane");
            return;
        };

        let workflows: Rc<Vec<crate::workflows::Workflow>> = Rc::new(crate::workflows::load_all());
        if workflows.is_empty() {
            log::debug!(
                "[workflows] no workflows in {}",
                crate::workflows::workflows_dir().display()
            );
            // Toast-like hint via a one-shot message dialog. Otherwise the
            // user gets no feedback at all and concludes the chord is dead.
            let dialog = adw::MessageDialog::builder()
                .heading("No workflows yet")
                .body(format!(
                    "Add `*.toml`, `*.yaml`, or `*.yml` workflow files to:\n\n{}",
                    crate::workflows::workflows_dir().display()
                ))
                .build();
            dialog.add_response("ok", "OK");
            dialog.set_transient_for(Some(&self.window));
            dialog.present();
            return;
        }

        let dialog = adw::Dialog::builder()
            .title("Workflows")
            .content_width(620)
            .content_height(480)
            .build();

        let header_bar = adw::HeaderBar::new();
        let filter_entry = SearchEntry::new();
        filter_entry.set_placeholder_text(Some("Filter workflows…"));
        filter_entry.set_hexpand(true);

        let list_box = ListBox::new();
        list_box.set_selection_mode(gtk4::SelectionMode::Single);
        list_box.add_css_class("boxed-list");
        list_box.set_margin_start(12);
        list_box.set_margin_end(12);
        list_box.set_margin_bottom(12);

        // Haystack = name + description + command, all lowercased.
        let haystacks: Rc<Vec<String>> = Rc::new(
            workflows
                .iter()
                .map(|w| {
                    format!(
                        "{} {} {} {}",
                        w.name,
                        w.description,
                        w.command,
                        w.tags.join(" ")
                    )
                    .to_lowercase()
                })
                .collect(),
        );

        for wf in workflows.iter() {
            let subtitle = if wf.description.is_empty() {
                wf.command.clone()
            } else {
                wf.description.clone()
            };
            let row = adw::ActionRow::builder()
                .title(&wf.name)
                .subtitle(&subtitle)
                .activatable(true)
                .build();
            list_box.append(&row);
        }
        if let Some(first_row) = list_box.row_at_index(0) {
            list_box.select_row(Some(&first_row));
        }

        let scrolled = ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&list_box)
            .build();

        let search_box = gtk4::Box::new(Orientation::Vertical, 0);
        filter_entry.set_margin_start(12);
        filter_entry.set_margin_end(12);
        filter_entry.set_margin_top(8);
        filter_entry.set_margin_bottom(8);
        search_box.append(&filter_entry);
        search_box.append(&scrolled);

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header_bar);
        toolbar_view.set_content(Some(&search_box));
        dialog.set_child(Some(&toolbar_view));

        let list_box_for_filter = list_box.clone();
        let haystacks_for_filter = haystacks.clone();
        filter_entry.connect_search_changed(move |entry| {
            let query = entry.text().to_string().to_lowercase();
            let mut first_visible: Option<gtk4::ListBoxRow> = None;
            for (idx, hay) in haystacks_for_filter.iter().enumerate() {
                if let Some(row) = list_box_for_filter.row_at_index(idx as i32) {
                    let visible = query.is_empty() || hay.contains(&query);
                    row.set_visible(visible);
                    if visible && first_visible.is_none() {
                        first_visible = Some(row);
                    }
                }
            }
            if let Some(row) = first_visible {
                list_box_for_filter.select_row(Some(&row));
            }
        });

        // Pick is the only verb here: either write the command directly
        // (no args) or hand off to the args dialog. Cloning the Vec is
        // cheap relative to the dialog work that follows.
        let workflows_for_pick = workflows.clone();
        let ui_self = self.clone();
        let pane_for_pick = pane.clone();
        let pick = Rc::new(move |idx: usize| {
            let Some(wf) = workflows_for_pick.get(idx).cloned() else {
                return;
            };
            if wf.args.is_empty() {
                ui_self.insert_review_text(&pane_for_pick, &wf.command);
            } else {
                ui_self.show_workflow_args_dialog(wf, pane_for_pick.clone());
            }
        });

        let pick_for_activate = pick.clone();
        let dialog_for_activate = dialog.clone();
        list_box.connect_row_activated(move |_, row| {
            let idx = row.index() as usize;
            dialog_for_activate.force_close();
            pick_for_activate(idx);
        });

        let key_controller = EventControllerKey::new();
        key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let dialog_ref = self.workflows_palette_dialog.clone();
        let list_box_for_key = list_box.clone();
        let dialog_for_key = dialog.clone();
        let pick_for_key = pick.clone();
        key_controller.connect_key_pressed(move |_, keyval, _, state| {
            if keyval == Key::Escape
                || (matches!(keyval, Key::M | Key::m)
                    && state.contains(ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK))
            {
                let dialog_to_close = dialog_ref.borrow_mut().take();
                if let Some(d) = dialog_to_close {
                    d.force_close();
                }
                return true.into();
            }
            if matches!(keyval, Key::Return | Key::KP_Enter) {
                if let Some(row) = list_box_for_key.selected_row() {
                    let idx = row.index() as usize;
                    dialog_for_key.force_close();
                    pick_for_key(idx);
                }
                return true.into();
            }
            if keyval == Key::Down {
                let current = list_box_for_key
                    .selected_row()
                    .map(|r| r.index())
                    .unwrap_or(-1);
                let mut next = current + 1;
                while let Some(row) = list_box_for_key.row_at_index(next) {
                    if row.is_visible() {
                        list_box_for_key.select_row(Some(&row));
                        break;
                    }
                    next += 1;
                }
                return true.into();
            }
            if keyval == Key::Up {
                let current = list_box_for_key
                    .selected_row()
                    .map(|r| r.index())
                    .unwrap_or(0);
                let mut prev = current - 1;
                while prev >= 0 {
                    if let Some(row) = list_box_for_key.row_at_index(prev) {
                        if row.is_visible() {
                            list_box_for_key.select_row(Some(&row));
                            break;
                        }
                    }
                    prev -= 1;
                }
                return true.into();
            }
            false.into()
        });
        dialog.add_controller(key_controller);

        let dialog_ref = self.workflows_palette_dialog.clone();
        dialog.connect_closed(move |_| {
            *dialog_ref.borrow_mut() = None;
        });

        *self.workflows_palette_dialog.borrow_mut() = Some(dialog.clone());
        dialog.present(Some(&self.window));
        filter_entry.grab_focus();
    }

    /// Modal arg-entry dialog for a workflow. One Entry per arg, default
    /// pre-filled; "Insert command" substitutes and writes the resolved command into
    /// the live PTY (without a trailing newline — user reviews and hits
    /// Enter). Cancel/Escape exits without touching the terminal.
    pub(crate) fn show_workflow_args_dialog(
        &self,
        wf: crate::workflows::Workflow,
        pane: crate::ui::PaneLeaf,
    ) {
        let dialog = adw::Dialog::builder()
            .title(format!("Workflow: {}", wf.name))
            .content_width(520)
            .build();

        let header_bar = adw::HeaderBar::new();
        let body = gtk4::Box::new(Orientation::Vertical, 8);
        body.set_margin_start(16);
        body.set_margin_end(16);
        body.set_margin_top(12);
        body.set_margin_bottom(12);

        if !wf.description.is_empty() {
            let desc = Label::new(Some(&wf.description));
            desc.set_xalign(0.0);
            desc.set_wrap(true);
            desc.add_css_class("dim-label");
            body.append(&desc);
        }

        // Preview of the template (so the user sees which placeholders
        // they're filling). Monospace-leaning.
        let preview = Label::new(Some(&wf.command));
        preview.set_xalign(0.0);
        preview.set_wrap(true);
        preview.set_selectable(true);
        preview.add_css_class("monospace");
        body.append(&preview);

        // One row per arg.
        let entries: Rc<RefCell<Vec<(String, gtk4::Entry)>>> = Rc::new(RefCell::new(Vec::new()));
        for arg in wf.args.iter() {
            let row = adw::EntryRow::builder()
                .title(&arg.name)
                .text(&arg.default)
                .build();
            if !arg.description.is_empty() {
                row.set_tooltip_text(Some(&arg.description));
            }
            body.append(&row);
            // EntryRow doesn't expose a stable `Entry` handle in this
            // gtk-rs version, so we stash a gtk4::Entry mirror that we
            // bind two-way. Simpler than digging the inner Entry out.
            let entry = gtk4::Entry::new();
            entry.set_text(&arg.default);
            entry.set_visible(false);
            body.append(&entry);
            {
                let entry_clone = entry.clone();
                row.connect_changed(move |r| {
                    entry_clone.set_text(&r.text());
                });
            }
            entries.borrow_mut().push((arg.name.clone(), entry));
        }

        let run_btn = gtk4::Button::with_label("Insert command");
        run_btn.add_css_class("suggested-action");
        run_btn.set_halign(gtk4::Align::End);
        run_btn.set_margin_top(8);
        body.append(&run_btn);

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header_bar);
        toolbar_view.set_content(Some(&body));
        dialog.set_child(Some(&toolbar_view));

        let entries_for_run = entries.clone();
        let pane_for_run = pane.clone();
        let ui_for_run = self.clone();
        let dialog_for_run = dialog.clone();
        let template = wf.command.clone();
        run_btn.connect_clicked(move |_| {
            let bindings: Vec<(String, String)> = entries_for_run
                .borrow()
                .iter()
                .map(|(n, e)| (n.clone(), e.text().to_string()))
                .collect();
            let resolved = crate::workflows::substitute(&template, &bindings);
            dialog_for_run.force_close();
            ui_for_run.insert_review_text(&pane_for_run, &resolved);
        });

        // Escape closes; Ctrl+Enter from any field inserts the command for
        // keyboard-only operation. It deliberately never sends Enter to PTY.
        let key_controller = EventControllerKey::new();
        key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let dialog_for_key = dialog.clone();
        let run_btn_for_key = run_btn.clone();
        key_controller.connect_key_pressed(move |_, keyval, _, state| {
            if keyval == Key::Escape {
                dialog_for_key.force_close();
                return true.into();
            }
            if matches!(keyval, Key::Return | Key::KP_Enter)
                && state.contains(ModifierType::CONTROL_MASK)
            {
                run_btn_for_key.emit_clicked();
                return true.into();
            }
            false.into()
        });
        dialog.add_controller(key_controller);

        dialog.present(Some(&self.window));
    }
}
