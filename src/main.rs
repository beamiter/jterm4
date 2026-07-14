use adw::prelude::*;
use gtk4::gdk::Key;
use gtk4::gdk::ModifierType;
use gtk4::gio::{self, Cancellable};
use gtk4::{
    glib, CssProvider, EventControllerKey, EventControllerScroll, EventControllerScrollFlags,
    Notebook, Orientation, ScrolledWindow, SearchBar, SearchEntry,
};
use libadwaita as adw;
use log::{LevelFilter, Log, Metadata, Record};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::config::{choose_shell_argv, config_file_path, load_config};
use crate::keybindings::{normalize_key, Action, KeyCombo};
use crate::state::{
    finalize_tabs_state, kill_all_terminal_children, load_tabs_state, save_tabs_state,
};
use crate::terminal::terminal_working_directory;
use crate::ui::{self, UiState};

struct SimpleStderrLogger {
    level: LevelFilter,
}

impl Log for SimpleStderrLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            eprintln!("[{}] {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

fn parse_level_filter(input: &str) -> LevelFilter {
    match input.trim().to_ascii_lowercase().as_str() {
        "off" => LevelFilter::Off,
        "error" => LevelFilter::Error,
        "warn" | "warning" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => LevelFilter::Warn,
    }
}

fn init_logging() {
    let level = std::env::var("JTERM4_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .as_deref()
        .map(parse_level_filter)
        .unwrap_or(LevelFilter::Warn);

    let _ = log::set_boxed_logger(Box::new(SimpleStderrLogger { level }));
    log::set_max_level(level);
}

fn env_is_unset(k: &str) -> bool {
    std::env::var_os(k).is_none_or(|v| v.is_empty())
}

fn gtk_path_has_fcitx_module(path: &Path) -> bool {
    path.join("4.0.0/immodules/im-fcitx5.so").exists()
        || path.join("4.0.0/immodules/im-fcitx.so").exists()
        || path.join("4.0.0/immodules/libim-fcitx5.so").exists()
        || path.join("4.0.0/immodules/libim-fcitx.so").exists()
}

fn gtk_path_has_ibus_module(path: &Path) -> bool {
    path.join("4.0.0/immodules/im-ibus.so").exists()
        || path.join("4.0.0/immodules/libim-ibus.so").exists()
}

fn gtk_path_has_xim_module(path: &Path) -> bool {
    path.join("4.0.0/immodules/im-xim.so").exists()
        || path.join("4.0.0/immodules/libim-xim.so").exists()
}

fn candidate_fcitx_gtk_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(path) = option_env!("FCITX5_GTK_PATH").filter(|p| !p.is_empty()) {
        paths.push(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("FCITX5_GTK_PATH").filter(|p| !p.is_empty()) {
        paths.push(PathBuf::from(path));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|p| !p.is_empty()) {
        paths.push(PathBuf::from(home).join(".nix-profile/lib/gtk-4.0"));
    }

    paths.extend(
        [
            "/run/current-system/sw/lib/gtk-4.0",
            "/usr/lib/gtk-4.0",
            "/usr/lib64/gtk-4.0",
            "/usr/lib/x86_64-linux-gnu/gtk-4.0",
            "/usr/local/lib/gtk-4.0",
        ]
        .into_iter()
        .map(PathBuf::from),
    );

    paths
}

fn prepend_gtk_path_if_missing(path: &Path) {
    let existing = std::env::var_os("GTK_PATH").unwrap_or_default();
    let already_present = std::env::split_paths(&existing).any(|p| p == path);
    if already_present {
        return;
    }

    let mut paths = vec![path.to_path_buf()];
    paths.extend(std::env::split_paths(&existing));
    match std::env::join_paths(paths) {
        Ok(combined) => unsafe { std::env::set_var("GTK_PATH", combined) },
        Err(err) => log::warn!("Failed to build GTK_PATH for input method: {err}"),
    }
}

fn should_use_xim_for_fcitx4(fcitx_gtk_path_found: bool, xim_gtk_path_found: bool) -> bool {
    !fcitx_gtk_path_found
        && xim_gtk_path_found
        && std::env::var("XMODIFIERS")
            .map(|s| s.contains("fcitx"))
            .unwrap_or(false)
        && !std::env::var_os("DISPLAY").is_none_or(|v| v.is_empty())
}

/// Make the GTK4 input-method module discoverable before GTK initializes, so
/// CJK preedit/commit works even when the binary is launched outside the nix
/// dev shell.
fn init_input_method_env() {
    let candidates = candidate_fcitx_gtk_paths();
    let fcitx_gtk_path = candidates.iter().find(|p| gtk_path_has_fcitx_module(p));
    let ibus_gtk_path = candidates.iter().find(|p| gtk_path_has_ibus_module(p));
    let xim_gtk_path = candidates.iter().find(|p| gtk_path_has_xim_module(p));

    if let Some(path) = fcitx_gtk_path {
        prepend_gtk_path_if_missing(path);
        log::debug!("Using fcitx GTK4 input module path {}", path.display());
    } else if let Some(path) = ibus_gtk_path {
        prepend_gtk_path_if_missing(path);
        log::debug!("Using ibus GTK4 input module path {}", path.display());
    } else if let Some(path) = xim_gtk_path {
        prepend_gtk_path_if_missing(path);
        log::debug!("Using xim GTK4 input module path {}", path.display());
    }

    let use_xim_for_fcitx4 =
        should_use_xim_for_fcitx4(fcitx_gtk_path.is_some(), xim_gtk_path.is_some());
    let gtk_im_module = std::env::var("GTK_IM_MODULE").unwrap_or_default();
    if gtk_im_module == "fcitx" && use_xim_for_fcitx4 {
        unsafe { std::env::set_var("GTK_IM_MODULE", "xim") };
        log::warn!(
            "GTK_IM_MODULE=fcitx but no GTK4 fcitx module was found; using xim via XMODIFIERS for fcitx4"
        );
    } else if gtk_im_module == "fcitx" && fcitx_gtk_path.is_none() {
        log::warn!(
            "GTK_IM_MODULE=fcitx but no GTK4 fcitx module was found. fcitx4 needs a GTK4 fcitx/xim module; install fcitx5-gtk or use ibus for GTK4 apps."
        );
    } else if env_is_unset("GTK_IM_MODULE") {
        let xmods = std::env::var("XMODIFIERS").unwrap_or_default();
        let module = if use_xim_for_fcitx4 {
            "xim"
        } else if xmods.contains("ibus")
            || (!std::env::var_os("IBUS_ADDRESS").is_none_or(|v| v.is_empty())
                && fcitx_gtk_path.is_none())
            || (fcitx_gtk_path.is_none() && ibus_gtk_path.is_some())
        {
            "ibus"
        } else {
            "fcitx"
        };
        unsafe { std::env::set_var("GTK_IM_MODULE", module) };
    }

    if env_is_unset("XMODIFIERS") {
        let module = std::env::var("GTK_IM_MODULE").unwrap_or_else(|_| "fcitx".to_string());
        if matches!(module.as_str(), "fcitx" | "ibus") {
            unsafe { std::env::set_var("XMODIFIERS", format!("@im={module}")) };
        }
    }
}

pub fn run() -> glib::ExitCode {
    if let Some(code) = crate::cli::handle_early_args() {
        return code;
    }
    init_logging();
    init_input_method_env();

    // NON_UNIQUE: each launch is its own process with its own window, instead of
    // the second invocation activating the first instance and then exiting.
    let app = adw::Application::builder()
        .application_id("app.jterm4")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_activate(|app| {
        let (config, themes, keybinding_map) = load_config();

        // Cache shell selection once to avoid extra process probes per new tab.
        let shell_argv = Rc::new(RefCell::new(choose_shell_argv(config.shell.as_deref())));

        let window_opacity = Rc::new(Cell::new(config.window_opacity));
        let config = Rc::new(RefCell::new(config));
        let available_themes = Rc::new(themes);
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .default_width(800)
            .default_height(600)
            .title("jterm4")
            .name("win_name")
            .opacity(window_opacity.get())
            .build();

        // Create notebook for tabs (tabs hidden — custom tab bar is used instead)
        let notebook = Notebook::builder()
            .hexpand(true)
            .vexpand(true)
            .scrollable(true)
            .show_border(false)
            .show_tabs(false)
            .build();
        notebook.add_css_class("hidden-tabs");

        // Create search bar
        let search_entry = SearchEntry::new();
        search_entry.set_hexpand(true);

        let search_prev_btn = gtk4::Button::from_icon_name("go-up-symbolic");
        search_prev_btn.set_tooltip_text(Some("Previous match (Shift+Enter)"));
        search_prev_btn.set_focus_on_click(false);
        let search_next_btn = gtk4::Button::from_icon_name("go-down-symbolic");
        search_next_btn.set_tooltip_text(Some("Next match (Enter)"));
        search_next_btn.set_focus_on_click(false);
        let search_close_btn = gtk4::Button::from_icon_name("window-close-symbolic");
        search_close_btn.set_tooltip_text(Some("Close search (Escape)"));
        search_close_btn.set_focus_on_click(false);

        let search_box = gtk4::Box::new(Orientation::Horizontal, 4);
        search_box.append(&search_entry);
        search_box.append(&search_prev_btn);
        search_box.append(&search_next_btn);
        search_box.append(&search_close_btn);
        search_box.set_margin_start(4);
        search_box.set_margin_end(4);
        search_box.set_margin_top(2);
        search_box.set_margin_bottom(2);

        let search_bar = SearchBar::new();
        search_bar.set_child(Some(&search_box));
        search_bar.set_show_close_button(false);
        search_bar.connect_entry(&search_entry);

        // Custom tab bar CSS
        let css_provider = CssProvider::new();
        css_provider.load_from_string(
            ".tab-strip-btn { padding: 4px 8px; border-radius: 4px; border-bottom: 1px solid alpha(currentColor, 0.1); margin-bottom: 2px; }
             .tab-strip-btn:checked { font-weight: bold; border-radius: 4px; background-color: alpha(currentColor, 0.14); outline: 2px solid alpha(currentColor, 0.8); outline-offset: -2px; }
             .tab-strip-close { min-width: 16px; min-height: 16px; padding: 0; margin: 0; }
             .sidebar-box { min-width: 140px; padding: 2px 4px; }
             .top-bar { padding: 2px 4px; }
             .hidden-tabs > header { min-height: 0; border: none; background: none; padding: 0; margin: 0; }
             .hidden-tabs > header > * { min-height: 0; min-width: 0; padding: 0; margin: 0; }
             .terminal-box scrollbar slider { min-width: 6px; border-radius: 3px; }
             .terminal-box scrollbar { padding: 0; }
             .tab-activity { font-style: italic; }
             .tab-bell { color: #f1fa8c; }
             @keyframes bell-flash { 0% { opacity: 1.0; } 50% { opacity: 0.5; } 100% { opacity: 1.0; } }
             .tab-bell-flash { animation: bell-flash 0.3s ease-in-out 2; }
             .tab-pinned { font-weight: bold; }
             .tab-dragging { opacity: 0.5; }
             .tab-drop-target { background-color: alpha(currentColor, 0.15); }
             .tab-process-indicator { font-size: 0.8em; opacity: 0.6; margin-left: 4px; }
             .tab-pin-icon { font-size: 0.9em; opacity: 0.8; margin-right: 2px; color: #ffb86c; }
             .tab-selected { background-color: alpha(currentColor, 0.14); outline: 2px solid alpha(currentColor, 0.8); outline-offset: -2px; }
             .tab-conn-dot { font-size: 0.7em; margin-right: 2px; }
             @keyframes conn-pulse { 0% { opacity: 1.0; } 50% { opacity: 0.35; } 100% { opacity: 1.0; } }
             .tab-conn-dot.tab-connecting { color: #f1fa8c; animation: conn-pulse 1.2s ease-in-out infinite; }
             .tab-conn-dot.tab-connected { color: #50fa7b; }
             .tab-conn-dot.tab-disconnected { color: #ff5555; }
             .tab-strip-search { padding: 4px 8px; margin: 2px 4px; }
             .top-tabs .tab-strip-btn { border-bottom: none; margin-bottom: 0; margin-right: 2px; }
             .file-tree-box { border-top: 1px solid alpha(currentColor, 0.15); }
             .file-tree-header { padding: 2px 4px; }
             .file-tree-root { font-size: 0.85em; opacity: 0.7; }
             .file-tree { padding: 2px; }",
        );
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().expect("display"),
            &css_provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        // Top bar: [☰ toggle] [spacer] [+ new tab] [✕ close window]
        let toggle_sidebar_btn = gtk4::Button::from_icon_name("open-menu-symbolic");
        toggle_sidebar_btn.set_focus_on_click(false);
        toggle_sidebar_btn.set_can_focus(false);
        toggle_sidebar_btn.set_tooltip_text(Some("Toggle sidebar (Ctrl+\\)"));
        toggle_sidebar_btn.add_css_class("flat");

        let add_tab_button = gtk4::Button::from_icon_name("list-add-symbolic");
        add_tab_button.set_focus_on_click(false);
        add_tab_button.set_can_focus(false);
        add_tab_button.set_tooltip_text(Some("New tab (Ctrl+Shift+T)"));
        add_tab_button.add_css_class("flat");

        let close_window_button = gtk4::Button::from_icon_name("window-close-symbolic");
        close_window_button.set_focus_on_click(false);
        close_window_button.set_can_focus(false);
        close_window_button.set_tooltip_text(Some("Close window"));
        close_window_button.add_css_class("flat");

        // Toggles the tab bar between the left sidebar and the top bar.
        let toggle_placement_btn = gtk4::Button::from_icon_name("view-list-symbolic");
        toggle_placement_btn.set_focus_on_click(false);
        toggle_placement_btn.set_can_focus(false);
        toggle_placement_btn.set_tooltip_text(Some("Toggle tabs: sidebar / top bar"));
        toggle_placement_btn.add_css_class("flat");

        // Holder for the tab strip when it lives in the top bar (horizontal).
        let top_tab_scroll = ScrolledWindow::new();
        top_tab_scroll.set_hexpand(true);
        top_tab_scroll.set_vexpand(false);
        top_tab_scroll.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Never);
        top_tab_scroll.add_css_class("top-tab-scroll");
        top_tab_scroll.set_visible(false);

        let top_bar = gtk4::Box::new(Orientation::Horizontal, 4);
        top_bar.add_css_class("top-bar");
        top_bar.append(&toggle_sidebar_btn);
        top_bar.append(&toggle_placement_btn);
        top_bar.append(&top_tab_scroll);
        // Spacer pushes + and ✕ to the right (disabled when tabs fill the top bar)
        let spacer = gtk4::Box::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        top_bar.append(&spacer);
        top_bar.append(&add_tab_button);
        top_bar.append(&close_window_button);

        // Vertical sidebar with tab buttons (collapsible)
        let tab_strip = gtk4::Box::new(Orientation::Vertical, 2);
        tab_strip.set_hexpand(false);
        tab_strip.set_vexpand(true);
        tab_strip.set_valign(gtk4::Align::Start);

        let tab_strip_scroll = ScrolledWindow::new();
        tab_strip_scroll.set_hexpand(false);
        tab_strip_scroll.set_vexpand(true);
        tab_strip_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        tab_strip_scroll.set_child(Some(&tab_strip));

        let sidebar = gtk4::Box::new(Orientation::Vertical, 0);
        sidebar.add_css_class("sidebar-box");

        // Tab search entry for filtering.
        // Non-focusable by default to prevent GTK's automatic focus navigation
        // from landing here when alt-screen VTE is hidden. Enabled on mouse click
        // or via the FilterTabs keybinding.
        let tab_search_entry = SearchEntry::new();
        tab_search_entry.set_placeholder_text(Some("Filter tabs..."));
        tab_search_entry.add_css_class("tab-strip-search");
        tab_search_entry.set_can_focus(false);
        tab_search_entry.set_focusable(false);
        // Wrap in a clickable box so clicks on the text area are captured even
        // when the entry itself is non-focusable.
        let tab_search_wrapper = gtk4::Box::new(Orientation::Horizontal, 0);
        tab_search_wrapper.append(&tab_search_entry);
        {
            let entry_for_click = tab_search_entry.clone();
            let click_ctrl = gtk4::GestureClick::new();
            click_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
            click_ctrl.connect_pressed(move |gesture, _, _, _| {
                entry_for_click.set_can_focus(true);
                entry_for_click.set_focusable(true);
                entry_for_click.grab_focus();
                gesture.set_state(gtk4::EventSequenceState::Claimed);
            });
            tab_search_wrapper.add_controller(click_ctrl);
        }

        // Tabs view: filter entry + tab strip.
        let sidebar_tabs_page = gtk4::Box::new(Orientation::Vertical, 0);
        sidebar_tabs_page.set_vexpand(true);
        sidebar_tabs_page.append(&tab_search_wrapper);
        sidebar_tabs_page.append(&tab_strip_scroll);

        // File tree section (header + tree), shown in the sidebar.
        let (file_tree_model, file_tree) = ui::build_file_tree_widgets();

        let file_tree_scroll = ScrolledWindow::new();
        file_tree_scroll.set_hexpand(false);
        file_tree_scroll.set_vexpand(true);
        file_tree_scroll.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Automatic);
        file_tree_scroll.set_child(Some(&file_tree));

        let file_tree_root_label = gtk4::Label::new(Some("~"));
        file_tree_root_label.set_hexpand(true);
        file_tree_root_label.set_xalign(0.0);
        file_tree_root_label.set_ellipsize(gtk4::pango::EllipsizeMode::Start);
        file_tree_root_label.add_css_class("file-tree-root");

        let file_tree_cwd_btn = gtk4::Button::from_icon_name("go-home-symbolic");
        file_tree_cwd_btn.set_focus_on_click(false);
        file_tree_cwd_btn.set_can_focus(false);
        file_tree_cwd_btn.set_tooltip_text(Some("Jump to current tab directory"));
        file_tree_cwd_btn.add_css_class("flat");

        let file_tree_up_btn = gtk4::Button::from_icon_name("go-up-symbolic");
        file_tree_up_btn.set_focus_on_click(false);
        file_tree_up_btn.set_can_focus(false);
        file_tree_up_btn.set_tooltip_text(Some("Go to parent directory"));
        file_tree_up_btn.add_css_class("flat");

        let file_tree_header = gtk4::Box::new(Orientation::Horizontal, 2);
        file_tree_header.add_css_class("file-tree-header");
        file_tree_header.append(&file_tree_root_label);
        file_tree_header.append(&file_tree_up_btn);
        file_tree_header.append(&file_tree_cwd_btn);

        let file_tree_box = gtk4::Box::new(Orientation::Vertical, 0);
        file_tree_box.add_css_class("file-tree-box");
        file_tree_box.set_vexpand(true);
        file_tree_box.append(&file_tree_header);
        file_tree_box.append(&file_tree_scroll);

        // Segmented switcher at the top of the sidebar: Tabs | Files.
        let sidebar_tabs_btn = gtk4::ToggleButton::with_label("Tabs");
        sidebar_tabs_btn.set_focus_on_click(false);
        sidebar_tabs_btn.set_can_focus(false);
        sidebar_tabs_btn.set_hexpand(true);
        sidebar_tabs_btn.set_active(true);
        let sidebar_files_btn = gtk4::ToggleButton::with_label("Files");
        sidebar_files_btn.set_focus_on_click(false);
        sidebar_files_btn.set_can_focus(false);
        sidebar_files_btn.set_hexpand(true);
        let sidebar_switcher = gtk4::Box::new(Orientation::Horizontal, 0);
        sidebar_switcher.add_css_class("linked");
        sidebar_switcher.add_css_class("sidebar-switcher");
        sidebar_switcher.append(&sidebar_tabs_btn);
        sidebar_switcher.append(&sidebar_files_btn);

        // Stack shows exactly one sidebar view at a time.
        let sidebar_stack = gtk4::Stack::new();
        sidebar_stack.set_vexpand(true);
        sidebar_stack.add_named(&sidebar_tabs_page, Some("tabs"));
        sidebar_stack.add_named(&file_tree_box, Some("files"));

        sidebar.append(&sidebar_switcher);
        sidebar.append(&sidebar_stack);

        // Content area: resizable sidebar | notebook (draggable divider).
        let right_col = gtk4::Box::new(Orientation::Vertical, 0);
        right_col.set_hexpand(true);
        right_col.set_vexpand(true);
        right_col.append(&notebook);
        right_col.append(&search_bar);

        // AI sidebar: wraps `right_col` in another horizontal Paned so the
        // chat panel can dock on the right edge without disturbing the
        // existing sidebar / notebook layout. Built always; visibility is
        // controlled by adding/removing it as `ai_paned`'s end_child.
        let ai_panel_widget = ui::AiPanel::build(config.clone());
        let ai_paned = gtk4::Paned::new(Orientation::Horizontal);
        ai_paned.set_vexpand(true);
        ai_paned.set_wide_handle(true);
        ai_paned.set_start_child(Some(&right_col));
        ai_paned.set_resize_start_child(true);
        ai_paned.set_resize_end_child(false);
        ai_paned.set_shrink_start_child(true);
        ai_paned.set_shrink_end_child(false);
        let ai_initially_visible = config.borrow().ai_panel_visible;
        if ai_initially_visible {
            ai_paned.set_end_child(Some(&ai_panel_widget.root));
        }

        let content_box = gtk4::Paned::new(Orientation::Horizontal);
        content_box.set_vexpand(true);
        content_box.set_wide_handle(true);
        content_box.set_start_child(Some(&sidebar));
        content_box.set_end_child(Some(&ai_paned));
        content_box.set_resize_start_child(false);
        content_box.set_resize_end_child(true);
        content_box.set_shrink_start_child(false);
        content_box.set_shrink_end_child(true);
        content_box.set_position(config.borrow().sidebar_width as i32);

        // Main layout: top_bar + content_box (vertical)
        let main_box = gtk4::Box::new(Orientation::Vertical, 0);
        main_box.append(&top_bar);
        main_box.append(&content_box);

        // Shared state
        let font_scale = Rc::new(Cell::new(config.borrow().default_font_scale));
        let tab_counter = Rc::new(Cell::new(0));

        let ui = Rc::new(UiState {
            window: window.clone(),
            notebook: notebook.clone(),
            tab_counter: tab_counter.clone(),
            font_scale: font_scale.clone(),
            window_opacity: window_opacity.clone(),
            shell_argv: shell_argv.clone(),
            config: config.clone(),
            available_themes: available_themes.clone(),
            search_bar: search_bar.clone(),
            search_entry: search_entry.clone(),
            tab_strip: tab_strip.clone(),
            sidebar: sidebar.clone(),
            top_spacer: spacer.clone(),
            tab_strip_scroll: tab_strip_scroll.clone(),
            top_tab_scroll: top_tab_scroll.clone(),
            tab_placement: Rc::new(Cell::new(config.borrow().tab_placement)),
            sidebar_stack: sidebar_stack.clone(),
            sidebar_tabs_btn: sidebar_tabs_btn.clone(),
            sidebar_files_btn: sidebar_files_btn.clone(),
            sidebar_view: Rc::new(Cell::new(config.borrow().sidebar_view)),
            file_tree_model: file_tree_model.clone(),
            file_tree_root: Rc::new(RefCell::new(std::path::PathBuf::new())),
            file_tree_root_label: file_tree_root_label.clone(),
            tab_search_entry: tab_search_entry.clone(),
            selected_tabs: Rc::new(RefCell::new(Vec::new())),
            command_palette_dialog: Rc::new(RefCell::new(None)),
            remote_picker_dialog: Rc::new(RefCell::new(None)),
            history_palette_dialog: Rc::new(RefCell::new(None)),
            cross_block_search_dialog: Rc::new(RefCell::new(None)),
            workflows_palette_dialog: Rc::new(RefCell::new(None)),
            settings_dialog: Rc::new(RefCell::new(None)),
            debug_dashboard_dialog: Rc::new(RefCell::new(None)),
            keybinding_map: Rc::new(RefCell::new(keybinding_map)),
            zoom_state: Rc::new(RefCell::new(None)),
            scrollbar_css: CssProvider::new(),
            session_ids: Rc::new(RefCell::new(HashMap::new())),
            tab_connections: Rc::new(RefCell::new(HashMap::new())),
            ai_panel: ai_panel_widget.clone(),
            ai_paned: ai_paned.clone(),
            ai_panel_visible: Rc::new(Cell::new(ai_initially_visible)),
        });

        // Register the dynamic scrollbar CSS provider and apply initial colors
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().expect("display"),
            &ui.scrollbar_css,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );
        ui.apply_dynamic_css();

        // Wire toggle sidebar button
        let ui_for_toggle = ui.clone();
        toggle_sidebar_btn.connect_clicked(move |_| {
            ui_for_toggle.toggle_sidebar();
        });

        // Wire tab-placement toggle (sidebar <-> top bar)
        let ui_for_placement = ui.clone();
        toggle_placement_btn.connect_clicked(move |_| {
            ui_for_placement.toggle_tab_placement();
        });

        // Wire file-tree header buttons
        let ui_for_ft_cwd = ui.clone();
        file_tree_cwd_btn.connect_clicked(move |_| {
            ui_for_ft_cwd.file_tree_goto_current_cwd();
        });
        let ui_for_ft_up = ui.clone();
        file_tree_up_btn.connect_clicked(move |_| {
            ui_for_ft_up.file_tree_go_up();
        });

        // Wire file-tree expansion and file activation.
        ui.connect_file_tree_handlers(&file_tree);

        // Wire sidebar Tabs/Files segmented switcher
        let ui_for_tabs_view = ui.clone();
        sidebar_tabs_btn.connect_clicked(move |_| {
            ui_for_tabs_view.apply_sidebar_view(crate::config::SidebarView::Tabs, true);
        });
        let ui_for_files_view = ui.clone();
        sidebar_files_btn.connect_clicked(move |_| {
            ui_for_files_view.apply_sidebar_view(crate::config::SidebarView::Files, true);
        });

        // Initialize the file tree and apply the persisted tab placement
        // (which also restores the persisted sidebar view).
        ui.init_file_tree();
        ui.apply_tab_placement();

        // Wire close-window button
        let window_for_close = window.clone();
        close_window_button.connect_clicked(move |_| {
            window_for_close.close();
        });

        // Wire "+" button — inherit working directory from current session
        let ui_for_add = ui.clone();
        add_tab_button.connect_clicked(move |_| {
            let working_directory = ui_for_add
                .current_terminal()
                .as_ref()
                .and_then(terminal_working_directory);
            let startup = ui_for_add.config.borrow().startup_commands.clone();
            ui_for_add.add_new_tab(working_directory, None, None, startup);
        });

        // Atomically claim one ready window snapshot. Other running instances
        // keep separate active files, so concurrent windows cannot overwrite or
        // restore one another's state.
        let (saved_current, saved_tabs) = load_tabs_state();
        if saved_tabs.is_empty() {
            let startup = ui.config.borrow().startup_commands.clone();
            ui.add_new_tab(None, None, None, startup);
        } else {
            for (name, layout) in saved_tabs {
                ui.restore_pane_layout(layout, name);
            }

            if let Some(page) = saved_current {
                let n_pages = notebook.n_pages();
                if n_pages > 0 {
                    notebook.set_current_page(Some(page.min(n_pages.saturating_sub(1))));
                }
            }
        }

        // Auto-save tabs state when tabs are added or removed.
        // Use idle_add to defer saving until after the page is fully initialized.
        let session_ids_for_page_added = ui.session_ids.clone();
        let notebook_clone_for_added = notebook.clone();
        notebook.connect_page_added(move |_notebook, _child, _page_num| {
            let nb = notebook_clone_for_added.clone();
            let sids = session_ids_for_page_added.clone();
            glib::idle_add_local_once(move || {
                save_tabs_state(&nb, &sids.borrow());
            });
        });

        let session_ids_for_page_removed = ui.session_ids.clone();
        let notebook_clone_for_removed = notebook.clone();
        notebook.connect_page_removed(move |_notebook, _child, _page_num| {
            let nb = notebook_clone_for_removed.clone();
            let sids = session_ids_for_page_removed.clone();
            glib::idle_add_local_once(move || {
                save_tabs_state(&nb, &sids.borrow());
            });
        });

        // Save initial state after tabs are restored
        save_tabs_state(&notebook, &ui.session_ids.borrow());

        // Setup key controller on window level with Capture phase
        // This allows us to intercept shortcuts before the terminal processes them
        let key_controller = EventControllerKey::new();
        key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);

        let ui_clone = ui.clone();

        key_controller.connect_key_pressed(move |_controller, keyval, _keycode, state| {
            // Mask to only the modifier keys we care about
            let mods = state & (ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK | ModifierType::ALT_MASK);
            let combo = KeyCombo {
                modifiers: mods,
                key: normalize_key(keyval),
            };

            let action = {
                let bindings = ui_clone.keybinding_map.borrow();
                bindings.lookup(&combo).or_else(|| {
                    // Alt modifies Copy into "copy block output". The binding
                    // map is intentionally exact, so retry only this one
                    // documented variant without Alt instead of making every
                    // shortcut accidentally accept extra modifiers.
                    if mods.contains(
                        ModifierType::CONTROL_MASK
                            | ModifierType::SHIFT_MASK
                            | ModifierType::ALT_MASK,
                    ) {
                        let copy_combo = KeyCombo {
                            modifiers: mods & !ModifierType::ALT_MASK,
                            key: normalize_key(keyval),
                        };
                        (bindings.lookup(&copy_combo) == Some(Action::Copy))
                            .then_some(Action::Copy)
                    } else {
                        None
                    }
                })
            };

            if let Some(action) = action {
                match action {
                    Action::Copy => {
                        // Handle at the window level so the shortcut works no
                        // matter which child has focus — in particular, after
                        // mouse-selecting text inside a finished block,
                        // focus lives on that TextView and the per-VTE
                        // block-mode handler never fires.
                        // copy_to_clipboard handles Warp block-selection,
                        // VTE selection, TextBuffer selection, and PRIMARY
                        // fallback in priority order. Pass Alt for the
                        // CopyBlockOutput variant.
                        if let Some(term_view) = ui_clone.current_term_view() {
                            let alt_held = state.contains(ModifierType::ALT_MASK);
                            term_view.copy_to_clipboard_with_modifier(alt_held);
                            return true.into();
                        }
                        ui_clone.execute_action(action);
                        return true.into();
                    }
                    Action::Paste => {
                        ui_clone.execute_action(action);
                        return true.into();
                    }
                    _ => {
                        ui_clone.execute_action(action);
                        return true.into();
                    }
                }
            }

            false.into()
        });

        // Enter/Shift+Enter are handled by the capture-phase key controller
        // below (next/prev); incremental highlighting runs on search_changed.
        let ui_for_search_changed = ui.clone();
        search_entry.connect_search_changed(move |_| {
            ui_for_search_changed.search_apply();
        });

        let ui_for_search_next = ui.clone();
        search_next_btn.connect_clicked(move |_| {
            ui_for_search_next.search_next();
        });

        let ui_for_search_prev = ui.clone();
        search_prev_btn.connect_clicked(move |_| {
            ui_for_search_prev.search_prev();
        });

        let ui_for_search_close = ui.clone();
        search_close_btn.connect_clicked(move |_| {
            ui_for_search_close.toggle_search();
        });

        // Search entry key handler for Enter (next), Shift+Enter (prev), Escape.
        // Capture phase so Enter is consumed before it can reach the live VTE
        // (otherwise it submits a stray empty command to the shell).
        let search_key_controller = EventControllerKey::new();
        search_key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let ui_for_search_key = ui.clone();
        search_key_controller.connect_key_pressed(move |_, keyval, _, state| {
            match keyval {
                Key::Return | Key::KP_Enter => {
                    if state.contains(ModifierType::SHIFT_MASK) {
                        ui_for_search_key.search_prev();
                    } else {
                        ui_for_search_key.search_next();
                    }
                    return true.into();
                }
                Key::Escape => {
                    ui_for_search_key.toggle_search();
                    return true.into();
                }
                _ => {}
            }
            false.into()
        });
        search_entry.add_controller(search_key_controller);

        // Disable tab_search_entry focusability when focus leaves it
        {
            let entry_for_focus = tab_search_entry.clone();
            let focus_ctrl = gtk4::EventControllerFocus::new();
            focus_ctrl.connect_leave(move |_| {
                entry_for_focus.set_can_focus(false);
                entry_for_focus.set_focusable(false);
            });
            tab_search_entry.add_controller(focus_ctrl);
        }

        // Wire tab search entry: filter tabs by name
        let ui_for_tab_search = ui.clone();
        tab_search_entry.connect_search_changed(move |entry| {
            let query = entry.text().to_string().to_lowercase();
            let tab_strip = &ui_for_tab_search.tab_strip;

            let mut child = tab_strip.first_child();
            while let Some(ref c) = child {
                if let Ok(btn) = c.clone().downcast::<gtk4::ToggleButton>() {
                    let title = unsafe {
                        btn.data::<gtk4::Label>("tab-title-label")
                            .map(|label| label.as_ref().text().to_string())
                    }
                    // Compatibility fallback for old/restored strip buttons
                    // that predate the explicit title-label data.
                    .or_else(|| {
                        btn.child()
                            .and_then(|child| child.downcast::<gtk4::Label>().ok())
                            .map(|label| label.text().to_string())
                    })
                    .unwrap_or_default()
                    .to_lowercase();
                    c.set_visible(query.is_empty() || title.contains(query.as_str()));
                }
                child = c.next_sibling();
            }
        });

        // Focus terminal when switching tabs (split-aware) and sync tab strip
        let ui_for_switch = ui.clone();
        let notebook_for_switch = notebook.clone();
        notebook.connect_switch_page(move |_, widget, page_num| {
            if ui_for_switch.search_bar.is_search_mode() {
                ui_for_switch.search_apply();
                ui_for_switch.search_entry.grab_focus();
            } else {
                ui_for_switch.focus_terminal_in_page(widget);
            }
            // `switch-page` runs before GTK has completed map/allocation and a
            // held Ctrl+PageUp can queue several switches in one event burst.
            // Reclaim focus on the next frame, but only if this is still the
            // selected page; otherwise an older tab's deferred callback can
            // steal focus from the final tab in the cycle.
            let notebook_for_focus = notebook_for_switch.clone();
            let target_widget = widget.clone();
            let ui_for_focus = ui_for_switch.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(16), move || {
                if notebook_for_focus.current_page() == Some(page_num) {
                    if ui_for_focus.search_bar.is_search_mode() {
                        ui_for_focus.search_apply();
                        ui_for_focus.search_entry.grab_focus();
                    } else {
                        ui_for_focus.focus_terminal_in_page(&target_widget);
                    }
                }
            });
            // Clear activity/bell indicators for the tab being switched to
            let tab_name = widget.widget_name();
            ui_for_switch.clear_tab_indicators(tab_name.as_str());
            ui_for_switch.sync_tab_strip_active(Some(page_num));
            // File tree root follows the active tab's working directory.
            let ui_ft = ui_for_switch.clone();
            glib::idle_add_local_once(move || {
                ui_ft.file_tree_goto_current_cwd();
            });
        });

        window.add_controller(key_controller);

        // Ctrl+scroll to zoom font
        let scroll_controller = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
        scroll_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let ui_for_scroll = ui.clone();
        scroll_controller.connect_scroll(move |controller, _dx, dy| {
            let state = controller.current_event_state();
            if state.contains(ModifierType::CONTROL_MASK) {
                let font_step = 0.025;
                let current = ui_for_scroll.font_scale.get();
                let new_scale = if dy < 0.0 {
                    (current + font_step).min(10.0)
                } else {
                    (current - font_step).max(0.1)
                };
                ui_for_scroll.set_font_scale_all(new_scale);
                return true.into();
            }
            false.into()
        });
        window.add_controller(scroll_controller);

        // Save state *before* GTK starts destroying widgets.
        let notebook_for_close_request = notebook.clone();
        let session_ids_for_close = ui.session_ids.clone();
        let app_for_close = app.clone();
        let config_for_close = ui.config.clone();
        let paned_for_close = content_box.clone();
        window.connect_close_request(move |_| {
            // Persist the current sidebar width before teardown.
            let width = paned_for_close.position().max(120) as u32;
            config_for_close.borrow_mut().sidebar_width = width;
            crate::config::save_config(&config_for_close.borrow());

            save_tabs_state(&notebook_for_close_request, &session_ids_for_close.borrow());
            kill_all_terminal_children(&notebook_for_close_request);

            // Explicitly clear all pages to break reference cycles and allow TermView cleanup.
            // This ensures OwnedPty drops, closing PTY master FD and signaling reader threads.
            while notebook_for_close_request.n_pages() > 0 {
                notebook_for_close_request.remove_page(Some(0));
            }
            // Make the final snapshot visible only after this window is fully
            // quiesced. Any queued auto-save callbacks become no-ops.
            finalize_tabs_state();

            // Directly quit the application
            app_for_close.quit();

            false.into()
        });

        let app_clone = app.clone();
        window.connect_destroy(move |_| {
            app_clone.quit();
        });

        window.set_content(Some(&main_box));
        window.present();

        // Focus the active terminal after window is shown
        ui.focus_current_terminal();

        // Config file hot reload: watch config.toml for external changes
        let config_path = config_file_path();
        if let Some(parent_dir) = config_path.parent() {
            // Ensure config dir exists for the monitor
            let _ = fs::create_dir_all(parent_dir);
        }
        let config_file = gio::File::for_path(&config_path);
        match config_file.monitor_file(gio::FileMonitorFlags::NONE, None::<&Cancellable>) {
            Ok(monitor) => {
                let ui_for_reload = ui.clone();
                // Debounce: editors may write multiple events in rapid succession.
                let reload_pending: Rc<Cell<bool>> = Rc::new(Cell::new(false));
                monitor.connect_changed(move |_, _, _, event| {
                    if matches!(event, gio::FileMonitorEvent::Changed | gio::FileMonitorEvent::Created)
                        && !reload_pending.get() {
                            reload_pending.set(true);
                            let ui_reload = ui_for_reload.clone();
                            let pending = reload_pending.clone();
                            glib::timeout_add_local_once(
                                std::time::Duration::from_millis(200),
                                move || {
                                    pending.set(false);
                                    ui_reload.reload_config();
                                },
                            );
                        }
                });
                // Keep monitor alive by storing it on the window
                unsafe { window.set_data("config-monitor", monitor); }
            }
            Err(err) => {
                log::warn!("Failed to watch config file: {err}");
            }
        }
    });

    app.run()
}
