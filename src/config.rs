use gtk4::gdk::RGBA;
use gtk4::glib;
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use crate::keybindings::KeybindingMap;

// ---------------------------------------------------------------------------
// Terminal Mode
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum TerminalMode {
    Block,
    Vte,
}

// ---------------------------------------------------------------------------
// Remote host
// ---------------------------------------------------------------------------

/// A saved SSH target. A new tab can be opened that runs the remote shell over
/// `ssh -t`, reusing all local PTY/terminal infrastructure (OSC 133 markers
/// emitted by the remote shell flow through ssh, so block mode works remotely).
#[derive(Clone, Debug)]
pub struct RemoteHost {
    pub name: String,
    pub host: String,
    pub user: Option<String>,
    /// Shell to launch on the remote side (default "rsh").
    pub remote_shell: String,
    /// Stable session id passed to the remote rsh for resume-on-reconnect.
    pub session: Option<String>,
    /// Extra flags inserted before the target (e.g. ["-p", "2222"]).
    pub ssh_args: Vec<String>,
}

/// Build the local argv that connects to a remote host via ssh.
/// Produces e.g. `["ssh", "-t", "-p", "2222", "mm@100.x.x.x", "rsh --session home-main"]`.
pub(crate) fn build_remote_argv(host: &RemoteHost) -> Vec<String> {
    let target = match &host.user {
        Some(u) => format!("{u}@{}", host.host),
        None => host.host.clone(),
    };
    let mut remote_cmd = host.remote_shell.clone();
    if let Some(sid) = &host.session {
        remote_cmd.push_str(" --session ");
        remote_cmd.push_str(sid);
    }
    let mut argv = vec!["ssh".to_string(), "-t".to_string()];
    argv.extend(host.ssh_args.iter().cloned());
    argv.push(target);
    argv.push(remote_cmd);
    argv
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Config {
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
    /// Explicit shell path (overrides auto-detection). Useful when PATH is stripped by launchers.
    pub(crate) shell: Option<String>,
    /// Commands to feed to new shells on startup (comma-separated).
    pub(crate) startup_commands: Option<String>,
    pub(crate) terminal_mode: TerminalMode,
    // Block view optimizations
    pub(crate) ansi_cache_capacity: u32,
    pub(crate) max_visible_blocks: u32,
    pub(crate) output_batch_min_ms: u32,
    pub(crate) output_batch_max_ms: u32,
    pub(crate) lazy_load_threshold: u32,
    pub(crate) truncation_threshold_lines: u32,
    pub(crate) max_collapsed_output_lines: u32,
    pub(crate) virtual_scroll_margin: u32,
    pub(crate) block_history_path: Option<String>,
    pub(crate) block_history_compress: bool,
    pub(crate) editor_input: bool,
    /// Saved SSH targets selectable from the context menu.
    pub(crate) remote_hosts: Vec<RemoteHost>,
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
    shell: Option<String>,
    /// Commands to run when a new tab opens (comma-separated, e.g. "cd ~/project, nix develop").
    startup_commands: Option<String>,
    terminal_mode: Option<String>,
    // Block view optimizations
    ansi_cache_capacity: Option<u32>,
    max_visible_blocks: Option<u32>,
    output_batch_min_ms: Option<u32>,
    output_batch_max_ms: Option<u32>,
    lazy_load_threshold: Option<u32>,
    truncation_threshold_lines: Option<u32>,
    max_collapsed_output_lines: Option<u32>,
    virtual_scroll_margin: Option<u32>,
    block_history_path: Option<String>,
    block_history_compress: Option<bool>,
    editor_input: Option<bool>,
    remote_hosts: Vec<RemoteHost>,
}

fn load_file_config() -> FileConfig {
    let path = config_file_path();
    let Ok(contents) = fs::read_to_string(&path) else {
        return FileConfig { remote_hosts: default_remote_hosts(), ..Default::default() };
    };
    let Ok(table) = contents.parse::<toml::Table>() else {
        log::warn!("Failed to parse config file {}", path.display());
        return FileConfig { remote_hosts: default_remote_hosts(), ..Default::default() };
    };

    let colors = table.get("colors").and_then(|v| v.as_table());
    let remote_hosts = parse_remote_hosts(&table);

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
        shell: table.get("shell").and_then(|v| v.as_str()).map(|s| s.to_string()),
        startup_commands: table.get("startup_commands").and_then(|v| v.as_str()).map(|s| s.to_string()),
        terminal_mode: table.get("terminal_mode").and_then(|v| v.as_str()).map(|s| s.to_string()),
        ansi_cache_capacity: table.get("ansi_cache_capacity").and_then(|v| v.as_integer()).map(|v| v as u32),
        max_visible_blocks: table.get("max_visible_blocks").and_then(|v| v.as_integer()).map(|v| v as u32),
        output_batch_min_ms: table.get("output_batch_min_ms").and_then(|v| v.as_integer()).map(|v| v as u32),
        output_batch_max_ms: table.get("output_batch_max_ms").and_then(|v| v.as_integer()).map(|v| v as u32),
        lazy_load_threshold: table.get("lazy_load_threshold").and_then(|v| v.as_integer()).map(|v| v as u32),
        truncation_threshold_lines: table.get("truncation_threshold_lines").and_then(|v| v.as_integer()).map(|v| v as u32),
        max_collapsed_output_lines: table.get("max_collapsed_output_lines").and_then(|v| v.as_integer()).map(|v| v as u32),
        virtual_scroll_margin: table.get("virtual_scroll_margin").and_then(|v| v.as_integer()).map(|v| v as u32),
        block_history_path: table.get("block_history_path").and_then(|v| v.as_str()).map(|s| s.to_string()),
        block_history_compress: table.get("block_history_compress").and_then(|v| v.as_bool()),
        editor_input: table.get("editor_input").and_then(|v| v.as_bool()),
        remote_hosts,
    }
}

/// Parse `[[remote_hosts]]` array-of-tables. Entries missing a `host` are skipped.
fn parse_remote_hosts(table: &toml::Table) -> Vec<RemoteHost> {
    let Some(arr) = table.get("remote_hosts").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_table())
        .filter_map(|t| {
            let host = t.get("host").and_then(|v| v.as_str())?.to_string();
            let name = t.get("name").and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| host.clone());
            let user = t.get("user").and_then(|v| v.as_str()).map(|s| s.to_string());
            let remote_shell = t.get("remote_shell").and_then(|v| v.as_str())
                .unwrap_or("rsh")
                .to_string();
            let session = t.get("session").and_then(|v| v.as_str()).map(|s| s.to_string());
            let ssh_args = t.get("ssh_args").and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default();
            Some(RemoteHost { name, host, user, remote_shell, session, ssh_args })
        })
        .collect()
}

/// Built-in remote hosts used when no config file exists yet, so a fresh
/// install can connect without hand-writing `[[remote_hosts]]` first.
fn default_remote_hosts() -> Vec<RemoteHost> {
    vec![
        RemoteHost {
            name: "cloud-dev".into(),
            host: "10.21.31.17".into(),
            user: Some("root".into()),
            // Full path: a non-interactive ssh PATH resolves bare `rsh` to the
            // system ssh-alternative, not the block-mode rsh in ~/.cargo/bin.
            remote_shell: "/root/.cargo/bin/rsh".into(),
            session: Some("cloud-test".into()),
            ssh_args: Vec::new(),
        },
        RemoteHost {
            name: "localhost-test".into(),
            host: "localhost".into(),
            user: Some("mm".into()),
            remote_shell: "rsh".into(),
            session: Some("local-test".into()),
            ssh_args: Vec::new(),
        },
    ]
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
        // Use the "Mono" (NFM) Nerd Font variant: the plain "Nerd Font" (NF)
        // variant renders proportionally in VTE (glyphs draw at non-cell widths)
        // even though fontconfig reports it spacing=100, so output never aligns
        // like a real terminal. NFM forces single-cell glyphs.
        .unwrap_or_else(|| "SauceCodePro Nerd Font Mono 14".to_string());

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

    // Block view optimization settings
    let ansi_cache_capacity = env_u32("JTERM4_ANSI_CACHE_CAP")
        .or(fc.ansi_cache_capacity)
        .unwrap_or(256)
        .max(1);
    let max_visible_blocks = env_u32("JTERM4_MAX_BLOCKS")
        .or(fc.max_visible_blocks)
        .unwrap_or(200);
    let output_batch_min_ms = env_u32("JTERM4_BATCH_MIN")
        .or(fc.output_batch_min_ms)
        .unwrap_or(10);
    let output_batch_max_ms = env_u32("JTERM4_BATCH_MAX")
        .or(fc.output_batch_max_ms)
        .unwrap_or(100);
    let lazy_load_threshold = env_u32("JTERM4_LAZY_LINES")
        .or(fc.lazy_load_threshold)
        .unwrap_or(1000);
    let truncation_threshold_lines = env_u32("JTERM4_TRUNCATION_LINES")
        .or(fc.truncation_threshold_lines)
        .unwrap_or(50000);
    let max_collapsed_output_lines = env_u32("JTERM4_MAX_COLLAPSED_LINES")
        .or(fc.max_collapsed_output_lines)
        .unwrap_or(25);
    let virtual_scroll_margin = env_u32("JTERM4_VSCROLL_MARGIN")
        .or(fc.virtual_scroll_margin)
        .unwrap_or(1);
    let block_history_path = std::env::var("JTERM4_HISTORY_PATH").ok()
        .or(fc.block_history_path);
    let block_history_compress = fc.block_history_compress.unwrap_or(true);
    let shell = std::env::var("JTERM4_SHELL").ok().or(fc.shell);

    // Parse terminal mode (default: block)
    let terminal_mode_str = env_string("JTERM4_MODE")
        .or(fc.terminal_mode)
        .unwrap_or_else(|| "block".to_string());
    let terminal_mode = match terminal_mode_str.to_lowercase().as_str() {
        "vte" => TerminalMode::Vte,
        _ => TerminalMode::Block,
    };

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
        shell,
        startup_commands: fc.startup_commands,
        terminal_mode,
        ansi_cache_capacity,
        max_visible_blocks,
        output_batch_min_ms,
        output_batch_max_ms,
        lazy_load_threshold,
        truncation_threshold_lines,
        max_collapsed_output_lines,
        virtual_scroll_margin,
        block_history_path,
        block_history_compress,
        editor_input: fc.editor_input.unwrap_or(true),
        remote_hosts: fc.remote_hosts,
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
    table.insert("terminal_mode".into(), toml::Value::String(match config.terminal_mode {
        TerminalMode::Block => "block",
        TerminalMode::Vte => "vte",
    }.to_string()));

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

pub(crate) fn choose_shell_argv(configured_shell: Option<&str>) -> Vec<String> {
    // Explicit config / env var wins (needed when PATH is stripped by launchers like wofi).
    if let Some(path) = configured_shell {
        if is_executable(Path::new(path)) {
            return vec![path.to_string()];
        }
        log::warn!("Configured shell '{}' is not executable, falling back to auto-detection", path);
    }

    // Prefer rsh when it's on PATH.
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
