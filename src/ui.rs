use gtk4::gdk::ffi::GDK_BUTTON_PRIMARY;
use gtk4::gdk::Key;
use gtk4::gdk::ModifierType;
use gtk4::gdk::RGBA;
use gtk4::gio::{self};
use gtk4::pango::FontDescription;
use gtk4::{glib, Adjustment, Label, ListBox, Notebook, Orientation, Paned, Scale, ScrolledWindow};
use gtk4::{CssProvider, EventControllerKey, GestureClick, SearchBar, SearchEntry, ToggleButton};
use libadwaita as adw;
use adw::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use vte4::Format;
use vte4::{Terminal};
use vte4::{TerminalExt, TerminalExtManual};

use crate::config::{Config, Theme, load_config, save_config};
use crate::keybindings::{Action, Direction, KeybindingMap};
use crate::state::{generate_session_id, kill_terminal_child};
use crate::block_view::TermView;
use crate::terminal::{
    create_terminal, wrap_with_scrollbar, scrollbar_wrapper_of,
    terminal_working_directory, spawn_shell, open_uri,
    setup_terminal_click_handler, show_rename_dialog, show_rename_dialog_with_strip,
    default_tab_title,
    find_first_terminal, find_focused_terminal, collect_terminals, reattach_terminal_to_tree,
};

pub(crate) struct ZoomState {
    pub(crate) original_page: gtk4::Widget,
    pub(crate) zoomed_terminal: Terminal,
    pub(crate) page_index: u32,
    pub(crate) tab_label: Option<gtk4::Widget>,
}

#[derive(Clone)]
pub(crate) struct UiState {
    pub(crate) window: adw::ApplicationWindow,
    pub(crate) notebook: Notebook,
    pub(crate) tab_counter: Rc<Cell<u32>>,
    pub(crate) font_scale: Rc<Cell<f64>>,
    pub(crate) window_opacity: Rc<Cell<f64>>,
    pub(crate) shell_argv: Rc<Vec<String>>,
    pub(crate) config: Rc<RefCell<Config>>,
    pub(crate) available_themes: Rc<Vec<Theme>>,
    pub(crate) search_bar: SearchBar,
    pub(crate) search_entry: SearchEntry,
    pub(crate) tab_strip: gtk4::Box,
    pub(crate) sidebar: gtk4::Box,
    pub(crate) command_palette_dialog: Rc<RefCell<Option<adw::Dialog>>>,
    pub(crate) settings_dialog: Rc<RefCell<Option<adw::PreferencesDialog>>>,
    pub(crate) keybinding_map: Rc<RefCell<KeybindingMap>>,
    pub(crate) zoom_state: Rc<RefCell<Option<ZoomState>>>,
    pub(crate) scrollbar_css: CssProvider,
    /// Maps tab_num → session_id for rsh session persistence.
    pub(crate) session_ids: Rc<RefCell<HashMap<u32, String>>>,
}

impl UiState {
    pub(crate) fn execute_action(&self, action: Action) {
        let font_step = 0.025;
        let opacity_step = 0.025;
        let current_terminal = self.current_terminal();

        match action {
            Action::NewTab => {
                log::info!("New tab");
                let working_directory = current_terminal
                    .as_ref()
                    .and_then(terminal_working_directory);
                let startup = self.config.borrow().startup_commands.clone();
                self.add_new_tab(working_directory, None, None, startup);
            }
            Action::CloseTab => {
                log::info!("Close tab");
                self.remove_current_tab();
            }
            Action::ClosePaneOrTab => {
                log::info!("Close focused pane or tab");
                self.close_focused_pane_or_tab();
            }
            Action::Copy => {
                log::debug!("Copy");
                if let Some(ref term) = current_terminal {
                    term.copy_clipboard_format(Format::Text);
                }
            }
            Action::Paste => {
                log::debug!("Paste");
                if let Some(ref term) = current_terminal {
                    term.paste_clipboard();
                }
            }
            Action::FontIncrease => {
                log::debug!("Font increase");
                let new_scale = (self.font_scale.get() + font_step).min(10.0);
                self.set_font_scale_all(new_scale);
            }
            Action::FontDecrease => {
                log::debug!("Font decrease");
                let new_scale = (self.font_scale.get() - font_step).max(0.1);
                self.set_font_scale_all(new_scale);
            }
            Action::OpacityIncrease => {
                log::debug!("Opacity increase");
                self.window_opacity
                    .set((self.window_opacity.get() + opacity_step).clamp(0.01, 1.0));
                self.window.set_opacity(self.window_opacity.get());
            }
            Action::OpacityDecrease => {
                log::debug!("Opacity decrease");
                self.window_opacity
                    .set((self.window_opacity.get() - opacity_step).clamp(0.01, 1.0));
                self.window.set_opacity(self.window_opacity.get());
            }
            Action::ToggleSearch => {
                log::debug!("Toggle search");
                self.toggle_search();
            }
            Action::ToggleCommandPalette => {
                log::debug!("Toggle command palette");
                self.toggle_command_palette();
            }
            Action::ToggleSettings => {
                log::debug!("Toggle settings panel");
                self.toggle_settings_panel();
            }
            Action::ToggleSidebar => {
                log::debug!("Toggle sidebar");
                self.toggle_sidebar();
            }
            Action::SplitHorizontal => {
                log::debug!("Split horizontal");
                self.split_current(Orientation::Horizontal);
            }
            Action::SplitVertical => {
                log::debug!("Split vertical");
                self.split_current(Orientation::Vertical);
            }
            Action::PrevTab => {
                self.switch_tab(-1);
            }
            Action::NextTab => {
                self.switch_tab(1);
            }
            Action::ScrollUp => {
                if let Some(ref term) = current_terminal {
                    if let Some(adj) = term.vadjustment() {
                        let new_val = (adj.value() - adj.step_increment() * 3.0).max(adj.lower());
                        adj.set_value(new_val);
                    }
                }
            }
            Action::ScrollDown => {
                if let Some(ref term) = current_terminal {
                    if let Some(adj) = term.vadjustment() {
                        let max_val = adj.upper() - adj.page_size();
                        let new_val = (adj.value() + adj.step_increment() * 3.0).min(max_val);
                        adj.set_value(new_val);
                    }
                }
            }
            Action::CyclePaneFocusForward => {
                self.cycle_pane_focus(1);
            }
            Action::CyclePaneFocusBackward => {
                self.cycle_pane_focus(-1);
            }
            Action::QuickSwitchTab(n) => {
                let n_pages = self.notebook.n_pages();
                if n_pages > 0 {
                    let target = if n == 9 {
                        n_pages - 1
                    } else {
                        (n as u32).min(n_pages - 1)
                    };
                    self.notebook.set_current_page(Some(target));
                }
            }
            Action::ResizePaneLeft => {
                self.resize_pane(Orientation::Horizontal, -30);
            }
            Action::ResizePaneRight => {
                self.resize_pane(Orientation::Horizontal, 30);
            }
            Action::ResizePaneUp => {
                self.resize_pane(Orientation::Vertical, -30);
            }
            Action::ResizePaneDown => {
                self.resize_pane(Orientation::Vertical, 30);
            }
            Action::TogglePaneZoom => {
                self.toggle_pane_zoom();
            }
            Action::MovePaneToNewTab => {
                self.move_pane_to_new_tab();
            }
            Action::FocusPaneLeft => {
                self.focus_pane_directional(Direction::Left);
            }
            Action::FocusPaneRight => {
                self.focus_pane_directional(Direction::Right);
            }
            Action::FocusPaneUp => {
                self.focus_pane_directional(Direction::Up);
            }
            Action::FocusPaneDown => {
                self.focus_pane_directional(Direction::Down);
            }
        }
    }

    /// Update which tab strip button is :checked to match the active notebook page.
    pub(crate) fn sync_tab_strip_active(&self, active_page: Option<u32>) {
        let active = active_page.or(self.notebook.current_page()).unwrap_or(0);
        let mut idx = 0u32;
        let mut child = self.tab_strip.first_child();
        while let Some(c) = child {
            if let Ok(btn) = c.clone().downcast::<ToggleButton>() {
                btn.set_active(idx == active);
            }
            idx += 1;
            child = c.next_sibling();
        }
    }

    /// Hide sidebar when only one tab exists (zen mode).
    pub(crate) fn sync_tab_bar_visibility(&self) {
        self.sidebar.set_visible(self.notebook.n_pages() > 1);
    }

    /// Remove the tab strip button that corresponds to a notebook page widget.
    pub(crate) fn remove_strip_button_for(&self, widget: &gtk4::Widget) {
        let name = widget.widget_name();
        let mut child = self.tab_strip.first_child();
        while let Some(c) = child {
            if c.widget_name() == name {
                self.tab_strip.remove(&c);
                return;
            }
            child = c.next_sibling();
        }
    }

    pub(crate) fn focus_current_terminal(&self) {
        if let Some(page) = self.notebook.current_page() {
            if let Some(widget) = self.notebook.nth_page(Some(page)) {
                if let Some(term) = find_first_terminal(&widget) {
                    term.grab_focus();
                }
            }
        }
    }

    pub(crate) fn remove_tab_by_widget(&self, widget: &gtk4::Widget) {
        // Kill all shell processes in this tab before removing it
        let mut terms = Vec::new();
        collect_terminals(widget, &mut terms);
        for term in &terms {
            kill_terminal_child(term);
        }
        self.remove_strip_button_for(widget);
        if let Some(page_num) = self.notebook.page_num(widget) {
            self.notebook.remove_page(Some(page_num));
        }
        if self.notebook.n_pages() == 0 {
            self.window.destroy();
        } else {
            self.sync_tab_strip_active(None);
            self.sync_tab_bar_visibility();
            self.focus_current_terminal();
        }
    }

    /// Handle a terminal exiting: unsplit if in a Paned, or close the tab.
    pub(crate) fn handle_terminal_exited(&self, term_widget: &gtk4::Widget) {
        // Clear zoom state if the exiting terminal is the zoomed one
        {
            let zoom = self.zoom_state.borrow();
            if let Some(ref zs) = *zoom {
                if zs.zoomed_terminal.upcast_ref::<gtk4::Widget>() == term_widget {
                    drop(zoom);
                    self.zoom_state.borrow_mut().take();
                }
            }
        }

        // The terminal may be wrapped in a scrollbar Box. The "effective widget"
        // is the wrapper Box if present, otherwise the terminal itself.
        let effective_widget = scrollbar_wrapper_of(term_widget)
            .map(|bx| bx.upcast::<gtk4::Widget>())
            .unwrap_or_else(|| term_widget.clone());

        let Some(parent) = effective_widget.parent() else {
            return;
        };

        if let Ok(paned) = parent.clone().downcast::<Paned>() {
            let start = paned.start_child();
            let end = paned.end_child();
            let sibling = if start.as_ref() == Some(&effective_widget) {
                end
            } else {
                start
            };

            if let Some(sibling) = sibling {
                paned.set_start_child(None::<&gtk4::Widget>);
                paned.set_end_child(None::<&gtk4::Widget>);

                let paned_widget = paned.upcast::<gtk4::Widget>();
                if let Some(grandparent) = paned_widget.parent() {
                    if let Ok(gp_paned) = grandparent.clone().downcast::<Paned>() {
                        if gp_paned.start_child().as_ref() == Some(&paned_widget) {
                            gp_paned.set_start_child(Some(&sibling));
                        } else {
                            gp_paned.set_end_child(Some(&sibling));
                        }
                    } else {
                        for i in 0..self.notebook.n_pages() {
                            if let Some(page_widget) = self.notebook.nth_page(Some(i)) {
                                if page_widget == paned_widget {
                                    // Transfer widget name so strip button mapping is preserved
                                    sibling.set_widget_name(&page_widget.widget_name());
                                    let tab_label = self.notebook.tab_label(&page_widget);
                                    self.notebook.remove_page(Some(i));
                                    let new_page_num = self.notebook.insert_page(
                                        &sibling,
                                        tab_label.as_ref(),
                                        Some(i),
                                    );
                                    self.notebook.set_tab_reorderable(&sibling, true);
                                    self.notebook.set_current_page(Some(new_page_num));
                                    break;
                                }
                            }
                        }
                    }
                }

                if let Some(term) = find_first_terminal(&sibling) {
                    term.grab_focus();
                }
            }
        } else {
            self.remove_tab_by_widget(&effective_widget);
        }
    }

    pub(crate) fn set_font_scale_all(&self, new_scale: f64) {
        self.font_scale.set(new_scale);
        for i in 0..self.notebook.n_pages() {
            if let Some(widget) = self.notebook.nth_page(Some(i)) {
                let mut terms = Vec::new();
                collect_terminals(&widget, &mut terms);
                for term in terms {
                    term.set_font_scale(new_scale);
                }
            }
        }
    }

    pub(crate) fn for_each_terminal(&self, f: impl Fn(&Terminal)) {
        for i in 0..self.notebook.n_pages() {
            if let Some(widget) = self.notebook.nth_page(Some(i)) {
                let mut terms = Vec::new();
                collect_terminals(&widget, &mut terms);
                for term in terms {
                    f(&term);
                }
            }
        }
    }

    pub(crate) fn apply_colors_all(&self) {
        let config = self.config.borrow();
        let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
        self.for_each_terminal(|term| {
            term.set_colors(Some(&config.foreground), Some(&config.background), &palette_refs);
            term.set_color_bold(None);
            term.set_color_cursor(Some(&config.cursor));
            term.set_color_cursor_foreground(Some(&config.cursor_foreground));
        });
        drop(config);
        self.apply_dynamic_css();
    }

    pub(crate) fn apply_dynamic_css(&self) {
        let config = self.config.borrow();
        let bg = &config.background;
        let fg = &config.foreground;
        let br = (bg.red() * 255.0) as u8;
        let bg_g = (bg.green() * 255.0) as u8;
        let bb = (bg.blue() * 255.0) as u8;
        let fr = (fg.red() * 255.0) as u8;
        let fg_g = (fg.green() * 255.0) as u8;
        let fb = (fg.blue() * 255.0) as u8;
        let css = format!(
            ".terminal-box scrollbar {{ background-color: rgb({br},{bg_g},{bb}); }}
             .terminal-box scrollbar trough {{ background-color: rgb({br},{bg_g},{bb}); }}
             .terminal-box scrollbar slider {{ background-color: rgba({fr},{fg_g},{fb},0.4); }}
             .terminal-box scrollbar slider:hover {{ background-color: rgba({fr},{fg_g},{fb},0.7); }}
             .top-bar {{ background-color: rgb({br},{bg_g},{bb}); color: rgb({fr},{fg_g},{fb}); }}
             .top-bar button {{ color: rgb({fr},{fg_g},{fb}); }}
             .sidebar-box {{ background-color: rgb({br},{bg_g},{bb}); }}
             .tab-strip-btn {{ color: rgba({fr},{fg_g},{fb},0.6); }}
             .tab-strip-btn:checked {{ color: rgb({fr},{fg_g},{fb}); }}"
        );
        self.scrollbar_css.load_from_data(&css);
    }

    pub(crate) fn apply_font_all(&self) {
        let config = self.config.borrow();
        let font_desc = FontDescription::from_string(&config.font_desc);
        self.for_each_terminal(|term| {
            term.set_font(Some(&font_desc));
        });
    }

    pub(crate) fn apply_scrollback_all(&self) {
        let lines = self.config.borrow().terminal_scrollback_lines;
        self.for_each_terminal(|term| {
            term.set_scrollback_lines(lines as i64);
        });
    }

    pub(crate) fn apply_theme(&self, theme: &Theme) {
        {
            let mut config = self.config.borrow_mut();
            config.theme_name = theme.name.clone();
            config.foreground = theme.foreground;
            config.background = theme.background;
            config.cursor = theme.cursor;
            config.cursor_foreground = theme.cursor_foreground;
            config.palette = theme.palette;
        }
        self.apply_colors_all();
    }

    pub(crate) fn switch_tab(&self, direction: i32) {
        if let Some(page) = self.notebook.current_page() {
            let n = self.notebook.n_pages();
            if n == 0 {
                return;
            }
            let next = if direction > 0 {
                if page < n - 1 { page + 1 } else { 0 }
            } else {
                if page > 0 { page - 1 } else { n.saturating_sub(1) }
            };
            self.notebook.set_current_page(Some(next));
        }
    }

    pub(crate) fn remove_current_tab(&self) {
        if let Some(page_num) = self.notebook.current_page() {
            // Kill shell processes and remove the strip button for the current page
            if let Some(widget) = self.notebook.nth_page(Some(page_num)) {
                let mut terms = Vec::new();
                collect_terminals(&widget, &mut terms);
                for term in &terms {
                    kill_terminal_child(term);
                }
                self.remove_strip_button_for(&widget);
            }
            self.notebook.remove_page(Some(page_num));
            if self.notebook.n_pages() == 0 {
                self.window.destroy();
            } else {
                self.sync_tab_strip_active(None);
                self.sync_tab_bar_visibility();
                self.focus_current_terminal();
            }
        }
    }

    pub(crate) fn close_focused_pane_or_tab(&self) {
        if let Some(page_num) = self.notebook.current_page() {
            if let Some(widget) = self.notebook.nth_page(Some(page_num)) {
                // If the page has splits, close the focused pane only
                if widget.clone().downcast::<Paned>().is_ok() {
                    if let Some(term) = find_focused_terminal(&widget) {
                        kill_terminal_child(&term);
                        self.handle_terminal_exited(&term.upcast::<gtk4::Widget>());
                        return;
                    }
                }
            }
        }
        self.remove_current_tab();
    }

    pub(crate) fn current_terminal(&self) -> Option<Terminal> {
        self.notebook.current_page().and_then(|page_num| {
            self.notebook.nth_page(Some(page_num)).and_then(|widget| {
                // Try focused terminal first (for split panes), then fall back to first terminal
                find_focused_terminal(&widget).or_else(|| find_first_terminal(&widget))
            })
        })
    }

    pub(crate) fn toggle_search(&self) {
        let visible = self.search_bar.is_search_mode();
        self.search_bar.set_search_mode(!visible);
        if !visible {
            self.search_entry.grab_focus();
        } else {
            // Clear search highlight when closing
            if let Some(term) = self.current_terminal() {
                term.search_set_regex(None::<&vte4::Regex>, 0);
            }
            self.focus_current_terminal();
        }
    }

    pub(crate) fn search_apply(&self) {
        let text = self.search_entry.text();
        if text.is_empty() {
            return;
        }
        if let Some(term) = self.current_terminal() {
            let escaped = glib::Regex::escape_string(&text);
            let regex = vte4::Regex::for_search(&escaped, pcre2_sys::PCRE2_CASELESS);
            if let Ok(regex) = regex {
                term.search_set_regex(Some(&regex), 0);
                term.search_set_wrap_around(true);
                term.search_find_next();
            }
        }
    }

    pub(crate) fn search_next(&self) {
        if let Some(term) = self.current_terminal() {
            term.search_find_next();
        }
    }

    pub(crate) fn search_prev(&self) {
        if let Some(term) = self.current_terminal() {
            term.search_find_previous();
        }
    }

    pub(crate) fn toggle_sidebar(&self) {
        self.sidebar.set_visible(!self.sidebar.is_visible());
    }

    pub(crate) fn toggle_command_palette(&self) {
        if let Some(dialog) = self.command_palette_dialog.borrow_mut().take() {
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
                if let Some(d) = dialog_ref.borrow_mut().take() {
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

    pub(crate) fn toggle_settings_panel(&self) {
        if let Some(dialog) = self.settings_dialog.borrow_mut().take() {
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
        mono_fonts.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));

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
                if let Some(d) = dialog_ref.borrow_mut().take() {
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

    /// Find the strip button widget for a given tab widget name.
    pub(crate) fn find_strip_button(&self, widget_name: &str) -> Option<ToggleButton> {
        let mut child = self.tab_strip.first_child();
        while let Some(c) = child {
            if c.widget_name().as_str() == widget_name {
                return c.downcast::<ToggleButton>().ok();
            }
            child = c.next_sibling();
        }
        None
    }

    /// Mark a tab as having activity (new output on a non-active tab).
    pub(crate) fn mark_tab_activity(&self, tab_widget_name: &str) {
        if let Some(btn) = self.find_strip_button(tab_widget_name) {
            if !btn.is_active() {
                btn.add_css_class("tab-activity");
            }
        }
    }

    /// Mark a tab as having received a bell signal.
    pub(crate) fn mark_tab_bell(&self, tab_widget_name: &str) {
        if let Some(btn) = self.find_strip_button(tab_widget_name) {
            if !btn.is_active() {
                btn.add_css_class("tab-bell");
                btn.add_css_class("tab-bell-flash");
                // Remove flash animation class after it completes
                let btn_clone = btn.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
                    btn_clone.remove_css_class("tab-bell-flash");
                });
            }
        }
    }

    /// Clear activity/bell indicators when a tab becomes active.
    pub(crate) fn clear_tab_indicators(&self, tab_widget_name: &str) {
        if let Some(btn) = self.find_strip_button(tab_widget_name) {
            btn.remove_css_class("tab-activity");
            btn.remove_css_class("tab-bell");
            btn.remove_css_class("tab-bell-flash");
        }
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

    /// Reload configuration from disk and apply changes.
    pub(crate) fn reload_config(&self) {
        let (new_config, themes, new_keybindings) = load_config();

        // Apply theme (finds the theme by name from the fresh theme list)
        let theme = themes.iter()
            .find(|t| t.name == new_config.theme_name)
            .unwrap_or(&themes[0])
            .clone();

        {
            let mut config = self.config.borrow_mut();
            config.window_opacity = new_config.window_opacity;
            config.terminal_scrollback_lines = new_config.terminal_scrollback_lines;
            config.font_desc = new_config.font_desc;
            config.default_font_scale = new_config.default_font_scale;
            config.theme_name = new_config.theme_name;
            config.foreground = theme.foreground;
            config.background = theme.background;
            config.cursor = theme.cursor;
            config.cursor_foreground = theme.cursor_foreground;
            config.palette = theme.palette;
            config.startup_commands = new_config.startup_commands;
        }

        // Apply all visual changes
        self.window_opacity.set(new_config.window_opacity);
        self.window.set_opacity(new_config.window_opacity);
        self.set_font_scale_all(new_config.default_font_scale);
        self.apply_font_all();
        self.apply_colors_all();
        self.apply_scrollback_all();

        // Update keybindings
        *self.keybinding_map.borrow_mut() = new_keybindings;

        log::info!("Configuration reloaded from disk");
    }

    pub(crate) fn create_split_terminal(&self, working_directory: Option<&str>, tab_widget_name: Option<String>) -> Terminal {
        let terminal = create_terminal(&self.config.borrow());
        setup_terminal_click_handler(&terminal);
        self.setup_context_menu(&terminal);

        let ui_for_exit = UiState::clone(self);
        let terminal_clone = terminal.clone();
        terminal.connect_child_exited(move |_, _| {
            ui_for_exit.handle_terminal_exited(&terminal_clone.clone().upcast::<gtk4::Widget>());
        });

        // Bell and activity signals for split pane terminals
        if let Some(ref name) = tab_widget_name {
            let ui_for_bell = self.clone();
            let bell_name = name.clone();
            terminal.connect_bell(move |_| {
                log::debug!("Bell signal received (split)");
                ui_for_bell.mark_tab_bell(&bell_name);
            });

            let ui_for_activity = self.clone();
            let activity_name = name.clone();
            terminal.connect_commit(move |_, _, _| {
                ui_for_activity.mark_tab_activity(&activity_name);
            });
        }

        // Split panes get a fresh session ID (new shell instance)
        let sid = generate_session_id();
        spawn_shell(&terminal, self.shell_argv.as_ref(), working_directory, Some(&sid), None);
        terminal
    }

    pub(crate) fn split_current(&self, orientation: Orientation) {
        let Some(current_term) = self.current_terminal() else {
            return;
        };
        let working_directory = terminal_working_directory(&current_term);

        // Find the tab widget name for bell/activity signals
        let tab_widget_name = self.notebook.current_page()
            .and_then(|p| self.notebook.nth_page(Some(p)))
            .map(|w| w.widget_name().to_string());

        // The effective widget in the Paned/notebook tree is the scrollbar wrapper
        // (if present) rather than the bare terminal.
        let current_widget = scrollbar_wrapper_of(&current_term.clone().upcast::<gtk4::Widget>())
            .map(|bx| bx.upcast::<gtk4::Widget>())
            .unwrap_or_else(|| current_term.clone().upcast::<gtk4::Widget>());
        let parent = current_widget.parent();

        let new_term = self.create_split_terminal(working_directory.as_deref(), tab_widget_name);
        let new_widget = wrap_with_scrollbar(&new_term);

        let paned = Paned::new(orientation);
        paned.set_hexpand(true);
        paned.set_vexpand(true);

        if let Some(ref parent) = parent {
            if let Ok(parent_paned) = parent.clone().downcast::<Paned>() {
                // Current terminal is in a Paned - replace it with a new nested Paned
                let is_start = parent_paned.start_child().as_ref() == Some(&current_widget);
                if is_start {
                    parent_paned.set_start_child(Some(&paned));
                } else {
                    parent_paned.set_end_child(Some(&paned));
                }
                paned.set_start_child(Some(&current_widget));
                paned.set_end_child(Some(&new_widget));
            } else {
                // Parent is the notebook - replace the page
                for i in 0..self.notebook.n_pages() {
                    if let Some(page_widget) = self.notebook.nth_page(Some(i)) {
                        if page_widget == current_widget {
                            // Transfer widget name so strip button mapping is preserved
                            paned.set_widget_name(&page_widget.widget_name());
                            let tab_label = self.notebook.tab_label(&page_widget);
                            self.notebook.remove_page(Some(i));
                            paned.set_start_child(Some(&current_widget));
                            paned.set_end_child(Some(&new_widget));
                            let new_page_num = self.notebook.insert_page(
                                &paned,
                                tab_label.as_ref(),
                                Some(i),
                            );
                            self.notebook.set_tab_reorderable(&paned, true);
                            self.notebook.set_current_page(Some(new_page_num));
                            break;
                        }
                    }
                }
            }
        }

        new_term.grab_focus();
    }

    pub(crate) fn cycle_pane_focus(&self, direction: i32) {
        let Some(page_num) = self.notebook.current_page() else { return };
        let Some(widget) = self.notebook.nth_page(Some(page_num)) else { return };
        let mut terms = Vec::new();
        collect_terminals(&widget, &mut terms);
        if terms.len() <= 1 { return; }

        let focused_idx = terms.iter().position(|t| t.has_focus()).unwrap_or(0);
        let next_idx = if direction > 0 {
            (focused_idx + 1) % terms.len()
        } else {
            if focused_idx == 0 { terms.len() - 1 } else { focused_idx - 1 }
        };
        terms[next_idx].grab_focus();
    }

    pub(crate) fn resize_pane(&self, target_orientation: Orientation, delta: i32) {
        let Some(term) = self.current_terminal() else { return };
        let term_widget = term.upcast::<gtk4::Widget>();
        // Walk up from the terminal to find the nearest Paned with matching orientation
        let mut widget = term_widget.parent();
        while let Some(w) = widget {
            if let Ok(paned) = w.clone().downcast::<Paned>() {
                if paned.orientation() == target_orientation {
                    let new_pos = (paned.position() + delta).max(0);
                    paned.set_position(new_pos);
                    return;
                }
            }
            widget = w.parent();
        }
    }

    pub(crate) fn focus_pane_directional(&self, direction: Direction) {
        let Some(page_num) = self.notebook.current_page() else { return };
        let Some(page_widget) = self.notebook.nth_page(Some(page_num)) else { return };
        let mut terms = Vec::new();
        collect_terminals(&page_widget, &mut terms);
        if terms.len() <= 1 { return; }

        let focused = terms.iter().find(|t| t.has_focus());
        let Some(focused) = focused else { return };

        let focused_widget = focused.clone().upcast::<gtk4::Widget>();
        let Some(focused_bounds) = focused_widget.compute_bounds(&page_widget) else { return };
        let focused_cx = focused_bounds.x() + focused_bounds.width() / 2.0;
        let focused_cy = focused_bounds.y() + focused_bounds.height() / 2.0;

        let mut best: Option<(f32, &Terminal)> = None;

        for term in &terms {
            if term.has_focus() { continue; }

            let tw = term.clone().upcast::<gtk4::Widget>();
            let Some(bounds) = tw.compute_bounds(&page_widget) else { continue };
            let cx = bounds.x() + bounds.width() / 2.0;
            let cy = bounds.y() + bounds.height() / 2.0;

            let dx = cx - focused_cx;
            let dy = cy - focused_cy;

            let in_direction = match direction {
                Direction::Left => dx < -1.0,
                Direction::Right => dx > 1.0,
                Direction::Up => dy < -1.0,
                Direction::Down => dy > 1.0,
            };

            if !in_direction { continue; }

            let dist = match direction {
                Direction::Left | Direction::Right => dx.abs() + dy.abs() * 0.1,
                Direction::Up | Direction::Down => dy.abs() + dx.abs() * 0.1,
            };

            if best.is_none() || dist < best.unwrap().0 {
                best = Some((dist, term));
            }
        }

        if let Some((_, term)) = best {
            term.grab_focus();
        }
    }

    pub(crate) fn toggle_pane_zoom(&self) {
        let has_zoom = self.zoom_state.borrow().is_some();
        if has_zoom {
            let state = self.zoom_state.borrow_mut().take().unwrap();
            self.unzoom_pane(state);
        } else {
            self.zoom_pane();
        }
    }

    pub(crate) fn zoom_pane(&self) {
        let Some(page_num) = self.notebook.current_page() else { return };
        let Some(page_widget) = self.notebook.nth_page(Some(page_num)) else { return };

        // Only zoom if there are splits (page is a Paned)
        if page_widget.clone().downcast::<Paned>().is_err() { return; }

        let Some(term) = find_focused_terminal(&page_widget) else { return };
        // The effective widget (wrapper box or bare terminal) is what sits in the Paned.
        let eff_widget = scrollbar_wrapper_of(&term.clone().upcast::<gtk4::Widget>())
            .map(|bx| bx.upcast::<gtk4::Widget>())
            .unwrap_or_else(|| term.clone().upcast::<gtk4::Widget>());
        let Some(parent) = eff_widget.parent() else { return };
        let Ok(parent_paned) = parent.downcast::<Paned>() else { return };

        let tab_label = self.notebook.tab_label(&page_widget);

        // Detach terminal from its parent paned (leave None slot for reattach)
        if parent_paned.start_child().as_ref() == Some(&eff_widget) {
            parent_paned.set_start_child(None::<&gtk4::Widget>);
        } else {
            parent_paned.set_end_child(None::<&gtk4::Widget>);
        }

        let widget_name = page_widget.widget_name().to_string();
        self.notebook.remove_page(Some(page_num));

        // Add terminal (with scrollbar wrapper) as a standalone page
        eff_widget.set_widget_name(&widget_name);
        let new_page = self.notebook.insert_page(
            &eff_widget,
            tab_label.as_ref(),
            Some(page_num),
        );
        self.notebook.set_tab_reorderable(&eff_widget, true);
        self.notebook.set_current_page(Some(new_page));
        self.sync_tab_strip_active(Some(new_page));
        term.grab_focus();

        *self.zoom_state.borrow_mut() = Some(ZoomState {
            original_page: page_widget,
            zoomed_terminal: term,
            page_index: page_num,
            tab_label,
        });
    }

    pub(crate) fn unzoom_pane(&self, state: ZoomState) {
        let Some(page_num) = self.notebook.current_page() else { return };

        // Remove the zoomed terminal's standalone page
        self.notebook.remove_page(Some(page_num));

        // Re-attach the effective widget (wrapper box or terminal) to the Paned tree
        let eff_widget = scrollbar_wrapper_of(&state.zoomed_terminal.clone().upcast::<gtk4::Widget>())
            .map(|bx| bx.upcast::<gtk4::Widget>())
            .unwrap_or_else(|| state.zoomed_terminal.clone().upcast::<gtk4::Widget>());
        reattach_terminal_to_tree(&state.original_page, &eff_widget);

        // Re-add the original Paned tree as the page
        let widget_name = eff_widget.widget_name().to_string();
        state.original_page.set_widget_name(&widget_name);
        let new_page = self.notebook.insert_page(
            &state.original_page,
            state.tab_label.as_ref(),
            Some(state.page_index),
        );
        self.notebook.set_tab_reorderable(&state.original_page, true);
        self.notebook.set_current_page(Some(new_page));
        self.sync_tab_strip_active(Some(new_page));
        state.zoomed_terminal.grab_focus();
    }

    pub(crate) fn move_pane_to_new_tab(&self) {
        let Some(page_num) = self.notebook.current_page() else { return };
        let Some(page_widget) = self.notebook.nth_page(Some(page_num)) else { return };

        // Only works if there are splits
        if page_widget.clone().downcast::<Paned>().is_err() { return; }

        let Some(term) = find_focused_terminal(&page_widget) else { return };
        let eff_widget = scrollbar_wrapper_of(&term.clone().upcast::<gtk4::Widget>())
            .map(|bx| bx.upcast::<gtk4::Widget>())
            .unwrap_or_else(|| term.clone().upcast::<gtk4::Widget>());
        let Some(parent) = eff_widget.parent() else { return };
        let Ok(paned) = parent.clone().downcast::<Paned>() else { return };

        let start = paned.start_child();
        let end = paned.end_child();
        let sibling = if start.as_ref() == Some(&eff_widget) {
            end
        } else {
            start
        };

        // Detach both children
        paned.set_start_child(None::<&gtk4::Widget>);
        paned.set_end_child(None::<&gtk4::Widget>);

        // Promote sibling (same logic as handle_terminal_exited)
        if let Some(sibling) = sibling {
            let paned_widget = paned.upcast::<gtk4::Widget>();
            if let Some(grandparent) = paned_widget.parent() {
                if let Ok(gp_paned) = grandparent.clone().downcast::<Paned>() {
                    if gp_paned.start_child().as_ref() == Some(&paned_widget) {
                        gp_paned.set_start_child(Some(&sibling));
                    } else {
                        gp_paned.set_end_child(Some(&sibling));
                    }
                } else {
                    for i in 0..self.notebook.n_pages() {
                        if let Some(pw) = self.notebook.nth_page(Some(i)) {
                            if pw == paned_widget {
                                sibling.set_widget_name(&pw.widget_name());
                                let tab_label = self.notebook.tab_label(&pw);
                                self.notebook.remove_page(Some(i));
                                let new_page_num = self.notebook.insert_page(
                                    &sibling,
                                    tab_label.as_ref(),
                                    Some(i),
                                );
                                self.notebook.set_tab_reorderable(&sibling, true);
                                self.notebook.set_current_page(Some(new_page_num));
                                break;
                            }
                        }
                    }
                }
            }

            if let Some(sibling_term) = find_first_terminal(&sibling) {
                sibling_term.grab_focus();
            }
        }

        // Now the terminal is detached - add it as a new tab
        let working_directory = terminal_working_directory(&term);
        self.add_terminal_as_new_tab(term, working_directory);
    }

    /// Add an existing terminal widget as a new tab (used by move_pane_to_new_tab).
    pub(crate) fn add_terminal_as_new_tab(&self, terminal: Terminal, working_directory: Option<String>) {
        let tab_num = self.tab_counter.get();
        self.tab_counter.set(tab_num + 1);

        // Assign a session ID for the moved pane's new tab
        let sid = generate_session_id();
        self.session_ids.borrow_mut().insert(tab_num, sid);

        let tab_name = default_tab_title(tab_num, working_directory.as_deref());

        // Use existing scrollbar wrapper if present, otherwise create one
        let page_widget: gtk4::Widget = scrollbar_wrapper_of(&terminal.clone().upcast::<gtk4::Widget>())
            .map(|bx| bx.upcast::<gtk4::Widget>())
            .unwrap_or_else(|| wrap_with_scrollbar(&terminal).upcast::<gtk4::Widget>());
        page_widget.set_widget_name(&format!("tab-{tab_num}"));

        // Notebook label
        let label = Label::new(Some(&tab_name));
        let page_num = self.notebook.append_page(&page_widget, Some(&label));
        self.notebook.set_tab_reorderable(&page_widget, true);

        // Tab strip button
        let btn = ToggleButton::builder()
            .label(&tab_name)
            .css_classes(["flat", "tab-strip-btn"])
            .build();
        btn.set_focus_on_click(false);
        btn.set_can_focus(false);
        btn.set_widget_name(&format!("tab-{tab_num}"));

        let ui_for_btn = self.clone();
        btn.connect_clicked(move |b| {
            let target_name = b.widget_name();
            for i in 0..ui_for_btn.notebook.n_pages() {
                if let Some(page_widget) = ui_for_btn.notebook.nth_page(Some(i)) {
                    if page_widget.widget_name() == target_name {
                        ui_for_btn.notebook.set_current_page(Some(i));
                        break;
                    }
                }
            }
        });

        self.tab_strip.append(&btn);
        self.notebook.set_current_page(Some(page_num));
        self.sync_tab_strip_active(Some(page_num));
        self.sync_tab_bar_visibility();
        terminal.grab_focus();
    }

    pub(crate) fn add_new_tab(&self, working_directory: Option<String>, tab_name: Option<String>, session_id: Option<String>, initial_commands: Option<String>) -> Terminal {
        let tab_num = self.tab_counter.get();
        self.tab_counter.set(tab_num + 1);

        // Generate or reuse session ID for rsh session persistence
        let sid = session_id.unwrap_or_else(generate_session_id);
        self.session_ids.borrow_mut().insert(tab_num, sid.clone());

        // Create block-mode terminal view (owns PTY + parser + block rendering)
        let term_view = Rc::new(TermView::new(
            &self.config.borrow(),
            self.shell_argv.as_ref(),
            working_directory.as_deref(),
        ));
        let terminal = term_view.vte().clone();
        let term_view_widget = term_view.widget();

        // Setup click handler for hyperlinks and context menu (uses VTE inside TermView)
        setup_terminal_click_handler(&terminal);
        self.setup_context_menu(&terminal);

        // Connect child-exited to close the tab
        let ui_for_exit = UiState::clone(self);
        let exit_widget = term_view_widget.clone();
        term_view.connect_exited(move |_code| {
            ui_for_exit.handle_terminal_exited(&exit_widget);
        });

        // Create tab header with a close button
        let tab_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let computed_default_title = default_tab_title(tab_num + 1, working_directory.as_deref());
        let (label_text, is_custom) = match tab_name {
            Some(name) => {
                let custom = name != computed_default_title;
                (name, custom)
            }
            None => (computed_default_title, false),
        };
        let label = Label::new(Some(&label_text));
        let custom_title = Rc::new(Cell::new(is_custom));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_width_chars(24);
        label.set_max_width_chars(64);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);

        let rename_click = GestureClick::new();
        rename_click.set_button(GDK_BUTTON_PRIMARY as u32);
        let label_for_rename = label.clone();
        let window_for_rename = self.window.clone();
        let custom_title_for_rename = custom_title.clone();
        rename_click.connect_pressed(move |_, n_press, _, _| {
            if n_press == 2 {
                show_rename_dialog(&window_for_rename, &label_for_rename, custom_title_for_rename.clone());
            }
        });
        label.add_controller(rename_click);

        // Auto-update tab title from CWD changes reported via OSC 7
        let label_for_pwd = label.clone();
        let custom_title_for_pwd = custom_title.clone();
        let tab_index_for_pwd = tab_num + 1;
        let strip_btn_label: Rc<RefCell<Option<Label>>> = Rc::new(RefCell::new(None));
        let strip_btn_label_for_pwd = strip_btn_label.clone();
        term_view.connect_cwd_changed(move |dir| {
            if custom_title_for_pwd.get() {
                return;
            }
            let new_title = default_tab_title(tab_index_for_pwd, Some(dir));
            if label_for_pwd.text().as_str() != new_title {
                label_for_pwd.set_text(&new_title);
                if let Some(ref btn_label) = *strip_btn_label_for_pwd.borrow() {
                    btn_label.set_text(&new_title);
                }
            }
        });

        let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
        close_button.set_focus_on_click(false);
        close_button.set_can_focus(false);
        close_button.set_has_frame(false);
        close_button.add_css_class("flat");
        close_button.set_tooltip_text(Some("Close tab"));

        tab_box.append(&label);
        tab_box.append(&close_button);

        // TermView root widget (replaces wrap_with_scrollbar)
        let term_wrapper = {
            let w = term_view.widget();
            w.downcast::<gtk4::Box>().expect("TermView root must be a Box")
        };

        let ui_for_close = UiState::clone(self);
        let wrapper_for_close = term_wrapper.clone().upcast::<gtk4::Widget>();
        close_button.connect_clicked(move |_| {
            ui_for_close.remove_tab_by_widget(&wrapper_for_close);
        });

        // Add to notebook right after the current tab when possible.
        let page_num = if let Some(current_page) = self.notebook.current_page() {
            self.notebook
                .insert_page(&term_wrapper, Some(&tab_box), Some(current_page + 1))
        } else {
            self.notebook.append_page(&term_wrapper, Some(&tab_box))
        };
        self.notebook.set_tab_reorderable(&term_wrapper, true);
        self.notebook.set_current_page(Some(page_num));
        // Force tabs hidden — GTK may re-show them after page insertion
        self.notebook.set_show_tabs(false);

        // Create tab strip toggle button
        let strip_label = Label::new(Some(&label_text));
        strip_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        strip_label.set_hexpand(true);
        strip_label.set_xalign(0.0);
        *strip_btn_label.borrow_mut() = Some(strip_label.clone());

        let strip_close_icon = gtk4::Image::from_icon_name("window-close-symbolic");
        strip_close_icon.add_css_class("tab-strip-close");
        strip_close_icon.set_opacity(0.0);

        let strip_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        strip_box.append(&strip_label);
        strip_box.append(&strip_close_icon);

        let strip_btn = ToggleButton::new();
        strip_btn.set_child(Some(&strip_box));
        strip_btn.add_css_class("tab-strip-btn");
        strip_btn.add_css_class("flat");
        strip_btn.set_active(true); // new tab is current
        strip_btn.set_focus_on_click(false);
        strip_btn.set_can_focus(false);
        strip_btn.set_hexpand(true); // Fill sidebar width

        // Show close icon on hover, hide on leave
        let hover_ctrl = gtk4::EventControllerMotion::new();
        let close_for_enter = strip_close_icon.clone();
        hover_ctrl.connect_enter(move |_, _, _| {
            close_for_enter.set_opacity(1.0);
        });
        let close_for_leave = strip_close_icon.clone();
        hover_ctrl.connect_leave(move |_| {
            close_for_leave.set_opacity(0.0);
        });
        strip_btn.add_controller(hover_ctrl);
        // Give button a unique name to correlate with notebook page
        let tab_widget_name = format!("tab-{}", tab_num);
        strip_btn.set_widget_name(&tab_widget_name);
        // Also name the wrapper widget so we can find the button when removing
        term_wrapper.set_widget_name(&tab_widget_name);

        // Bell signal: flash the tab strip button when bell rings on non-active tab
        let ui_for_bell = self.clone();
        let bell_tab_name = tab_widget_name.clone();
        terminal.connect_bell(move |_| {
            log::debug!("Bell signal received");
            ui_for_bell.mark_tab_bell(&bell_tab_name);
        });

        // Activity indicator: mark tab when there's output on a non-active tab
        let ui_for_activity = self.clone();
        let activity_tab_name = tab_widget_name.clone();
        terminal.connect_commit(move |_, _, _| {
            ui_for_activity.mark_tab_activity(&activity_tab_name);
        });

        // Double-click to rename on strip button too
        let rename_click_strip = GestureClick::new();
        rename_click_strip.set_button(GDK_BUTTON_PRIMARY as u32);
        let label_for_rename_strip = label.clone();
        let strip_label_for_rename = strip_label.clone();
        let window_for_rename_strip = self.window.clone();
        let custom_title_for_rename_strip = custom_title.clone();
        rename_click_strip.connect_pressed(move |_, n_press, _, _| {
            if n_press == 2 {
                show_rename_dialog_with_strip(
                    &window_for_rename_strip,
                    &label_for_rename_strip,
                    &strip_label_for_rename,
                    custom_title_for_rename_strip.clone(),
                );
            }
        });
        strip_btn.add_controller(rename_click_strip);

        // Close icon click: use a capture-phase gesture on the ToggleButton so we
        // intercept the press before the button's own toggle handler.
        let close_gesture = GestureClick::new();
        close_gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let ui_for_strip_close = self.clone();
        let wrapper_for_strip_close = term_wrapper.clone().upcast::<gtk4::Widget>();
        let close_icon_for_hit = strip_close_icon.clone();
        let strip_btn_for_close = strip_btn.clone();
        close_gesture.connect_pressed(move |gesture, _n, x, y| {
            // Check if the click landed on the close icon area
            let btn_widget = strip_btn_for_close.upcast_ref::<gtk4::Widget>();
            let icon_widget = close_icon_for_hit.upcast_ref::<gtk4::Widget>();
            if let Some((ix, iy)) = btn_widget.translate_coordinates(icon_widget, x, y) {
                let w = icon_widget.width() as f64;
                let h = icon_widget.height() as f64;
                if ix >= 0.0 && iy >= 0.0 && ix <= w && iy <= h {
                    gesture.set_state(gtk4::EventSequenceState::Claimed);
                    ui_for_strip_close.remove_tab_by_widget(&wrapper_for_strip_close);
                }
            }
        });
        strip_btn.add_controller(close_gesture);

        // Click to switch tab
        let notebook_for_strip = self.notebook.clone();
        let tab_strip_for_click = self.tab_strip.clone();
        strip_btn.connect_clicked(move |btn| {
            // Find the index of this button in the strip
            let mut idx = 0u32;
            let mut child = tab_strip_for_click.first_child();
            while let Some(ref c) = child {
                if c == btn.upcast_ref::<gtk4::Widget>() {
                    break;
                }
                idx += 1;
                child = c.next_sibling();
            }
            notebook_for_strip.set_current_page(Some(idx));
        });

        // Drag source: carry the widget name so we can identify the dragged button
        let drag_source = gtk4::DragSource::new();
        drag_source.set_actions(gtk4::gdk::DragAction::MOVE);
        let strip_btn_for_drag = strip_btn.clone();
        drag_source.connect_prepare(move |_, _, _| {
            let name = strip_btn_for_drag.widget_name().to_string();
            Some(gtk4::gdk::ContentProvider::for_value(&name.to_value()))
        });
        strip_btn.add_controller(drag_source);

        // Drop target: reorder strip buttons and notebook pages
        let drop_target = gtk4::DropTarget::new(glib::Type::STRING, gtk4::gdk::DragAction::MOVE);
        let tab_strip_for_drop = self.tab_strip.clone();
        let notebook_for_drop = self.notebook.clone();
        let strip_btn_for_drop = strip_btn.clone();
        drop_target.connect_drop(move |_, value, _, _| {
            let Ok(drag_name) = value.get::<String>() else { return false };
            let target_name = strip_btn_for_drop.widget_name().to_string();
            if drag_name == target_name {
                return false; // dropped on itself
            }

            // Find source and target indices in the strip
            let mut src_idx: Option<u32> = None;
            let mut dst_idx: Option<u32> = None;
            let mut src_widget: Option<gtk4::Widget> = None;
            let mut idx = 0u32;
            let mut child = tab_strip_for_drop.first_child();
            while let Some(ref c) = child {
                if c.widget_name().as_str() == drag_name {
                    src_idx = Some(idx);
                    src_widget = Some(c.clone());
                }
                if c.widget_name().as_str() == target_name {
                    dst_idx = Some(idx);
                }
                idx += 1;
                child = c.next_sibling();
            }

            let (Some(src), Some(dst), Some(src_w)) = (src_idx, dst_idx, src_widget) else {
                return false;
            };

            // Reorder strip item: move src before/after dst
            let mut target_w: Option<gtk4::Widget> = None;
            let mut child = tab_strip_for_drop.first_child();
            while let Some(ref c) = child {
                if c.widget_name().as_str() == target_name {
                    target_w = Some(c.clone());
                    break;
                }
                child = c.next_sibling();
            }
            let Some(target_w) = target_w else { return false };

            if src < dst {
                src_w.insert_after(&tab_strip_for_drop, Some(&target_w));
            } else {
                src_w.insert_before(&tab_strip_for_drop, Some(&target_w));
            }

            // Reorder notebook page to match
            if let Some(page_widget) = notebook_for_drop.nth_page(Some(src)) {
                notebook_for_drop.reorder_child(&page_widget, Some(dst));
            }

            // Sync active indicator
            if let Some(current) = notebook_for_drop.current_page() {
                let mut child = tab_strip_for_drop.first_child();
                let mut i = 0u32;
                while let Some(c) = child {
                    if let Ok(btn) = c.clone().downcast::<ToggleButton>() {
                        btn.set_active(i == current);
                    }
                    i += 1;
                    child = c.next_sibling();
                }
            }

            true
        });
        strip_btn.add_controller(drop_target);

        // Insert strip button at the correct position
        if page_num as i32 >= self.tab_strip.observe_children().n_items() as i32 {
            self.tab_strip.append(&strip_btn);
        } else {
            // Insert before the sibling at page_num position
            let mut child = self.tab_strip.first_child();
            for _ in 0..page_num {
                child = child.and_then(|c| c.next_sibling());
            }
            if let Some(sibling) = child {
                strip_btn.insert_before(&self.tab_strip, Some(&sibling));
            } else {
                self.tab_strip.append(&strip_btn);
            }
        }

        // Deactivate all other strip buttons
        self.sync_tab_strip_active(Some(page_num));
        self.sync_tab_bar_visibility();

        // Focus the new terminal
        term_view.grab_focus();

        terminal
    }
}
