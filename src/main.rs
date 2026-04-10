use gtk4::gdk::ffi::GDK_BUTTON_PRIMARY;
use gtk4::gdk::Key;
use gtk4::gdk::ModifierType;
use gtk4::gdk::RGBA;
use gtk4::glib::translate::IntoGlib;
use gtk4::gio::{self, Cancellable};
use gtk4::gio::prelude::FileExt as GioFileExt;
use gtk4::glib::SpawnFlags;
use gtk4::pango::FontDescription;
use gtk4::{glib, Adjustment, Entry, Label, ListBox, Notebook, Orientation, Paned, Scale, ScrolledWindow};
use gtk4::{CssProvider, EventControllerKey, GestureClick, SearchBar, SearchEntry, ToggleButton};
use libadwaita as adw;
use adw::prelude::*;
use log::{LevelFilter, Log, Metadata, Record};
use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
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
    theme_name: String,
    foreground: RGBA,
    background: RGBA,
    cursor: RGBA,
    cursor_foreground: RGBA,
    palette: [RGBA; 16],
}

#[derive(Clone)]
struct UiState {
    window: adw::ApplicationWindow,
    notebook: Notebook,
    tab_counter: Rc<Cell<u32>>,
    font_scale: Rc<Cell<f64>>,
    window_opacity: Rc<Cell<f64>>,
    shell_argv: Rc<Vec<String>>,
    config: Rc<RefCell<Config>>,
    available_themes: Rc<Vec<Theme>>,
    search_bar: SearchBar,
    search_entry: SearchEntry,
    tab_strip: gtk4::Box,
    keybindings_dialog: Rc<RefCell<Option<adw::Dialog>>>,
    settings_dialog: Rc<RefCell<Option<adw::PreferencesDialog>>>,
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
    theme: Option<String>,
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
        theme: table.get("theme").and_then(|v| v.as_str()).map(|s| s.to_string()),
        foreground: colors.and_then(|c| c.get("foreground")).and_then(|v| v.as_str()).map(|s| s.to_string()),
        background: colors.and_then(|c| c.get("background")).and_then(|v| v.as_str()).map(|s| s.to_string()),
        cursor: colors.and_then(|c| c.get("cursor")).and_then(|v| v.as_str()).map(|s| s.to_string()),
        cursor_foreground: colors.and_then(|c| c.get("cursor_foreground")).and_then(|v| v.as_str()).map(|s| s.to_string()),
    }
}

fn load_config() -> (Config, Vec<Theme>) {
    let fc = load_file_config();
    let themes = builtin_themes();

    // Resolve active theme
    let theme_name = env_string("JTERM4_THEME")
        .or(fc.theme)
        .unwrap_or_else(|| "default".to_string());
    let theme = themes.iter().find(|t| t.name == theme_name)
        .unwrap_or(&themes[0]);

    // Priority: env var > config file > theme default
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
        .unwrap_or(theme.foreground);
    let background = env_rgba("JTERM4_BG")
        .or_else(|| fc.background.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(theme.background);
    let cursor = env_rgba("JTERM4_CURSOR")
        .or_else(|| fc.cursor.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(theme.cursor);
    let cursor_foreground = env_rgba("JTERM4_CURSOR_FG")
        .or_else(|| fc.cursor_foreground.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(theme.cursor_foreground);

    let config = Config {
        window_opacity,
        terminal_scrollback_lines,
        font_desc,
        default_font_scale,
        theme_name: theme.name.clone(),
        foreground,
        background,
        cursor,
        cursor_foreground,
        palette: theme.palette,
    };
    (config, themes)
}

fn rgba_to_hex(c: &RGBA) -> String {
    format!("#{:02x}{:02x}{:02x}",
        (c.red() * 255.0) as u8,
        (c.green() * 255.0) as u8,
        (c.blue() * 255.0) as u8)
}

fn save_config(config: &Config) {
    let path = config_file_path();
    if let Some(parent) = path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            log::warn!("Failed to create config dir {}: {err}", parent.display());
            return;
        }
    }

    let mut table = toml::Table::new();
    table.insert("opacity".into(), toml::Value::Float(config.window_opacity));
    table.insert("scrollback".into(), toml::Value::Integer(config.terminal_scrollback_lines as i64));
    table.insert("font".into(), toml::Value::String(config.font_desc.clone()));
    table.insert("font_scale".into(), toml::Value::Float(config.default_font_scale));
    table.insert("theme".into(), toml::Value::String(config.theme_name.clone()));

    let mut colors = toml::Table::new();
    colors.insert("foreground".into(), toml::Value::String(rgba_to_hex(&config.foreground)));
    colors.insert("background".into(), toml::Value::String(rgba_to_hex(&config.background)));
    colors.insert("cursor".into(), toml::Value::String(rgba_to_hex(&config.cursor)));
    colors.insert("cursor_foreground".into(), toml::Value::String(rgba_to_hex(&config.cursor_foreground)));
    table.insert("colors".into(), toml::Value::Table(colors));

    let content = table.to_string();
    let tmp_path = path.with_extension("toml.tmp");
    if let Err(err) = fs::write(&tmp_path, &content) {
        log::warn!("Failed to write config {}: {err}", tmp_path.display());
        return;
    }
    if let Err(err) = fs::rename(&tmp_path, &path) {
        let _ = fs::remove_file(&path);
        if let Err(err2) = fs::rename(&tmp_path, &path) {
            log::warn!("Failed to move config into place: {err} / {err2}");
            let _ = fs::remove_file(&tmp_path);
        }
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

#[derive(Clone)]
struct Theme {
    name: String,
    foreground: RGBA,
    background: RGBA,
    cursor: RGBA,
    cursor_foreground: RGBA,
    palette: [RGBA; 16],
}

fn parse_palette(hex: [&str; 16]) -> [RGBA; 16] {
    hex.map(|s| RGBA::parse(s).unwrap())
}

fn builtin_themes() -> Vec<Theme> {
    vec![
        Theme {
            name: "default".into(),
            foreground: RGBA::parse("#f8f7e9").unwrap(),
            background: RGBA::parse("#121616").unwrap(),
            cursor: RGBA::parse("#7fb80e").unwrap(),
            cursor_foreground: RGBA::parse("#1b315e").unwrap(),
            palette: parse_palette([
                "#130c0e", "#ed1941", "#45b97c", "#fdb933",
                "#2585a6", "#ae5039", "#009ad6", "#fffef9",
                "#7c8577", "#f05b72", "#84bf96", "#ffc20e",
                "#7bbfea", "#f58f98", "#33a3dc", "#f6f5ec",
            ]),
        },
        Theme {
            name: "light".into(),
            foreground: RGBA::parse("#2e3440").unwrap(),
            background: RGBA::parse("#eceff4").unwrap(),
            cursor: RGBA::parse("#4c566a").unwrap(),
            cursor_foreground: RGBA::parse("#eceff4").unwrap(),
            palette: parse_palette([
                "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b",
                "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
                "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b",
                "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4",
            ]),
        },
        Theme {
            name: "solarized-dark".into(),
            foreground: RGBA::parse("#839496").unwrap(),
            background: RGBA::parse("#002b36").unwrap(),
            cursor: RGBA::parse("#93a1a1").unwrap(),
            cursor_foreground: RGBA::parse("#002b36").unwrap(),
            palette: parse_palette([
                "#073642", "#dc322f", "#859900", "#b58900",
                "#268bd2", "#d33682", "#2aa198", "#eee8d5",
                "#002b36", "#cb4b16", "#586e75", "#657b83",
                "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
            ]),
        },
        Theme {
            name: "solarized-light".into(),
            foreground: RGBA::parse("#657b83").unwrap(),
            background: RGBA::parse("#fdf6e3").unwrap(),
            cursor: RGBA::parse("#586e75").unwrap(),
            cursor_foreground: RGBA::parse("#fdf6e3").unwrap(),
            palette: parse_palette([
                "#073642", "#dc322f", "#859900", "#b58900",
                "#268bd2", "#d33682", "#2aa198", "#eee8d5",
                "#002b36", "#cb4b16", "#586e75", "#657b83",
                "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
            ]),
        },
        Theme {
            name: "gruvbox-dark".into(),
            foreground: RGBA::parse("#ebdbb2").unwrap(),
            background: RGBA::parse("#282828").unwrap(),
            cursor: RGBA::parse("#ebdbb2").unwrap(),
            cursor_foreground: RGBA::parse("#282828").unwrap(),
            palette: parse_palette([
                "#282828", "#cc241d", "#98971a", "#d79921",
                "#458588", "#b16286", "#689d6a", "#a89984",
                "#928374", "#fb4934", "#b8bb26", "#fabd2f",
                "#83a598", "#d3869b", "#8ec07c", "#ebdbb2",
            ]),
        },
        Theme {
            name: "gruvbox-light".into(),
            foreground: RGBA::parse("#3c3836").unwrap(),
            background: RGBA::parse("#fbf1c7").unwrap(),
            cursor: RGBA::parse("#3c3836").unwrap(),
            cursor_foreground: RGBA::parse("#fbf1c7").unwrap(),
            palette: parse_palette([
                "#fbf1c7", "#cc241d", "#98971a", "#d79921",
                "#458588", "#b16286", "#689d6a", "#7c6f64",
                "#928374", "#9d0006", "#79740e", "#b57614",
                "#076678", "#8f3f71", "#427b58", "#3c3836",
            ]),
        },
        Theme {
            name: "dracula".into(),
            foreground: RGBA::parse("#f8f8f2").unwrap(),
            background: RGBA::parse("#282a36").unwrap(),
            cursor: RGBA::parse("#f8f8f2").unwrap(),
            cursor_foreground: RGBA::parse("#282a36").unwrap(),
            palette: parse_palette([
                "#21222c", "#ff5555", "#50fa7b", "#f1fa8c",
                "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
                "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5",
                "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
            ]),
        },
        Theme {
            name: "nord".into(),
            foreground: RGBA::parse("#d8dee9").unwrap(),
            background: RGBA::parse("#2e3440").unwrap(),
            cursor: RGBA::parse("#d8dee9").unwrap(),
            cursor_foreground: RGBA::parse("#2e3440").unwrap(),
            palette: parse_palette([
                "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b",
                "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
                "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b",
                "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4",
            ]),
        },
    ]
}

fn create_terminal(config: &Config) -> Terminal {
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

fn show_rename_dialog(window: &adw::ApplicationWindow, label: &Label, custom_title: Rc<Cell<bool>>) {
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

fn show_rename_dialog_with_strip(
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
    /// Update which tab strip button is :checked to match the active notebook page.
    fn sync_tab_strip_active(&self, active_page: Option<u32>) {
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

    /// Remove the tab strip button that corresponds to a notebook page widget.
    fn remove_strip_button_for(&self, widget: &gtk4::Widget) {
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
        self.remove_strip_button_for(widget);
        if let Some(page_num) = self.notebook.page_num(widget) {
            self.notebook.remove_page(Some(page_num));
        }
        if self.notebook.n_pages() == 0 {
            self.window.destroy();
        } else {
            self.sync_tab_strip_active(None);
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

    fn for_each_terminal(&self, f: impl Fn(&Terminal)) {
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

    fn apply_colors_all(&self) {
        let config = self.config.borrow();
        let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
        self.for_each_terminal(|term| {
            term.set_colors(Some(&config.foreground), Some(&config.background), &palette_refs);
            term.set_color_bold(None);
            term.set_color_cursor(Some(&config.cursor));
            term.set_color_cursor_foreground(Some(&config.cursor_foreground));
        });
    }

    fn apply_font_all(&self) {
        let config = self.config.borrow();
        let font_desc = FontDescription::from_string(&config.font_desc);
        self.for_each_terminal(|term| {
            term.set_font(Some(&font_desc));
        });
    }

    fn apply_scrollback_all(&self) {
        let lines = self.config.borrow().terminal_scrollback_lines;
        self.for_each_terminal(|term| {
            term.set_scrollback_lines(lines as i64);
        });
    }

    fn apply_theme(&self, theme: &Theme) {
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
            // Remove the strip button for the current page's widget
            if let Some(widget) = self.notebook.nth_page(Some(page_num)) {
                self.remove_strip_button_for(&widget);
            }
            self.notebook.remove_page(Some(page_num));
            if self.notebook.n_pages() == 0 {
                self.window.destroy();
            } else {
                self.sync_tab_strip_active(None);
                self.focus_current_terminal();
            }
        }
    }

    fn close_focused_pane_or_tab(&self) {
        if let Some(page_num) = self.notebook.current_page() {
            if let Some(widget) = self.notebook.nth_page(Some(page_num)) {
                // If the page has splits, close the focused pane only
                if widget.clone().downcast::<Paned>().is_ok() {
                    if let Some(term) = find_focused_terminal(&widget) {
                        self.handle_terminal_exited(&term.upcast::<gtk4::Widget>());
                        return;
                    }
                }
            }
        }
        self.remove_current_tab();
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

    fn toggle_keybindings_panel(&self) {
        if let Some(dialog) = self.keybindings_dialog.borrow_mut().take() {
            dialog.force_close();
            return;
        }

        const KEYBINDINGS: &[(&str, &str)] = &[
            ("Ctrl+Shift+T", "New tab"),
            ("Ctrl+Shift+W", "Close focused pane or tab"),
            ("Ctrl+Shift+C", "Copy"),
            ("Ctrl+Shift+V", "Paste"),
            ("Ctrl+Shift++", "Font size increase"),
            ("Ctrl+Shift+I", "Font size decrease"),
            ("Ctrl+Shift+J", "Opacity decrease"),
            ("Ctrl+Shift+K", "Opacity increase"),
            ("Ctrl+Shift+F", "Toggle search"),
            ("Ctrl+Shift+O", "Toggle settings panel"),
            ("Ctrl+Shift+P", "Toggle keybindings panel"),
            ("Ctrl+Shift+E", "Split horizontal"),
            ("Ctrl+Shift+D", "Split vertical"),
            ("Ctrl+Shift+PageUp", "Previous tab"),
            ("Ctrl+Shift+PageDown", "Next tab"),
            ("Ctrl+Shift+Tab", "Previous tab"),
            ("Ctrl+W", "Close tab"),
            ("Ctrl+Tab", "Next tab"),
            ("Ctrl+Up", "Scroll up"),
            ("Ctrl+Down", "Scroll down"),
            ("Ctrl+-", "Font size decrease"),
            ("Ctrl+PageUp", "Previous tab"),
            ("Ctrl+PageDown", "Next tab"),
            ("Ctrl+0~9", "Quick switch to tab N"),
            ("Alt+Tab", "Cycle pane focus forward"),
            ("Alt+Shift+Tab", "Cycle pane focus backward"),
            ("Double-click tab", "Rename tab"),
            ("Ctrl+Click link", "Open hyperlink"),
        ];

        let dialog = adw::Dialog::builder()
            .title("Keybindings")
            .content_width(480)
            .content_height(420)
            .build();

        let header_bar = adw::HeaderBar::new();
        let filter_entry = SearchEntry::new();
        filter_entry.set_placeholder_text(Some("Search keybindings..."));
        filter_entry.set_hexpand(true);

        let list_box = ListBox::new();
        list_box.set_selection_mode(gtk4::SelectionMode::None);
        list_box.add_css_class("boxed-list");
        list_box.set_margin_start(12);
        list_box.set_margin_end(12);
        list_box.set_margin_bottom(12);

        for &(shortcut, description) in KEYBINDINGS {
            let row = adw::ActionRow::builder()
                .title(description)
                .build();
            let key_label = Label::new(Some(shortcut));
            key_label.add_css_class("dim-label");
            row.add_suffix(&key_label);
            list_box.append(&row);
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
        let list_box_clone = list_box.clone();
        filter_entry.connect_search_changed(move |entry| {
            let query = entry.text().to_string().to_lowercase();
            let mut idx = 0;
            while let Some(row) = list_box_clone.row_at_index(idx) {
                if query.is_empty() {
                    row.set_visible(true);
                } else {
                    let shortcut = KEYBINDINGS[idx as usize].0.to_lowercase();
                    let desc = KEYBINDINGS[idx as usize].1.to_lowercase();
                    row.set_visible(shortcut.contains(&query) || desc.contains(&query));
                }
                idx += 1;
            }
        });

        // Key controller: Escape / Ctrl+Shift+P to close
        let key_controller = EventControllerKey::new();
        key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let dialog_ref = self.keybindings_dialog.clone();
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
            false.into()
        });
        dialog.add_controller(key_controller);

        // Clear tracking when dialog is closed
        let dialog_ref = self.keybindings_dialog.clone();
        dialog.connect_closed(move |_| {
            *dialog_ref.borrow_mut() = None;
        });

        *self.keybindings_dialog.borrow_mut() = Some(dialog.clone());
        dialog.present(Some(&self.window));
        filter_entry.grab_focus();
    }

    fn toggle_settings_panel(&self) {
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

    fn create_split_terminal(&self, working_directory: Option<&str>) -> Terminal {
        let terminal = create_terminal(&self.config.borrow());
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
                            // Transfer widget name so strip button mapping is preserved
                            paned.set_widget_name(&page_widget.widget_name());
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

    fn cycle_pane_focus(&self, direction: i32) {
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

    fn add_new_tab(&self, working_directory: Option<String>, tab_name: Option<String>) -> Terminal {
        let tab_num = self.tab_counter.get();
        self.tab_counter.set(tab_num + 1);

        let terminal = create_terminal(&self.config.borrow());

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
        // We'll also keep a reference to the strip button so its label can be synced.
        let strip_btn_label: Rc<RefCell<Option<Label>>> = Rc::new(RefCell::new(None));
        let strip_btn_label_for_pwd = strip_btn_label.clone();
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
                // Also update the tab strip button label
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
        // Force tabs hidden — GTK may re-show them after page insertion
        self.notebook.set_show_tabs(false);

        // Create tab strip toggle button
        let strip_label = Label::new(Some(&label_text));
        strip_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        strip_label.set_max_width_chars(24);
        *strip_btn_label.borrow_mut() = Some(strip_label.clone());

        let strip_btn = ToggleButton::new();
        strip_btn.set_child(Some(&strip_label));
        strip_btn.add_css_class("tab-strip-btn");
        strip_btn.add_css_class("flat");
        strip_btn.set_active(true); // new tab is current
        strip_btn.set_focus_on_click(false);
        strip_btn.set_can_focus(false);
        strip_btn.set_hexpand(false);
        // Give button a unique name to correlate with notebook page
        strip_btn.set_widget_name(&format!("tab-{}", tab_num));
        // Also name the terminal widget so we can find the button when removing
        terminal.set_widget_name(&format!("tab-{}", tab_num));

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

        // Focus the new terminal
        terminal.grab_focus();

        terminal
    }
}

fn main() -> glib::ExitCode {
    init_logging();

    // Ensure fcitx5 GTK4 IM module is discoverable at runtime.
    // FCITX5_GTK_PATH is baked in at compile time (set by nix develop shellHook).
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

    // Shell selection is handled per-terminal spawn:
    // - prefer fish if available
    // - if bass works, import ~/.bashrc before showing the prompt
    // - otherwise fall back to plain fish, and if fish is missing then bash

    let app = adw::Application::builder().application_id("app.jterm4").build();

    app.connect_activate(|app| {
        let (config, themes) = load_config();

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
            ".tab-strip-btn { padding: 2px 8px; border-radius: 4px; }
             .tab-strip-btn:checked { font-weight: bold; }
             .tab-bar-box { padding: 2px 4px; }
             .hidden-tabs > header { min-height: 0; border: none; background: none; padding: 0; margin: 0; }
             .hidden-tabs > header > * { min-height: 0; min-width: 0; padding: 0; margin: 0; }",
        );
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().expect("display"),
            &css_provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        // Custom tab bar: [scrollable tab strip] [+] [close]
        let tab_strip = gtk4::Box::new(Orientation::Horizontal, 2);
        tab_strip.set_hexpand(false);
        tab_strip.set_halign(gtk4::Align::Start);

        let scrolled_tabs = ScrolledWindow::builder()
            .hexpand(true)
            .hscrollbar_policy(gtk4::PolicyType::Automatic)
            .vscrollbar_policy(gtk4::PolicyType::Never)
            .child(&tab_strip)
            .build();

        let add_tab_button = gtk4::Button::with_label("+");
        add_tab_button.set_focus_on_click(false);
        add_tab_button.set_can_focus(false);
        add_tab_button.set_tooltip_text(Some("New tab (Ctrl+Shift+T)"));
        add_tab_button.add_css_class("flat");
        add_tab_button.set_hexpand(false);

        let close_window_button = gtk4::Button::from_icon_name("window-close-symbolic");
        close_window_button.set_focus_on_click(false);
        close_window_button.set_can_focus(false);
        close_window_button.set_tooltip_text(Some("Close window"));
        close_window_button.add_css_class("flat");
        close_window_button.set_hexpand(false);

        let tab_bar_box = gtk4::Box::new(Orientation::Horizontal, 4);
        tab_bar_box.add_css_class("tab-bar-box");
        tab_bar_box.append(&scrolled_tabs);
        tab_bar_box.append(&add_tab_button);
        tab_bar_box.append(&close_window_button);

        // Main layout: tab bar + notebook + search bar
        let main_box = gtk4::Box::new(Orientation::Vertical, 0);
        main_box.append(&tab_bar_box);
        main_box.append(&notebook);
        main_box.append(&search_bar);

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
            keybindings_dialog: Rc::new(RefCell::new(None)),
            settings_dialog: Rc::new(RefCell::new(None)),
        });

        // Wire "+" button
        let ui_for_add = ui.clone();
        add_tab_button.connect_clicked(move |_| {
            ui_for_add.add_new_tab(None, None);
        });

        // Wire close-window button
        let window_for_close = window.clone();
        close_window_button.connect_clicked(move |_| {
            window_for_close.close();
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
                        log::info!("Close focused pane or tab");
                        ui_clone.close_focused_pane_or_tab();
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
                    Key::plus => {
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
                        ui_clone.window_opacity
                            .set((ui_clone.window_opacity.get() - opacity_step).clamp(0.01, 1.0));
                        ui_clone.window.set_opacity(ui_clone.window_opacity.get());
                        return true.into();
                    }
                    Key::K | Key::k => {
                        log::debug!("Opacity increase");
                        ui_clone.window_opacity
                            .set((ui_clone.window_opacity.get() + opacity_step).clamp(0.01, 1.0));
                        ui_clone.window.set_opacity(ui_clone.window_opacity.get());
                        return true.into();
                    }
                    Key::F | Key::f => {
                        log::debug!("Toggle search");
                        ui_clone.toggle_search();
                        return true.into();
                    }
                    Key::P | Key::p => {
                        log::debug!("Toggle keybindings panel");
                        ui_clone.toggle_keybindings_panel();
                        return true.into();
                    }
                    Key::O | Key::o => {
                        log::debug!("Toggle settings panel");
                        ui_clone.toggle_settings_panel();
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
                    Key::Tab | Key::ISO_Left_Tab => {
                        ui_clone.switch_tab(-1);
                        return true.into();
                    }
                    _ => {}
                }
            }

            if state.contains(ModifierType::CONTROL_MASK) && !state.contains(ModifierType::SHIFT_MASK) {
                match keyval {
                    Key::W | Key::w => {
                        log::info!("Close tab (Ctrl+W)");
                        ui_clone.remove_current_tab();
                        return true.into();
                    }
                    Key::Tab => {
                        ui_clone.switch_tab(1);
                        return true.into();
                    }
                    Key::Up => {
                        if let Some(ref term) = current_terminal {
                            if let Some(adj) = term.vadjustment() {
                                let new_val = (adj.value() - adj.step_increment() * 3.0).max(adj.lower());
                                adj.set_value(new_val);
                            }
                        }
                        return true.into();
                    }
                    Key::Down => {
                        if let Some(ref term) = current_terminal {
                            if let Some(adj) = term.vadjustment() {
                                let max_val = adj.upper() - adj.page_size();
                                let new_val = (adj.value() + adj.step_increment() * 3.0).min(max_val);
                                adj.set_value(new_val);
                            }
                        }
                        return true.into();
                    }
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
                    // Ctrl+0~9: quick switch to tab N (0-indexed, Ctrl+9 = last)
                    Key::_0 | Key::_1 | Key::_2 | Key::_3 | Key::_4 | Key::_5
                    | Key::_6 | Key::_7 | Key::_8 | Key::_9 => {
                        let n_pages = ui_clone.notebook.n_pages();
                        if n_pages > 0 {
                            let target = if keyval == Key::_9 {
                                n_pages - 1
                            } else {
                                let idx = keyval.into_glib() - Key::_0.into_glib();
                                idx.min(n_pages - 1)
                            };
                            ui_clone.notebook.set_current_page(Some(target));
                        }
                        return true.into();
                    }
                    _ => {}
                }
            }

            // Alt+Tab / Alt+Shift+Tab: cycle pane focus
            if state.contains(ModifierType::ALT_MASK) && !state.contains(ModifierType::CONTROL_MASK) {
                if matches!(keyval, Key::Tab | Key::ISO_Left_Tab) {
                    if state.contains(ModifierType::SHIFT_MASK) {
                        ui_clone.cycle_pane_focus(-1);
                    } else {
                        ui_clone.cycle_pane_focus(1);
                    }
                    return true.into();
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

        // Focus terminal when switching tabs (split-aware) and sync tab strip
        let ui_for_switch = ui.clone();
        notebook.connect_switch_page(move |_, widget, page_num| {
            if let Some(term) = find_first_terminal(widget) {
                term.grab_focus();
            }
            ui_for_switch.sync_tab_strip_active(Some(page_num));
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

        window.set_content(Some(&main_box));
        window.show();

        // Focus the active terminal after window is shown
        ui.focus_current_terminal();
    });

    app.run()
}
