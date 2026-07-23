use gtk4::gdk::RGBA;
use gtk4::glib;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::keybindings::{KeyCombo, KeybindingMap};

// ---------------------------------------------------------------------------
// Terminal Mode
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum TerminalMode {
    Block,
    Vte,
}

// ---------------------------------------------------------------------------
// Tab placement
// ---------------------------------------------------------------------------

/// Where the custom tab bar is shown: down the left sidebar (vertical) or
/// along the top bar (horizontal).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabPlacement {
    Sidebar,
    TopBar,
}

impl TabPlacement {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TabPlacement::Sidebar => "sidebar",
            TabPlacement::TopBar => "top",
        }
    }

    pub(crate) fn parse(s: &str) -> TabPlacement {
        match s.to_lowercase().as_str() {
            "top" | "topbar" | "top_bar" => TabPlacement::TopBar,
            _ => TabPlacement::Sidebar,
        }
    }
}

fn resolve_sidebar_visibility(explicit: Option<bool>, placement: TabPlacement) -> bool {
    explicit.unwrap_or(placement == TabPlacement::Sidebar)
}

/// Which single view the sidebar shows (tab list vs file tree).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarView {
    Tabs,
    Files,
}

impl SidebarView {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SidebarView::Tabs => "tabs",
            SidebarView::Files => "files",
        }
    }

    pub(crate) fn parse(s: &str) -> SidebarView {
        match s.to_lowercase().as_str() {
            "files" | "file" | "filetree" | "file_tree" => SidebarView::Files,
            _ => SidebarView::Tabs,
        }
    }
}

// ---------------------------------------------------------------------------
// Remote host
// ---------------------------------------------------------------------------

/// A saved SSH target. A new tab can be opened that runs the remote shell over
/// `ssh -t`, reusing all local PTY/terminal infrastructure. OSC 133 markers
/// emitted by the remote shell flow through ssh are preserved so session-aware
/// terminal behavior keeps working for remote tabs.
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
    /// Run the remote command through a login shell (`bash -lc 'exec ...'`) so the
    /// user's profile (PATH, ~/.cargo/env, etc.) is loaded. ssh's plain command
    /// channel runs a non-login, non-interactive shell, which leaves tools like
    /// cargo off PATH. Defaults to true.
    pub login_shell: bool,
    /// Reuse one ssh connection for repeat tabs to this host (ControlMaster), so
    /// the 2nd+ tab skips the handshake/auth. Defaults to true.
    pub multiplex: bool,
}

/// Directory for ssh ControlMaster sockets. Prefers `$XDG_RUNTIME_DIR`, falls
/// back to `~/.cache/jterm4`. Created if missing.
fn control_socket_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/jterm4")))?;
    if let Err(err) = fs::create_dir_all(&base) {
        log::warn!(
            "Failed to create ssh control socket dir {}: {err}",
            base.display()
        );
        return None;
    }
    Some(base)
}

fn shell_single_quote(s: &str) -> String {
    let mut quoted = String::with_capacity(s.len() + 2);
    quoted.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            quoted.push_str("'\"'\"'");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

fn wrap_exec_in_login_bash(command: &str) -> String {
    format!("bash -lc 'exec {}'", command.replace('\'', "'\\''"))
}

fn wrap_rsh_argv_in_interactive_bash(rsh_path: &str) -> Option<Vec<String>> {
    let bash_path = find_executable_in_path("bash")?;
    Some(vec![
        bash_path.to_string_lossy().to_string(),
        "-ic".to_string(),
        format!("exec {}", shell_single_quote(rsh_path)),
    ])
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
    if host.login_shell {
        remote_cmd = wrap_exec_in_login_bash(&remote_cmd);
    }
    let mut argv = vec!["ssh".to_string(), "-t".to_string()];
    if host.multiplex {
        if let Some(dir) = control_socket_dir() {
            // %C is ssh's hash of (local user, host, port, user) — a safe filename.
            let ctl_path = dir.join("cm-%C");
            argv.push("-o".to_string());
            argv.push("ControlMaster=auto".to_string());
            argv.push("-o".to_string());
            argv.push("ControlPersist=120".to_string());
            argv.push("-o".to_string());
            argv.push(format!("ControlPath={}", ctl_path.display()));
        }
    }
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
    /// Where the tab bar is shown (left sidebar vs top bar).
    pub(crate) tab_placement: TabPlacement,
    /// Which single view the sidebar shows (tab list vs file tree).
    pub(crate) sidebar_view: SidebarView,
    /// Whether the left sidebar is visible. When absent from an older config,
    /// startup derives the default from tab placement: open for sidebar tabs,
    /// closed for top-bar tabs.
    pub(crate) sidebar_visible: bool,
    /// Sidebar width in pixels (resizable divider position).
    pub(crate) sidebar_width: u32,
    // Block view optimizations
    pub(crate) max_visible_blocks: u32,
    pub(crate) lazy_load_threshold: u32,
    pub(crate) truncation_threshold_lines: u32,
    /// Output rows shown before a finished block is considered long and gains
    /// top/bottom navigation controls.
    pub(crate) finished_block_viewport_rows: u32,
    #[allow(dead_code)]
    pub(crate) max_collapsed_output_lines: u32,
    pub(crate) virtual_scroll_margin: u32,
    /// Lightweight JSONL command index. Unlike block snapshots this stores no
    /// terminal output, only command metadata used by history/palette UIs.
    pub(crate) command_history_enabled: bool,
    pub(crate) command_history_path: Option<String>,
    pub(crate) command_history_max_entries: u32,
    pub(crate) block_history_path: Option<String>,
    pub(crate) block_history_compress: bool,
    /// Use jterm1/Warp-style denser block spacing.
    pub(crate) block_compact: bool,
    /// Saved SSH targets selectable from the context menu.
    pub(crate) remote_hosts: Vec<RemoteHost>,
    /// Forward mouse button events (CSI ?1000/?1002/?1003/?1006 etc.) to apps.
    pub(crate) mouse_reporting_enabled: bool,
    /// Forward scroll-wheel events to alt-screen apps that requested mouse mode.
    pub(crate) scroll_reporting_enabled: bool,
    /// Forward window focus in/out (CSI ?1004) events to apps.
    pub(crate) focus_reporting_enabled: bool,
    /// Block mode only: also keep completed output in the live VTE scrollback.
    /// Disabled by default because finished blocks already own that history;
    /// enabling it deliberately presents both the VTE and structured views.
    pub(crate) preserve_live_scrollback: bool,
    /// Master switch for every network-backed AI feature.
    pub(crate) ai_enabled: bool,
    /// Agent mode can be disabled independently while leaving chat and
    /// natural-language command generation available.
    pub(crate) agent_enabled: bool,
    /// Maximum number of model replies in one Agent session.
    pub(crate) agent_max_turns: u32,
    /// Offer an editable, review-first correction when a Block command fails
    /// with a narrow typo-shaped error. Nothing is inserted or run
    /// automatically.
    pub(crate) command_correction_enabled: bool,
    /// Provider wire protocol: anthropic, openai-compatible, or ollama.
    pub(crate) ai_provider: String,
    /// Provider API root. Endpoint suffixes are added by the AI client.
    pub(crate) ai_base_url: String,
    /// Optional owner-only file used when no AI API key environment variable
    /// is present. The path is persisted; the credential itself is not.
    pub(crate) ai_api_key_file: Option<String>,
    /// Show the right-side AI chat panel. Toggled via Ctrl+Alt+Shift+A and
    /// persisted across sessions.
    pub(crate) ai_panel_visible: bool,
    /// Width in pixels of the AI panel when visible (right Paned position is
    /// computed from window width minus this).
    pub(crate) ai_panel_width: u32,
    /// Provider-specific model id.
    pub(crate) ai_model: String,
    /// Per-request max output tokens.
    pub(crate) ai_max_tokens: u32,
    /// Run AI-bound text (system prompt block context + chat turns) through
    /// the secrets redactor before posting to the API. On by default; flip
    /// off only if the noise of mass `[REDACTED:...]` markers in a session
    /// full of legitimately-looking-secret-shaped data outweighs the risk.
    pub(crate) ai_redact_secrets: bool,
    /// Allow OSC 52 SET (`\e]52;c;<base64>\e\\`) from remote/local apps to
    /// overwrite the system clipboard. Off by default — a malicious or buggy
    /// remote process can otherwise silently replace the user's clipboard.
    pub(crate) allow_remote_clipboard_write: bool,
    /// When a block runs longer than `notify_long_block_threshold_ms`, post a
    /// desktop notification on completion via `notify-send`. The terminal
    /// emulator equivalent of the "your build is done" toast.
    pub(crate) notify_long_blocks: bool,
    /// Threshold (in milliseconds) above which `notify_long_blocks` fires.
    /// Set high enough that interactive commands don't generate noise.
    pub(crate) notify_long_block_threshold_ms: u64,
    /// Show a thin strip at the bottom of each block view with the active
    /// repo's branch, dirty marker, and ahead/behind counts. Hides itself
    /// when cwd isn't inside a git repository.
    pub(crate) show_repo_strip: bool,
    /// Exact disk revision this loaded configuration is allowed to replace.
    /// Clones from one window share the revision and advance it only after a
    /// durable save; independently loaded windows retain their own revisions.
    pub(crate) persistence_revision:
        std::sync::Arc<std::sync::Mutex<Option<crate::config_store::ConfigRevision>>>,
}

impl Config {
    /// Replace the complete configuration with an isolated, built-in VTE
    /// profile. This deliberately ignores both the user's file and JTERM4_*
    /// appearance/behavior overrides, making safe mode useful for diagnosis.
    #[cfg(test)]
    pub(crate) fn apply_safe_mode(&mut self) {
        *self = Self::safe_defaults();
    }

    fn safe_defaults() -> Self {
        let themes = builtin_themes();
        let theme = &themes[0];
        Self {
            window_opacity: 0.95,
            terminal_scrollback_lines: 5_000,
            font_desc: "SauceCodePro Nerd Font Mono 14".to_string(),
            default_font_scale: 1.0,
            theme_name: theme.name.clone(),
            foreground: theme.foreground,
            background: theme.background,
            cursor: theme.cursor,
            cursor_foreground: theme.cursor_foreground,
            palette: theme.palette,
            shell: None,
            startup_commands: None,
            terminal_mode: TerminalMode::Vte,
            tab_placement: TabPlacement::Sidebar,
            sidebar_view: SidebarView::Tabs,
            sidebar_visible: true,
            sidebar_width: 220,
            max_visible_blocks: 200,
            lazy_load_threshold: 1_000,
            truncation_threshold_lines: 50_000,
            finished_block_viewport_rows: 24,
            max_collapsed_output_lines: 25,
            virtual_scroll_margin: 1,
            command_history_enabled: false,
            command_history_path: None,
            command_history_max_entries: 10_000,
            block_history_path: None,
            block_history_compress: true,
            block_compact: false,
            remote_hosts: Vec::new(),
            mouse_reporting_enabled: true,
            scroll_reporting_enabled: true,
            focus_reporting_enabled: true,
            preserve_live_scrollback: false,
            ai_enabled: false,
            agent_enabled: false,
            agent_max_turns: 20,
            command_correction_enabled: false,
            ai_provider: "anthropic".to_string(),
            ai_base_url: "https://api.anthropic.com".to_string(),
            ai_api_key_file: None,
            ai_panel_visible: false,
            ai_panel_width: 360,
            ai_model: "claude-sonnet-4-6".to_string(),
            ai_max_tokens: 1_024,
            ai_redact_secrets: true,
            allow_remote_clipboard_write: false,
            notify_long_blocks: false,
            notify_long_block_threshold_ms: 10_000,
            show_repo_strip: false,
            persistence_revision: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }
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
                "#130c0e", "#ed1941", "#45b97c", "#fdb933", "#2585a6", "#ae5039", "#009ad6",
                "#fffef9", "#7c8577", "#f05b72", "#84bf96", "#ffc20e", "#7bbfea", "#f58f98",
                "#33a3dc", "#f6f5ec",
            ]),
        },
        Theme {
            name: "light".into(),
            foreground: RGBA::parse("#2e3440").unwrap(),
            background: RGBA::parse("#eceff4").unwrap(),
            cursor: RGBA::parse("#4c566a").unwrap(),
            cursor_foreground: RGBA::parse("#eceff4").unwrap(),
            palette: parse_palette([
                "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0",
                "#e5e9f0", "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead",
                "#8fbcbb", "#eceff4",
            ]),
        },
        Theme {
            name: "solarized-dark".into(),
            foreground: RGBA::parse("#839496").unwrap(),
            background: RGBA::parse("#002b36").unwrap(),
            cursor: RGBA::parse("#93a1a1").unwrap(),
            cursor_foreground: RGBA::parse("#002b36").unwrap(),
            palette: parse_palette([
                "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198",
                "#eee8d5", "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4",
                "#93a1a1", "#fdf6e3",
            ]),
        },
        Theme {
            name: "solarized-light".into(),
            foreground: RGBA::parse("#657b83").unwrap(),
            background: RGBA::parse("#fdf6e3").unwrap(),
            cursor: RGBA::parse("#586e75").unwrap(),
            cursor_foreground: RGBA::parse("#fdf6e3").unwrap(),
            palette: parse_palette([
                "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198",
                "#eee8d5", "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4",
                "#93a1a1", "#fdf6e3",
            ]),
        },
        Theme {
            name: "gruvbox-dark".into(),
            foreground: RGBA::parse("#ebdbb2").unwrap(),
            background: RGBA::parse("#282828").unwrap(),
            cursor: RGBA::parse("#ebdbb2").unwrap(),
            cursor_foreground: RGBA::parse("#282828").unwrap(),
            palette: parse_palette([
                "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a",
                "#a89984", "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b",
                "#8ec07c", "#ebdbb2",
            ]),
        },
        Theme {
            name: "gruvbox-light".into(),
            foreground: RGBA::parse("#3c3836").unwrap(),
            background: RGBA::parse("#fbf1c7").unwrap(),
            cursor: RGBA::parse("#3c3836").unwrap(),
            cursor_foreground: RGBA::parse("#fbf1c7").unwrap(),
            palette: parse_palette([
                "#fbf1c7", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a",
                "#7c6f64", "#928374", "#9d0006", "#79740e", "#b57614", "#076678", "#8f3f71",
                "#427b58", "#3c3836",
            ]),
        },
        Theme {
            name: "dracula".into(),
            foreground: RGBA::parse("#f8f8f2").unwrap(),
            background: RGBA::parse("#282a36").unwrap(),
            cursor: RGBA::parse("#f8f8f2").unwrap(),
            cursor_foreground: RGBA::parse("#282a36").unwrap(),
            palette: parse_palette([
                "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd",
                "#f8f8f2", "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df",
                "#a4ffff", "#ffffff",
            ]),
        },
        Theme {
            name: "nord".into(),
            foreground: RGBA::parse("#d8dee9").unwrap(),
            background: RGBA::parse("#2e3440").unwrap(),
            cursor: RGBA::parse("#d8dee9").unwrap(),
            cursor_foreground: RGBA::parse("#2e3440").unwrap(),
            palette: parse_palette([
                "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0",
                "#e5e9f0", "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead",
                "#8fbcbb", "#eceff4",
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

fn env_bool(name: &str) -> Option<bool> {
    let value = std::env::var(name).ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
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
    if let Some(path) = std::env::var_os("JTERM4_CONFIG").filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    glib::user_config_dir().join("jterm4").join("config.toml")
}

pub(crate) fn default_ai_api_key_path() -> String {
    glib::user_config_dir()
        .join("jterm4")
        .join("ai.key")
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn default_command_history_path() -> String {
    xdg_state_home()
        .join("jterm4")
        .join("history.jsonl")
        .to_string_lossy()
        .into_owned()
}

/// GLib only exposes `g_get_user_state_dir()` behind a newer API feature than
/// jterm4 currently requires, so implement the XDG Base Directory rule
/// directly: an absolute `$XDG_STATE_HOME`, otherwise `$HOME/.local/state`.
fn xdg_state_home() -> PathBuf {
    xdg_state_home_from(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        &glib::home_dir(),
    )
}

fn xdg_state_home_from(
    xdg_state_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    fallback_home: &Path,
) -> PathBuf {
    if let Some(path) = xdg_state_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return path;
    }
    home.filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback_home.to_path_buf())
        .join(".local/state")
}

/// Severity reported by the headless config checker and startup diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigIssueLevel {
    Warning,
    Error,
}

/// One actionable problem in a TOML configuration file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigIssue {
    pub level: ConfigIssueLevel,
    pub path: String,
    pub message: String,
}

impl ConfigIssue {
    pub fn is_error(&self) -> bool {
        self.level == ConfigIssueLevel::Error
    }
}

impl std::fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let level = match self.level {
            ConfigIssueLevel::Warning => "warning",
            ConfigIssueLevel::Error => "error",
        };
        write!(f, "{level}: {}: {}", self.path, self.message)
    }
}

const KNOWN_CONFIG_KEYS: &[&str] = &[
    "opacity",
    "scrollback",
    "font",
    "font_scale",
    "theme",
    "colors",
    "keybindings",
    "shell",
    "startup_commands",
    "terminal_mode",
    "tab_placement",
    "sidebar_view",
    "sidebar_visible",
    "sidebar_width",
    "max_visible_blocks",
    "lazy_load_threshold",
    "truncation_threshold_lines",
    "finished_block_viewport_rows",
    "max_collapsed_output_lines",
    "virtual_scroll_margin",
    "command_history_enabled",
    "command_history_path",
    "command_history_max_entries",
    "block_history_path",
    "block_history_compress",
    "block_compact",
    "remote_hosts",
    "mouse_reporting_enabled",
    "scroll_reporting_enabled",
    "focus_reporting_enabled",
    "preserve_live_scrollback",
    "ai_enabled",
    "agent_enabled",
    "agent_max_turns",
    "command_correction_enabled",
    "ai_provider",
    "ai_base_url",
    "ai_api_key_file",
    "ai_panel_visible",
    "ai_panel_width",
    "ai_model",
    "ai_max_tokens",
    "ai_redact_secrets",
    "allow_remote_clipboard_write",
    "notify_long_blocks",
    "notify_long_block_threshold_ms",
    "show_repo_strip",
];

fn config_issue(
    issues: &mut Vec<ConfigIssue>,
    level: ConfigIssueLevel,
    path: impl Into<String>,
    message: impl Into<String>,
) {
    issues.push(ConfigIssue {
        level,
        path: path.into(),
        message: message.into(),
    });
}

fn validate_value_types(table: &toml::Table, issues: &mut Vec<ConfigIssue>) {
    let strings = [
        "font",
        "theme",
        "shell",
        "startup_commands",
        "terminal_mode",
        "tab_placement",
        "sidebar_view",
        "command_history_path",
        "block_history_path",
        "ai_provider",
        "ai_base_url",
        "ai_api_key_file",
        "ai_model",
    ];
    let integers = [
        "scrollback",
        "sidebar_width",
        "max_visible_blocks",
        "lazy_load_threshold",
        "truncation_threshold_lines",
        "finished_block_viewport_rows",
        "max_collapsed_output_lines",
        "virtual_scroll_margin",
        "command_history_max_entries",
        "agent_max_turns",
        "ai_panel_width",
        "ai_max_tokens",
        "notify_long_block_threshold_ms",
    ];
    let booleans = [
        "block_history_compress",
        "block_compact",
        "command_history_enabled",
        "mouse_reporting_enabled",
        "scroll_reporting_enabled",
        "focus_reporting_enabled",
        "preserve_live_scrollback",
        "sidebar_visible",
        "ai_enabled",
        "agent_enabled",
        "command_correction_enabled",
        "ai_panel_visible",
        "ai_redact_secrets",
        "allow_remote_clipboard_write",
        "notify_long_blocks",
        "show_repo_strip",
    ];

    for key in strings {
        if table.get(key).is_some_and(|v| !v.is_str()) {
            config_issue(issues, ConfigIssueLevel::Error, key, "expected a string");
        }
    }
    for key in integers {
        if table.get(key).is_some_and(|v| !v.is_integer()) {
            config_issue(issues, ConfigIssueLevel::Error, key, "expected an integer");
        }
    }
    for key in booleans {
        if table.get(key).is_some_and(|v| !v.is_bool()) {
            config_issue(
                issues,
                ConfigIssueLevel::Error,
                key,
                "expected true or false",
            );
        }
    }
    for key in ["opacity", "font_scale"] {
        if table.get(key).is_some_and(|v| !v.is_float()) {
            config_issue(
                issues,
                ConfigIssueLevel::Error,
                key,
                "expected a decimal number (for example 0.95)",
            );
        }
    }
}

fn validate_config_table(table: &toml::Table) -> Vec<ConfigIssue> {
    use ConfigIssueLevel::{Error, Warning};

    let mut issues = Vec::new();
    for key in table.keys() {
        if !KNOWN_CONFIG_KEYS.contains(&key.as_str()) {
            let message = match key.as_str() {
                "ansi_cache_capacity" | "output_batch_min_ms" | "output_batch_max_ms" => {
                    "obsolete option; remove it because batching and caching are automatic"
                }
                _ => "unknown option; it will be ignored",
            };
            config_issue(&mut issues, Warning, key, message);
        }
    }
    validate_value_types(table, &mut issues);

    let warn_float_range = |issues: &mut Vec<ConfigIssue>, key: &str, min: f64, max: f64| {
        if let Some(value) = table.get(key).and_then(toml::Value::as_float) {
            if !(min..=max).contains(&value) {
                config_issue(
                    issues,
                    Warning,
                    key,
                    format!("{value} is outside {min}..={max}; it will be clamped"),
                );
            }
        }
    };
    let warn_int_range = |issues: &mut Vec<ConfigIssue>, key: &str, min: i64, max: i64| {
        if let Some(value) = table.get(key).and_then(toml::Value::as_integer) {
            if !(min..=max).contains(&value) {
                config_issue(
                    issues,
                    Warning,
                    key,
                    format!("{value} is outside {min}..={max}; it will be clamped"),
                );
            }
        }
    };
    warn_float_range(&mut issues, "opacity", 0.01, 1.0);
    warn_float_range(&mut issues, "font_scale", 0.1, 10.0);
    warn_int_range(&mut issues, "scrollback", 0, 1_000_000);
    warn_int_range(&mut issues, "sidebar_width", 120, 800);
    warn_int_range(&mut issues, "max_visible_blocks", 1, 100_000);
    warn_int_range(&mut issues, "lazy_load_threshold", 1, 10_000_000);
    warn_int_range(&mut issues, "truncation_threshold_lines", 1, 10_000_000);
    warn_int_range(&mut issues, "finished_block_viewport_rows", 3, 5_000);
    warn_int_range(&mut issues, "max_collapsed_output_lines", 1, 1_000_000);
    warn_int_range(&mut issues, "virtual_scroll_margin", 0, 10_000);
    warn_int_range(&mut issues, "command_history_max_entries", 100, 1_000_000);
    warn_int_range(&mut issues, "agent_max_turns", 1, 100);
    warn_int_range(&mut issues, "ai_panel_width", 240, 1200);
    warn_int_range(&mut issues, "ai_max_tokens", 64, 32_768);
    warn_int_range(&mut issues, "notify_long_block_threshold_ms", 0, i64::MAX);

    if let Some(mode) = table.get("terminal_mode").and_then(toml::Value::as_str) {
        if !matches!(mode.to_ascii_lowercase().as_str(), "block" | "vte") {
            config_issue(
                &mut issues,
                Error,
                "terminal_mode",
                "expected 'block' or 'vte'",
            );
        }
    }
    if let Some(provider) = table.get("ai_provider").and_then(toml::Value::as_str) {
        if !matches!(
            provider.trim().to_ascii_lowercase().as_str(),
            "anthropic"
                | "claude"
                | "openai"
                | "openai-compatible"
                | "openai_compatible"
                | "ollama"
        ) {
            config_issue(
                &mut issues,
                Error,
                "ai_provider",
                "expected 'anthropic', 'openai-compatible', or 'ollama'",
            );
        }
    }
    if let Some(model) = table.get("ai_model").and_then(toml::Value::as_str) {
        if model.trim().is_empty() {
            config_issue(&mut issues, Error, "ai_model", "must not be empty");
        }
    }
    if let Some(url) = table.get("ai_base_url").and_then(toml::Value::as_str) {
        let url = url.trim();
        let valid = (url.starts_with("http://") || url.starts_with("https://"))
            && url
                .split_once("://")
                .is_some_and(|(_, authority)| !authority.is_empty())
            && !url.chars().any(char::is_whitespace);
        if !valid {
            config_issue(
                &mut issues,
                Error,
                "ai_base_url",
                "expected an absolute http(s) URL without whitespace",
            );
        }
    }
    if let Some(path) = table.get("ai_api_key_file").and_then(toml::Value::as_str) {
        let path = path.trim();
        if path.is_empty() {
            config_issue(&mut issues, Error, "ai_api_key_file", "must not be empty");
        } else if !(path.starts_with('/') || path == "~" || path.starts_with("~/")) {
            config_issue(
                &mut issues,
                Error,
                "ai_api_key_file",
                "expected an absolute path or a path beginning with ~/",
            );
        }
    }
    if let Some(value) = table.get("tab_placement").and_then(toml::Value::as_str) {
        if !matches!(
            value.to_ascii_lowercase().as_str(),
            "sidebar" | "top" | "topbar" | "top_bar"
        ) {
            config_issue(
                &mut issues,
                Error,
                "tab_placement",
                "expected 'sidebar' or 'top'",
            );
        }
    }
    if let Some(value) = table.get("sidebar_view").and_then(toml::Value::as_str) {
        if !matches!(
            value.to_ascii_lowercase().as_str(),
            "tabs" | "files" | "file" | "filetree" | "file_tree"
        ) {
            config_issue(
                &mut issues,
                Error,
                "sidebar_view",
                "expected 'tabs' or 'files'",
            );
        }
    }
    if let Some(theme) = table.get("theme").and_then(toml::Value::as_str) {
        if !builtin_themes()
            .iter()
            .any(|candidate| candidate.name == theme)
        {
            config_issue(
                &mut issues,
                Error,
                "theme",
                format!("unknown built-in theme '{theme}'"),
            );
        }
    }

    if let Some(colors) = table.get("colors") {
        if let Some(colors) = colors.as_table() {
            for key in colors.keys() {
                if !matches!(
                    key.as_str(),
                    "foreground" | "background" | "cursor" | "cursor_foreground"
                ) {
                    config_issue(
                        &mut issues,
                        Warning,
                        format!("colors.{key}"),
                        "unknown color option",
                    );
                }
            }
            for (key, value) in colors {
                let path = format!("colors.{key}");
                match value.as_str() {
                    Some(raw) if RGBA::parse(raw).is_ok() => {}
                    Some(raw) => config_issue(
                        &mut issues,
                        Error,
                        path,
                        format!("'{raw}' is not a valid CSS color"),
                    ),
                    None => config_issue(&mut issues, Error, path, "expected a color string"),
                }
            }
        } else {
            config_issue(&mut issues, Error, "colors", "expected a table");
        }
    }

    if let Some(bindings) = table.get("keybindings") {
        if let Some(bindings) = bindings.as_table() {
            let known: std::collections::HashSet<&str> = crate::keybindings::Action::all_actions()
                .into_iter()
                .filter_map(|action| action.config_key())
                .collect();
            let mut chords: HashMap<KeyCombo, &str> = HashMap::new();
            for (action, value) in bindings {
                let path = format!("keybindings.{action}");
                if !known.contains(action.as_str()) {
                    config_issue(&mut issues, Error, &path, "unknown action");
                    continue;
                }
                if value.as_bool() == Some(false) {
                    continue;
                }
                let Some(chord) = value.as_str() else {
                    config_issue(
                        &mut issues,
                        Error,
                        &path,
                        "expected a chord string or false",
                    );
                    continue;
                };
                if chord.trim().is_empty()
                    || chord.eq_ignore_ascii_case("none")
                    || chord.eq_ignore_ascii_case("disabled")
                {
                    continue;
                }
                match crate::keybindings::parse_key_combo(chord) {
                    Ok(combo) => {
                        if let Some(previous) = chords.insert(combo, action) {
                            config_issue(
                                &mut issues,
                                Warning,
                                &path,
                                format!("same chord as keybindings.{previous}; last one wins"),
                            );
                        }
                    }
                    Err(err) => config_issue(&mut issues, Error, &path, err),
                }
            }
        } else {
            config_issue(&mut issues, Error, "keybindings", "expected a table");
        }
    }

    if let Some(hosts) = table.get("remote_hosts") {
        if let Some(hosts) = hosts.as_array() {
            for (index, host) in hosts.iter().enumerate() {
                let path = format!("remote_hosts[{index}]");
                let Some(host) = host.as_table() else {
                    config_issue(&mut issues, Error, path, "expected a table");
                    continue;
                };
                match host.get("host").and_then(toml::Value::as_str) {
                    Some(value) if !value.trim().is_empty() => {}
                    _ => config_issue(
                        &mut issues,
                        Error,
                        format!("{path}.host"),
                        "missing non-empty host",
                    ),
                }
                if let Some(args) = host.get("ssh_args") {
                    if !args
                        .as_array()
                        .is_some_and(|values| values.iter().all(toml::Value::is_str))
                    {
                        config_issue(
                            &mut issues,
                            Error,
                            format!("{path}.ssh_args"),
                            "expected an array of strings",
                        );
                    }
                }
            }
        } else {
            config_issue(
                &mut issues,
                Error,
                "remote_hosts",
                "expected an array of tables",
            );
        }
    }

    issues
}

/// Parse and semantically validate TOML without starting GTK. Syntax errors are
/// returned separately so CLI callers can show TOML's line/column diagnostic.
pub fn validate_config_contents(contents: &str) -> Result<Vec<ConfigIssue>, toml::de::Error> {
    let table = contents.parse::<toml::Table>()?;
    Ok(validate_config_table(&table))
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
    tab_placement: Option<String>,
    sidebar_view: Option<String>,
    sidebar_visible: Option<bool>,
    sidebar_width: Option<u32>,
    // Block view optimizations
    max_visible_blocks: Option<u32>,
    lazy_load_threshold: Option<u32>,
    truncation_threshold_lines: Option<u32>,
    finished_block_viewport_rows: Option<u32>,
    max_collapsed_output_lines: Option<u32>,
    virtual_scroll_margin: Option<u32>,
    command_history_enabled: Option<bool>,
    command_history_path: Option<String>,
    command_history_max_entries: Option<u32>,
    block_history_path: Option<String>,
    block_history_compress: Option<bool>,
    block_compact: Option<bool>,
    remote_hosts: Vec<RemoteHost>,
    mouse_reporting_enabled: Option<bool>,
    scroll_reporting_enabled: Option<bool>,
    focus_reporting_enabled: Option<bool>,
    preserve_live_scrollback: Option<bool>,
    ai_enabled: Option<bool>,
    agent_enabled: Option<bool>,
    agent_max_turns: Option<u32>,
    command_correction_enabled: Option<bool>,
    ai_provider: Option<String>,
    ai_base_url: Option<String>,
    ai_api_key_file: Option<String>,
    ai_panel_visible: Option<bool>,
    ai_panel_width: Option<u32>,
    ai_model: Option<String>,
    ai_max_tokens: Option<u32>,
    ai_redact_secrets: Option<bool>,
    allow_remote_clipboard_write: Option<bool>,
    notify_long_blocks: Option<bool>,
    notify_long_block_threshold_ms: Option<u64>,
    show_repo_strip: Option<bool>,
}

fn table_u32(table: &toml::Table, key: &str) -> Option<u32> {
    table
        .get(key)
        .and_then(toml::Value::as_integer)
        .and_then(|value| u32::try_from(value).ok())
}

fn table_u64(table: &toml::Table, key: &str) -> Option<u64> {
    table
        .get(key)
        .and_then(toml::Value::as_integer)
        .and_then(|value| u64::try_from(value).ok())
}

fn load_file_config() -> (FileConfig, Option<crate::config_store::ConfigRevision>) {
    let path = config_file_path();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (
                FileConfig {
                    remote_hosts: default_remote_hosts(),
                    ..Default::default()
                },
                Some(crate::config_store::ConfigRevision::missing()),
            );
        }
        Err(error) => {
            log::warn!("Failed to read config file {}: {error}", path.display());
            return (
                FileConfig {
                    remote_hosts: default_remote_hosts(),
                    ..Default::default()
                },
                None,
            );
        }
    };
    let revision = crate::config_store::ConfigRevision::from_bytes(&bytes);
    let Ok(contents) = std::str::from_utf8(&bytes) else {
        log::warn!("Config file {} is not valid UTF-8", path.display());
        return (
            FileConfig {
                remote_hosts: default_remote_hosts(),
                ..Default::default()
            },
            Some(revision),
        );
    };
    let Ok(table) = contents.parse::<toml::Table>() else {
        log::warn!("Failed to parse config file {}", path.display());
        return (
            FileConfig {
                remote_hosts: default_remote_hosts(),
                ..Default::default()
            },
            Some(revision),
        );
    };
    for issue in validate_config_table(&table) {
        match issue.level {
            ConfigIssueLevel::Warning => log::warn!("Config {issue}"),
            ConfigIssueLevel::Error => log::error!("Config {issue}"),
        }
    }

    let colors = table.get("colors").and_then(|v| v.as_table());
    // Fall back to built-in defaults when the section is entirely absent (e.g. a
    // config file first created to persist some other setting). An explicit,
    // possibly empty, [[remote_hosts]] array is respected as-is.
    let remote_hosts = if table.contains_key("remote_hosts") {
        parse_remote_hosts(&table)
    } else {
        default_remote_hosts()
    };

    let file_config = FileConfig {
        opacity: table.get("opacity").and_then(|v| v.as_float()),
        scrollback: table_u32(&table, "scrollback"),
        font: table
            .get("font")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        font_scale: table.get("font_scale").and_then(|v| v.as_float()),
        theme: table
            .get("theme")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        foreground: colors
            .and_then(|c| c.get("foreground"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        background: colors
            .and_then(|c| c.get("background"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        cursor: colors
            .and_then(|c| c.get("cursor"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        cursor_foreground: colors
            .and_then(|c| c.get("cursor_foreground"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        keybindings: table.get("keybindings").and_then(|v| v.as_table()).cloned(),
        shell: table
            .get("shell")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        startup_commands: table
            .get("startup_commands")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        terminal_mode: table
            .get("terminal_mode")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        tab_placement: table
            .get("tab_placement")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        sidebar_view: table
            .get("sidebar_view")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        sidebar_visible: table.get("sidebar_visible").and_then(|v| v.as_bool()),
        sidebar_width: table_u32(&table, "sidebar_width"),
        max_visible_blocks: table_u32(&table, "max_visible_blocks"),
        lazy_load_threshold: table_u32(&table, "lazy_load_threshold"),
        truncation_threshold_lines: table_u32(&table, "truncation_threshold_lines"),
        finished_block_viewport_rows: table_u32(&table, "finished_block_viewport_rows"),
        max_collapsed_output_lines: table_u32(&table, "max_collapsed_output_lines"),
        virtual_scroll_margin: table_u32(&table, "virtual_scroll_margin"),
        command_history_enabled: table
            .get("command_history_enabled")
            .and_then(|v| v.as_bool()),
        command_history_path: table
            .get("command_history_path")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        command_history_max_entries: table_u32(&table, "command_history_max_entries"),
        block_history_path: table
            .get("block_history_path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        block_history_compress: table
            .get("block_history_compress")
            .and_then(|v| v.as_bool()),
        block_compact: table.get("block_compact").and_then(|v| v.as_bool()),
        remote_hosts,
        mouse_reporting_enabled: table
            .get("mouse_reporting_enabled")
            .and_then(|v| v.as_bool()),
        scroll_reporting_enabled: table
            .get("scroll_reporting_enabled")
            .and_then(|v| v.as_bool()),
        focus_reporting_enabled: table
            .get("focus_reporting_enabled")
            .and_then(|v| v.as_bool()),
        preserve_live_scrollback: table
            .get("preserve_live_scrollback")
            .and_then(|v| v.as_bool()),
        ai_enabled: table.get("ai_enabled").and_then(|v| v.as_bool()),
        agent_enabled: table.get("agent_enabled").and_then(|v| v.as_bool()),
        agent_max_turns: table_u32(&table, "agent_max_turns"),
        command_correction_enabled: table
            .get("command_correction_enabled")
            .and_then(|v| v.as_bool()),
        ai_provider: table
            .get("ai_provider")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        ai_base_url: table
            .get("ai_base_url")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        ai_api_key_file: table
            .get("ai_api_key_file")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        ai_panel_visible: table.get("ai_panel_visible").and_then(|v| v.as_bool()),
        ai_panel_width: table_u32(&table, "ai_panel_width"),
        ai_model: table
            .get("ai_model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        ai_max_tokens: table_u32(&table, "ai_max_tokens"),
        ai_redact_secrets: table.get("ai_redact_secrets").and_then(|v| v.as_bool()),
        allow_remote_clipboard_write: table
            .get("allow_remote_clipboard_write")
            .and_then(|v| v.as_bool()),
        notify_long_blocks: table.get("notify_long_blocks").and_then(|v| v.as_bool()),
        notify_long_block_threshold_ms: table_u64(&table, "notify_long_block_threshold_ms"),
        show_repo_strip: table.get("show_repo_strip").and_then(|v| v.as_bool()),
    };
    (file_config, Some(revision))
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
            let name = t
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| host.clone());
            let user = t
                .get("user")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let remote_shell = t
                .get("remote_shell")
                .and_then(|v| v.as_str())
                .unwrap_or("rsh")
                .to_string();
            let session = t
                .get("session")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let ssh_args = t
                .get("ssh_args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let login_shell = t
                .get("login_shell")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let multiplex = t.get("multiplex").and_then(|v| v.as_bool()).unwrap_or(true);
            Some(RemoteHost {
                name,
                host,
                user,
                remote_shell,
                session,
                ssh_args,
                login_shell,
                multiplex,
            })
        })
        .collect()
}

/// Serialize a `RemoteHost` back into a TOML table that `parse_remote_hosts`
/// round-trips. Optional fields are only emitted when present.
pub(crate) fn remote_host_to_toml(h: &RemoteHost) -> toml::Value {
    let mut t = toml::Table::new();
    t.insert("name".into(), toml::Value::String(h.name.clone()));
    t.insert("host".into(), toml::Value::String(h.host.clone()));
    if let Some(user) = &h.user {
        t.insert("user".into(), toml::Value::String(user.clone()));
    }
    t.insert(
        "remote_shell".into(),
        toml::Value::String(h.remote_shell.clone()),
    );
    if let Some(session) = &h.session {
        t.insert("session".into(), toml::Value::String(session.clone()));
    }
    if !h.ssh_args.is_empty() {
        let args: Vec<toml::Value> = h
            .ssh_args
            .iter()
            .map(|a| toml::Value::String(a.clone()))
            .collect();
        t.insert("ssh_args".into(), toml::Value::Array(args));
    }
    t.insert("login_shell".into(), toml::Value::Boolean(h.login_shell));
    t.insert("multiplex".into(), toml::Value::Boolean(h.multiplex));
    toml::Value::Table(t)
}

/// No network target is assumed on a fresh install. Remote hosts are personal
/// data and must be explicitly configured by the user.
fn default_remote_hosts() -> Vec<RemoteHost> {
    Vec::new()
}

// ---------------------------------------------------------------------------
// load_config
// ---------------------------------------------------------------------------

pub(crate) fn load_config() -> (Config, Vec<Theme>, KeybindingMap) {
    let (fc, persistence_revision) = load_file_config();
    let themes = builtin_themes();

    // Resolve active theme
    let theme_name = env_string("JTERM4_THEME")
        .or(fc.theme)
        .unwrap_or_else(|| "default".to_string());
    let theme = themes
        .iter()
        .find(|t| t.name == theme_name)
        .unwrap_or(&themes[0]);

    // Priority: env var > config file > theme default
    let window_opacity = env_f64("JTERM4_OPACITY")
        .or(fc.opacity)
        .unwrap_or(0.95)
        .clamp(0.01, 1.0);
    let terminal_scrollback_lines = env_u32("JTERM4_SCROLLBACK")
        .or(fc.scrollback)
        .unwrap_or(5000)
        .min(1_000_000);
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
        .or_else(|| {
            fc.cursor_foreground
                .as_deref()
                .and_then(|v| RGBA::parse(v).ok())
        })
        .unwrap_or(theme.cursor_foreground);

    // Block view optimization settings
    let max_visible_blocks = env_u32("JTERM4_MAX_BLOCKS")
        .or(fc.max_visible_blocks)
        .unwrap_or(200)
        .clamp(1, 100_000);
    let lazy_load_threshold = env_u32("JTERM4_LAZY_LINES")
        .or(fc.lazy_load_threshold)
        .unwrap_or(1000)
        .clamp(1, 10_000_000);
    let truncation_threshold_lines = env_u32("JTERM4_TRUNCATION_LINES")
        .or(fc.truncation_threshold_lines)
        .unwrap_or(50000)
        .clamp(1, 10_000_000);
    let finished_block_viewport_rows = env_u32("JTERM4_FINISHED_VIEWPORT_ROWS")
        .or(fc.finished_block_viewport_rows)
        .unwrap_or(24)
        .clamp(3, 5_000);
    let max_collapsed_output_lines = env_u32("JTERM4_MAX_COLLAPSED_LINES")
        .or(fc.max_collapsed_output_lines)
        .unwrap_or(25)
        .clamp(1, 1_000_000);
    let virtual_scroll_margin = env_u32("JTERM4_VSCROLL_MARGIN")
        .or(fc.virtual_scroll_margin)
        .unwrap_or(1)
        .min(10_000);
    let command_history_enabled = fc.command_history_enabled.unwrap_or(true);
    let command_history_path = command_history_enabled.then(|| {
        env_string("JTERM4_COMMAND_HISTORY_PATH")
            .or(fc.command_history_path)
            .unwrap_or_else(default_command_history_path)
    });
    let command_history_max_entries = fc
        .command_history_max_entries
        .unwrap_or(10_000)
        .clamp(100, 1_000_000);
    let block_history_path = std::env::var("JTERM4_HISTORY_PATH")
        .ok()
        .or(fc.block_history_path);
    let block_history_compress = fc.block_history_compress.unwrap_or(true);
    let block_compact = match std::env::var("JTERM4_BLOCK_COMPACT").ok().as_deref() {
        Some("1") | Some("true") => Some(true),
        Some("0") | Some("false") => Some(false),
        _ => None,
    }
    .or(fc.block_compact)
    .unwrap_or(false);
    let shell = std::env::var("JTERM4_SHELL").ok().or(fc.shell);

    // Block-first like jterm1; VTE remains available for compatibility and
    // safe mode.
    let terminal_mode_str = env_string("JTERM4_MODE")
        .or(fc.terminal_mode)
        .unwrap_or_else(|| "block".to_string());
    let terminal_mode = match terminal_mode_str.to_ascii_lowercase().as_str() {
        "block" => TerminalMode::Block,
        "vte" => TerminalMode::Vte,
        other => {
            log::warn!("Unknown terminal_mode '{other}', using block");
            TerminalMode::Block
        }
    };

    let tab_placement = TabPlacement::parse(
        &env_string("JTERM4_TAB_PLACEMENT")
            .or(fc.tab_placement)
            .unwrap_or_else(|| "sidebar".to_string()),
    );
    let sidebar_visible = resolve_sidebar_visibility(fc.sidebar_visible, tab_placement);

    let ai_enabled = env_bool("JTERM4_AI_ENABLED")
        .or(fc.ai_enabled)
        .unwrap_or(true);
    let agent_enabled = env_bool("JTERM4_AGENT_ENABLED")
        .or(fc.agent_enabled)
        .unwrap_or(true);
    let agent_max_turns = env_u32("JTERM4_AGENT_MAX_TURNS")
        .or(fc.agent_max_turns)
        .unwrap_or(20)
        .clamp(1, 100);
    let command_correction_enabled = env_bool("JTERM4_COMMAND_CORRECTION_ENABLED")
        .or(fc.command_correction_enabled)
        .unwrap_or(true);
    let requested_provider = env_string("JTERM4_AI_PROVIDER")
        .or(fc.ai_provider)
        .unwrap_or_else(|| "anthropic".to_string());
    let ai_provider = match requested_provider.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => "anthropic",
        "openai" | "openai-compatible" | "openai_compatible" => "openai-compatible",
        "ollama" => "ollama",
        other => {
            log::warn!("Unknown ai_provider '{other}', using anthropic");
            "anthropic"
        }
    }
    .to_string();
    let (default_ai_model, default_ai_base_url) = match ai_provider.as_str() {
        "openai-compatible" => ("gpt-4o-mini", "https://api.openai.com/v1"),
        "ollama" => ("codellama:7b", "http://localhost:11434"),
        _ => ("claude-sonnet-4-6", "https://api.anthropic.com"),
    };
    let ai_model = env_string("JTERM4_AI_MODEL")
        .or(fc.ai_model)
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| default_ai_model.to_string());
    let ai_base_url = env_string("JTERM4_AI_BASE_URL")
        .or(fc.ai_base_url)
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| default_ai_base_url.to_string())
        .trim_end_matches('/')
        .to_string();
    let ai_api_key_file = env_string("JTERM4_AI_API_KEY_FILE")
        .or(fc.ai_api_key_file)
        .filter(|path| !path.trim().is_empty());

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
        tab_placement,
        sidebar_view: SidebarView::parse(&fc.sidebar_view.unwrap_or_else(|| "tabs".to_string())),
        sidebar_visible,
        sidebar_width: fc.sidebar_width.unwrap_or(220).clamp(120, 800),
        max_visible_blocks,
        lazy_load_threshold,
        truncation_threshold_lines,
        finished_block_viewport_rows,
        max_collapsed_output_lines,
        virtual_scroll_margin,
        command_history_enabled,
        command_history_path,
        command_history_max_entries,
        block_history_path,
        block_history_compress,
        block_compact,
        remote_hosts: fc.remote_hosts,
        mouse_reporting_enabled: fc.mouse_reporting_enabled.unwrap_or(true),
        scroll_reporting_enabled: fc.scroll_reporting_enabled.unwrap_or(true),
        focus_reporting_enabled: fc.focus_reporting_enabled.unwrap_or(true),
        preserve_live_scrollback: fc.preserve_live_scrollback.unwrap_or(false),
        ai_enabled,
        agent_enabled,
        agent_max_turns,
        command_correction_enabled,
        ai_provider,
        ai_base_url,
        ai_api_key_file,
        ai_panel_visible: fc.ai_panel_visible.unwrap_or(false),
        ai_panel_width: fc.ai_panel_width.unwrap_or(360).clamp(240, 1200),
        ai_model,
        ai_max_tokens: env_u32("JTERM4_AI_MAX_TOKENS")
            .or(fc.ai_max_tokens)
            .unwrap_or(1024)
            .clamp(64, 32_768),
        ai_redact_secrets: env_bool("JTERM4_AI_REDACT_SECRETS")
            .or(fc.ai_redact_secrets)
            .unwrap_or(true),
        allow_remote_clipboard_write: fc.allow_remote_clipboard_write.unwrap_or(false),
        notify_long_blocks: fc.notify_long_blocks.unwrap_or(true),
        notify_long_block_threshold_ms: fc.notify_long_block_threshold_ms.unwrap_or(10_000),
        show_repo_strip: fc.show_repo_strip.unwrap_or(true),
        persistence_revision: std::sync::Arc::new(std::sync::Mutex::new(persistence_revision)),
    };

    let mut keybinding_map = KeybindingMap::from_defaults();
    if let Some(ref kb_table) = fc.keybindings {
        keybinding_map.apply_user_overrides(kb_table);
    }

    (config, themes, keybinding_map)
}

/// Load no external configuration at all. Unlike applying a partial override
/// after `load_config`, this cannot block on or inherit a user-selected config
/// path, and it also resets custom keybindings.
pub(crate) fn load_safe_config() -> (Config, Vec<Theme>, KeybindingMap) {
    (
        Config::safe_defaults(),
        builtin_themes(),
        KeybindingMap::from_defaults(),
    )
}

// ---------------------------------------------------------------------------
// save_config
// ---------------------------------------------------------------------------

pub(crate) fn rgba_to_hex(c: &RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c.red() * 255.0) as u8,
        (c.green() * 255.0) as u8,
        (c.blue() * 255.0) as u8
    )
}

pub(crate) fn save_config(config: &Config) -> Result<(), crate::config_store::ConfigWriteError> {
    if safe_mode_persistence_disabled(std::env::var_os("JTERM4_SAFE_MODE").as_deref()) {
        log::debug!("Skipping configuration save in safe mode");
        return Ok(());
    }
    crate::config_store::save_config(config)
        .map(|_| ())
        .inspect_err(|error| log::warn!("Failed to save configuration: {error}"))
}

fn safe_mode_persistence_disabled(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| {
        let value = value.to_string_lossy();
        value == "1" || value.eq_ignore_ascii_case("true")
    })
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

fn choose_flatpak_host_shell_argv(configured_shell: Option<&str>) -> Vec<String> {
    if let Some(shell) = configured_shell.filter(|value| !value.trim().is_empty()) {
        let shell_name = Path::new(shell)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if shell_name == "rsh" && crate::host::command_available("bash") {
            return vec![
                "bash".to_string(),
                "-ic".to_string(),
                format!("exec {}", shell_single_quote(shell)),
            ];
        }
        return vec![shell.to_string()];
    }

    if crate::host::command_available("rsh") {
        if crate::host::command_available("bash") {
            return vec![
                "bash".to_string(),
                "-ic".to_string(),
                "exec rsh".to_string(),
            ];
        }
        return vec!["rsh".to_string()];
    }

    if let Some(shell) = std::env::var("SHELL")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return vec![shell, "-l".to_string()];
    }
    if crate::host::command_available("bash") {
        return vec!["bash".to_string(), "-l".to_string()];
    }
    vec!["sh".to_string()]
}

pub(crate) fn choose_shell_argv(configured_shell: Option<&str>) -> Vec<String> {
    if crate::host::is_flatpak() {
        return choose_flatpak_host_shell_argv(configured_shell);
    }

    // Explicit config / env var wins (needed when PATH is stripped by launchers like wofi).
    if let Some(path) = configured_shell {
        if is_executable(Path::new(path)) {
            let shell_name = Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if shell_name == "rsh" {
                if let Some(argv) = wrap_rsh_argv_in_interactive_bash(path) {
                    return argv;
                }
            }
            return vec![path.to_string()];
        }
        log::warn!(
            "Configured shell '{}' is not executable, falling back to auto-detection",
            path
        );
    }

    // Prefer rsh when it's on PATH.
    if let Some(rsh_path) = find_executable_in_path("rsh") {
        if let Some(argv) = wrap_rsh_argv_in_interactive_bash(&rsh_path.to_string_lossy()) {
            return argv;
        }
        return vec![rsh_path.to_string_lossy().to_string()];
    }

    // Fallback: bash
    if let Some(bash_path) = find_executable_in_path("bash") {
        return vec![bash_path.to_string_lossy().to_string(), "-l".to_string()];
    }

    // Last resort: POSIX sh
    vec!["sh".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> RemoteHost {
        RemoteHost {
            name: "h".into(),
            host: "1.2.3.4".into(),
            user: Some("yj".into()),
            remote_shell: "/home/yj/.cargo/bin/rsh".into(),
            session: Some("cloud-test".into()),
            ssh_args: Vec::new(),
            login_shell: true,
            // Off by default in tests so exact-argv assertions stay deterministic
            // (multiplex injects an env-dependent ControlPath).
            multiplex: false,
        }
    }

    #[test]
    fn login_shell_wraps_in_bash_lc() {
        let argv = build_remote_argv(&host());
        assert_eq!(
            argv,
            vec![
                "ssh",
                "-t",
                "yj@1.2.3.4",
                "bash -lc 'exec /home/yj/.cargo/bin/rsh --session cloud-test'",
            ]
        );
    }

    #[test]
    fn no_login_shell_passes_command_bare() {
        let mut h = host();
        h.login_shell = false;
        let argv = build_remote_argv(&h);
        assert_eq!(
            argv.last().unwrap(),
            "/home/yj/.cargo/bin/rsh --session cloud-test"
        );
    }

    #[test]
    fn single_quotes_in_payload_are_escaped() {
        let mut h = host();
        h.session = Some("it's".into());
        let argv = build_remote_argv(&h);
        assert_eq!(
            argv.last().unwrap(),
            r#"bash -lc 'exec /home/yj/.cargo/bin/rsh --session it'\''s'"#
        );
    }

    #[test]
    fn local_rsh_is_wrapped_in_interactive_bash() {
        let argv = wrap_rsh_argv_in_interactive_bash("/home/yj/.cargo/bin/rsh")
            .expect("bash should be available on the test runner");
        assert_eq!(argv[1], "-ic");
        assert_eq!(argv[2], "exec '/home/yj/.cargo/bin/rsh'");
    }

    #[test]
    fn multiplex_injects_controlmaster_flags() {
        let mut h = host();
        h.multiplex = true;
        std::env::set_var("XDG_RUNTIME_DIR", std::env::temp_dir());
        let argv = build_remote_argv(&h);
        assert!(
            argv.iter().any(|a| a == "ControlMaster=auto"),
            "argv: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a == "ControlPersist=120"),
            "argv: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a.starts_with("ControlPath=")),
            "argv: {argv:?}"
        );
        // ControlMaster flags must precede the target.
        let target_idx = argv.iter().position(|a| a == "yj@1.2.3.4").unwrap();
        let cm_idx = argv.iter().position(|a| a == "ControlMaster=auto").unwrap();
        assert!(cm_idx < target_idx);
    }

    #[test]
    fn no_multiplex_omits_controlmaster_flags() {
        let argv = build_remote_argv(&host()); // multiplex=false
        assert!(
            !argv.iter().any(|a| a.contains("ControlMaster")),
            "argv: {argv:?}"
        );
    }

    #[test]
    fn config_validator_reports_unknown_invalid_and_colliding_values() {
        let input = r#"
terminal_mode = "warp"
opacity = 2.0
obsolete_thing = true

[colors]
foreground = "definitely-not-a-color"

[keybindings]
copy = "Ctrl+Shift+X"
paste = "Ctrl+Shift+X"
unknown_action = "F8"
"#;
        let issues = validate_config_contents(input).unwrap();
        assert!(issues.iter().any(|issue| {
            issue.path == "terminal_mode" && issue.level == ConfigIssueLevel::Error
        }));
        assert!(issues.iter().any(|issue| issue.path == "opacity"));
        assert!(issues.iter().any(|issue| issue.path == "obsolete_thing"));
        assert!(issues.iter().any(|issue| issue.path == "colors.foreground"));
        assert!(issues
            .iter()
            .any(|issue| issue.path == "keybindings.unknown_action"));
        assert!(issues
            .iter()
            .any(|issue| issue.message.contains("same chord")));
    }

    #[test]
    fn disabled_keybinding_is_valid() {
        let issues = validate_config_contents("[keybindings]\ncopy = false\n").unwrap();
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn invalid_toml_is_rejected() {
        assert!(validate_config_contents("opacity = [").is_err());
    }

    #[test]
    fn fresh_install_has_no_personal_remote_targets() {
        assert!(default_remote_hosts().is_empty());
    }

    #[test]
    fn command_history_config_is_validated_and_uses_xdg_state_semantics() {
        let issues = validate_config_contents(
            "command_history_enabled = true\ncommand_history_path = '/tmp/history.jsonl'\ncommand_history_max_entries = 99\n",
        )
        .unwrap();
        assert!(issues.iter().all(|issue| !issue.is_error()), "{issues:?}");
        assert!(issues.iter().any(|issue| {
            issue.path == "command_history_max_entries" && issue.level == ConfigIssueLevel::Warning
        }));

        let wrong_types = validate_config_contents(
            "command_history_enabled = 'yes'\ncommand_history_path = false\ncommand_history_max_entries = 'many'\n",
        )
        .unwrap();
        assert_eq!(
            wrong_types.iter().filter(|issue| issue.is_error()).count(),
            3
        );

        assert_eq!(
            xdg_state_home_from(
                Some(std::ffi::OsStr::new("/var/state")),
                Some(std::ffi::OsStr::new("/home/test")),
                Path::new("/fallback")
            ),
            PathBuf::from("/var/state")
        );
        assert_eq!(
            xdg_state_home_from(
                Some(std::ffi::OsStr::new("relative-state")),
                Some(std::ffi::OsStr::new("/home/test")),
                Path::new("/fallback")
            ),
            PathBuf::from("/home/test/.local/state")
        );
    }

    #[test]
    fn ai_and_agent_config_is_semantically_validated() {
        let valid = validate_config_contents(
            "ai_enabled = true\nagent_enabled = true\nagent_max_turns = 20\ncommand_correction_enabled = true\nai_provider = 'openai-compatible'\nai_base_url = 'http://localhost:8000/v1'\nai_api_key_file = '~/.config/jterm4/ai.key'\nai_model = 'local-model'\nai_max_tokens = 4096\nai_redact_secrets = true\n",
        )
        .unwrap();
        assert!(valid.is_empty(), "{valid:?}");

        let invalid = validate_config_contents(
            "agent_max_turns = 0\nai_provider = 'mystery'\nai_base_url = 'file:///tmp/model'\nai_api_key_file = 'relative.key'\nai_model = ''\nai_max_tokens = 999999\n",
        )
        .unwrap();
        assert!(invalid.iter().any(|issue| {
            issue.path == "agent_max_turns" && issue.level == ConfigIssueLevel::Warning
        }));
        assert!(invalid.iter().any(|issue| issue.path == "ai_max_tokens"));
        for key in ["ai_provider", "ai_base_url", "ai_api_key_file", "ai_model"] {
            assert!(invalid
                .iter()
                .any(|issue| issue.path == key && issue.is_error()));
        }
    }

    #[test]
    fn safe_mode_removes_external_and_persistent_state() {
        let (mut config, _, _) = load_config();
        config.window_opacity = 0.2;
        config.terminal_scrollback_lines = 42;
        config.font_desc = "User Font 30".into();
        config.default_font_scale = 3.0;
        config.tab_placement = TabPlacement::TopBar;
        config.sidebar_view = SidebarView::Files;
        config.sidebar_visible = false;
        config.mouse_reporting_enabled = false;
        config.show_repo_strip = true;
        config.shell = Some("/custom/shell".into());
        config.startup_commands = Some("touch /tmp/should-not-run".into());
        config.command_history_enabled = true;
        config.command_history_path = Some("/tmp/history".into());
        config.block_history_path = Some("/tmp/blocks".into());
        config.ai_enabled = true;
        config.ai_api_key_file = Some("/tmp/ai-key".into());
        config.agent_enabled = true;
        config.command_correction_enabled = true;
        config.ai_panel_visible = true;
        config.notify_long_blocks = true;
        config.allow_remote_clipboard_write = true;
        config.remote_hosts.push(host());

        config.apply_safe_mode();

        assert!(matches!(config.terminal_mode, TerminalMode::Vte));
        assert_eq!(config.window_opacity, 0.95);
        assert_eq!(config.terminal_scrollback_lines, 5_000);
        assert_eq!(config.font_desc, "SauceCodePro Nerd Font Mono 14");
        assert_eq!(config.default_font_scale, 1.0);
        assert_eq!(config.theme_name, "default");
        assert_eq!(config.tab_placement, TabPlacement::Sidebar);
        assert_eq!(config.sidebar_view, SidebarView::Tabs);
        assert!(config.sidebar_visible);
        assert!(config.mouse_reporting_enabled);
        assert!(!config.show_repo_strip);
        assert!(config.shell.is_none());
        assert!(config.startup_commands.is_none());
        assert!(!config.command_history_enabled);
        assert!(config.command_history_path.is_none());
        assert!(config.block_history_path.is_none());
        assert!(!config.ai_enabled);
        assert!(config.ai_api_key_file.is_none());
        assert!(!config.agent_enabled);
        assert!(!config.command_correction_enabled);
        assert!(!config.ai_panel_visible);
        assert!(!config.notify_long_blocks);
        assert!(!config.allow_remote_clipboard_write);
        assert!(config.remote_hosts.is_empty());
    }

    #[test]
    fn safe_mode_environment_disables_configuration_writes() {
        assert!(safe_mode_persistence_disabled(Some(std::ffi::OsStr::new(
            "1"
        ))));
        assert!(safe_mode_persistence_disabled(Some(std::ffi::OsStr::new(
            "TRUE"
        ))));
        assert!(!safe_mode_persistence_disabled(None));
        assert!(!safe_mode_persistence_disabled(Some(std::ffi::OsStr::new(
            "0"
        ))));
    }

    #[test]
    fn sidebar_visibility_default_follows_tab_placement() {
        assert!(resolve_sidebar_visibility(None, TabPlacement::Sidebar));
        assert!(!resolve_sidebar_visibility(None, TabPlacement::TopBar));
        assert!(resolve_sidebar_visibility(Some(true), TabPlacement::TopBar));
        assert!(!resolve_sidebar_visibility(
            Some(false),
            TabPlacement::Sidebar
        ));

        let issues = validate_config_contents(
            "tab_placement = \"top\"\nsidebar_visible = false\nsidebar_width = 220\n",
        )
        .unwrap();
        assert!(issues.is_empty(), "{issues:?}");
    }
}
