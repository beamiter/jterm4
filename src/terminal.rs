use gtk4::gdk::ffi::GDK_BUTTON_PRIMARY;
use gtk4::gdk::ModifierType;
use gtk4::gdk::RGBA;
use gtk4::gio::{self, Cancellable};
use gtk4::glib::translate::IntoGlib;
use gtk4::glib::SpawnFlags;
use gtk4::pango::FontDescription;
use gtk4::{glib, Entry, Label, Orientation, Paned};
use gtk4::GestureClick;
use libadwaita as adw;
use adw::prelude::*;
use std::cell::Cell;
use std::rc::Rc;
use vte4::{CursorBlinkMode, CursorShape, PtyFlags, Terminal};
use vte4::{TerminalExt, TerminalExtManual};

use crate::block_view::TermView;
use crate::config::Config;

/// Create a block-mode TermView.  This is the preferred constructor for new tabs.
pub(crate) fn create_block_terminal(
    config: &Config,
    shell_argv: &[String],
    working_directory: Option<&str>,
) -> TermView {
    TermView::new(config, shell_argv, working_directory)
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
        .cursor_blink_mode(CursorBlinkMode::Off)
        .cursor_shape(CursorShape::Block)
        .font_scale(font_scale)
        .opacity(1.0)
        .pointer_autohide(true)
        .enable_sixel(true)
        .build();

    terminal.set_mouse_autohide(true);

    // Set colors
    let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
    terminal.set_colors(Some(&config.foreground), Some(&config.background), &palette_refs);
    terminal.set_color_bold(None);
    terminal.set_color_cursor(Some(&config.cursor));
    terminal.set_color_cursor_foreground(Some(&config.cursor_foreground));

    // Set font
    let font_desc = FontDescription::from_string(&config.font_desc);
    terminal.set_font(Some(&font_desc));

    // Set regex for hyperlinks
    let regex_pattern = vte4::Regex::for_match(
        r"[a-z]+://[[:graph:]]+",
        pcre2_sys::PCRE2_CASELESS | pcre2_sys::PCRE2_MULTILINE,
    );
    terminal.match_add_regex(&regex_pattern.unwrap(), 0);

    terminal
}

/// Wrap a terminal in an hbox with a scrollbar on the right side.
pub(crate) fn wrap_with_scrollbar(terminal: &Terminal) -> gtk4::Box {
    let hbox = gtk4::Box::new(Orientation::Horizontal, 0);
    hbox.set_hexpand(true);
    hbox.set_vexpand(true);
    hbox.add_css_class("terminal-box");
    let scrollbar = gtk4::Scrollbar::new(
        Orientation::Vertical,
        terminal.vadjustment().as_ref(),
    );
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
        if let Some(path) = file.path().map(|p| p.to_string_lossy().to_string()).filter(|s| !s.is_empty()) {
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
    // Append --session <id> to argv when restoring a session
    let mut argv_vec: Vec<String> = argv_owned.to_vec();
    if let Some(sid) = session_id {
        argv_vec.push("--session".to_string());
        argv_vec.push(sid.to_string());
    }
    let argv: Vec<&str> = argv_vec.iter().map(|s| s.as_str()).collect();

    // Use empty envv to inherit all environment variables from parent process
    let envv: &[&str] = &[];
    let spawn_flags = SpawnFlags::SEARCH_PATH;
    let cancellable: Option<&Cancellable> = None;
    let home = std::env::var("HOME").ok();
    let working_directory = working_directory.or(home.as_deref());
    let terminal_for_pid = terminal.clone();

    // If initial commands are provided, send them after the shell starts.
    let init_cmds = initial_commands.map(|s| s.to_string());
    let terminal_for_init = terminal.clone();

    terminal.spawn_async(
        PtyFlags::DEFAULT,
        working_directory,
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
                    glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                        let lines: Vec<&str> = cmds.split(", ").collect();
                        for line in lines {
                            let text = format!("{}\r", line.trim());
                            terminal_for_init.feed_child(text.as_bytes());
                        }
                    });
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

pub(crate) fn show_rename_dialog(window: &adw::ApplicationWindow, label: &Label, custom_title: Rc<Cell<bool>>) {
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
    let mut resolved_dir = working_directory.filter(|s| !s.trim().is_empty()).map(|s| s.to_string());

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

pub(crate) fn looks_like_legacy_default_title(title: &str) -> bool {
    let trimmed = title.trim();
    let Some(rest) = trimmed.strip_prefix("Terminal ") else {
        return false;
    };
    rest.trim().parse::<u32>().is_ok()
}

pub(crate) fn setup_terminal_click_handler(terminal: &Terminal) {
    let click_controller = GestureClick::new();
    click_controller.set_button(0);
    let terminal_clone = terminal.clone();

    click_controller.connect_pressed(move |controller, n_press, x, y| {
        if n_press == 1 && controller.current_button() == GDK_BUTTON_PRIMARY as u32 {
            let state = controller.current_event_state();
            if state.contains(ModifierType::CONTROL_MASK) {
                if let Some(uri) = terminal_clone.check_match_at(x, y).0 {
                    open_uri(&uri);
                }
            }
        }
    });

    terminal.add_controller(click_controller);
}

/// Find the first Terminal in a widget tree (depth-first).
pub(crate) fn find_first_terminal(widget: &gtk4::Widget) -> Option<Terminal> {
    if let Ok(term) = widget.clone().downcast::<Terminal>() {
        return Some(term);
    }
    if let Ok(bx) = widget.clone().downcast::<gtk4::Box>() {
        if bx.has_css_class("terminal-box") {
            let mut child = bx.first_child();
            while let Some(c) = child {
                if let Some(term) = find_first_terminal(&c) {
                    return Some(term);
                }
                child = c.next_sibling();
            }
        }
    }
    if let Ok(paned) = widget.clone().downcast::<Paned>() {
        if let Some(child) = paned.start_child() {
            if let Some(term) = find_first_terminal(&child) {
                return Some(term);
            }
        }
        if let Some(child) = paned.end_child() {
            if let Some(term) = find_first_terminal(&child) {
                return Some(term);
            }
        }
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
    if let Ok(bx) = widget.clone().downcast::<gtk4::Box>() {
        if bx.has_css_class("terminal-box") {
            let mut child = bx.first_child();
            while let Some(c) = child {
                if let Some(term) = find_focused_terminal(&c) {
                    return Some(term);
                }
                child = c.next_sibling();
            }
        }
    }
    if let Ok(paned) = widget.clone().downcast::<Paned>() {
        if let Some(child) = paned.start_child() {
            if let Some(term) = find_focused_terminal(&child) {
                return Some(term);
            }
        }
        if let Some(child) = paned.end_child() {
            if let Some(term) = find_focused_terminal(&child) {
                return Some(term);
            }
        }
    }
    None
}

/// Collect all terminals in a widget tree.
pub(crate) fn collect_terminals(widget: &gtk4::Widget, out: &mut Vec<Terminal>) {
    if let Ok(term) = widget.clone().downcast::<Terminal>() {
        out.push(term);
        return;
    }
    if let Ok(bx) = widget.clone().downcast::<gtk4::Box>() {
        if bx.has_css_class("terminal-box") {
            let mut child = bx.first_child();
            while let Some(c) = child {
                collect_terminals(&c, out);
                child = c.next_sibling();
            }
            return;
        }
    }
    if let Ok(paned) = widget.clone().downcast::<Paned>() {
        if let Some(child) = paned.start_child() {
            collect_terminals(&child, out);
        }
        if let Some(child) = paned.end_child() {
            collect_terminals(&child, out);
        }
    }
}

/// Walk the Paned tree and reattach a terminal to the first None child slot found.
pub(crate) fn reattach_terminal_to_tree(widget: &gtk4::Widget, child_to_reattach: &gtk4::Widget) -> bool {
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
