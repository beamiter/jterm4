mod config;
mod keybindings;
mod state;
mod terminal;
mod ui;

use gtk4::gdk::Key;
use gtk4::gdk::ModifierType;
use gtk4::gio::{self, Cancellable};
use gtk4::{glib, Notebook, Orientation, CssProvider, EventControllerKey, EventControllerScroll,
           EventControllerScrollFlags, ScrolledWindow, SearchBar, SearchEntry};
use libadwaita as adw;
use adw::prelude::*;
use log::{LevelFilter, Log, Metadata, Record};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use config::{load_config, config_file_path, choose_shell_argv};
use keybindings::{KeyCombo, normalize_key};
use state::{load_tabs_state, save_tabs_state, kill_all_terminal_children};
use terminal::{terminal_working_directory, find_first_terminal, looks_like_legacy_default_title};
use ui::UiState;

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

fn main() -> glib::ExitCode {
    // Ensure fcitx5 GTK4 IM module is discoverable at runtime.
    // FCITX5_GTK_PATH is baked in at compile time (set by nix develop shellHook).
    // SAFETY: Must run before GTK init (single-threaded at this point).
    if let Some(fcitx5_path) = option_env!("FCITX5_GTK_PATH") {
        let gtk_path = match std::env::var("GTK_PATH") {
            Ok(existing) if !existing.contains(fcitx5_path) => {
                format!("{}:{}", fcitx5_path, existing)
            }
            Ok(existing) => existing,
            Err(_) => fcitx5_path.to_string(),
        };
        unsafe { std::env::set_var("GTK_PATH", &gtk_path); }
    }

    init_logging();

    let app = adw::Application::builder().application_id("app.jterm4").build();

    app.connect_activate(|app| {
        let (config, themes, keybinding_map) = load_config();

        // Cache shell selection once to avoid extra process probes per new tab.
        let shell_argv = Rc::new(choose_shell_argv());

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
        css_provider.load_from_data(
            ".tab-strip-btn { padding: 4px 8px; border-radius: 4px; overflow: hidden; }
             .tab-strip-btn:checked { font-weight: bold; border-left: 3px solid currentColor; border-radius: 0; }
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
             .tab-bell-flash { animation: bell-flash 0.3s ease-in-out 2; }",
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

        let top_bar = gtk4::Box::new(Orientation::Horizontal, 4);
        top_bar.add_css_class("top-bar");
        top_bar.append(&toggle_sidebar_btn);
        // Spacer pushes + and ✕ to the right
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
        sidebar.append(&tab_strip_scroll);

        // Content area: sidebar + notebook side by side
        let content_box = gtk4::Box::new(Orientation::Horizontal, 0);
        content_box.set_vexpand(true);
        content_box.append(&sidebar);
        let right_col = gtk4::Box::new(Orientation::Vertical, 0);
        right_col.set_hexpand(true);
        right_col.set_vexpand(true);
        right_col.append(&notebook);
        right_col.append(&search_bar);
        content_box.append(&right_col);

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
            command_palette_dialog: Rc::new(RefCell::new(None)),
            settings_dialog: Rc::new(RefCell::new(None)),
            keybinding_map: Rc::new(RefCell::new(keybinding_map)),
            zoom_state: Rc::new(RefCell::new(None)),
            scrollbar_css: CssProvider::new(),
            session_ids: Rc::new(RefCell::new(HashMap::new())),
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

        // Restore tabs from last session snapshot (and delete it immediately).
        // Each instance saves its own state on close; the last one closed wins.
        let (saved_current, saved_tabs) = load_tabs_state();
        if saved_tabs.is_empty() {
            let startup = ui.config.borrow().startup_commands.clone();
            ui.add_new_tab(None, None, None, startup);
        } else {
            for (name, path, session_id, commands) in saved_tabs {
                let dir = if Path::new(&path).is_dir() { Some(path) } else { None };
                let effective_name = if dir.is_some() {
                    name.and_then(|n| if looks_like_legacy_default_title(&n) { None } else { Some(n) })
                } else {
                    name
                };
                ui.add_new_tab(dir, effective_name, session_id, commands);
            }

            if let Some(page) = saved_current {
                let n_pages = notebook.n_pages();
                if n_pages > 0 {
                    notebook.set_current_page(Some(page.min(n_pages.saturating_sub(1))));
                }
            }
        }

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

            if let Some(action) = ui_clone.keybinding_map.borrow().lookup(&combo) {
                ui_clone.execute_action(action);
                return true.into();
            }

            false.into()
        });

        // Wire up search entry: activate (Enter) = next, Shift+Enter = prev
        let ui_for_search_activate = ui.clone();
        search_entry.connect_activate(move |_| {
            ui_for_search_activate.search_apply();
        });

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

        // Search entry key handler for Shift+Enter (prev) and Escape
        let search_key_controller = EventControllerKey::new();
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

        // Focus terminal when switching tabs (split-aware) and sync tab strip
        let ui_for_switch = ui.clone();
        notebook.connect_switch_page(move |_, widget, page_num| {
            if let Some(term) = find_first_terminal(widget) {
                term.grab_focus();
            }
            // Clear activity/bell indicators for the tab being switched to
            let tab_name = widget.widget_name();
            ui_for_switch.clear_tab_indicators(tab_name.as_str());
            ui_for_switch.sync_tab_strip_active(Some(page_num));
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
        window.connect_close_request(move |_| {
            kill_all_terminal_children(&notebook_for_close_request);
            save_tabs_state(&notebook_for_close_request, &session_ids_for_close.borrow());
            false.into()
        });

        let app_clone = app.clone();
        window.connect_destroy(move |_| {
            app_clone.quit();
        });

        window.set_content(Some(&main_box));
        window.show();

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
                    if matches!(event, gio::FileMonitorEvent::Changed | gio::FileMonitorEvent::Created) {
                        if !reload_pending.get() {
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
