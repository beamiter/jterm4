//! dialogs — UiState methods extracted from ui (mechanical split, no logic changes)
use gtk4::gdk::Key;
use gtk4::gdk::ModifierType;
use gtk4::gio::{self};
use gtk4::pango::FontDescription;
use gtk4::{Adjustment, Label, ListBox, Orientation, Scale, ScrolledWindow};
use gtk4::{EventControllerKey, GestureClick, SearchEntry};
use libadwaita as adw;
use adw::prelude::*;
use std::rc::Rc;
use vte4::Format;
use vte4::{Terminal};
use vte4::TerminalExt;

use crate::config::save_config;
use crate::keybindings::Action;
use crate::terminal::open_uri;
use super::*;

impl UiState {

    pub(crate) async fn confirm_close_tab_with_process(
        window: &adw::ApplicationWindow,
        process_info: &str,
    ) -> bool {
        let dialog = adw::MessageDialog::builder()
            .heading("Close tab with running process?")
            .body(format!(
                "This tab has a running process:\n\n{}\n\nClosing will terminate it.",
                process_info
            ))
            .transient_for(window)
            .modal(true)
            .build();

        dialog.add_response("cancel", "Cancel");
        dialog.add_response("close", "Close Tab");
        dialog.set_response_appearance("close", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        dialog.choose_future().await == "close"
    }

    pub(crate) fn toggle_sidebar(&self) {
        self.sidebar.set_visible(!self.sidebar.is_visible());
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
            bound_actions.iter().map(|(action, binding)| {
                (Some(*action), action.name().to_string(), binding.clone())
            }).chain(
                extra_hints.iter().map(|(shortcut, desc)| {
                    (None, desc.to_string(), shortcut.to_string())
                })
            ).collect()
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
                let current = list_box_for_key.selected_row().map(|r| r.index()).unwrap_or(-1);
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
                let current = list_box_for_key.selected_row().map(|r| r.index()).unwrap_or(0);
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

        // Populate `content` from the current block-mode view's debug snapshot.
        let ui_for_populate = self.clone();
        let populate = Rc::new(move |content: &gtk4::Box| {
            while let Some(child) = content.first_child() {
                content.remove(&child);
            }
            let Some(term_view) = ui_for_populate.current_term_view() else {
                let label = Label::new(Some("Debug dashboard is only available in block mode."));
                label.add_css_class("dim-label");
                label.set_wrap(true);
                content.append(&label);
                return;
            };
            for (section, rows) in term_view.debug_info() {
                let group = adw::PreferencesGroup::new();
                group.set_title(section);
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

        let config = self.config.borrow();

        // --- Theme ---
        let theme_names: Vec<String> = self.available_themes.iter().map(|t| t.name.clone()).collect();
        let theme_model = gtk4::StringList::new(&theme_names.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let theme_row = adw::ComboRow::builder()
            .title("Theme")
            .model(&theme_model)
            .build();
        let current_theme_idx = self.available_themes.iter()
            .position(|t| t.name == config.theme_name)
            .unwrap_or(0);
        theme_row.set_selected(current_theme_idx as u32);
        group.add(&theme_row);

        // --- Font (monospace fonts from Pango) ---
        let pango_ctx = self.window.pango_context();
        let families = pango_ctx.list_families();
        let mut mono_fonts: Vec<String> = families.iter()
            .filter(|f| f.is_monospace())
            .map(|f| f.name().to_string())
            .collect();
        mono_fonts.sort_by_key(|a| a.to_lowercase());

        let current_font_desc = FontDescription::from_string(&config.font_desc);
        let current_family = current_font_desc.family()
            .map(|f| f.to_string())
            .unwrap_or_default();

        let font_model = gtk4::StringList::new(&mono_fonts.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let font_row = adw::ComboRow::builder()
            .title("Font")
            .model(&font_model)
            .build();
        let current_font_idx = mono_fonts.iter()
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
        let opacity_row = adw::ActionRow::builder()
            .title("Opacity")
            .build();
        let opacity_scale = Scale::with_range(Orientation::Horizontal, 0.01, 1.0, 0.025);
        opacity_scale.set_value(self.window_opacity.get());
        opacity_scale.set_hexpand(true);
        opacity_row.add_suffix(&opacity_scale);
        group.add(&opacity_row);

        // --- Scrollback ---
        let scrollback_adj = Adjustment::new(
            config.terminal_scrollback_lines as f64,
            0.0, 1_000_000.0, 100.0, 1000.0, 0.0,
        );
        let scrollback_row = adw::SpinRow::new(Some(&scrollback_adj), 100.0, 0);
        scrollback_row.set_title("Scrollback Lines");
        group.add(&scrollback_row);

        page.add(&group);
        dialog.add(&page);

        drop(config);

        // --- Signal: Theme ---
        let ui = self.clone();
        let themes = self.available_themes.clone();
        theme_row.connect_notify_local(Some("selected"), move |row, _| {
            let idx = row.selected() as usize;
            if let Some(theme) = themes.get(idx) {
                ui.apply_theme(theme);
                save_config(&ui.config.borrow());
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
                save_config(&ui.config.borrow());
            }
        });

        // --- Signal: Font Size ---
        let ui = self.clone();
        let mono_fonts_clone2 = mono_fonts;
        let font_row_clone = font_row.clone();
        font_size_row.connect_notify_local(Some("value"), move |row, _| {
            let idx = font_row_clone.selected() as usize;
            let family = mono_fonts_clone2.get(idx)
                .map(|s| s.as_str())
                .unwrap_or("Monospace");
            let size = row.value() as i32;
            let new_desc = format!("{} {}", family, size);
            ui.config.borrow_mut().font_desc = new_desc;
            ui.apply_font_all();
            save_config(&ui.config.borrow());
        });

        // --- Signal: Font Scale ---
        let ui = self.clone();
        font_scale_row.connect_notify_local(Some("value"), move |row, _| {
            let new_scale = row.value();
            ui.set_font_scale_all(new_scale);
            ui.config.borrow_mut().default_font_scale = new_scale;
            save_config(&ui.config.borrow());
        });

        // --- Signal: Opacity ---
        let ui = self.clone();
        opacity_scale.connect_value_changed(move |scale| {
            let val = scale.value();
            ui.window_opacity.set(val);
            ui.window.set_opacity(val);
            ui.config.borrow_mut().window_opacity = val;
            save_config(&ui.config.borrow());
        });

        // --- Signal: Scrollback ---
        let ui = self.clone();
        scrollback_row.connect_notify_local(Some("value"), move |row, _| {
            let val = row.value() as u32;
            ui.config.borrow_mut().terminal_scrollback_lines = val;
            ui.apply_scrollback_all();
            save_config(&ui.config.borrow());
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

            let menu = gio::Menu::new();
            menu.append(Some("Copy"), Some("ctx.copy"));
            menu.append(Some("Paste"), Some("ctx.paste"));

            let split_section = gio::Menu::new();
            split_section.append(Some("Split Right"), Some("ctx.split-h"));
            split_section.append(Some("Split Down"), Some("ctx.split-v"));
            menu.append_section(None, &split_section);

            let tab_section = gio::Menu::new();
            tab_section.append(Some("New Tab"), Some("ctx.new-tab"));
            tab_section.append(Some("Close Pane"), Some("ctx.close-pane"));
            menu.append_section(None, &tab_section);

            // Check if there's a hyperlink under cursor
            if let (Some(uri), _) = term.check_match_at(x, y) {
                let link_section = gio::Menu::new();
                link_section.append(Some("Open Link"), Some("ctx.open-link"));
                menu.append_section(None, &link_section);
                // Store the URI for the action
                unsafe { term.set_data::<String>("ctx-link-uri", uri.to_string()); }
            }

            let popover = gtk4::PopoverMenu::from_model(Some(&menu));
            popover.set_parent(&term);
            popover.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.set_has_arrow(false);

            // Register actions on the terminal widget
            let action_group = gio::SimpleActionGroup::new();

            let ui_copy = ui.clone();
            let copy_action = gio::SimpleAction::new("copy", None);
            let term_copy = term.clone();
            copy_action.connect_activate(move |_, _| {
                term_copy.copy_clipboard_format(Format::Text);
                let _ = &ui_copy;
            });
            action_group.add_action(&copy_action);

            let paste_action = gio::SimpleAction::new("paste", None);
            let term_paste = term.clone();
            paste_action.connect_activate(move |_, _| {
                term_paste.paste_clipboard();
            });
            action_group.add_action(&paste_action);

            let ui_split_h = ui.clone();
            let split_h_action = gio::SimpleAction::new("split-h", None);
            split_h_action.connect_activate(move |_, _| {
                ui_split_h.split_current(Orientation::Horizontal);
            });
            action_group.add_action(&split_h_action);

            let ui_split_v = ui.clone();
            let split_v_action = gio::SimpleAction::new("split-v", None);
            split_v_action.connect_activate(move |_, _| {
                ui_split_v.split_current(Orientation::Vertical);
            });
            action_group.add_action(&split_v_action);

            let ui_new_tab = ui.clone();
            let new_tab_action = gio::SimpleAction::new("new-tab", None);
            new_tab_action.connect_activate(move |_, _| {
                ui_new_tab.execute_action(Action::NewTab);
            });
            action_group.add_action(&new_tab_action);

            let ui_close = ui.clone();
            let close_action = gio::SimpleAction::new("close-pane", None);
            close_action.connect_activate(move |_, _| {
                ui_close.execute_action(Action::ClosePaneOrTab);
            });
            action_group.add_action(&close_action);

            let open_link_action = gio::SimpleAction::new("open-link", None);
            let term_link = term.clone();
            open_link_action.connect_activate(move |_, _| {
                let uri: Option<String> = unsafe {
                    term_link.data::<String>("ctx-link-uri").map(|p| p.as_ref().clone())
                };
                if let Some(uri) = uri {
                    open_uri(&uri);
                }
            });
            action_group.add_action(&open_link_action);

            term.insert_action_group("ctx", Some(&action_group));

            // Clean up when popover closes
            let term_cleanup = term.clone();
            popover.connect_closed(move |p| {
                p.unparent();
                term_cleanup.insert_action_group("ctx", None::<&gio::SimpleActionGroup>);
            });

            popover.popup();
        });

        terminal.add_controller(right_click);
    }
}
