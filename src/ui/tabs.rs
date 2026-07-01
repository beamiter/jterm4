//! tabs — UiState methods extracted from ui (mechanical split, no logic changes)
use adw::prelude::*;
use gtk4::gdk::ffi::GDK_BUTTON_PRIMARY;
use gtk4::{glib, Label, Paned};
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
    collect_terminals, default_tab_title, find_first_terminal, find_focused_terminal,
    scrollbar_wrapper_of, setup_terminal_click_handler, show_rename_dialog,
    show_rename_dialog_with_strip, terminal_working_directory, wrap_with_scrollbar,
    VteTerminalView,
};

impl UiState {
    pub(crate) fn remove_tab_by_widget(&self, widget: &gtk4::Widget) {
        // Check for running process before closing
        if let Some(terminal) = find_first_terminal(widget) {
            if let Some(process_info) = crate::state::get_restorable_commands(&terminal) {
                // Spawn async confirmation dialog
                let ui_state = self.clone();
                let widget_clone = widget.clone();
                glib::MainContext::default().spawn_local(async move {
                    if Self::confirm_close_tab_with_process(&ui_state.window, &process_info).await {
                        ui_state.remove_tab_by_widget_internal(&widget_clone);
                    }
                });
                return;
            }
        }

        // No running process, close immediately
        self.remove_tab_by_widget_internal(widget);
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

    pub(crate) fn remove_current_tab(&self) {
        if let Some(page_num) = self.notebook.current_page() {
            if let Some(widget) = self.notebook.nth_page(Some(page_num)) {
                // Check for running process before closing
                if let Some(terminal) = find_first_terminal(&widget) {
                    if let Some(process_info) = crate::state::get_restorable_commands(&terminal) {
                        // Spawn async confirmation dialog
                        let ui_state = self.clone();
                        let widget_clone = widget.clone();
                        glib::MainContext::default().spawn_local(async move {
                            if Self::confirm_close_tab_with_process(&ui_state.window, &process_info)
                                .await
                            {
                                ui_state.remove_tab_by_widget_internal(&widget_clone);
                            }
                        });
                        return;
                    }
                }

                // No running process, close immediately
                self.remove_tab_by_widget_internal(&widget);
            }
        }
    }

    fn remove_tab_by_widget_internal(&self, widget: &gtk4::Widget) {
        // Kill shell processes and remove the strip button for the current page
        if !kill_widget_child_processes(widget) {
            let mut terms = Vec::new();
            collect_terminals(widget, &mut terms);
            for term in &terms {
                kill_terminal_child(term);
            }
        }
        self.remove_strip_button_for(widget);

        // Drop per-tab bookkeeping keyed by tab_num parsed from the widget name.
        if let Some(tab_num) = widget
            .widget_name()
            .strip_prefix("tab-")
            .and_then(|s| s.parse::<u32>().ok())
        {
            self.session_ids.borrow_mut().remove(&tab_num);
            self.tab_connections.borrow_mut().remove(&tab_num);
        }

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

    pub(crate) fn duplicate_current_tab(&self) {
        if let Some(page) = self.notebook.current_page() {
            if let Some(widget) = self.notebook.nth_page(Some(page)) {
                let working_directory = find_first_terminal(&widget)
                    .as_ref()
                    .and_then(terminal_working_directory);
                self.add_new_tab(working_directory, None, None, None);
            }
        }
    }

    /// Add an existing terminal widget as a new tab (used by move_pane_to_new_tab).
    pub(crate) fn add_terminal_as_new_tab(
        &self,
        terminal: Terminal,
        working_directory: Option<String>,
    ) {
        let tab_num = self.tab_counter.get();
        self.tab_counter.set(tab_num + 1);

        // Assign a session ID for the moved pane's new tab
        let sid = generate_session_id();
        self.session_ids.borrow_mut().insert(tab_num, sid);

        let tab_name = default_tab_title(tab_num, working_directory.as_deref());

        // Use existing scrollbar wrapper if present, otherwise create one
        let page_widget: gtk4::Widget =
            scrollbar_wrapper_of(&terminal.clone().upcast::<gtk4::Widget>())
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

    pub(crate) fn add_new_tab(
        &self,
        working_directory: Option<String>,
        tab_name: Option<String>,
        session_id: Option<String>,
        initial_commands: Option<String>,
    ) -> Terminal {
        self.add_tab_with_argv(
            working_directory,
            tab_name,
            session_id,
            initial_commands,
            None,
            None,
        )
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
        log::info!("[remote] connecting to {} via {:?}", host.name, argv);
        self.add_tab_with_argv(
            None,
            Some(host.name.clone()),
            None,
            None,
            Some(argv),
            Some((host.clone(), attempt)),
        )
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
    pub(crate) fn handle_tab_exit(&self, tab_num: u32, code: i32) {
        let conn = self.tab_connections.borrow().get(&tab_num).cloned();
        if let Some(conn) = conn {
            if code != 0 {
                self.schedule_reconnect(tab_num, conn, code);
                return;
            }
            // Clean exit (user typed `exit`/logout): drop record, close normally.
            self.tab_connections.borrow_mut().remove(&tab_num);
        }
        self.close_tab_by_num(tab_num);
    }

    /// Start a backoff countdown then respawn the remote connection in place.
    fn schedule_reconnect(&self, tab_num: u32, conn: TabConnection, code: i32) {
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
        self.set_tab_strip_label(tab_num, &format!("{} — reconnect {}s", host.name, delay));

        glib::timeout_add_seconds_local(1, move || {
            // If the page is gone, the user closed the tab → cancel reconnect.
            let exists = (0..ui.notebook.n_pages()).any(|i| {
                ui.notebook
                    .nth_page(Some(i))
                    .map(|w| w.widget_name() == format!("tab-{}", tab_num))
                    .unwrap_or(false)
            });
            if !exists {
                ui.tab_connections.borrow_mut().remove(&tab_num);
                return glib::ControlFlow::Break;
            }
            let left = remaining.get();
            if left > 1 {
                remaining.set(left - 1);
                ui.set_tab_strip_label(
                    tab_num,
                    &format!("{} — reconnect {}s", host.name, left - 1),
                );
                return glib::ControlFlow::Continue;
            }
            ui.do_reconnect(tab_num, &host, next_attempt);
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
    fn add_tab_with_argv(
        &self,
        working_directory: Option<String>,
        tab_name: Option<String>,
        session_id: Option<String>,
        initial_commands: Option<String>,
        argv_override: Option<Vec<String>>,
        remote: Option<(crate::config::RemoteHost, u32)>,
    ) -> Terminal {
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
                    host: host.clone(),
                    status: ConnStatus::Connecting,
                    attempt: *attempt,
                    spawn_at: std::time::Instant::now(),
                },
            );
        }

        let shell_argv: &[String] = argv_override
            .as_deref()
            .unwrap_or(self.shell_argv.as_slice());

        // Create terminal view based on configured mode
        let (view_type, terminal) = {
            let config = self.config.borrow();
            match &config.terminal_mode {
                crate::config::TerminalMode::Block => {
                    drop(config);
                    let term_view = Rc::new(TermView::new(
                        &self.config.borrow(),
                        shell_argv,
                        working_directory.as_deref(),
                        Some(&sid),
                        initial_commands.as_deref(),
                    ));
                    let terminal = term_view.vte().clone();
                    (TerminalViewType::Block(term_view), terminal)
                }
                crate::config::TerminalMode::Vte => {
                    drop(config);
                    let vte_view = Rc::new(VteTerminalView::new(
                        self.config.clone(),
                        shell_argv,
                        working_directory.as_deref(),
                        Some(&sid),
                        initial_commands.as_deref(),
                    ));
                    let terminal = vte_view.vte().clone();
                    (TerminalViewType::Vte(vte_view), terminal)
                }
            }
        };

        // Setup click handler for hyperlinks and context menu (uses VTE inside both views)
        setup_terminal_click_handler(&terminal);
        self.setup_context_menu(&terminal);

        // Connect callbacks based on view type
        match &view_type {
            TerminalViewType::Block(term_view) => {
                let ui_for_exit = UiState::clone(self);
                let term_view_for_exit = term_view.clone();
                let tab_num_for_exit = tab_num;
                term_view.connect_exited(move |code| {
                    let _ = term_view_for_exit.save_history();
                    ui_for_exit.handle_tab_exit(tab_num_for_exit, code);
                });

                let conns_for_session = self.tab_connections.clone();
                term_view.connect_remote_session_id(move |id| {
                    if let Some(conn) = conns_for_session.borrow_mut().get_mut(&tab_num) {
                        conn.host.session = Some(id.to_string());
                    }
                });
            }
            TerminalViewType::Vte(vte_view) => {
                let ui_for_exit = UiState::clone(self);
                let tab_num_for_exit = tab_num;
                vte_view.connect_exited(move |code| {
                    ui_for_exit.handle_tab_exit(tab_num_for_exit, code);
                });
            }
        }

        // For remote tabs, flip the status badge to green on first output.
        if remote.is_some() {
            let ui_for_conn = self.clone();
            let fired = Rc::new(Cell::new(false));
            terminal.connect_contents_changed(move |_| {
                if fired.get() {
                    return;
                }
                fired.set(true);
                ui_for_conn.mark_tab_connected(tab_num);
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
            TerminalViewType::Block(term_view) => {
                let term_view_for_pwd = term_view.clone();
                term_view_for_pwd.connect_cwd_changed(move |dir| {
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
            }
            TerminalViewType::Vte(vte_view) => {
                let vte_view_for_pwd = vte_view.clone();
                vte_view_for_pwd.connect_cwd_changed(move |dir| {
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

        // Get the widget from the view
        let term_wrapper = match &view_type {
            TerminalViewType::Block(term_view) => {
                let w = term_view.widget();
                w.downcast::<gtk4::Box>()
                    .expect("TermView root must be a Box")
            }
            TerminalViewType::Vte(vte_view) => {
                let w = vte_view.widget();
                w.downcast::<gtk4::Box>()
                    .expect("VteTerminalView root must be a Box")
            }
        };

        // Store the view type on the widget
        let term_wrapper_for_name = term_wrapper.clone();
        term_wrapper_for_name.set_widget_name(&format!("tab-{}", tab_num));
        unsafe {
            term_wrapper.set_data::<TerminalViewType>("terminal-view-type", view_type.clone());
            if let TerminalViewType::Block(term_view) = &view_type {
                term_wrapper.set_data::<Rc<TermView>>("term-view", term_view.clone());
            }
        }

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
        let terminal_for_tooltip = terminal.clone();
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

            // Add working directory
            if let Some(cwd) = terminal_working_directory(&terminal_for_tooltip) {
                tooltip_parts.push(format!("Dir: {}", cwd));
            }

            // Add foreground process name
            if let Some(proc_name) =
                crate::state::get_foreground_process_name(&terminal_for_tooltip)
            {
                tooltip_parts.push(format!("Process: {}", proc_name));
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
        let terminal_for_proc = terminal.clone();
        let process_label_for_update = process_label.clone();
        glib::timeout_add_local(std::time::Duration::from_secs(2), move || {
            // Check if widget is still alive
            if process_label_for_update.parent().is_none() {
                return glib::ControlFlow::Break;
            }

            if let Some(proc_name) = crate::state::get_foreground_process_name(&terminal_for_proc) {
                process_label_for_update.set_text(&proc_name);
                process_label_for_update.set_visible(true);
            } else {
                process_label_for_update.set_visible(false);
            }
            glib::ControlFlow::Continue
        });

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

        // Right-click context menu on tab button
        let right_click_gesture = GestureClick::new();
        right_click_gesture.set_button(3);
        let ui_for_ctx = self.clone();
        let strip_btn_for_ctx = strip_btn.clone();
        let _tab_name_for_ctx = tab_widget_name.clone();
        let term_wrapper_for_ctx = term_wrapper.clone();
        let terminal_for_dup = terminal.clone();
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
                let wd_for_dup = terminal_working_directory(&terminal_for_dup)
                    .or_else(|| std::env::var("HOME").ok());
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    ui_duplicate_ctx.add_new_tab(wd_for_dup.clone(), None, None, None);
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
                let term_wrapper_pin = term_wrapper_for_ctx.clone();
                let pin_icon_pin = pin_icon.clone();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    if strip_btn_pin.has_css_class("tab-pinned") {
                        strip_btn_pin.remove_css_class("tab-pinned");
                        pin_icon_pin.set_visible(false);
                        unsafe {
                            strip_btn_pin.set_data::<bool>("pinned", false);
                        }
                        unsafe {
                            term_wrapper_pin.set_data::<bool>("pinned", false);
                        }
                    } else {
                        strip_btn_pin.add_css_class("tab-pinned");
                        pin_icon_pin.set_visible(true);
                        unsafe {
                            strip_btn_pin.set_data::<bool>("pinned", true);
                        }
                        unsafe {
                            term_wrapper_pin.set_data::<bool>("pinned", true);
                        }
                    }
                });
                vbox.append(&item);
            }

            // Close
            {
                let item = make_item("Close");
                let popover_c = popover.clone();
                let ui_close_ctx = ui_for_ctx.clone();
                let wrapper_for_ctx_close = term_wrapper_for_ctx.clone().upcast::<gtk4::Widget>();
                item.connect_clicked(move |_| {
                    popover_c.popdown();
                    ui_close_ctx.remove_tab_by_widget(&wrapper_for_ctx_close);
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
            TerminalViewType::Block(term_view) => term_view.grab_focus(),
            TerminalViewType::Vte(vte_view) => vte_view.grab_focus(),
        }

        terminal
    }
}
