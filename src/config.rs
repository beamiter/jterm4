use gtk4::gdk::RGBA;
use gtk4::glib;
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use crate::keybindings::KeybindingMap;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct Config {
    pub(crate) window_opacity: f64,
    pub(crate) terminal_scrollback_lines: u32,
    pub(crate) font_desc: String,
    pub(crate) default_font_scale: f64,
    pub(crate) theme_name: String,
    pub(crate) foreground: RGBA,
    pub(crate) background: RGBA,
    pub(crate) cursor: RGBA,
    pub(crate) cursor_foreground: RGBA,
    pub(crate) palette: [RGBA; 16],
    /// Commands to feed to new shells on startup (comma-separated).
    pub(crate) startup_commands: Option<String>,
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct Theme {
    pub(crate) name: String,
    pub(crate) foreground: RGBA,
    pub(crate) background: RGBA,
    pub(crate) cursor: RGBA,
    pub(crate) cursor_foreground: RGBA,
    pub(crate) palette: [RGBA; 16],
}

fn parse_palette(hex: [&str; 16]) -> [RGBA; 16] {
    hex.map(|s| RGBA::parse(s).unwrap())
}

pub(crate) fn builtin_themes() -> Vec<Theme> {
    thread_local! {
        static CACHED: RefCell<Option<Vec<Theme>>> = const { RefCell::new(None) };
    }
    if let Some(themes) = CACHED.with(|c| c.borrow().clone()) {
        return themes;
    }
    let themes = vec![
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
    ];
    CACHED.with(|c| *c.borrow_mut() = Some(themes.clone()));
    themes
}

// ---------------------------------------------------------------------------
// Env helpers
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// File config
// ---------------------------------------------------------------------------

pub(crate) fn config_file_path() -> PathBuf {
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
    keybindings: Option<toml::Table>,
    /// Commands to run when a new tab opens (comma-separated, e.g. "cd ~/project, nix develop").
    startup_commands: Option<String>,
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
        keybindings: table.get("keybindings").and_then(|v| v.as_table()).cloned(),
        startup_commands: table.get("startup_commands").and_then(|v| v.as_str()).map(|s| s.to_string()),
    }
}

// ---------------------------------------------------------------------------
// load_config
// ---------------------------------------------------------------------------

pub(crate) fn load_config() -> (Config, Vec<Theme>, KeybindingMap) {
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
        .unwrap_or_else(|| "SauceCodePro Nerd Font Regular 14".to_string());

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
        startup_commands: fc.startup_commands,
    };

    let mut keybinding_map = KeybindingMap::from_defaults();
    if let Some(ref kb_table) = fc.keybindings {
        keybinding_map.apply_user_overrides(kb_table);
    }

    (config, themes, keybinding_map)
}

// ---------------------------------------------------------------------------
// save_config
// ---------------------------------------------------------------------------

pub(crate) fn rgba_to_hex(c: &RGBA) -> String {
    format!("#{:02x}{:02x}{:02x}",
        (c.red() * 255.0) as u8,
        (c.green() * 255.0) as u8,
        (c.blue() * 255.0) as u8)
}

pub(crate) fn save_config(config: &Config) {
    let path = config_file_path();
    if let Some(parent) = path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            log::warn!("Failed to create config dir {}: {err}", parent.display());
            return;
        }
    }

    // Read existing config to preserve user-authored sections (e.g. [keybindings])
    let mut table = fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.parse::<toml::Table>().ok())
        .unwrap_or_default();

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

// ---------------------------------------------------------------------------
// Shell selection
// ---------------------------------------------------------------------------

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

pub(crate) fn choose_shell_argv() -> Vec<String> {
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
