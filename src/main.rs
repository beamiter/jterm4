use gtk4::gdk::ffi::GDK_BUTTON_PRIMARY;
use gtk4::gdk::Key;
use gtk4::gdk::ModifierType;
use gtk4::gdk::RGBA;
use gtk4::glib::translate::IntoGlib;
use gtk4::gio::{self, Cancellable};
use gtk4::gio::prelude::FileExt as GioFileExt;
use gtk4::glib::SpawnFlags;
use gtk4::pango::FontDescription;
use gtk4::prelude::*;
use gtk4::{glib, Application, ApplicationWindow, Dialog, Entry, Label, Notebook, Orientation, Paned, ResponseType};
use gtk4::{EventControllerKey, GestureClick, SearchBar, SearchEntry};
use log::{LevelFilter, Log, Metadata, Record};
use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::LazyLock;
use vte4::Format;
use vte4::{CursorBlinkMode, CursorShape, PtyFlags, Terminal};
use vte4::{TerminalExt, TerminalExtManual};

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

#[derive(Clone)]
struct Config {
    window_opacity: f64,
    terminal_scrollback_lines: u32,
    font_desc: String,
    default_font_scale: f64,
    foreground: RGBA,
    background: RGBA,
    cursor: RGBA,
    cursor_foreground: RGBA,
}

#[derive(Clone)]
struct UiState {
    window: ApplicationWindow,
    notebook: Notebook,
    tab_counter: Rc<Cell<u32>>,
    font_scale: Rc<Cell<f64>>,
    shell_argv: Rc<Vec<String>>,
    config: Rc<Config>,
    search_bar: SearchBar,
    search_entry: SearchEntry,
}

fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name).ok().and_then(|v| v.parse::<f64>().ok())
}

fn env_u32(name: &str) -> Option<u32> {
    std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok())
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

fn env_rgba(name: &str) -> Option<RGBA> {
    env_string(name).and_then(|v| RGBA::parse(&v).ok())
}

fn config_file_path() -> PathBuf {
    glib::user_config_dir().join("jterm4").join("config.toml")
}

/// Parsed TOML config file structure.
#[derive(Default)]
struct FileConfig {
    opacity: Option<f64>,
    scrollback: Option<u32>,
    font: Option<String>,
    font_scale: Option<f64>,
    foreground: Option<String>,
    background: Option<String>,
    cursor: Option<String>,
    cursor_foreground: Option<String>,
}

fn load_file_config() -> FileConfig {
    let path = config_file_path();
    let Ok(contents) = fs::read_to_string(&path) else {
        return FileConfig::default();
    };
    let Ok(table) = contents.parse::<toml::Table>() else {
        log::warn!("Failed to parse config file {}", path.display());
        return FileConfig::default();
    };

    let colors = table.get("colors").and_then(|v| v.as_table());

    FileConfig {
        opacity: table.get("opacity").and_then(|v| v.as_float()),
        scrollback: table.get("scrollback").and_then(|v| v.as_integer()).map(|v| v as u32),
        font: table.get("font").and_then(|v| v.as_str()).map(|s| s.to_string()),
        font_scale: table.get("font_scale").and_then(|v| v.as_float()),
        foreground: colors.and_then(|c| c.get("foreground")).and_then(|v| v.as_str()).map(|s| s.to_string()),
        background: colors.and_then(|c| c.get("background")).and_then(|v| v.as_str()).map(|s| s.to_string()),
        cursor: colors.and_then(|c| c.get("cursor")).and_then(|v| v.as_str()).map(|s| s.to_string()),
        cursor_foreground: colors.and_then(|c| c.get("cursor_foreground")).and_then(|v| v.as_str()).map(|s| s.to_string()),
    }
}

fn load_config() -> Config {
    let fc = load_file_config();

    let default_foreground = RGBA::parse("#f8f7e9").unwrap();
    let default_background = RGBA::parse("#121616").unwrap();
    let default_cursor = RGBA::parse("#7fb80e").unwrap();
    let default_cursor_foreground = RGBA::parse("#1b315e").unwrap();

    // Priority: env var > config file > default
    let window_opacity = env_f64("JTERM4_OPACITY")
        .or(fc.opacity)
        .unwrap_or(0.95)
        .clamp(0.01, 1.0);
    let terminal_scrollback_lines = env_u32("JTERM4_SCROLLBACK")
        .or(fc.scrollback)
        .unwrap_or(5000);
    let default_font_scale = env_f64("JTERM4_FONT_SCALE")
        .or(fc.font_scale)
        .unwrap_or(1.0)
        .clamp(0.1, 10.0);
    let font_desc = env_string("JTERM4_FONT")
        .or(fc.font)
        .unwrap_or_else(|| "SauceCodePro Nerd Font Regular 12".to_string());

    let foreground = env_rgba("JTERM4_FG")
        .or_else(|| fc.foreground.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(default_foreground);
    let background = env_rgba("JTERM4_BG")
        .or_else(|| fc.background.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(default_background);
    let cursor = env_rgba("JTERM4_CURSOR")
        .or_else(|| fc.cursor.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(default_cursor);
    let cursor_foreground = env_rgba("JTERM4_CURSOR_FG")
        .or_else(|| fc.cursor_foreground.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(default_cursor_foreground);

    Config {
        window_opacity,
        terminal_scrollback_lines,
        font_desc,
        default_font_scale,
        foreground,
        background,
        cursor,
        cursor_foreground,
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn find_executable_in_path(exe_name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(exe_name))
        .find(|candidate| is_executable(candidate))
}

fn choose_shell_argv() -> Vec<String> {
    // Prefer rsh.
    if let Some(rsh_path) = find_executable_in_path("rsh") {
        return vec![rsh_path.to_string_lossy().to_string()];
    }

    // Fallback: bash
    if let Some(bash_path) = find_executable_in_path("bash") {
        return vec![bash_path.to_string_lossy().to_string(), "-l".to_string()];
    }

    // Last resort: POSIX sh
    vec!["sh".to_string()]
}

static PALETTE: LazyLock<[RGBA; 16]> = LazyLock::new(|| [
    RGBA::parse("#130c0e").unwrap(),
    RGBA::parse("#ed1941").unwrap(),
    RGBA::parse("#45b97c").unwrap(),
    RGBA::parse("#fdb933").unwrap(),
    RGBA::parse("#2585a6").unwrap(),
    RGBA::parse("#ae5039").unwrap(),
    RGBA::parse("#009ad6").unwrap(),
    RGBA::parse("#fffef9").unwrap(),
    RGBA::parse("#7c8577").unwrap(),
    RGBA::parse("#f05b72").unwrap(),
    RGBA::parse("#84bf96").unwrap(),
    RGBA::parse("#ffc20e").unwrap(),
    RGBA::parse("#7bbfea").unwrap(),
    RGBA::parse("#f58f98").unwrap(),
    RGBA::parse("#33a3dc").unwrap(),
    RGBA::parse("#f6f5ec").unwrap(),
]);

fn create_terminal(config: &Config, font_scale: f64) -> Terminal {
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
        .build();

    terminal.set_mouse_autohide(true);

    // Set colors
    let palette_refs: Vec<&RGBA> = PALETTE.iter().collect();
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

    terminal.connect_bell(move |_| {
        log::debug!("Bell signal received");
    });

    terminal
}

fn terminal_working_directory(terminal: &Terminal) -> Option<String> {
    let uri = terminal.current_directory_uri()?;
    let file = gio::File::for_uri(uri.as_str());
    file.path()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
}

fn tabs_state_file_path() -> PathBuf {
    glib::user_config_dir()
        .join("jterm4")
        .join("tabs.state")
}

fn escape_tab_state(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

fn unescape_tab_state(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek().copied() {
                Some('t') => {
                    out.push('\t');
                    chars.next();
                }
                Some('n') => {
                    out.push('\n');
                    chars.next();
                }
                Some('\\') => {
                    out.push('\\');
                    chars.next();
                }
                _ => out.push(ch),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn parse_tabs_state(contents: &str) -> (Option<u32>, Vec<(Option<String>, String)>) {
    let mut current_page: Option<u32> = None;
    let mut tabs: Vec<(Option<String>, String)> = Vec::new();

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("current_page=") {
            current_page = rest.trim().parse::<u32>().ok();
            continue;
        }
        if let Some(rest) = line.strip_prefix("tab=") {
            if let Some((name_raw, dir_raw)) = rest.split_once('\t') {
                let name = unescape_tab_state(name_raw);
                let dir = unescape_tab_state(dir_raw);
                tabs.push((Some(name), dir));
            } else {
                let dir = unescape_tab_state(rest);
                tabs.push((None, dir));
            }
            continue;
        }
        tabs.push((None, line.to_string()));
    }

    (current_page, tabs)
}

fn load_tabs_state() -> (Option<u32>, Vec<(Option<String>, String)>) {
    let path = tabs_state_file_path();
    let Ok(contents) = fs::read_to_string(&path) else {
        return (None, Vec::new());
    };

    // Consume-on-start: delete after read so only one instance restores this snapshot.
    // Each instance writes its own state on close; the last one closed wins.
    if let Err(err) = fs::remove_file(&path) {
        log::debug!("Failed to remove tabs state {}: {err}", path.display());
    }

    parse_tabs_state(&contents)
}

fn tab_label_text(notebook: &Notebook, widget: &gtk4::Widget) -> Option<String> {
    let tab_label = notebook.tab_label(widget)?;
    let tab_box = tab_label.downcast::<gtk4::Box>().ok()?;
    let first_child = tab_box.first_child()?;
    let label = first_child.downcast::<Label>().ok()?;
    Some(label.text().to_string())
}

fn save_tabs_state(notebook: &Notebook) {
    let path = tabs_state_file_path();
    if let Some(parent) = path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            log::warn!("Failed to create state dir {}: {err}", parent.display());
            return;
        }
    }

    let home = std::env::var("HOME").ok();
    let n_pages = notebook.n_pages();
    let mut lines: Vec<String> = Vec::with_capacity((n_pages as usize) + 1);
    if let Some(current) = notebook.current_page() {
        lines.push(format!("current_page={current}"));
    }

    for i in 0..n_pages {
        let Some(widget) = notebook.nth_page(Some(i)) else {
            continue;
        };
        // Find first terminal in possibly-split page
        let Some(terminal) = find_first_terminal(&widget) else {
            continue;
        };

        let dir = terminal_working_directory(&terminal)
            .or_else(|| home.clone())
            .unwrap_or_else(|| "/".to_string());
        let label_text = tab_label_text(notebook, &widget)
            .unwrap_or_else(|| format!("Terminal {}", i + 1));
        let line = format!(
            "tab={}\t{}",
            escape_tab_state(&label_text),
            escape_tab_state(&dir)
        );
        lines.push(line);
    }

    let payload = lines.join("\n") + "\n";

    // Write atomically to avoid partially-written state when the process is interrupted.
    let tmp_path = path.with_file_name(
        path.file_name()
            .and_then(|s| s.to_str())
            .map(|name| format!("{name}.tmp"))
            .unwrap_or_else(|| "tabs.state.tmp".to_string()),
    );

    if let Err(err) = fs::write(&tmp_path, &payload) {
        log::warn!(
            "Failed to write temp state file {}: {err}",
            tmp_path.display()
        );
        return;
    }

    if let Err(err) = fs::rename(&tmp_path, &path) {
        // On some platforms rename may fail if the destination exists; fall back to remove+rename.
        let _ = fs::remove_file(&path);
        if let Err(err2) = fs::rename(&tmp_path, &path) {
            log::warn!(
                "Failed to move temp state file {} into place {}: {err} / {err2}",
                tmp_path.display(),
                path.display()
            );
            let _ = fs::remove_file(&tmp_path);
        }
    }
}

fn spawn_shell(terminal: &Terminal, argv_owned: &[String], working_directory: Option<&str>) {
    let argv: Vec<&str> = argv_owned.iter().map(|s| s.as_str()).collect();

    // Use empty envv to inherit all environment variables from parent process
    let envv: &[&str] = &[];
    let spawn_flags = SpawnFlags::SEARCH_PATH;
    let cancellable: Option<&Cancellable> = None;
    let home = std::env::var("HOME").ok();
    let working_directory = working_directory.or(home.as_deref());
    terminal.spawn_async(
        PtyFlags::DEFAULT,
        working_directory,
        &argv,
        envv,
        spawn_flags,
        || {},
        -1,
        cancellable,
        |res| log::debug!("spawn_async: {res:?}"),
    );
}

fn open_uri(uri: &str) {
    if let Err(err) = gio::AppInfo::launch_default_for_uri(uri, None::<&gio::AppLaunchContext>) {
        log::warn!("Failed to open URI {uri}: {err}");
    }
}

fn show_rename_dialog(window: &ApplicationWindow, label: &Label, custom_title: Rc<Cell<bool>>) {
    let dialog = Dialog::builder()
        .transient_for(window)
        .modal(true)
        .title("Rename tab")
        .build();
    dialog.add_button("Cancel", ResponseType::Cancel);
    dialog.add_button("Rename", ResponseType::Accept);
    dialog.set_default_response(ResponseType::Accept);

    let entry = Entry::new();
    entry.set_text(&label.text());
    entry.set_activates_default(true);
    dialog.content_area().append(&entry);

    let label_clone = label.clone();
    let custom_title_clone = custom_title.clone();
    let value = entry.clone();
    dialog.connect_response(move |dialog, response| {
        if response == ResponseType::Accept {
            let text = value.text();
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                label_clone.set_text(trimmed);
                custom_title_clone.set(true);
            }
        }
        dialog.close();
    });

    dialog.show();
    entry.grab_focus();
}

fn default_tab_title(tab_index_1based: u32, working_directory: Option<&str>) -> String {
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

fn looks_like_legacy_default_title(title: &str) -> bool {
    let trimmed = title.trim();
    let Some(rest) = trimmed.strip_prefix("Terminal ") else {
        return false;
    };
    rest.trim().parse::<u32>().is_ok()
}

fn setup_terminal_click_handler(terminal: &Terminal) {
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
fn find_first_terminal(widget: &gtk4::Widget) -> Option<Terminal> {
    if let Ok(term) = widget.clone().downcast::<Terminal>() {
        return Some(term);
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
fn find_focused_terminal(widget: &gtk4::Widget) -> Option<Terminal> {
    if let Ok(term) = widget.clone().downcast::<Terminal>() {
        if term.has_focus() {
            return Some(term);
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
fn collect_terminals(widget: &gtk4::Widget, out: &mut Vec<Terminal>) {
    if let Ok(term) = widget.clone().downcast::<Terminal>() {
        out.push(term);
        return;
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

impl UiState {
    fn focus_current_terminal(&self) {
        if let Some(page) = self.notebook.current_page() {
            if let Some(widget) = self.notebook.nth_page(Some(page)) {
                if let Some(term) = find_first_terminal(&widget) {
                    term.grab_focus();
                }
            }
        }
    }

    fn remove_tab_by_widget(&self, widget: &gtk4::Widget) {
        if let Some(page_num) = self.notebook.page_num(widget) {
            self.notebook.remove_page(Some(page_num));
        }
        if self.notebook.n_pages() == 0 {
            self.window.destroy();
        } else {
            self.focus_current_terminal();
        }
    }

    /// Handle a terminal exiting: unsplit if in a Paned, or close the tab.
    fn handle_terminal_exited(&self, term_widget: &gtk4::Widget) {
        let Some(parent) = term_widget.parent() else {
            return;
        };

        if let Ok(paned) = parent.clone().downcast::<Paned>() {
            let start = paned.start_child();
            let end = paned.end_child();
            let sibling = if start.as_ref() == Some(term_widget) {
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
            self.remove_tab_by_widget(term_widget);
        }
    }

    fn set_font_scale_all(&self, new_scale: f64) {
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

    fn switch_tab(&self, direction: i32) {
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

    fn remove_current_tab(&self) {
        if let Some(page_num) = self.notebook.current_page() {
            self.notebook.remove_page(Some(page_num));
            if self.notebook.n_pages() == 0 {
                self.window.destroy();
            } else {
                self.focus_current_terminal();
            }
        }
    }

    fn current_terminal(&self) -> Option<Terminal> {
        self.notebook.current_page().and_then(|page_num| {
            self.notebook.nth_page(Some(page_num)).and_then(|widget| {
                // Try focused terminal first (for split panes), then fall back to first terminal
                find_focused_terminal(&widget).or_else(|| find_first_terminal(&widget))
            })
        })
    }

    fn toggle_search(&self) {
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

    fn search_apply(&self) {
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

    fn search_next(&self) {
        if let Some(term) = self.current_terminal() {
            term.search_find_next();
        }
    }

    fn search_prev(&self) {
        if let Some(term) = self.current_terminal() {
            term.search_find_previous();
        }
    }

    fn create_split_terminal(&self, working_directory: Option<&str>) -> Terminal {
        let terminal = create_terminal(&self.config, self.font_scale.get());
        setup_terminal_click_handler(&terminal);

        let ui_for_exit = UiState::clone(self);
        let terminal_clone = terminal.clone();
        terminal.connect_child_exited(move |_, _| {
            ui_for_exit.handle_terminal_exited(&terminal_clone.clone().upcast::<gtk4::Widget>());
        });

        spawn_shell(&terminal, self.shell_argv.as_ref(), working_directory);
        terminal
    }

    fn split_current(&self, orientation: Orientation) {
        let Some(current_term) = self.current_terminal() else {
            return;
        };
        let working_directory = terminal_working_directory(&current_term);

        let current_widget = current_term.clone().upcast::<gtk4::Widget>();
        let parent = current_widget.parent();

        let new_term = self.create_split_terminal(working_directory.as_deref());

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
                paned.set_start_child(Some(&current_term));
                paned.set_end_child(Some(&new_term));
            } else {
                // Parent is the notebook - replace the page
                for i in 0..self.notebook.n_pages() {
                    if let Some(page_widget) = self.notebook.nth_page(Some(i)) {
                        if page_widget == current_widget {
                            let tab_label = self.notebook.tab_label(&page_widget);
                            self.notebook.remove_page(Some(i));
                            paned.set_start_child(Some(&current_term));
                            paned.set_end_child(Some(&new_term));
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

    fn add_new_tab(&self, working_directory: Option<String>, tab_name: Option<String>) -> Terminal {
        let tab_num = self.tab_counter.get();
        self.tab_counter.set(tab_num + 1);

        let terminal = create_terminal(&self.config, self.font_scale.get());

        // Setup click handler for hyperlinks
        setup_terminal_click_handler(&terminal);

        // Connect child-exited to close the tab (or unsplit if in a Paned)
        let ui_for_exit = UiState::clone(self);
        let terminal_clone = terminal.clone();
        terminal.connect_child_exited(move |_, _| {
            ui_for_exit.handle_terminal_exited(&terminal_clone.clone().upcast::<gtk4::Widget>());
        });

        // Spawn shell
        spawn_shell(
            &terminal,
            self.shell_argv.as_ref(),
            working_directory.as_deref(),
        );

        // Create tab header with a close button
        let tab_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let computed_default_title = default_tab_title(tab_num + 1, working_directory.as_deref());
        let (label_text, is_custom) = match tab_name {
            Some(name) => {
                // Treat as non-custom if it matches the computed default title.
                let custom = name != computed_default_title;
                (name, custom)
            }
            None => (computed_default_title, false),
        };
        let label = Label::new(Some(&label_text));
        let custom_title = Rc::new(Cell::new(is_custom));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        // Make tabs wider by default so the title is visible.
        // These are character-based hints; the notebook may still shrink tabs when crowded.
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

        // Auto-update tab title when PWD changes (unless user manually renamed it).
        let label_for_pwd = label.clone();
        let custom_title_for_pwd = custom_title.clone();
        let tab_index_for_pwd = tab_num + 1;
        terminal.connect_notify_local(Some("current-directory-uri"), move |term, _| {
            if custom_title_for_pwd.get() {
                return;
            }
            let Some(dir) = terminal_working_directory(term) else {
                return;
            };
            let new_title = default_tab_title(tab_index_for_pwd, Some(&dir));
            if label_for_pwd.text().as_str() != new_title {
                label_for_pwd.set_text(&new_title);
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

        let ui_for_close = UiState::clone(self);
        let terminal_widget_for_close = terminal.clone().upcast::<gtk4::Widget>();
        close_button.connect_clicked(move |_| {
            ui_for_close.remove_tab_by_widget(&terminal_widget_for_close);
        });

        // Add to notebook right after the current tab when possible.
        let page_num = if let Some(current_page) = self.notebook.current_page() {
            self.notebook
                .insert_page(&terminal, Some(&tab_box), Some(current_page + 1))
        } else {
            self.notebook.append_page(&terminal, Some(&tab_box))
        };
        self.notebook.set_tab_reorderable(&terminal, true);
        self.notebook.set_current_page(Some(page_num));

        // Focus the new terminal
        terminal.grab_focus();

        terminal
    }
}

fn main() -> glib::ExitCode {
    init_logging();

    // Shell selection is handled per-terminal spawn:
    // - prefer fish if available
    // - if bass works, import ~/.bashrc before showing the prompt
    // - otherwise fall back to plain fish, and if fish is missing then bash

    let app = Application::builder().application_id("app.jterm4").build();

    app.connect_activate(|app| {
        let config = Rc::new(load_config());

        // Cache shell selection once to avoid extra process probes per new tab.
        let shell_argv = Rc::new(choose_shell_argv());

        let window_opacity = Rc::new(Cell::new(config.window_opacity));
        let window = ApplicationWindow::builder()
            .application(app)
            .default_width(800)
            .default_height(600)
            .title("jterm4")
            .name("win_name")
            .opacity(window_opacity.get())
            .build();

        // Create notebook for tabs
        let notebook = Notebook::builder()
            .hexpand(true)
            .vexpand(true)
            .scrollable(true)
            .show_border(false)
            .build();

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

        // Main layout: notebook + search bar
        let main_box = gtk4::Box::new(Orientation::Vertical, 0);
        main_box.append(&notebook);
        main_box.append(&search_bar);

        // Shared state
        let font_scale = Rc::new(Cell::new(config.default_font_scale));
        let tab_counter = Rc::new(Cell::new(0));

        let ui = Rc::new(UiState {
            window: window.clone(),
            notebook: notebook.clone(),
            tab_counter: tab_counter.clone(),
            font_scale: font_scale.clone(),
            shell_argv: shell_argv.clone(),
            config: config.clone(),
            search_bar: search_bar.clone(),
            search_entry: search_entry.clone(),
        });

        // Restore tabs from last session snapshot (and delete it immediately).
        // Each instance saves its own state on close; the last one closed wins.
        let (saved_current, saved_tabs) = load_tabs_state();
        if saved_tabs.is_empty() {
            ui.add_new_tab(None, None);
        } else {
            for (name, path) in saved_tabs {
                let dir = if Path::new(&path).is_dir() { Some(path) } else { None };
                let effective_name = if dir.is_some() {
                    name.and_then(|n| if looks_like_legacy_default_title(&n) { None } else { Some(n) })
                } else {
                    name
                };
                ui.add_new_tab(dir, effective_name);
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

        let window_clone = window.clone();
        let window_opacity_clone = window_opacity.clone();
        let ui_clone = ui.clone();

        key_controller.connect_key_pressed(move |_controller, keyval, _keycode, state| {
            let font_step = 0.025;
            let opacity_step = 0.025;

            // Get current terminal (split-aware)
            let current_terminal = ui_clone.current_terminal();

            if state.contains(ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK) {
                log::debug!(
                    "Ctrl+Shift shortcut: {} ({})",
                    keyval,
                    keyval.name().unwrap_or_default()
                );
                match keyval {
                    Key::T | Key::t => {
                        log::info!("New tab");
                        let working_directory = current_terminal
                            .as_ref()
                            .and_then(terminal_working_directory);
                        ui_clone.add_new_tab(working_directory, None);
                        return true.into();
                    }
                    Key::W | Key::w => {
                        log::info!("Close tab");
                        ui_clone.remove_current_tab();
                        return true.into();
                    }
                    Key::C | Key::c => {
                        log::debug!("Copy");
                        if let Some(ref term) = current_terminal {
                            term.copy_clipboard_format(Format::Text);
                        }
                        return true.into();
                    }
                    Key::V | Key::v => {
                        log::debug!("Paste");
                        if let Some(ref term) = current_terminal {
                            term.paste_clipboard();
                        }
                        return true.into();
                    }
                    Key::plus | Key::O | Key::o => {
                        log::debug!("Font increase");
                        let new_scale = (ui_clone.font_scale.get() + font_step).min(10.0);
                        ui_clone.set_font_scale_all(new_scale);
                        return true.into();
                    }
                    Key::I | Key::i => {
                        log::debug!("Font decrease");
                        let new_scale = (ui_clone.font_scale.get() - font_step).max(0.1);
                        ui_clone.set_font_scale_all(new_scale);
                        return true.into();
                    }
                    Key::J | Key::j => {
                        log::debug!("Opacity decrease");
                        window_opacity_clone
                            .set((window_opacity_clone.get() - opacity_step).clamp(0.01, 1.0));
                        window_clone.set_opacity(window_opacity_clone.get());
                        return true.into();
                    }
                    Key::K | Key::k => {
                        log::debug!("Opacity increase");
                        window_opacity_clone
                            .set((window_opacity_clone.get() + opacity_step).clamp(0.01, 1.0));
                        window_clone.set_opacity(window_opacity_clone.get());
                        return true.into();
                    }
                    Key::F | Key::f => {
                        log::debug!("Toggle search");
                        ui_clone.toggle_search();
                        return true.into();
                    }
                    Key::E | Key::e => {
                        log::debug!("Split horizontal");
                        ui_clone.split_current(Orientation::Horizontal);
                        return true.into();
                    }
                    Key::D | Key::d => {
                        log::debug!("Split vertical");
                        ui_clone.split_current(Orientation::Vertical);
                        return true.into();
                    }
                    Key::Page_Up => {
                        ui_clone.switch_tab(-1);
                        return true.into();
                    }
                    Key::Page_Down => {
                        ui_clone.switch_tab(1);
                        return true.into();
                    }
                    _ => {}
                }
            }

            if state.contains(ModifierType::CONTROL_MASK) && !state.contains(ModifierType::SHIFT_MASK) {
                match keyval {
                    Key::minus => {
                        log::debug!("Font decrease");
                        let new_scale = (ui_clone.font_scale.get() - font_step).max(0.1);
                        ui_clone.set_font_scale_all(new_scale);
                        return true.into();
                    }
                    Key::Page_Up => {
                        ui_clone.switch_tab(-1);
                        return true.into();
                    }
                    Key::Page_Down => {
                        ui_clone.switch_tab(1);
                        return true.into();
                    }
                    // Ctrl+1~9: quick switch to tab N
                    Key::_1 | Key::_2 | Key::_3 | Key::_4 | Key::_5
                    | Key::_6 | Key::_7 | Key::_8 | Key::_9 => {
                        let n_pages = ui_clone.notebook.n_pages();
                        if n_pages > 0 {
                            let target = if keyval == Key::_9 {
                                n_pages - 1
                            } else {
                                let idx = keyval.into_glib() - Key::_1.into_glib();
                                idx.min(n_pages - 1)
                            };
                            ui_clone.notebook.set_current_page(Some(target));
                        }
                        return true.into();
                    }
                    _ => {}
                }
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

        // Focus terminal when switching tabs (split-aware)
        notebook.connect_switch_page(move |_, widget, _page_num| {
            if let Some(term) = find_first_terminal(widget) {
                term.grab_focus();
            }
        });

        window.add_controller(key_controller);

        // Save state *before* GTK starts destroying widgets.
        let notebook_for_close_request = notebook.clone();
        window.connect_close_request(move |_| {
            save_tabs_state(&notebook_for_close_request);
            false.into()
        });

        let app_clone = app.clone();
        window.connect_destroy(move |_| {
            app_clone.quit();
        });

        window.set_child(Some(&main_box));
        window.show();

        // Focus the active terminal after window is shown
        ui.focus_current_terminal();
    });

    app.run()
}
