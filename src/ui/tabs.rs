//! tabs — UiState methods extracted from ui (mechanical split, no logic changes)
use adw::prelude::*;
use gtk4::gdk::ffi::GDK_BUTTON_PRIMARY;
use gtk4::{glib, Label};
use gtk4::{GestureClick, ToggleButton};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::Terminal;
use vte4::TerminalExt;

use super::*;
use crate::block_view::TermView;
use crate::keybindings::Action;
use crate::state::{generate_session_id, kill_terminal_child, kill_widget_child_processes};
use crate::terminal::{
    collect_terminals, default_tab_title, find_first_terminal, scrollbar_wrapper_of,
    setup_terminal_click_handler, show_rename_dialog, show_rename_dialog_with_strip,
    terminal_working_directory, VteTerminalView,
};

struct TabLaunch {
    working_directory: Option<String>,
    tab_name: Option<String>,
    session_id: Option<String>,
    initial_commands: Option<String>,
    argv_override: Option<Vec<String>>,
    remote: Option<(crate::config::RemoteHost, u32)>,
    terminal_mode: crate::config::TerminalMode,
}

type TitleChangedCallback = Box<dyn Fn(&str)>;
const CUSTOM_TITLE_DATA: &str = "jterm4-custom-title";

fn tab_num_for_widget(widget: &gtk4::Widget) -> Option<u32> {
    widget
        .widget_name()
        .strip_prefix("tab-")
        .and_then(|value| value.parse().ok())
}

fn notebook_page_named(notebook: &gtk4::Notebook, name: &str) -> Option<gtk4::Widget> {
    (0..notebook.n_pages()).find_map(|index| {
        notebook
            .nth_page(Some(index))
            .filter(|page| page.widget_name() == name)
    })
}

fn custom_tab_title(notebook: &gtk4::Notebook, page: &gtk4::Widget) -> Option<String> {
    let is_custom = unsafe {
        page.data::<Rc<Cell<bool>>>(CUSTOM_TITLE_DATA)
            .is_some_and(|flag| flag.as_ref().get())
    };
    is_custom
        .then(|| crate::state::tab_label_text(notebook, page))
        .flatten()
}

impl UiState {
    fn retitle_tab_from_active_leaf(&self, tab_num: u32) {
        let name = format!("tab-{tab_num}");
        let Some(page) = notebook_page_named(&self.notebook, &name) else {
            return;
        };
        let cwd = PaneNode::from_widget(&page)
            .and_then(|node| node.active_terminal())
            .and_then(|terminal| terminal_working_directory(&terminal));
        let title = default_tab_title(tab_num + 1, cwd.as_deref());
        self.set_tab_strip_label(tab_num, &title);
        if let Some(tab_label) = self.notebook.tab_label(&page) {
            if let Some(label) = tab_label
                .first_child()
                .and_then(|child| child.downcast::<Label>().ok())
            {
                label.set_text(&title);
            }
        }
    }

    fn restore_zoom_before_close(&self) {
        if let Some(state) = self.zoom_state.borrow_mut().take() {
            self.unzoom_pane(state);
        }
    }

    /// Resolve a descendant/controller widget to the actual Notebook page.
    /// Split-tab close buttons retain the original leaf handle, while the page
    /// itself becomes a `Paned`, so process scans and removal must start here.
    fn notebook_page_for_widget(&self, widget: &gtk4::Widget) -> Option<gtk4::Widget> {
        let mut candidate = Some(widget.clone());
        while let Some(current) = candidate {
            if self.notebook.page_num(&current).is_some() {
                return Some(current);
            }
            candidate = current.parent();
        }

        let target_name = widget.widget_name();
        if !target_name.is_empty() {
            for page in 0..self.notebook.n_pages() {
                let candidate = self.notebook.nth_page(Some(page))?;
                if candidate.widget_name() == target_name {
                    return Some(candidate);
                }
            }
        }
        None
    }

    pub(crate) fn running_processes_in_widget(widget: &gtk4::Widget) -> Vec<String> {
        if let Some(node) = PaneNode::from_widget(widget) {
            return node
                .leaves()
                .into_iter()
                .enumerate()
                .filter_map(|(index, leaf)| {
                    leaf.foreground_process_name()
                        .map(|process| format!("Pane {}: {process}", index + 1))
                })
                .collect();
        }

        // Compatibility fallback for a page created before typed PaneLeaf
        // attachment was introduced. New pages always take the branch above.
        let mut terminals = Vec::new();
        collect_terminals(widget, &mut terminals);
        terminals
            .into_iter()
            .enumerate()
            .filter_map(|(index, terminal)| {
                crate::state::get_foreground_process_name(&terminal)
                    .map(|process| format!("Pane {}: {process}", index + 1))
            })
            .collect()
    }

    fn running_process_summary_in_widget(widget: &gtk4::Widget) -> Option<String> {
        let running = Self::running_processes_in_widget(widget);
        (!running.is_empty()).then(|| running.join("\n"))
    }

    /// Summarise every foreground non-shell process in the window. This is
    /// intentionally independent of session-restorability: editors, pagers and
    /// arbitrary commands all deserve a close confirmation even when they
    /// cannot be replayed on startup.
    pub(crate) fn running_process_summary_for_notebook(
        notebook: &gtk4::Notebook,
    ) -> Option<String> {
        let mut running = Vec::new();
        for page in 0..notebook.n_pages() {
            let Some(widget) = notebook.nth_page(Some(page)) else {
                continue;
            };
            let tab_name = crate::state::tab_label_text(notebook, &widget)
                .unwrap_or_else(|| format!("Tab {}", page + 1));
            for process in Self::running_processes_in_widget(&widget) {
                running.push(format!("{tab_name} — {process}"));
            }
        }

        const MAX_SHOWN: usize = 8;
        let hidden = running.len().saturating_sub(MAX_SHOWN);
        running.truncate(MAX_SHOWN);
        if hidden > 0 {
            running.push(format!("…and {hidden} more"));
        }
        (!running.is_empty()).then(|| running.join("\n"))
    }

    pub(crate) fn remove_tab_by_widget(&self, widget: &gtk4::Widget) {
        self.restore_zoom_before_close();
        let page_widget = self
            .notebook_page_for_widget(widget)
            .unwrap_or_else(|| widget.clone());
        if let Some(process_info) = Self::running_process_summary_in_widget(&page_widget) {
            let ui_state = self.clone();
            glib::MainContext::default().spawn_local(async move {
                if Self::confirm_close_tab_with_process(&ui_state.window, &process_info).await {
                    ui_state.remove_tab_by_widget_internal(&page_widget);
                }
            });
            return;
        }

        self.remove_tab_by_widget_internal(&page_widget);
    }

    /// Handle a terminal exiting: collapse its split or close the whole tab.
    pub(crate) fn handle_terminal_exited(&self, term_widget: &gtk4::Widget) {
        // A zoom swap temporarily removes the sibling tree from the Notebook.
        // Restore it before collapsing the exited leaf; merely dropping the
        // swap would lose the still-running sibling and its PTY.
        self.restore_zoom_before_close();

        let leaf_root = scrollbar_wrapper_of(term_widget)
            .map(|wrapper| wrapper.upcast::<gtk4::Widget>())
            .unwrap_or_else(|| term_widget.clone());

        if let Some(sibling) = detach_leaf_and_promote(&self.notebook, &leaf_root) {
            if let Some(node) = PaneNode::from_widget(&sibling) {
                node.grab_focus();
            }
        } else {
            self.remove_tab_by_widget(&leaf_root);
        }
    }

    pub(crate) fn remove_current_tab(&self) {
        if let Some(page_num) = self.notebook.current_page() {
            if let Some(widget) = self.notebook.nth_page(Some(page_num)) {
                self.remove_tab_by_widget(&widget);
            }
        }
    }

    pub(crate) fn remove_tab_by_widget_internal(&self, widget: &gtk4::Widget) {
        let widget = self
            .notebook_page_for_widget(widget)
            .unwrap_or_else(|| widget.clone());
        // Kill shell processes and remove the strip button for the current page
        if !kill_widget_child_processes(&widget) {
            let mut terms = Vec::new();
            collect_terminals(&widget, &mut terms);
            for term in &terms {
                kill_terminal_child(term);
            }
        }
        self.remove_strip_button_for(&widget);

        // Drop per-tab bookkeeping keyed by tab_num parsed from the widget name.
        if let Some(tab_num) = widget
            .widget_name()
            .strip_prefix("tab-")
            .and_then(|s| s.parse::<u32>().ok())
        {
            self.session_ids.borrow_mut().remove(&tab_num);
            self.tab_connections.borrow_mut().remove(&tab_num);
        }

        if let Some(page_num) = self.notebook.page_num(&widget) {
            self.notebook.remove_page(Some(page_num));
        }

        if self.notebook.n_pages() == 0 {
            // Route the last-tab path through the window close handler so it
            // synchronously publishes an empty snapshot before the process
            // quits. Direct destroy could race the deferred page-removed save
            // and resurrect a previously closed workspace on next launch.
            self.window.close();
        } else {
            self.sync_tab_strip_active(None);
            self.sync_tab_bar_visibility();
            self.focus_current_terminal();
        }
    }

    pub(crate) fn close_focused_pane_or_tab(&self) {
        self.restore_zoom_before_close();
        let Some(page_num) = self.notebook.current_page() else {
            return;
        };
        let Some(page_widget) = self.notebook.nth_page(Some(page_num)) else {
            return;
        };
        if let Some(node) = PaneNode::from_widget(&page_widget) {
            if node.is_split() {
                if let Some(leaf) = node.active_leaf() {
                    let leaf_root = leaf.root_widget();
                    let pending_remote =
                        leaf.is_remote()
                            && tab_num_for_widget(&leaf_root).is_some_and(|tab_num| {
                                self.tab_connections.borrow().get(&tab_num).is_some_and(
                                    |connection| connection.status == ConnStatus::Disconnected,
                                )
                            });
                    if pending_remote {
                        if let Some(tab_num) = tab_num_for_widget(&leaf_root) {
                            self.tab_connections.borrow_mut().remove(&tab_num);
                            self.clear_tab_conn_status(tab_num);
                        }
                        // This child already emitted `exited`; killing it again
                        // cannot drive the structural collapse callback.
                        self.handle_terminal_exited(&leaf_root);
                        if let Some(tab_num) = tab_num_for_widget(&leaf_root) {
                            self.retitle_tab_from_active_leaf(tab_num);
                        }
                        return;
                    }
                    if let Some(process) = leaf.foreground_process_name() {
                        let ui_state = self.clone();
                        glib::MainContext::default().spawn_local(async move {
                            if Self::confirm_close_with_processes(
                                &ui_state.window,
                                "Close pane with running process?",
                                "Close Pane",
                                &process,
                            )
                            .await
                            {
                                // The process may have exited and collapsed this
                                // leaf while the confirmation was open. Never use
                                // its now-stale PID after it left the pane tree.
                                if leaf.root_widget().parent().is_some() {
                                    leaf.kill();
                                }
                            }
                        });
                    } else {
                        leaf.kill();
                    }
                    return;
                }
            }
        }
        self.remove_current_tab();
    }

    pub(crate) fn duplicate_current_tab(&self) {
        if let Some(page) = self.notebook.current_page() {
            if let Some(widget) = self.notebook.nth_page(Some(page)) {
                let working_directory = find_first_terminal(&widget)
                    .as_ref()
                    .and_then(terminal_working_directory);
                let title = custom_tab_title(&self.notebook, &widget);
                self.add_new_tab(working_directory, title, None, None);
            }
        }
    }

    /// Add an existing typed pane leaf as a new tab.
    pub(crate) fn add_pane_leaf_as_new_tab(
        &self,
        leaf: PaneLeaf,
        working_directory: Option<String>,
    ) {
        let tab_num = self.tab_counter.get();
        self.tab_counter.set(tab_num + 1);
        let page_widget = leaf.root_widget();

        // Remote reconnect/session callbacks resolve the tab number from this
        // leaf's current widget identity. Move their connection record at the
        // same time as the widget so reconnect remains attached to the pane.
        let old_tab_num = tab_num_for_widget(&page_widget);
        let moved_connection = if leaf.is_remote() {
            old_tab_num.and_then(|old| self.tab_connections.borrow_mut().remove(&old))
        } else {
            None
        };
        if let Some(old) = old_tab_num.filter(|_| moved_connection.is_some()) {
            self.clear_tab_conn_status(old);
        }
        let moved_connection_status = moved_connection.as_ref().map(|conn| conn.status);
        if let Some(connection) = moved_connection {
            self.tab_connections
                .borrow_mut()
                .insert(tab_num, connection);
        }

        // Moving a pane must not silently change the shell/session identity that
        // was attached to the live PTY. The tab-number map is only an index for
        // top-level compatibility; the leaf remains the source of truth.
        let sid = leaf.session_id().unwrap_or_else(generate_session_id);
        leaf.set_session_id(&sid);
        self.session_ids.borrow_mut().insert(tab_num, sid);
        let tab_name = self
            .tab_connections
            .borrow()
            .get(&tab_num)
            .map(|connection| connection.host.name.clone())
            .unwrap_or_else(|| default_tab_title(tab_num + 1, working_directory.as_deref()));
        let pinned = unsafe {
            page_widget
                .data::<bool>("pinned")
                .is_some_and(|value| *value.as_ref())
        };
        let tab_widget_name = format!("tab-{tab_num}");
        page_widget.set_widget_name(&tab_widget_name);

        // Build the same shaped Notebook header as ordinary tabs so title
        // extraction/session save and the header close affordance keep working.
        let label = Label::new(Some(&tab_name));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_width_chars(24);
        label.set_max_width_chars(64);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
        close_button.set_focus_on_click(false);
        close_button.set_can_focus(false);
        close_button.set_has_frame(false);
        close_button.add_css_class("flat");
        close_button.set_tooltip_text(Some("Close tab"));
        let tab_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        tab_box.append(&label);
        tab_box.append(&close_button);

        let desired_page = self
            .notebook
            .current_page()
            .map(|page| page + 1)
            .unwrap_or_else(|| self.notebook.n_pages());
        let page_num = self
            .notebook
            .insert_page(&page_widget, Some(&tab_box), Some(desired_page));
        self.notebook.set_tab_reorderable(&page_widget, true);

        let ui_for_header_close = self.clone();
        let page_name_for_header_close = tab_widget_name.clone();
        close_button.connect_clicked(move |_| {
            if let Some(page) =
                notebook_page_named(&ui_for_header_close.notebook, &page_name_for_header_close)
            {
                ui_for_header_close.remove_tab_by_widget(&page);
            }
        });

        // Full strip shape: title/process/pin/close children are deliberately the
        // same as a freshly spawned tab, so helpers and CSS do not special-case
        // pane-move tabs.
        let strip_label = Label::new(Some(&tab_name));
        strip_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        strip_label.set_hexpand(true);
        strip_label.set_xalign(0.0);
        let process_label = Label::new(None);
        process_label.add_css_class("tab-process-indicator");
        process_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        process_label.set_max_width_chars(15);
        let pin_icon = gtk4::Image::from_icon_name("bookmark-symbolic");
        pin_icon.add_css_class("tab-pin-icon");
        pin_icon.set_visible(pinned);
        let close_icon = gtk4::Image::from_icon_name("window-close-symbolic");
        close_icon.add_css_class("tab-strip-close");
        close_icon.set_opacity(0.0);
        let conn_dot = Label::new(Some("\u{25CF}"));
        conn_dot.add_css_class("tab-conn-dot");
        conn_dot.set_visible(false);
        let strip_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        strip_box.append(&conn_dot);
        strip_box.append(&strip_label);
        strip_box.append(&process_label);
        strip_box.append(&pin_icon);
        strip_box.append(&close_icon);

        let button = ToggleButton::new();
        button.set_child(Some(&strip_box));
        button.add_css_class("flat");
        button.add_css_class("tab-strip-btn");
        if pinned {
            button.add_css_class("tab-pinned");
        }
        button.set_focus_on_click(false);
        button.set_can_focus(false);
        button.set_hexpand(true);
        button.set_widget_name(&tab_widget_name);
        unsafe {
            button.set_data::<Label>("tab-title-label", strip_label.clone());
            button.set_data::<bool>("pinned", pinned);
        }

        let hover = gtk4::EventControllerMotion::new();
        let close_for_enter = close_icon.clone();
        hover.connect_enter(move |_, _, _| close_for_enter.set_opacity(1.0));
        let close_for_leave = close_icon.clone();
        hover.connect_leave(move |_| close_for_leave.set_opacity(0.0));
        button.add_controller(hover);

        // Header and strip rename remain in lockstep.
        // Remote tabs intentionally keep their host label instead of allowing
        // OSC title/cwd updates to replace it, matching ordinary remote tabs.
        let custom_title = Rc::new(Cell::new(leaf.is_remote()));
        unsafe {
            page_widget.set_data::<Rc<Cell<bool>>>(CUSTOM_TITLE_DATA, custom_title.clone());
        }
        let rename_header = GestureClick::new();
        rename_header.set_button(GDK_BUTTON_PRIMARY as u32);
        let window_for_rename = self.window.clone();
        let label_for_rename = label.clone();
        let strip_for_rename = strip_label.clone();
        let custom_for_rename = custom_title.clone();
        rename_header.connect_pressed(move |_, presses, _, _| {
            if presses == 2 {
                show_rename_dialog_with_strip(
                    &window_for_rename,
                    &label_for_rename,
                    &strip_for_rename,
                    custom_for_rename.clone(),
                );
            }
        });
        label.add_controller(rename_header);

        // A pane may be detached more than once. Each binding is scoped to the
        // tab identity assigned by this move, so older callbacks become inert
        // instead of updating a sibling tab's chrome.
        match &leaf {
            PaneLeaf::Block(view) => {
                let identity = page_widget.clone();
                let expected_name = tab_widget_name.clone();
                let header = label.clone();
                let strip = strip_label.clone();
                let custom = custom_title.clone();
                view.connect_cwd_changed(move |dir| {
                    if identity.widget_name() != expected_name || custom.get() {
                        return;
                    }
                    let title = default_tab_title(tab_num + 1, Some(dir));
                    header.set_text(&title);
                    strip.set_text(&title);
                });
                let identity = page_widget.clone();
                let expected_name = tab_widget_name.clone();
                let header = label.clone();
                let strip = strip_label.clone();
                let custom = custom_title.clone();
                view.connect_title_changed(move |title| {
                    if identity.widget_name() != expected_name || custom.get() {
                        return;
                    }
                    header.set_text(title);
                    strip.set_text(title);
                });
            }
            PaneLeaf::Vte(view) => {
                let identity = page_widget.clone();
                let expected_name = tab_widget_name.clone();
                let header = label.clone();
                let strip = strip_label.clone();
                let custom = custom_title.clone();
                view.connect_cwd_changed(move |dir| {
                    if identity.widget_name() != expected_name || custom.get() {
                        return;
                    }
                    let title = default_tab_title(tab_num + 1, Some(dir));
                    header.set_text(&title);
                    strip.set_text(&title);
                });
                let identity = page_widget.clone();
                let expected_name = tab_widget_name.clone();
                let header = label.clone();
                let strip = strip_label.clone();
                let custom = custom_title.clone();
                view.connect_title_changed(move |title| {
                    if identity.widget_name() != expected_name || custom.get() {
                        return;
                    }
                    header.set_text(title);
                    strip.set_text(title);
                });
            }
        }

        let ui_for_select = self.clone();
        let button_for_select = button.clone();
        let name_for_select = tab_widget_name.clone();
        let select = GestureClick::new();
        select.set_button(GDK_BUTTON_PRIMARY as u32);
        select.set_propagation_phase(gtk4::PropagationPhase::Capture);
        select.connect_pressed(move |gesture, presses, _, _| {
            if presses != 1 {
                return;
            }
            let modifiers = gesture.current_event_state();
            if modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                ui_for_select.toggle_tab_selection(&name_for_select);
                return;
            }
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            ui_for_select.clear_tab_selection();
            for index in 0..ui_for_select.notebook.n_pages() {
                if ui_for_select
                    .notebook
                    .nth_page(Some(index))
                    .is_some_and(|page| page.widget_name() == button_for_select.widget_name())
                {
                    ui_for_select.notebook.set_current_page(Some(index));
                    ui_for_select.sync_tab_strip_active(Some(index));
                    break;
                }
            }
        });
        button.add_controller(select);

        let ui_for_toggle = self.clone();
        button.connect_toggled(move |_| {
            let ui = ui_for_toggle.clone();
            glib::idle_add_local_once(move || ui.sync_tab_strip_active(None));
        });

        let close = GestureClick::new();
        close.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let ui_for_close = self.clone();
        let page_name_for_close = tab_widget_name.clone();
        let button_for_close = button.clone();
        let icon_for_close = close_icon.clone();
        close.connect_pressed(move |gesture, _, x, y| {
            let point = gtk4::graphene::Point::new(x as f32, y as f32);
            if let Some(mapped) = button_for_close
                .upcast_ref::<gtk4::Widget>()
                .compute_point(icon_for_close.upcast_ref::<gtk4::Widget>(), &point)
            {
                let icon = icon_for_close.upcast_ref::<gtk4::Widget>();
                if mapped.x() >= 0.0
                    && mapped.y() >= 0.0
                    && mapped.x() <= icon.width() as f32
                    && mapped.y() <= icon.height() as f32
                {
                    gesture.set_state(gtk4::EventSequenceState::Claimed);
                    if let Some(page) =
                        notebook_page_named(&ui_for_close.notebook, &page_name_for_close)
                    {
                        ui_for_close.remove_tab_by_widget(&page);
                    }
                }
            }
        });
        button.add_controller(close);

        // Keep process/tool-tip behavior aligned with normal tabs.
        let notebook_for_process = self.notebook.clone();
        let page_name_for_process = tab_widget_name.clone();
        let process_for_tick = process_label.clone();
        glib::timeout_add_local(std::time::Duration::from_secs(2), move || {
            if process_for_tick.parent().is_none() {
                return glib::ControlFlow::Break;
            }
            let process = notebook_page_named(&notebook_for_process, &page_name_for_process)
                .and_then(|page| PaneNode::from_widget(&page))
                .and_then(|node| {
                    node.leaves()
                        .into_iter()
                        .find_map(|leaf| leaf.foreground_process_name())
                });
            if let Some(process) = process {
                process_for_tick.set_text(&process);
                process_for_tick.set_visible(true);
            } else {
                process_for_tick.set_visible(false);
            }
            glib::ControlFlow::Continue
        });
        button.set_has_tooltip(true);
        let notebook_for_tooltip = self.notebook.clone();
        let page_name_for_tooltip = tab_widget_name.clone();
        button.connect_query_tooltip(move |button, _, _, _, tooltip| {
            let mut parts = Vec::new();
            if let Some(leaf) = notebook_page_named(&notebook_for_tooltip, &page_name_for_tooltip)
                .and_then(|page| PaneNode::from_widget(&page))
                .and_then(|node| node.active_leaf())
            {
                if let Some(cwd) = terminal_working_directory(leaf.terminal()) {
                    parts.push(format!("Dir: {cwd}"));
                }
                if let Some(process) = leaf.foreground_process_name() {
                    parts.push(format!("Process: {process}"));
                }
            }
            if button.has_css_class("tab-pinned") {
                parts.push("Status: pinned".to_string());
            }
            if parts.is_empty() {
                false
            } else {
                tooltip.set_text(Some(&parts.join("\n")));
                true
            }
        });

        // A compact context menu still exposes the stateful operations whose
        // backing data is consumed by session save.
        let context = GestureClick::new();
        context.set_button(3);
        let ui_for_context = self.clone();
        let button_for_context = button.clone();
        let page_name_for_context = tab_widget_name.clone();
        let pin_for_context = pin_icon.clone();
        let label_for_context = label.clone();
        let strip_for_context = strip_label.clone();
        let custom_for_context = custom_title.clone();
        context.connect_pressed(move |gesture, _, x, y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            let popover = gtk4::Popover::new();
            popover.set_parent(&button_for_context);
            popover.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.set_has_arrow(false);
            let menu = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

            let rename = gtk4::Button::with_label("Rename");
            rename.set_has_frame(false);
            rename.add_css_class("flat");
            let popover_for_rename = popover.clone();
            let window = ui_for_context.window.clone();
            let label = label_for_context.clone();
            let strip = strip_for_context.clone();
            let custom = custom_for_context.clone();
            rename.connect_clicked(move |_| {
                popover_for_rename.popdown();
                show_rename_dialog_with_strip(&window, &label, &strip, custom.clone());
            });
            menu.append(&rename);

            let pin = gtk4::Button::with_label("Pin Tab");
            pin.set_has_frame(false);
            pin.add_css_class("flat");
            let popover_for_pin = popover.clone();
            let button = button_for_context.clone();
            let ui = ui_for_context.clone();
            let page_name = page_name_for_context.clone();
            let icon = pin_for_context.clone();
            pin.connect_clicked(move |_| {
                popover_for_pin.popdown();
                let pinned = !button.has_css_class("tab-pinned");
                if pinned {
                    button.add_css_class("tab-pinned");
                } else {
                    button.remove_css_class("tab-pinned");
                }
                icon.set_visible(pinned);
                unsafe {
                    button.set_data::<bool>("pinned", pinned);
                }
                if let Some(page) = notebook_page_named(&ui.notebook, &page_name) {
                    UiState::set_tab_page_pinned(&page, pinned);
                    ui.reorder_pinned_first();
                }
            });
            menu.append(&pin);

            let close = gtk4::Button::with_label("Close");
            close.set_has_frame(false);
            close.add_css_class("flat");
            let popover_for_close = popover.clone();
            let ui = ui_for_context.clone();
            let page_name = page_name_for_context.clone();
            close.connect_clicked(move |_| {
                popover_for_close.popdown();
                if let Some(page) = notebook_page_named(&ui.notebook, &page_name) {
                    ui.remove_tab_by_widget(&page);
                }
            });
            menu.append(&close);
            popover.set_child(Some(&menu));
            popover.connect_closed(|popover| popover.unparent());
            popover.popup();
        });
        button.add_controller(context);

        // Match the strip index to the Notebook insertion index.
        let mut sibling = self.tab_strip.first_child();
        for _ in 0..page_num {
            sibling = sibling.and_then(|child| child.next_sibling());
        }
        if let Some(sibling) = sibling {
            button.insert_before(&self.tab_strip, Some(&sibling));
        } else {
            self.tab_strip.append(&button);
        }
        if let Some(status) = moved_connection_status {
            self.set_tab_conn_status(tab_num, status);
        }
        self.apply_strip_btn_placement(&button);
        self.notebook.set_current_page(Some(page_num));
        self.sync_tab_strip_active(Some(page_num));
        self.sync_tab_bar_visibility();
        leaf.grab_focus();
    }

    pub(crate) fn add_new_tab(
        &self,
        working_directory: Option<String>,
        tab_name: Option<String>,
        session_id: Option<String>,
        initial_commands: Option<String>,
    ) -> Terminal {
        let terminal_mode = self.config.borrow().terminal_mode.clone();
        self.add_tab_with_argv(TabLaunch {
            working_directory,
            tab_name,
            session_id,
            initial_commands,
            argv_override: None,
            remote: None,
            terminal_mode,
        })
    }

    /// Launch an explicit argv instead of the configured interactive shell.
    /// Used by `--execute`; unlike startup commands this preserves argv
    /// boundaries and never rebuilds a shell command string.
    pub(crate) fn add_new_tab_with_argv(
        &self,
        working_directory: Option<String>,
        argv: Vec<String>,
    ) -> Terminal {
        let terminal_mode = self.config.borrow().terminal_mode.clone();
        self.add_tab_with_argv(TabLaunch {
            working_directory,
            tab_name: argv.first().cloned(),
            session_id: None,
            initial_commands: None,
            argv_override: Some(argv),
            remote: None,
            terminal_mode,
        })
    }

    /// Open a new tab connecting to a saved remote host over ssh.
    pub(crate) fn connect_remote(&self, host: &crate::config::RemoteHost) -> Terminal {
        self.connect_remote_with_attempt(host, 0)
    }

    /// Like `connect_remote`, but seeds the new tab's reconnect-backoff counter.
    /// Used by auto-reconnect to carry the attempt count across respawns.
    fn connect_remote_with_attempt(
        &self,
        host: &crate::config::RemoteHost,
        attempt: u32,
    ) -> Terminal {
        let argv = crate::config::build_remote_argv(host);
        // Remote tabs use Block so OSC 7/133/7770 metadata, command results and
        // reconnect session identifiers are observed consistently, matching
        // jterm1 even when local tabs default to conventional VTE.
        let terminal_mode = crate::config::TerminalMode::Block;
        log::info!("[remote] connecting to {} via {:?}", host.name, argv);
        self.add_tab_with_argv(TabLaunch {
            working_directory: None,
            tab_name: Some(host.name.clone()),
            session_id: None,
            initial_commands: None,
            argv_override: Some(argv),
            remote: Some((host.clone(), attempt)),
            terminal_mode,
        })
    }

    /// Mark a remote tab as connected (green badge). Called on first output.
    /// Note: this is a visual signal only — backoff reset is decided at exit
    /// time by `spawn_at` duration, so a fast ssh error banner can't reset it.
    fn mark_tab_connected(&self, tab_num: u32) {
        if let Some(conn) = self.tab_connections.borrow_mut().get_mut(&tab_num) {
            conn.status = ConnStatus::Connected;
        }
        self.set_tab_conn_status(tab_num, ConnStatus::Connected);
    }

    /// Close the tab with the given tab_num via the normal exit path.
    fn close_tab_by_num(&self, tab_num: u32) {
        for i in 0..self.notebook.n_pages() {
            if let Some(page_widget) = self.notebook.nth_page(Some(i)) {
                if page_widget.widget_name() == format!("tab-{}", tab_num) {
                    let eff_widget = scrollbar_wrapper_of(&page_widget)
                        .map(|bx| bx.upcast::<gtk4::Widget>())
                        .unwrap_or_else(|| page_widget.clone());
                    self.handle_terminal_exited(&eff_widget);
                    break;
                }
            }
        }
    }

    /// Decide what to do when a tab's child process exits: for a remote tab that
    /// died abnormally (non-zero exit), schedule an auto-reconnect; otherwise
    /// close the tab normally.
    pub(crate) fn handle_tab_exit(&self, tab_num: u32, code: i32, dead_leaf: &gtk4::Widget) {
        let conn = self.tab_connections.borrow().get(&tab_num).cloned();
        if let Some(conn) = conn {
            if code != 0 {
                self.schedule_reconnect(tab_num, conn, code, dead_leaf.clone());
                return;
            }
            // Clean exit (user typed `exit`/logout): drop record, close normally.
            self.tab_connections.borrow_mut().remove(&tab_num);
        }
        self.close_tab_by_num(tab_num);
    }

    /// Start a backoff countdown then respawn the remote connection in place.
    fn schedule_reconnect(
        &self,
        tab_num: u32,
        conn: TabConnection,
        code: i32,
        dead_leaf: gtk4::Widget,
    ) {
        const MAX_ATTEMPT: u32 = 6;
        // A session that stayed up long enough is treated as a healthy link that
        // dropped → reset backoff. A short-lived one (failed handshake/auth)
        // grows it.
        let stable = conn.spawn_at.elapsed() >= std::time::Duration::from_secs(10);
        let next_attempt = if stable { 0 } else { conn.attempt + 1 };

        if next_attempt > MAX_ATTEMPT {
            log::warn!(
                "[remote] giving up reconnect for '{}' (tab {}) after {} attempts",
                conn.host.name,
                tab_num,
                conn.attempt
            );
            if let Some(c) = self.tab_connections.borrow_mut().get_mut(&tab_num) {
                c.status = ConnStatus::Disconnected;
            }
            self.set_tab_conn_status(tab_num, ConnStatus::Disconnected);
            return;
        }

        let delay = if next_attempt == 0 {
            1u64
        } else {
            (1u64 << next_attempt.min(5)).min(30)
        };

        if let Some(c) = self.tab_connections.borrow_mut().get_mut(&tab_num) {
            c.status = ConnStatus::Disconnected;
            c.attempt = next_attempt;
        }
        self.set_tab_conn_status(tab_num, ConnStatus::Disconnected);

        let host = conn.host.clone();
        let connection_identity = conn.identity;
        log::info!(
            "[remote] '{}' (tab {}) disconnected (exit {}); reconnecting in {}s (attempt {})",
            host.name,
            tab_num,
            code,
            delay,
            next_attempt
        );

        let ui = self.clone();
        let remaining = Rc::new(Cell::new(delay));
        let dead_terminal = PaneLeaf::from_widget(&dead_leaf).map(|leaf| leaf.terminal().clone());
        self.set_tab_strip_label(tab_num, &format!("{} — reconnect {}s", host.name, delay));

        glib::timeout_add_seconds_local(1, move || {
            // Follow the connection record rather than the old tab number: a
            // dead remote pane can move while this countdown is active.
            let current_tab_num =
                ui.tab_connections
                    .borrow()
                    .iter()
                    .find_map(|(number, connection)| {
                        (connection.identity == connection_identity).then_some(*number)
                    });
            let Some(current_tab_num) = current_tab_num else {
                return glib::ControlFlow::Break;
            };
            // If the destination page is gone, the user closed it.
            let exists = (0..ui.notebook.n_pages()).any(|i| {
                ui.notebook
                    .nth_page(Some(i))
                    .map(|w| w.widget_name() == format!("tab-{current_tab_num}"))
                    .unwrap_or(false)
            });
            if !exists {
                ui.tab_connections.borrow_mut().remove(&current_tab_num);
                return glib::ControlFlow::Break;
            }
            let direct_page = notebook_page_named(&ui.notebook, &format!("tab-{current_tab_num}"))
                .is_some_and(|page| page == dead_leaf);
            let zoomed_dead_leaf = dead_terminal.as_ref().is_some_and(|terminal| {
                ui.zoom_state
                    .borrow()
                    .as_ref()
                    .is_some_and(|state| state.zoomed_terminal == *terminal)
            });
            if !direct_page || zoomed_dead_leaf {
                // Reconnect-in-place is a whole-page operation. If the user
                // split or zoomed after the child died, remove only that dead
                // leaf and preserve every live sibling instead.
                ui.tab_connections.borrow_mut().remove(&current_tab_num);
                ui.clear_tab_conn_status(current_tab_num);
                let still_attached = dead_leaf.parent().is_some()
                    || ui.notebook.page_num(&dead_leaf).is_some()
                    || zoomed_dead_leaf;
                if still_attached {
                    ui.handle_terminal_exited(&dead_leaf);
                }
                ui.retitle_tab_from_active_leaf(current_tab_num);
                return glib::ControlFlow::Break;
            }
            let left = remaining.get();
            if left > 1 {
                remaining.set(left - 1);
                ui.set_tab_strip_label(
                    current_tab_num,
                    &format!("{} — reconnect {}s", host.name, left - 1),
                );
                return glib::ControlFlow::Continue;
            }
            ui.do_reconnect(current_tab_num, &host, next_attempt);
            glib::ControlFlow::Break
        });
    }

    /// Respawn a dead remote tab in place: insert a fresh connection at the dead
    /// tab's notebook slot (preserving position), then remove the dead page. The
    /// rebuilt argv reuses the host's baked-in `--session` id so rsh restores the
    /// snapshot.
    fn do_reconnect(&self, dead_tab_num: u32, host: &crate::config::RemoteHost, attempt: u32) {
        let dead_name = format!("tab-{}", dead_tab_num);
        let dead_idx = (0..self.notebook.n_pages()).find(|&i| {
            self.notebook
                .nth_page(Some(i))
                .map(|w| w.widget_name() == dead_name)
                .unwrap_or(false)
        });
        let Some(dead_idx) = dead_idx else {
            self.tab_connections.borrow_mut().remove(&dead_tab_num);
            return;
        };

        // Insert the replacement right after the dead page.
        self.notebook.set_current_page(Some(dead_idx));
        self.connect_remote_with_attempt(host, attempt);

        // Remove the now-stale dead page (still at dead_idx); position is preserved.
        self.tab_connections.borrow_mut().remove(&dead_tab_num);
        if let Some(dead_widget) = self.notebook.nth_page(Some(dead_idx)) {
            if dead_widget.widget_name() == dead_name {
                self.remove_tab_by_widget_internal(&dead_widget);
            }
        }
    }

    /// Core tab-creation routine. When `argv_override` is `Some`, the tab runs that
    /// argv (e.g. an ssh command) instead of the configured local shell. When
    /// `remote` is `Some`, the tab is tracked as an ssh connection (status badge +
    /// auto-reconnect) via `tab_connections`.
    fn add_tab_with_argv(&self, launch: TabLaunch) -> Terminal {
        let TabLaunch {
            working_directory,
            tab_name,
            session_id,
            initial_commands,
            argv_override,
            remote,
            terminal_mode,
        } = launch;
        let tab_num = self.tab_counter.get();
        self.tab_counter.set(tab_num + 1);

        // Generate or reuse session ID for rsh session persistence
        let sid = session_id.unwrap_or_else(generate_session_id);
        self.session_ids.borrow_mut().insert(tab_num, sid.clone());

        // Record the per-tab connection so we can show status and auto-reconnect.
        if let Some((host, attempt)) = &remote {
            self.tab_connections.borrow_mut().insert(
                tab_num,
                TabConnection {
                    identity: tab_num,
                    host: host.clone(),
                    status: ConnStatus::Connecting,
                    attempt: *attempt,
                    spawn_at: std::time::Instant::now(),
                },
            );
        }

        let configured_shell = self.shell_argv.borrow();
        let shell_argv: &[String] = argv_override
            .as_deref()
            .unwrap_or(configured_shell.as_slice());

        // Create terminal view based on configured mode
        let (view_type, terminal) = {
            match &terminal_mode {
                crate::config::TerminalMode::Block => {
                    let term_view = Rc::new(TermView::new(
                        &self.config.borrow(),
                        shell_argv,
                        working_directory.as_deref(),
                        Some(&sid),
                        initial_commands.as_deref(),
                    ));
                    let terminal = term_view.vte().clone();
                    (PaneLeaf::Block(term_view), terminal)
                }
                crate::config::TerminalMode::Vte => {
                    let vte_view = Rc::new(VteTerminalView::new(
                        self.config.clone(),
                        shell_argv,
                        working_directory.as_deref(),
                        Some(&sid),
                        initial_commands.as_deref(),
                    ));
                    let terminal = vte_view.vte().clone();
                    (PaneLeaf::Vte(vte_view), terminal)
                }
            }
        };

        // Setup click handler for hyperlinks and context menu (uses VTE inside both views)
        setup_terminal_click_handler(&terminal);
        self.setup_context_menu(&terminal);

        // Connect callbacks based on view type. A local tab's original leaf may
        // later become one side of a split, so its exit must follow the same
        // pane-aware collapse path as leaves created by `split_current`.
        // Remote primaries retain tab-level reconnect semantics.
        let is_remote = remote.is_some();
        match &view_type {
            PaneLeaf::Block(term_view) => {
                let ui_for_exit = UiState::clone(self);
                let term_view_for_exit = term_view.clone();
                let root_for_exit = term_view.widget();
                let tab_num_for_exit = tab_num;
                term_view.connect_exited(move |code| {
                    let _ = term_view_for_exit.save_history();
                    let current_tab_num =
                        tab_num_for_widget(&root_for_exit).unwrap_or(tab_num_for_exit);
                    let is_split = root_for_exit
                        .parent()
                        .is_some_and(|parent| parent.is::<gtk4::Paned>())
                        || ui_for_exit
                            .zoom_state
                            .borrow()
                            .as_ref()
                            .is_some_and(|state| {
                                state.zoomed_terminal == *term_view_for_exit.vte()
                            });
                    if is_remote && !is_split {
                        ui_for_exit.handle_tab_exit(current_tab_num, code, &root_for_exit);
                    } else {
                        if is_remote {
                            ui_for_exit
                                .tab_connections
                                .borrow_mut()
                                .remove(&current_tab_num);
                            ui_for_exit.clear_tab_conn_status(current_tab_num);
                        }
                        ui_for_exit.handle_terminal_exited(&root_for_exit);
                    }
                });

                let conns_for_session = self.tab_connections.clone();
                let root_for_session = term_view.widget();
                term_view.connect_remote_session_id(move |id| {
                    let current_tab_num = tab_num_for_widget(&root_for_session).unwrap_or(tab_num);
                    if let Some(conn) = conns_for_session.borrow_mut().get_mut(&current_tab_num) {
                        conn.host.session = Some(id.to_string());
                    }
                });

                // Keep a lightweight cross-session index. Full block output
                // remains governed by block_history_path; this record contains
                // only command metadata and is safe for palette use.
                let config_for_history = self.config.clone();
                let view_for_history = Rc::downgrade(term_view);
                term_view.connect_block_finished(move |command, exit_code, _output_sample| {
                    let config = config_for_history.borrow();
                    if !config.command_history_enabled {
                        return;
                    }
                    let Some(path) = config.command_history_path.as_deref() else {
                        return;
                    };
                    let cwd = view_for_history
                        .upgrade()
                        .map(|view| view.cwd())
                        .filter(|cwd| !cwd.is_empty());
                    let end_time_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
                    if let Err(err) = crate::command_history::append(
                        std::path::Path::new(path),
                        config.command_history_max_entries as usize,
                        &command,
                        cwd.as_deref(),
                        exit_code,
                        end_time_ms,
                    ) {
                        log::warn!("failed to append command history: {err}");
                    }
                });
            }
            PaneLeaf::Vte(vte_view) => {
                let ui_for_exit = UiState::clone(self);
                let root_for_exit = vte_view.widget();
                let terminal_for_exit = vte_view.vte().clone();
                let tab_num_for_exit = tab_num;
                vte_view.connect_exited(move |code| {
                    let current_tab_num =
                        tab_num_for_widget(&root_for_exit).unwrap_or(tab_num_for_exit);
                    let is_split = root_for_exit
                        .parent()
                        .is_some_and(|parent| parent.is::<gtk4::Paned>())
                        || ui_for_exit
                            .zoom_state
                            .borrow()
                            .as_ref()
                            .is_some_and(|state| state.zoomed_terminal == terminal_for_exit);
                    if is_remote && !is_split {
                        ui_for_exit.handle_tab_exit(current_tab_num, code, &root_for_exit);
                    } else {
                        if is_remote {
                            ui_for_exit
                                .tab_connections
                                .borrow_mut()
                                .remove(&current_tab_num);
                            ui_for_exit.clear_tab_conn_status(current_tab_num);
                        }
                        ui_for_exit.handle_terminal_exited(&root_for_exit);
                    }
                });
            }
        }

        // For remote tabs, flip the status badge to green on first output.
        if remote.is_some() {
            let ui_for_conn = self.clone();
            let fired = Rc::new(Cell::new(false));
            let root_for_conn = view_type.root_widget();
            terminal.connect_contents_changed(move |_| {
                if fired.get() {
                    return;
                }
                fired.set(true);
                let current_tab_num = tab_num_for_widget(&root_for_conn).unwrap_or(tab_num);
                ui_for_conn.mark_tab_connected(current_tab_num);
            });
        }

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
                show_rename_dialog(
                    &window_for_rename,
                    &label_for_rename,
                    custom_title_for_rename.clone(),
                );
            }
        });
        label.add_controller(rename_click);

        // Auto-update tab title from CWD changes reported via OSC 7
        let label_for_pwd = label.clone();
        let custom_title_for_pwd = custom_title.clone();
        let tab_index_for_pwd = tab_num + 1;
        let strip_btn_label: Rc<RefCell<Option<Label>>> = Rc::new(RefCell::new(None));
        let strip_btn_label_for_pwd = strip_btn_label.clone();

        match &view_type {
            PaneLeaf::Block(term_view) => {
                let term_view_for_pwd = term_view.clone();
                let identity_for_pwd = view_type.root_widget();
                let expected_name_for_pwd = format!("tab-{tab_num}");
                term_view_for_pwd.connect_cwd_changed(move |dir| {
                    if identity_for_pwd.widget_name() != expected_name_for_pwd
                        || custom_title_for_pwd.get()
                    {
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
            }
            PaneLeaf::Vte(vte_view) => {
                let vte_view_for_pwd = vte_view.clone();
                let identity_for_pwd = view_type.root_widget();
                let expected_name_for_pwd = format!("tab-{tab_num}");
                vte_view_for_pwd.connect_cwd_changed(move |dir| {
                    if identity_for_pwd.widget_name() != expected_name_for_pwd
                        || custom_title_for_pwd.get()
                    {
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
            }
        }

        // Keep the existing tab widgets alive for OSC 0/2 title changes.
        // Some applications animate their title with a spinner; replacing the
        // strip button for every frame loses in-flight click and drag gestures.
        let identity_for_title = view_type.root_widget();
        let expected_name_for_title = format!("tab-{tab_num}");
        let update_title = |connect: &dyn Fn(TitleChangedCallback)| {
            let label_for_title = label.clone();
            let strip_btn_label_for_title = strip_btn_label.clone();
            let custom_title_for_title = custom_title.clone();
            let identity_for_title = identity_for_title.clone();
            let expected_name_for_title = expected_name_for_title.clone();
            connect(Box::new(move |title| {
                if identity_for_title.widget_name() != expected_name_for_title
                    || custom_title_for_title.get()
                    || label_for_title.text().as_str() == title
                {
                    return;
                }
                label_for_title.set_text(title);
                if let Some(ref btn_label) = *strip_btn_label_for_title.borrow() {
                    btn_label.set_text(title);
                }
            }));
        };
        match &view_type {
            PaneLeaf::Block(term_view) => {
                update_title(&|callback| term_view.connect_title_changed(callback));
            }
            PaneLeaf::Vte(vte_view) => {
                update_title(&|callback| vte_view.connect_title_changed(callback));
            }
        }

        let close_button = gtk4::Button::from_icon_name("window-close-symbolic");
        close_button.set_focus_on_click(false);
        close_button.set_can_focus(false);
        close_button.set_has_frame(false);
        close_button.add_css_class("flat");
        close_button.set_tooltip_text(Some("Close tab"));

        tab_box.append(&label);
        tab_box.append(&close_button);

        // PaneLeaf owns the distinction between Block and conventional VTE
        // roots. Tab construction only requires the common GTK leaf widget.
        let term_wrapper = view_type
            .root_widget()
            .downcast::<gtk4::Box>()
            .expect("pane root must be a Box");

        // Keep controller attachment behind PaneLeaf's single GTK object-data
        // boundary. Block tabs no longer publish a second legacy `term-view` key.
        let term_wrapper_for_name = term_wrapper.clone();
        term_wrapper_for_name.set_widget_name(&format!("tab-{}", tab_num));
        let term_wrapper_widget = term_wrapper.clone().upcast::<gtk4::Widget>();
        unsafe {
            term_wrapper_widget.set_data::<Rc<Cell<bool>>>(CUSTOM_TITLE_DATA, custom_title.clone());
        }
        view_type.attach_to(&term_wrapper_widget);
        view_type.set_session_id(&sid);
        view_type.set_remote(is_remote);

        let ui_for_close = UiState::clone(self);
        let page_name_for_close = format!("tab-{tab_num}");
        close_button.connect_clicked(move |_| {
            if let Some(page) = notebook_page_named(&ui_for_close.notebook, &page_name_for_close) {
                ui_for_close.remove_tab_by_widget(&page);
            }
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

        // Process indicator label
        let process_label = Label::new(None);
        process_label.add_css_class("tab-process-indicator");
        process_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        process_label.set_max_width_chars(15);

        // Pin indicator icon
        let pin_icon = gtk4::Image::new();
        pin_icon.set_icon_name(Some("bookmark-symbolic"));
        pin_icon.add_css_class("tab-pin-icon");
        pin_icon.set_visible(false); // Hidden by default

        // Connection-status dot (remote tabs only): yellow→green→red.
        let conn_dot = Label::new(Some("\u{25CF}"));
        conn_dot.add_css_class("tab-conn-dot");
        if remote.is_some() {
            conn_dot.add_css_class("tab-connecting");
        } else {
            conn_dot.set_visible(false);
        }

        let strip_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        strip_box.append(&conn_dot);
        strip_box.append(&strip_label);
        strip_box.append(&process_label);
        strip_box.append(&pin_icon);
        strip_box.append(&strip_close_icon);

        let strip_btn = ToggleButton::new();
        strip_btn.set_child(Some(&strip_box));
        // Keep an explicit handle to the title label. The strip also contains
        // connection and process labels, so walking to the first Label filters
        // on the status dot rather than the tab title.
        unsafe {
            strip_btn.set_data::<Label>("tab-title-label", strip_label.clone());
        }
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

        // Dynamic tooltip: working directory, process name, and status
        strip_btn.set_has_tooltip(true);
        let notebook_for_tooltip = self.notebook.clone();
        let tab_strip_for_tooltip = self.tab_strip.clone();
        let strip_btn_for_tooltip = strip_btn.clone();
        let tab_connections_for_tooltip = self.tab_connections.clone();
        strip_btn.connect_query_tooltip(move |_, _x, _y, _keyboard, tooltip| {
            // Build tooltip text with cwd, process name, and status
            let mut tooltip_parts = Vec::new();

            // Remote connection info, if this is a remote tab.
            if let Some(num) = strip_btn_for_tooltip
                .widget_name()
                .strip_prefix("tab-")
                .and_then(|s| s.parse::<u32>().ok())
            {
                if let Some(conn) = tab_connections_for_tooltip.borrow().get(&num) {
                    let state = match conn.status {
                        ConnStatus::Connecting => "connecting",
                        ConnStatus::Connected => "connected",
                        ConnStatus::Disconnected => "disconnected",
                    };
                    tooltip_parts.push(format!("Remote: {} ({})", conn.host.name, state));
                }
            }

            if let Some(page) = notebook_page_named(
                &notebook_for_tooltip,
                strip_btn_for_tooltip.widget_name().as_str(),
            ) {
                if let Some(leaf) = PaneNode::from_widget(&page).and_then(|node| node.active_leaf())
                {
                    if let Some(cwd) = terminal_working_directory(leaf.terminal()) {
                        tooltip_parts.push(format!("Dir: {cwd}"));
                    }
                    if let Some(proc_name) = leaf.foreground_process_name() {
                        tooltip_parts.push(format!("Process: {proc_name}"));
                    }
                }
            }

            // Add status indicators
            let mut status = Vec::new();
            let btn_name = strip_btn_for_tooltip.widget_name();
            if !btn_name.is_empty() {
                let mut child = tab_strip_for_tooltip.first_child();
                while let Some(ref c) = child {
                    if c.widget_name() == btn_name {
                        if c.has_css_class("tab-activity") {
                            status.push("activity");
                        }
                        if c.has_css_class("tab-bell") {
                            status.push("bell");
                        }
                        if c.has_css_class("tab-pinned") {
                            status.push("pinned");
                        }
                        break;
                    }
                    child = c.next_sibling();
                }
            }
            if !status.is_empty() {
                tooltip_parts.push(format!("Status: {}", status.join(", ")));
            }

            if !tooltip_parts.is_empty() {
                tooltip.set_text(Some(&tooltip_parts.join("\n")));
                true
            } else {
                false
            }
        });

        // Give button a unique name to correlate with notebook page
        let tab_widget_name = format!("tab-{}", tab_num);
        strip_btn.set_widget_name(&tab_widget_name);
        // Also name the wrapper widget so we can find the button when removing
        term_wrapper.set_widget_name(&tab_widget_name);

        // Periodic process indicator update (every 2 seconds)
        let notebook_for_proc = self.notebook.clone();
        let page_name_for_proc = tab_widget_name.clone();
        let process_label_for_update = process_label.clone();
        glib::timeout_add_local(std::time::Duration::from_secs(2), move || {
            // Check if widget is still alive
            if process_label_for_update.parent().is_none() {
                return glib::ControlFlow::Break;
            }

            let process = notebook_page_named(&notebook_for_proc, &page_name_for_proc)
                .and_then(|page| PaneNode::from_widget(&page))
                .and_then(|node| {
                    node.leaves()
                        .into_iter()
                        .find_map(|leaf| leaf.foreground_process_name())
                });
            if let Some(proc_name) = process {
                process_label_for_update.set_text(&proc_name);
                process_label_for_update.set_visible(true);
            } else {
                process_label_for_update.set_visible(false);
            }
            glib::ControlFlow::Continue
        });

        // Bell signal: flash the tab strip button when bell rings on non-active tab
        let ui_for_bell = self.clone();
        let leaf_for_bell = view_type.clone();
        terminal.connect_bell(move |_| {
            log::debug!("Bell signal received");
            ui_for_bell.mark_tab_bell(&leaf_for_bell.root_widget().widget_name());
        });

        // Activity indicator: mark tab when there's output on a non-active tab
        let ui_for_activity = self.clone();
        let leaf_for_activity = view_type.clone();
        terminal.connect_commit(move |_, _, _| {
            ui_for_activity.mark_tab_activity(&leaf_for_activity.root_widget().widget_name());
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

        // Right-click context menu on tab button
        let right_click_gesture = GestureClick::new();
        right_click_gesture.set_button(3);
        let ui_for_ctx = self.clone();
        let strip_btn_for_ctx = strip_btn.clone();
        let tab_name_for_ctx = tab_widget_name.clone();
        right_click_gesture.connect_pressed(move |gesture, _, x, y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);

            // NOTE: We deliberately build this menu out of plain Buttons inside a
            // Popover rather than a PopoverMenu/gio::Menu model. The GAction-based
            // dispatch (insert_action_group + "tab-ctx.*" detailed names) silently
            // fails to activate in this GTK build, so direct connect_clicked closures
            // are used instead.
            let remote_hosts = ui_for_ctx.config.borrow().remote_hosts.clone();

            let popover = gtk4::Popover::new();
            popover.set_parent(&strip_btn_for_ctx);
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

            // Rename
            {
                let item = make_item("Rename");
                let popover_c = popover.clone();
                let window_for_rename = ui_for_ctx.window.clone();
                let label_for_rename = label.clone();
                let strip_label_for_rename = strip_label.clone();
                let custom_title_for_rename = custom_title.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    show_rename_dialog_with_strip(
                        &window_for_rename,
                        &label_for_rename,
                        &strip_label_for_rename,
                        custom_title_for_rename.clone(),
                    );
                });
                vbox.append(&item);
            }

            // Duplicate
            {
                let item = make_item("Duplicate");
                let popover_c = popover.clone();
                let ui_duplicate_ctx = ui_for_ctx.clone();
                let page_name = tab_name_for_ctx.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    let page = notebook_page_named(&ui_duplicate_ctx.notebook, &page_name);
                    let working_directory = page
                        .as_ref()
                        .and_then(PaneNode::from_widget)
                        .and_then(|node| node.active_terminal())
                        .and_then(|terminal| terminal_working_directory(&terminal))
                        .or_else(|| std::env::var("HOME").ok());
                    let title = page
                        .as_ref()
                        .and_then(|page| custom_tab_title(&ui_duplicate_ctx.notebook, page));
                    ui_duplicate_ctx.add_new_tab(working_directory, title, None, None);
                });
                vbox.append(&item);
            }

            // Mark Important
            {
                let item = make_item("Mark Important");
                let popover_c = popover.clone();
                let strip_btn_mark = strip_btn_for_ctx.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    if strip_btn_mark.has_css_class("tab-marked") {
                        strip_btn_mark.remove_css_class("tab-marked");
                        unsafe {
                            strip_btn_mark.set_data::<bool>("marked", false);
                        }
                    } else {
                        strip_btn_mark.add_css_class("tab-marked");
                        unsafe {
                            strip_btn_mark.set_data::<bool>("marked", true);
                        }
                    }
                });
                vbox.append(&item);
            }

            // Pin Tab
            {
                let item = make_item("Pin Tab");
                let popover_c = popover.clone();
                let strip_btn_pin = strip_btn_for_ctx.clone();
                let ui_for_pin = ui_for_ctx.clone();
                let page_name = tab_name_for_ctx.clone();
                let pin_icon_pin = pin_icon.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    let pinned = !strip_btn_pin.has_css_class("tab-pinned");
                    if !pinned {
                        strip_btn_pin.remove_css_class("tab-pinned");
                        pin_icon_pin.set_visible(false);
                    } else {
                        strip_btn_pin.add_css_class("tab-pinned");
                        pin_icon_pin.set_visible(true);
                    }
                    unsafe {
                        strip_btn_pin.set_data::<bool>("pinned", pinned);
                    }
                    if let Some(page) = notebook_page_named(&ui_for_pin.notebook, &page_name) {
                        UiState::set_tab_page_pinned(&page, pinned);
                        ui_for_pin.reorder_pinned_first();
                    }
                });
                vbox.append(&item);
            }

            // Close
            {
                let item = make_item("Close");
                let popover_c = popover.clone();
                let ui_close_ctx = ui_for_ctx.clone();
                let page_name = tab_name_for_ctx.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    if let Some(page) = notebook_page_named(&ui_close_ctx.notebook, &page_name) {
                        ui_close_ctx.remove_tab_by_widget(&page);
                    }
                });
                vbox.append(&item);
            }

            // New Tab
            {
                let item = make_item("New Tab");
                let popover_c = popover.clone();
                let ui_new_ctx = ui_for_ctx.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    ui_new_ctx.execute_action(Action::NewTab);
                });
                vbox.append(&item);
            }

            // Close Selected Tabs (only if there are selected tabs)
            if !ui_for_ctx.selected_tabs.borrow().is_empty() {
                let item = make_item("Close Selected Tabs");
                let popover_c = popover.clone();
                let ui_close_selected = ui_for_ctx.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    ui_close_selected.close_selected_tabs();
                });
                vbox.append(&item);
            }

            // Remote connect items
            for h in remote_hosts.iter() {
                let item = make_item(&format!("Remote: {}", h.name));
                let popover_c = popover.clone();
                let ui_remote = ui_for_ctx.clone();
                let host = h.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    ui_remote.connect_remote(&host);
                });
                vbox.append(&item);
            }

            popover.set_child(Some(&vbox));

            // Clean up the parent reference when the popover closes
            popover.connect_closed(move |p| {
                p.unparent();
            });

            popover.popup();
        });
        strip_btn.add_controller(right_click_gesture);

        // Close icon click: use a capture-phase gesture on the ToggleButton so we
        // intercept the press before the button's own toggle handler.
        let close_gesture = GestureClick::new();
        close_gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let ui_for_strip_close = self.clone();
        let page_name_for_strip_close = tab_widget_name.clone();
        let close_icon_for_hit = strip_close_icon.clone();
        let strip_btn_for_close = strip_btn.clone();
        close_gesture.connect_pressed(move |gesture, _n, x, y| {
            // Check if the click landed on the close icon area
            let btn_widget = strip_btn_for_close.upcast_ref::<gtk4::Widget>();
            let icon_widget = close_icon_for_hit.upcast_ref::<gtk4::Widget>();
            let point = gtk4::graphene::Point::new(x as f32, y as f32);
            if let Some(mapped) = btn_widget.compute_point(icon_widget, &point) {
                let ix = mapped.x() as f64;
                let iy = mapped.y() as f64;
                let w = icon_widget.width() as f64;
                let h = icon_widget.height() as f64;
                if ix >= 0.0 && iy >= 0.0 && ix <= w && iy <= h {
                    gesture.set_state(gtk4::EventSequenceState::Claimed);
                    if let Some(page) = notebook_page_named(
                        &ui_for_strip_close.notebook,
                        &page_name_for_strip_close,
                    ) {
                        ui_for_strip_close.remove_tab_by_widget(&page);
                    }
                }
            }
        });
        strip_btn.add_controller(close_gesture);

        // Multi-select click handler: supports Ctrl+Click (toggle), Shift+Click (range), plain click (normal)
        let click_gesture = GestureClick::new();
        click_gesture.set_button(1);
        click_gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);

        let ui_for_select = self.clone();
        let strip_btn_for_select = strip_btn.clone();
        let tab_name_for_select = tab_widget_name.clone();
        let notebook_for_select = self.notebook.clone();
        let tab_strip_for_select = self.tab_strip.clone();

        click_gesture.connect_pressed(move |gesture, n_press, _, _| {
            if n_press != 1 {
                return; // Only handle single press
            }

            let event_state = gesture.current_event_state();
            let has_ctrl = event_state.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
            let has_shift = event_state.contains(gtk4::gdk::ModifierType::SHIFT_MASK);

            if has_ctrl {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                ui_for_select.toggle_tab_selection(&tab_name_for_select);
            } else if has_shift {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                // Find the last selected tab or current tab
                let selected = ui_for_select.selected_tabs.borrow();
                let from_name = if let Some(last) = selected.last() {
                    last.clone()
                } else {
                    // Use currently active tab
                    if let Some(page) = notebook_for_select.current_page() {
                        if let Some(page_widget) = notebook_for_select.nth_page(Some(page)) {
                            page_widget.widget_name().to_string()
                        } else {
                            tab_name_for_select.clone()
                        }
                    } else {
                        tab_name_for_select.clone()
                    }
                };
                drop(selected);
                ui_for_select.select_tab_range(&from_name, &tab_name_for_select);
            } else {
                // Plain click: clear selection and switch tab
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                ui_for_select.clear_tab_selection();

                // Find the index of this button in the strip
                let mut idx = 0u32;
                let mut child = tab_strip_for_select.first_child();
                while let Some(ref c) = child {
                    if c == strip_btn_for_select.upcast_ref::<gtk4::Widget>() {
                        break;
                    }
                    idx += 1;
                    child = c.next_sibling();
                }
                notebook_for_select.set_current_page(Some(idx));
                ui_for_select.sync_tab_strip_active(Some(idx));
            }
        });
        strip_btn.add_controller(click_gesture);

        // A ToggleButton flips its own `active` state when clicked (on release),
        // which fights the notebook-driven selection: clicking a tab would set
        // it active via switch-page, then the release toggle would clear it,
        // dropping the :checked styling. Re-assert the correct state after the
        // toggle settles. Scheduling on idle lets the in-progress toggle finish
        // first; sync only emits `toggled` for buttons that actually change, so
        // this converges instead of looping.
        let ui_for_toggle = self.clone();
        strip_btn.connect_toggled(move |_| {
            let ui_for_idle = ui_for_toggle.clone();
            glib::idle_add_local_once(move || {
                ui_for_idle.sync_tab_strip_active(None);
            });
        });

        // Drag source: carry the widget name so we can identify the dragged button
        let drag_source = gtk4::DragSource::new();
        drag_source.set_actions(gtk4::gdk::DragAction::MOVE);
        let strip_btn_for_drag = strip_btn.clone();
        drag_source.connect_prepare(move |_, _, _| {
            let name = strip_btn_for_drag.widget_name().to_string();
            Some(gtk4::gdk::ContentProvider::for_value(&name.to_value()))
        });

        // Visual feedback during drag
        let strip_btn_drag_begin = strip_btn.clone();
        drag_source.connect_drag_begin(move |_, _| {
            strip_btn_drag_begin.add_css_class("tab-dragging");
        });

        let strip_btn_drag_end = strip_btn.clone();
        drag_source.connect_drag_end(move |_, _, _| {
            strip_btn_drag_end.remove_css_class("tab-dragging");
        });

        strip_btn.add_controller(drag_source);

        // Drop target: reorder strip buttons and notebook pages
        let drop_target = gtk4::DropTarget::new(glib::Type::STRING, gtk4::gdk::DragAction::MOVE);
        let tab_strip_for_drop = self.tab_strip.clone();
        let notebook_for_drop = self.notebook.clone();
        let strip_btn_for_drop = strip_btn.clone();
        drop_target.connect_drop(move |_, value, _, _| {
            let Ok(drag_name) = value.get::<String>() else {
                return false;
            };
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
            let Some(target_w) = target_w else {
                return false;
            };

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

        // Visual feedback for drop target
        let strip_btn_for_drop_motion = strip_btn.clone();
        drop_target.connect_motion(move |_, _x, _y| {
            strip_btn_for_drop_motion.add_css_class("tab-drop-target");
            gtk4::gdk::DragAction::MOVE
        });

        let strip_btn_for_drop_leave = strip_btn.clone();
        drop_target.connect_leave(move |_| {
            strip_btn_for_drop_leave.remove_css_class("tab-drop-target");
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

        // Size the new button for the active placement (sidebar vs top bar)
        self.apply_strip_btn_placement(&strip_btn);

        // Deactivate all other strip buttons
        self.sync_tab_strip_active(Some(page_num));
        self.sync_tab_bar_visibility();

        // Focus the new terminal
        match &view_type {
            PaneLeaf::Block(term_view) => term_view.grab_focus(),
            PaneLeaf::Vte(vte_view) => vte_view.grab_focus(),
        }

        terminal
    }
}
