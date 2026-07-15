use adw::prelude::*;
use gtk4::gdk::ffi::GDK_BUTTON_PRIMARY;
use gtk4::gdk::ModifierType;
use gtk4::gdk::RGBA;
use gtk4::gio::{self, Cancellable};
use gtk4::glib::translate::IntoGlib;
use gtk4::glib::SpawnFlags;
use gtk4::pango::FontDescription;
use gtk4::GestureClick;
use gtk4::{glib, Entry, Label, Orientation, Paned};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::{CursorBlinkMode, CursorShape, PtyFlags, Terminal};
use vte4::{TerminalExt, TerminalExtManual};

use crate::config::Config;

/// Focus a terminal now and once after GTK finishes the current page/focus
/// transition. Notebook switches can emit `switch-page` before the newly
/// selected child is mapped, so the immediate grab is occasionally discarded
/// by the container's own focus reconciliation.
pub(crate) fn focus_terminal_deferred(terminal: &Terminal) {
    terminal.grab_focus();
    let deferred = terminal.clone();
    glib::idle_add_local_once(move || {
        deferred.grab_focus();
    });
}

/// Apply the visual profile shared by regular VTE mode, block mode's live
/// surface, and block snapshots. Keeping this in one place prevents a runtime
/// theme change from making the two terminal modes drift apart.
pub(crate) fn apply_terminal_theme(terminal: &Terminal, config: &Config) {
    let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
    terminal.set_colors(
        Some(&config.foreground),
        Some(&config.background),
        &palette_refs,
    );
    terminal.set_color_bold(None);
    terminal.set_color_cursor(Some(&config.cursor));
    terminal.set_color_cursor_foreground(Some(&config.cursor_foreground));
    let font_desc = FontDescription::from_string(&config.font_desc);
    terminal.set_font(Some(&font_desc));
    terminal.set_font_scale(config.default_font_scale);
}

pub(crate) fn create_terminal(config: &Config) -> Terminal {
    let font_scale = config.default_font_scale;
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(true)
        .name("term_name")
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(true)
        .scrollback_lines(config.terminal_scrollback_lines)
        .cursor_blink_mode(CursorBlinkMode::System)
        .cursor_shape(CursorShape::Block)
        .font_scale(font_scale)
        .opacity(1.0)
        .pointer_autohide(true)
        .enable_sixel(true)
        .build();

    terminal.set_mouse_autohide(true);

    apply_terminal_theme(&terminal, config);

    // Set regex for hyperlinks
    let regex_pattern = vte4::Regex::for_match(
        r"[a-z]+://[[:graph:]]+",
        pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
    );
    terminal.match_add_regex(&regex_pattern.unwrap(), 0);

    terminal
}

// ─── VteTerminalView ──────────────────────────────────────────────────────

/// Shared lists of observer callbacks, keyed by the payload they receive.
type StrCallbacks = Rc<RefCell<Vec<Box<dyn Fn(&str)>>>>;
type IntCallbacks = Rc<RefCell<Vec<Box<dyn Fn(i32)>>>>;
type VoidCallbacks = Rc<RefCell<Vec<Box<dyn Fn()>>>>;

#[allow(dead_code)]
pub struct VteTerminalView {
    root: gtk4::Box,
    terminal: Terminal,
    config: Rc<RefCell<Config>>,
    cwd_callbacks: StrCallbacks,
    exited_callbacks: IntCallbacks,
    bell_callbacks: VoidCallbacks,
    title_callbacks: StrCallbacks,
    activity_callbacks: VoidCallbacks,
}

#[allow(dead_code)]
impl VteTerminalView {
    pub fn new(
        config: Rc<RefCell<Config>>,
        shell_argv: &[String],
        working_directory: Option<&str>,
        session_id: Option<&str>,
        initial_commands: Option<&str>,
    ) -> Self {
        // Create Terminal widget
        let terminal = create_terminal(&config.borrow());

        // Wrap with scrollbar
        let root = wrap_with_scrollbar(&terminal);
        root.add_css_class("vte-view-root");

        let cwd_callbacks = Rc::new(RefCell::new(Vec::<Box<dyn Fn(&str)>>::new()));
        let exited_callbacks = Rc::new(RefCell::new(Vec::<Box<dyn Fn(i32)>>::new()));
        let bell_callbacks = Rc::new(RefCell::new(Vec::<Box<dyn Fn()>>::new()));
        let title_callbacks = Rc::new(RefCell::new(Vec::<Box<dyn Fn(&str)>>::new()));
        let activity_callbacks = Rc::new(RefCell::new(Vec::<Box<dyn Fn()>>::new()));

        // Listen for OSC 7 (CWD changes)
        let cwd_callbacks_clone = cwd_callbacks.clone();
        let terminal_for_cwd = terminal.clone();
        terminal.connect_current_directory_uri_notify(move |_| {
            if let Some(uri) = terminal_for_cwd.current_directory_uri() {
                let file = gio::File::for_uri(uri.as_str());
                if let Some(path) = file
                    .path()
                    .map(|p| p.to_string_lossy().to_string())
                    .filter(|s| !s.is_empty())
                {
                    for callback in cwd_callbacks_clone.borrow().iter() {
                        callback(&path);
                    }
                }
            }
        });

        // Listen for child-exited signal
        let exited_callbacks_clone = exited_callbacks.clone();
        terminal.connect_child_exited(move |_term, status| {
            for callback in exited_callbacks_clone.borrow().iter() {
                callback(status);
            }
        });

        // Listen for bell signal
        let bell_callbacks_clone = bell_callbacks.clone();
        terminal.connect_bell(move |_term| {
            for callback in bell_callbacks_clone.borrow().iter() {
                callback();
            }
        });

        // Listen for window-title-changed signal
        let title_callbacks_clone = title_callbacks.clone();
        let terminal_for_title = terminal.clone();
        terminal.connect_window_title_changed(move |_term| {
            if let Some(title) = terminal_for_title.window_title() {
                let title_str = title.to_string();
                if !title_str.is_empty() {
                    for callback in title_callbacks_clone.borrow().iter() {
                        callback(&title_str);
                    }
                }
            }
        });

        // Listen for contents-changed signal (activity)
        let activity_callbacks_clone = activity_callbacks.clone();
        terminal.connect_contents_changed(move |_term| {
            for callback in activity_callbacks_clone.borrow().iter() {
                callback();
            }
        });

        // Spawn shell
        spawn_shell(
            &terminal,
            shell_argv,
            working_directory,
            session_id,
            initial_commands,
        );

        VteTerminalView {
            root,
            terminal,
            config,
            cwd_callbacks,
            exited_callbacks,
            bell_callbacks,
            title_callbacks,
            activity_callbacks,
        }
    }

    pub fn widget(&self) -> gtk4::Widget {
        self.root.clone().upcast()
    }

    pub fn vte(&self) -> &Terminal {
        &self.terminal
    }

    pub fn connect_cwd_changed<F>(&self, callback: F)
    where
        F: Fn(&str) + 'static,
    {
        self.cwd_callbacks.borrow_mut().push(Box::new(callback));
    }

    pub fn connect_exited<F>(&self, callback: F)
    where
        F: Fn(i32) + 'static,
    {
        self.exited_callbacks.borrow_mut().push(Box::new(callback));
    }

    pub fn grab_focus(&self) {
        focus_terminal_deferred(&self.terminal);
    }

    pub fn copy_to_clipboard(&self) {
        self.terminal.copy_clipboard_format(vte4::Format::Text);
    }

    pub fn paste_from_clipboard(&self) {
        self.terminal.paste_clipboard();
    }

    pub fn connect_bell<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        self.bell_callbacks.borrow_mut().push(Box::new(callback));
    }

    pub fn connect_title_changed<F>(&self, callback: F)
    where
        F: Fn(&str) + 'static,
    {
        self.title_callbacks.borrow_mut().push(Box::new(callback));
    }

    pub fn connect_activity<F>(&self, callback: F)
    where
        F: Fn() + 'static,
    {
        self.activity_callbacks
            .borrow_mut()
            .push(Box::new(callback));
    }

    pub fn set_font(&self, font_desc: &FontDescription) {
        self.terminal.set_font(Some(font_desc));
    }

    pub fn set_font_scale(&self, scale: f64) {
        self.terminal.set_font_scale(scale);
    }

    pub fn apply_theme(&self) {
        let config = self.config.borrow();
        apply_terminal_theme(&self.terminal, &config);
    }

    pub fn write_input(&self, data: &[u8]) {
        self.terminal.feed_child(data);
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Some(pty) = self.terminal.pty() {
            let _ = pty.set_size(rows as i32, cols as i32);
        }
    }

    pub fn kill(&self) {
        // Send SIGHUP to child process to gracefully terminate
        if let Some(pid) = unsafe { self.terminal.data::<i32>("child-pid") } {
            let pid_val = unsafe { *pid.as_ref() };
            unsafe {
                nix::libc::kill(pid_val, nix::libc::SIGHUP);
            }
        }
    }

    pub fn pid_i32(&self) -> i32 {
        unsafe {
            self.terminal
                .data::<i32>("child-pid")
                .map(|pid| *pid.as_ref())
                .unwrap_or(0)
        }
    }
}

/// Wrap a terminal in an hbox with a scrollbar on the right side.
pub(crate) fn wrap_with_scrollbar(terminal: &Terminal) -> gtk4::Box {
    let hbox = gtk4::Box::new(Orientation::Horizontal, 0);
    hbox.set_hexpand(true);
    hbox.set_vexpand(true);
    hbox.add_css_class("terminal-box");
    let scrollbar = gtk4::Scrollbar::new(Orientation::Vertical, terminal.vadjustment().as_ref());
    hbox.append(terminal);
    hbox.append(&scrollbar);
    hbox
}

/// If the widget is a terminal inside a scrollbar wrapper box, return the wrapper box.
pub(crate) fn scrollbar_wrapper_of(term_widget: &gtk4::Widget) -> Option<gtk4::Box> {
    let parent = term_widget.parent()?;
    let bx = parent.clone().downcast::<gtk4::Box>().ok()?;
    if bx.has_css_class("terminal-box") {
        Some(bx)
    } else {
        None
    }
}

pub(crate) fn terminal_working_directory(terminal: &Terminal) -> Option<String> {
    // Prefer OSC 7 reported directory
    if let Some(uri) = terminal.current_directory_uri() {
        let file = gio::File::for_uri(uri.as_str());
        if let Some(path) = file
            .path()
            .map(|p| p.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
        {
            return Some(path);
        }
    }
    // Fallback: read /proc/<pid>/cwd
    let pid: i32 = unsafe { *terminal.data::<i32>("child-pid")?.as_ref() };
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

pub(crate) fn spawn_shell(
    terminal: &Terminal,
    argv_owned: &[String],
    working_directory: Option<&str>,
    session_id: Option<&str>,
    initial_commands: Option<&str>,
) {
    // Append --session <id> to argv when restoring a session (only for rsh)
    let mut argv_vec: Vec<String> = argv_owned.to_vec();
    if let Some(sid) = session_id {
        let is_rsh = argv_vec
            .first()
            .and_then(|s| std::path::Path::new(s).file_name())
            .and_then(|f| f.to_str())
            .map(|name| name == "rsh")
            .unwrap_or(false);

        if is_rsh {
            argv_vec.push("--session".to_string());
            argv_vec.push(sid.to_string());
        }
    }
    let home = std::env::var("HOME").ok();
    let requested_working_directory = working_directory.or(home.as_deref());
    let argv_vec = crate::host::wrap_argv(&argv_vec, requested_working_directory, &[]);
    let argv: Vec<&str> = argv_vec.iter().map(|s| s.as_str()).collect();

    // Use empty envv to inherit all environment variables from parent process.
    // In Flatpak mode, cwd and host environment forwarding are encoded in the
    // flatpak-spawn argv above instead of being applied to the sandbox helper.
    let envv: &[&str] = &[];
    let spawn_flags = SpawnFlags::SEARCH_PATH;
    let cancellable: Option<&Cancellable> = None;
    let spawn_working_directory = if crate::host::is_flatpak() {
        None
    } else {
        requested_working_directory
    };
    let terminal_for_pid = terminal.clone();

    // If initial commands are provided, send them after the shell starts.
    let init_cmds = initial_commands.map(|s| s.to_string());
    let terminal_for_init = terminal.clone();

    terminal.spawn_async(
        PtyFlags::DEFAULT,
        spawn_working_directory,
        &argv,
        envv,
        spawn_flags,
        || {},
        -1,
        cancellable,
        move |res| {
            log::debug!("spawn_async: {res:?}");
            if let Ok(pid) = res {
                let pid_i32: i32 = pid.into_glib();
                unsafe {
                    terminal_for_pid.set_data::<i32>("child-pid", pid_i32);
                }
            }
            // Feed initial commands after the shell has fully initialized.
            // We delay to ensure the shell has entered raw mode; sending \r
            // too early would hit the kernel's cooked-mode icrnl translation
            // (turning \r into \n), which raw-mode shells don't treat as Enter.
            if let Some(ref cmds) = init_cmds {
                if !cmds.is_empty() {
                    let cmds = cmds.clone();
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(500),
                        move || {
                            let lines: Vec<&str> = cmds.split(", ").collect();
                            for line in lines {
                                let text = format!("{}\r", line.trim());
                                terminal_for_init.feed_child(text.as_bytes());
                            }
                        },
                    );
                }
            }
        },
    );
}

pub(crate) fn open_uri(uri: &str) {
    if let Err(err) = gio::AppInfo::launch_default_for_uri(uri, None::<&gio::AppLaunchContext>) {
        log::warn!("Failed to open URI {uri}: {err}");
    }
}

pub(crate) fn show_rename_dialog(
    window: &adw::ApplicationWindow,
    label: &Label,
    custom_title: Rc<Cell<bool>>,
) {
    let dialog = adw::AlertDialog::new(Some("Rename tab"), None);
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("rename", "Rename");
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);

    let entry = Entry::new();
    entry.set_text(&label.text());
    entry.set_activates_default(true);
    dialog.set_extra_child(Some(&entry));

    let label_clone = label.clone();
    let custom_title_clone = custom_title.clone();
    let value = entry.clone();
    dialog.connect_response(None, move |_dialog, response| {
        if response == "rename" {
            let text = value.text();
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                label_clone.set_text(trimmed);
                custom_title_clone.set(true);
            }
        }
    });

    dialog.present(Some(window));
}

pub(crate) fn show_rename_dialog_with_strip(
    window: &adw::ApplicationWindow,
    label: &Label,
    strip_label: &Label,
    custom_title: Rc<Cell<bool>>,
) {
    let dialog = adw::AlertDialog::new(Some("Rename tab"), None);
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("rename", "Rename");
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);

    let entry = Entry::new();
    entry.set_text(&label.text());
    entry.set_activates_default(true);
    dialog.set_extra_child(Some(&entry));

    let label_clone = label.clone();
    let strip_label_clone = strip_label.clone();
    let custom_title_clone = custom_title.clone();
    let value = entry.clone();
    dialog.connect_response(None, move |_dialog, response| {
        if response == "rename" {
            let text = value.text();
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                label_clone.set_text(trimmed);
                strip_label_clone.set_text(trimmed);
                custom_title_clone.set(true);
            }
        }
    });

    dialog.present(Some(window));
}

pub(crate) fn default_tab_title(tab_index_1based: u32, working_directory: Option<&str>) -> String {
    let mut resolved_dir = working_directory
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string());

    // If no directory is known (e.g. first launch), default to HOME so the tab has a meaningful title.
    if resolved_dir.is_none() {
        resolved_dir = std::env::var("HOME").ok();
    }

    let Some(dir) = resolved_dir.as_deref() else {
        return format!("Terminal {tab_index_1based}");
    };

    // Normalize trailing slashes.
    let mut normalized = dir.trim_end_matches('/');
    if normalized.is_empty() {
        normalized = "/";
    }

    // Shorten $HOME to ~.
    let home = std::env::var("HOME").ok();
    let display_dir = if let Some(home) = home.as_deref() {
        if normalized == home {
            "~".to_string()
        } else if let Some(rest) = normalized.strip_prefix(home) {
            if rest.starts_with('/') {
                format!("~{rest}")
            } else {
                normalized.to_string()
            }
        } else {
            normalized.to_string()
        }
    } else {
        normalized.to_string()
    };

    if display_dir == "/" || display_dir == "~" {
        return display_dir;
    }

    // Fish-like prompt_pwd: abbreviate intermediate components, keep the last component.
    // Example: /usr/local/bin -> /u/l/bin, ~/projects/rust-project/jwm -> ~/p/r/jwm
    fn shorten_component(component: &str) -> String {
        if component.is_empty() {
            return String::new();
        }
        if component == "." || component == ".." {
            return component.to_string();
        }

        let mut chars = component.chars();
        let first = chars.next().unwrap();
        if first == '.' {
            // Better readability for dot-dirs: ".config" -> ".c".
            if let Some(second) = chars.next() {
                let mut out = String::new();
                out.push(first);
                out.push(second);
                out
            } else {
                ".".to_string()
            }
        } else {
            first.to_string()
        }
    }

    let (prefix, rest) = if let Some(r) = display_dir.strip_prefix("~/") {
        ("~/", r)
    } else if let Some(r) = display_dir.strip_prefix('/') {
        ("/", r)
    } else {
        ("", display_dir.as_str())
    };

    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 1 {
        return format!("{prefix}{rest}");
    }

    let mut out_parts: Vec<String> = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        if i + 1 == parts.len() {
            out_parts.push((*part).to_string());
        } else {
            out_parts.push(shorten_component(part));
        }
    }

    format!("{prefix}{}", out_parts.join("/"))
}

pub(crate) fn setup_terminal_click_handler(terminal: &Terminal) {
    // Use a click gesture in Capture phase to intercept Ctrl+Click before VTE sees it
    // For normal clicks, let them pass through to VTE for text selection
    let click_controller = GestureClick::new();
    click_controller.set_button(GDK_BUTTON_PRIMARY as u32);
    click_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let terminal_clone = terminal.clone();

    click_controller.connect_pressed(move |controller, n_press, x, y| {
        // Only intercept single Ctrl+Click on hyperlinks
        // Let all other clicks pass through to VTE for selection
        if n_press == 1 {
            let state = controller.current_event_state();
            if state.contains(ModifierType::CONTROL_MASK) {
                if let Some(uri) = terminal_clone.check_match_at(x, y).0 {
                    open_uri(&uri);
                    // Claim this event to prevent VTE from processing it
                    controller.set_state(gtk4::EventSequenceState::Claimed);
                    return;
                }
            }
        }
        // Explicitly deny to pass event to VTE for text selection
        controller.set_state(gtk4::EventSequenceState::Denied);
    });

    terminal.add_controller(click_controller);
}

/// Find the first Terminal in a widget tree (depth-first). Traverses children
/// generically so the live VTE buried under ScrolledWindow → Viewport →
/// block_list → active_holder is reachable.
pub(crate) fn find_first_terminal(widget: &gtk4::Widget) -> Option<Terminal> {
    if let Ok(term) = widget.clone().downcast::<Terminal>() {
        return Some(term);
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(term) = find_first_terminal(&c) {
            return Some(term);
        }
        child = c.next_sibling();
    }
    None
}

/// Find the focused Terminal in a widget tree.
pub(crate) fn find_focused_terminal(widget: &gtk4::Widget) -> Option<Terminal> {
    if let Ok(term) = widget.clone().downcast::<Terminal>() {
        if term.has_focus() {
            return Some(term);
        }
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(term) = find_focused_terminal(&c) {
            return Some(term);
        }
        child = c.next_sibling();
    }
    None
}

/// Collect all terminals in a widget tree.
pub(crate) fn collect_terminals(widget: &gtk4::Widget, out: &mut Vec<Terminal>) {
    if let Ok(term) = widget.clone().downcast::<Terminal>() {
        out.push(term);
        return;
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        collect_terminals(&c, out);
        child = c.next_sibling();
    }
}

/// Walk the Paned tree and reattach a terminal to the first None child slot found.
pub(crate) fn reattach_terminal_to_tree(
    widget: &gtk4::Widget,
    child_to_reattach: &gtk4::Widget,
) -> bool {
    if let Ok(paned) = widget.clone().downcast::<Paned>() {
        if paned.start_child().is_none() {
            paned.set_start_child(Some(child_to_reattach));
            return true;
        }
        if paned.end_child().is_none() {
            paned.set_end_child(Some(child_to_reattach));
            return true;
        }
        if let Some(start) = paned.start_child() {
            if reattach_terminal_to_tree(&start, child_to_reattach) {
                return true;
            }
        }
        if let Some(end) = paned.end_child() {
            if reattach_terminal_to_tree(&end, child_to_reattach) {
                return true;
            }
        }
    }
    false
}
