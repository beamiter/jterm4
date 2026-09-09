use gtk4::gdk::RGBA;
use gtk4::glib;
use std::cell::RefCell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use crate::keybindings::KeybindingMap;

// ---------------------------------------------------------------------------
// Terminal Mode
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalMode {
    Block,
    Vte,
    /// One long-lived full-size VTE driven by the same OSC 133 lifecycle
    /// engine as Block, with no per-command block chrome: finished commands
    /// are recorded as zones over the single continuous surface instead of
    /// being snapshotted into separate widgets. Experimental.
    Unified,
}

impl TerminalMode {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            TerminalMode::Block => "block",
            TerminalMode::Vte => "vte",
            TerminalMode::Unified => "unified",
        }
    }

    /// `None` for an unrecognized value; callers warn and fall back so a typo
    /// in the config file cannot silently change the backend.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "block" => Some(TerminalMode::Block),
            "vte" => Some(TerminalMode::Vte),
            "unified" => Some(TerminalMode::Unified),
            _ => None,
        }
    }

    /// Whether this mode is rendered by `TermView` (the OSC 133 engine), as
    /// opposed to a conventional VTE pane.
    pub(crate) fn uses_term_view(&self) -> bool {
        matches!(self, TerminalMode::Block | TerminalMode::Unified)
    }

    /// Whether a `TermView` in this mode renders through the Unified backend
    /// (one long-lived full-size surface) rather than Block chrome. This is
    /// the *only* input to that choice: `TermView::new` selects its backend
    /// from the mode its caller passes, never by re-reading the shared
    /// `terminal_mode`, so a pane pinned to Block (managed remote tabs, which
    /// need Block's reconnect metadata and history save/restore) stays Block
    /// under `terminal_mode = "unified"`.
    pub(crate) fn is_unified(&self) -> bool {
        matches!(self, TerminalMode::Unified)
    }
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

/// Motion level for the ASCII organism's visual body. `None` in the config
/// means "auto": follow the desktop's animation preference at attach time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrganismMotion {
    /// Animated frames, wandering, walking transitions.
    Full,
    /// Poses still change at behavior boundaries, but nothing animates:
    /// no wandering, no bobbing, no frame alternation, snap movement.
    Calm,
    /// Inline card only; no live surface body, no sticky avatar.
    Static,
}

impl OrganismMotion {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Calm => "calm",
            Self::Static => "static",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "full" => Some(Self::Full),
            "calm" => Some(Self::Calm),
            "static" => Some(Self::Static),
            _ => None,
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

/// When to check whether a newer jsh has been published. Shared with the other
/// terminals so one config vocabulary covers the family.
pub use crate::jsh_install::UpdateCheck as JshUpdateCheck;

// ---------------------------------------------------------------------------
// Remote host
// ---------------------------------------------------------------------------

/// A saved SSH target. A new tab can be opened that runs the remote shell over
/// `ssh -t`, reusing all local PTY/terminal infrastructure. OSC 133 markers
/// emitted by the remote shell flow through ssh are preserved so session-aware
/// terminal behavior keeps working for remote tabs.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteHost {
    pub name: String,
    /// The ssh destination, or — when `docker` is set — the name of a running
    /// container.
    pub host: String,
    /// The ssh login, or the `docker exec -u` user inside the container.
    pub user: Option<String>,
    /// Reach `host` with `docker exec` instead of ssh. The container has to be
    /// running already: this attaches to one, it does not start one.
    ///
    /// `ssh_args`, `multiplex` and `login_shell` have no meaning here and are
    /// ignored, which is also what the shared launcher does with them.
    pub docker: bool,
    /// A jsh built on this machine for `deploy` to push, instead of the
    /// published release it would otherwise fetch. Without it, deployment on a
    /// machine whose jsh has no release — or with no network — spends a few
    /// seconds failing to reach the release host and then falls back to shell
    /// integration, which keeps blocks but none of jsh's own behaviour.
    ///
    /// Must be an absolute path, and must be a jsh the destination can run:
    /// the launcher checks the binary's own version banner after it lands, but
    /// nothing here can tell whether it was built for that libc.
    pub deploy_artifact: Option<String>,
    /// Shell to launch on the remote side (default "jsh").
    pub remote_shell: String,
    /// Stable session id passed to the remote jsh for resume-on-reconnect.
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
    /// Put a jsh on the destination for the life of the session instead of
    /// hoping one is installed there. `off` (the default) keeps the historical
    /// behaviour: run `remote_shell` over plain ssh and take what is there.
    ///
    /// This is what makes a remote tab a *jterm* tab on a machine nobody has
    /// prepared — blocks, cwd tracking and exit codes all come from jsh, so
    /// without it a bare `sh` on the far side silently drops them.
    pub deploy: jterm_core::jsh_remote::Deploy,
}

pub(crate) const MAX_REMOTE_HOSTS: usize = 128;
const MAX_REMOTE_HOST_FIELD_BYTES: usize = 4 * 1024;
const MAX_REMOTE_SSH_ARGS: usize = 64;
const MAX_FONT_DESC_BYTES: usize = 1024;
const MAX_CONFIG_PATH_BYTES: usize = 16 * 1024;
const MAX_AI_IDENTIFIER_BYTES: usize = 1024;
const MAX_STARTUP_COMMANDS_BYTES: usize = jterm_core::review_input::MAX_REVIEW_INPUT_BYTES;

pub(crate) fn remote_field_is_safe(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_REMOTE_HOST_FIELD_BYTES
        && !value.chars().any(char::is_control)
        && !jterm_core::review_input::contains_visual_spoofing(value)
}

const MAX_REMOTE_ARGV_BYTES: usize = 256 * 1024;

/// `ssh_args` is an option vector, not a second destination/command channel.
/// Accept OpenSSH short options (including attached operands), while rejecting
/// a bare argument or `--` that would make ssh reinterpret the real target as
/// remote command text.
fn remote_ssh_args_are_structured(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let option = args[index].as_str();
        if option == "--" || !option.starts_with('-') || option.starts_with("--") {
            return false;
        }
        let bytes = option.as_bytes();
        if bytes.len() < 2 || !bytes[1].is_ascii_alphabetic() {
            return false;
        }
        let flag = bytes[1] as char;
        let takes_operand = "BbcDEeFIiJLlmOoPpQRSWw".contains(flag);
        if takes_operand {
            if bytes.len() == 2 {
                index += 1;
                if index >= args.len() {
                    return false;
                }
            }
        } else if !option[1..]
            .chars()
            .all(|value| "46AaCfgKkMNnqTtVvXxYy".contains(value))
        {
            return false;
        }
        index += 1;
    }
    true
}

/// The single application-level execution gate for every remote consumer.
/// Config validation is advisory; this function is deliberately repeated at
/// the final argv boundary so a stale snapshot or runtime mutation cannot
/// bypass byte, visual-spoofing, and semantic checks.
pub(crate) fn validate_remote_host(host: &RemoteHost) -> Result<(), &'static str> {
    if !remote_field_is_safe(&host.name) {
        return Err("Remote profile name is invalid; edit it in Settings or config.toml.");
    }
    if !remote_field_is_safe(&host.host)
        || host.host.starts_with('-')
        || host.host.chars().any(char::is_whitespace)
    {
        return Err("Remote target is invalid; edit it in Settings or config.toml.");
    }
    if host.user.as_deref().is_some_and(|user| {
        !remote_field_is_safe(user) || user.contains('@') || user.chars().any(char::is_whitespace)
    }) {
        return Err("Remote user is invalid; edit it in Settings or config.toml.");
    }
    if !remote_field_is_safe(&host.remote_shell) {
        return Err("Remote shell is invalid; edit it in config.toml.");
    }
    if host
        .session
        .as_deref()
        .is_some_and(|session| !jterm_core::execution_journal::is_valid_jsh_session_id(session))
    {
        return Err("Remote session id is invalid; edit it in config.toml.");
    }
    if host.ssh_args.len() > MAX_REMOTE_SSH_ARGS
        || host.ssh_args.iter().any(|arg| !remote_field_is_safe(arg))
        || !remote_ssh_args_are_structured(&host.ssh_args)
    {
        return Err("Remote SSH options are invalid; edit them in config.toml.");
    }
    if host.deploy_artifact.as_deref().is_some_and(|artifact| {
        !remote_field_is_safe(artifact) || !Path::new(artifact).is_absolute()
    }) {
        return Err("Remote deployment artifact is invalid; edit it in config.toml.");
    }
    let argv_bytes = host
        .ssh_args
        .iter()
        .try_fold(
            host.host
                .len()
                .saturating_add(host.name.len())
                .saturating_add(host.remote_shell.len())
                .saturating_add(host.user.as_deref().map_or(0, str::len))
                .saturating_add(host.session.as_deref().map_or(0, str::len))
                .saturating_add(host.deploy_artifact.as_deref().map_or(0, str::len)),
            |total, argument| total.checked_add(argument.len()),
        )
        .unwrap_or(usize::MAX);
    if argv_bytes > MAX_REMOTE_ARGV_BYTES {
        return Err("Remote profile exceeds the execution byte budget.");
    }
    Ok(())
}

pub(crate) fn checked_remote_host(
    hosts: &[RemoteHost],
    index: usize,
) -> Result<&RemoteHost, &'static str> {
    if index >= MAX_REMOTE_HOSTS {
        return Err("Remote host index exceeds the supported 128-profile limit.");
    }
    let host = hosts
        .get(index)
        .ok_or("That remote host is no longer configured.")?;
    validate_remote_host(host)?;
    Ok(host)
}

fn setting_text_is_safe(value: &str, max_bytes: usize) -> bool {
    !value.trim().is_empty()
        && value.len() <= max_bytes
        && !value.chars().any(char::is_control)
        && !jterm_core::review_input::contains_visual_spoofing(value)
}

/// Resolve a textual environment override without letting an invalid override
/// erase a valid file setting. Values are trimmed before they reach parsers or
/// UI labels, and both sources pass through the same display-safety boundary.
fn resolve_setting_text(
    override_value: Option<String>,
    configured_value: Option<String>,
    max_bytes: usize,
) -> Option<String> {
    let normalize = |value: String| {
        let value = value.trim();
        setting_text_is_safe(value, max_bytes).then(|| value.to_string())
    };
    override_value
        .and_then(&normalize)
        .or_else(|| configured_value.and_then(normalize))
}

fn ai_base_url_is_structurally_safe(value: &str) -> bool {
    let value = value.trim();
    if !setting_text_is_safe(value, jagent::provider::MAX_BASE_URL_BYTES)
        || !(value.starts_with("http://") || value.starts_with("https://"))
        || value.chars().any(char::is_whitespace)
        || value.contains(['?', '#', '\\'])
    {
        return false;
    }
    value.split_once("://").is_some_and(|(_, remainder)| {
        let authority = remainder.split('/').next().unwrap_or_default();
        !authority.is_empty() && !authority.contains('@')
    })
}

pub(crate) fn ai_base_url_is_safe(provider: &str, value: &str) -> bool {
    if !ai_base_url_is_structurally_safe(value) {
        return false;
    }
    let provider = match provider.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => jagent::Provider::Anthropic,
        "openai" | "openai-compatible" | "openai_compatible" => jagent::Provider::OpenAiCompatible,
        "ollama" => jagent::Provider::Ollama,
        _ => return false,
    };
    let mut contract = jagent::ChatConfig::new(provider);
    contract.base_url = value.trim().to_string();
    contract.validate().is_ok()
}

/// Resolve an optional endpoint without ever redirecting an explicit invalid
/// destination to a provider's public default. A structurally safe but
/// transport-invalid value is preserved so the canonical client validator
/// rejects it before network I/O and Settings can show the value that needs
/// fixing. Malformed, spoofed, credential-bearing, or unbounded text becomes
/// an empty endpoint, which is likewise an offline failure.
fn resolve_ai_base_url(requested: Option<String>, default: &str) -> String {
    match requested {
        None => default.to_string(),
        Some(value) if ai_base_url_is_structurally_safe(&value) => {
            value.trim().trim_end_matches('/').to_string()
        }
        Some(_) => String::new(),
    }
}

fn configured_path_is_safe(value: &str, require_absolute_or_home: bool) -> bool {
    setting_text_is_safe(value, MAX_CONFIG_PATH_BYTES)
        && (!require_absolute_or_home
            || Path::new(value).is_absolute()
            || (value.starts_with("~/") && !value.starts_with("~//")))
}

/// Normalize a user-configurable history path before it reaches any reader or
/// writer. Only the shell-style `~/` prefix is expanded; a missing, empty, or
/// relative HOME makes that spelling unusable instead of leaving a literal
/// relative path for a later subsystem to interpret differently.
fn expand_configured_path_with(value: &str, home: Option<&Path>) -> Option<String> {
    if !configured_path_is_safe(value, true) {
        return None;
    }
    if let Some(rest) = value.strip_prefix("~/") {
        let home = home.filter(|home| home.is_absolute())?;
        let expanded = home.join(rest).into_os_string().into_string().ok()?;
        return configured_path_is_safe(&expanded, true).then_some(expanded);
    }
    Some(value.to_string())
}

fn normalize_history_path(value: Option<String>, setting: &str) -> Option<String> {
    let path = value?;
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    let expanded = expand_configured_path_with(&path, home.as_deref());
    if expanded.is_none() {
        log::warn!("{setting} is not a safe absolute or ~/ path; ignoring it");
    }
    expanded
}

#[cfg(unix)]
fn open_owned_directory(path: &Path) -> io::Result<fs::File> {
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { nix::libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory is not an owned, non-writable namespace",
        ));
    }
    Ok(directory)
}

#[cfg(unix)]
fn open_trusted_owned_directory(path: &Path) -> io::Result<(PathBuf, fs::File)> {
    let original = open_owned_directory(path)?;
    let canonical = fs::canonicalize(path)?;
    let directory = open_owned_directory(&canonical)?;
    let original_metadata = original.metadata()?;
    let canonical_metadata = directory.metadata()?;
    if original_metadata.dev() != canonical_metadata.dev()
        || original_metadata.ino() != canonical_metadata.ino()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime namespace changed while it was being validated",
        ));
    }
    for ancestor in canonical.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)?;
        if !metadata.is_dir() || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "runtime namespace has an unsafe writable ancestor",
            ));
        }
    }
    Ok((canonical, directory))
}

#[cfg(unix)]
fn ensure_owned_child_directory(
    parent: &fs::File,
    parent_path: &Path,
    name: &str,
    tighten_existing: bool,
) -> io::Result<(PathBuf, fs::File)> {
    let name_c = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid directory name"))?;
    // SAFETY: `parent` and `name_c` remain alive for the call. EEXIST is
    // intentionally accepted and the entry is then opened without following a
    // symlink, so a concurrent creator cannot redirect the namespace.
    let created = unsafe { nix::libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o700) };
    if created != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    // SAFETY: openat returns a new descriptor on success; it is immediately
    // owned by `File` and closed on drop.
    let fd = unsafe {
        nix::libc::openat(
            parent.as_raw_fd(),
            name_c.as_ptr(),
            nix::libc::O_RDONLY
                | nix::libc::O_DIRECTORY
                | nix::libc::O_NOFOLLOW
                | nix::libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let directory = unsafe { fs::File::from_raw_fd(fd) };
    let path = parent_path.join(name);
    let metadata = directory.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { nix::libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not an owned, non-writable directory", path.display()),
        ));
    }
    if tighten_existing || created == 0 {
        directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    Ok((path, directory))
}

#[cfg(unix)]
fn private_control_socket_dir() -> io::Result<PathBuf> {
    let mut failures = Vec::new();
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        match open_trusted_owned_directory(&runtime).and_then(|(runtime, parent)| {
            ensure_owned_child_directory(&parent, &runtime, "forge", true).map(|(path, _)| path)
        }) {
            Ok(path) => return Ok(path),
            Err(error) => failures.push(format!("{}: {error}", runtime.display())),
        }
    }

    // A launcher can omit or overwrite XDG_RUNTIME_DIR even though the
    // systemd-style per-user runtime directory still exists. Validate the
    // conventional location with the same ownership rules before considering
    // a cache fallback.
    let system_runtime = PathBuf::from(format!("/run/user/{}", unsafe { nix::libc::geteuid() }));
    match open_trusted_owned_directory(&system_runtime).and_then(|(system_runtime, parent)| {
        ensure_owned_child_directory(&parent, &system_runtime, "forge", true).map(|(path, _)| path)
    }) {
        Ok(path) => return Ok(path),
        Err(error) => failures.push(format!("{}: {error}", system_runtime.display())),
    }

    if let Some(home) = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        let fallback = (|| {
            let (home, home_directory) = open_trusted_owned_directory(&home)?;
            let (cache_path, cache_directory) =
                ensure_owned_child_directory(&home_directory, &home, ".cache", false)?;
            ensure_owned_child_directory(&cache_directory, &cache_path, "forge", true)
                .map(|(path, _)| path)
        })();
        match fallback {
            Ok(path) => return Ok(path),
            Err(error) => failures.push(format!("{}: {error}", home.display())),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "no private runtime namespace is available{}",
            if failures.is_empty() {
                String::new()
            } else {
                format!(": {}", failures.join("; "))
            }
        ),
    ))
}

#[cfg(not(unix))]
fn private_control_socket_dir() -> io::Result<PathBuf> {
    let path = glib::user_cache_dir().join("forge");
    fs::create_dir_all(&path)?;
    Ok(path)
}

/// Directory for ssh ControlMaster sockets. It is always an owned private
/// child namespace; unsafe XDG/HOME overrides disable multiplexing instead of
/// placing an authentication socket in a writable directory.
fn control_socket_path_is_safe(path: &Path) -> bool {
    path.to_str().is_some_and(|path| {
        path.len() <= MAX_CONFIG_PATH_BYTES
            && !path.contains('%')
            && !path.chars().any(char::is_control)
            && !jterm_core::review_input::contains_visual_spoofing(path)
    })
}

fn control_socket_dir() -> Option<PathBuf> {
    let path = match private_control_socket_dir() {
        Ok(path) => path,
        Err(error) => {
            log::warn!(
                "SSH multiplexing disabled: {}",
                jterm_core::review_input::safe_inline_display(&error.to_string(), 1024)
            );
            return None;
        }
    };
    if !control_socket_path_is_safe(&path) {
        log::warn!(
            "SSH multiplexing disabled because its private ControlPath cannot be represented safely"
        );
        return None;
    }
    #[cfg(unix)]
    {
        // Linux sockaddr_un paths are short. `%C` expands to a 40-byte hash;
        // disable multiplexing rather than handing OpenSSH a predictably
        // unusable or truncated ControlPath.
        let expanded_len = path.join("cm-").as_os_str().as_bytes().len() + 40;
        if expanded_len > 100 {
            log::warn!("SSH multiplexing disabled because its private ControlPath is too long");
            return None;
        }
    }
    Some(path)
}

fn wrap_exec_in_login_bash(command: &str) -> String {
    format!(
        "bash -lc {}",
        crate::process::shell_single_quote(&format!("exec {command}"))
    )
}

fn wrap_jsh_argv_in_interactive_bash(jsh_path: &str) -> Option<Vec<String>> {
    let bash_path = crate::host::interactive_bash_path()?;
    Some(vec![
        bash_path.to_string_lossy().to_string(),
        "-ic".to_string(),
        // `bash -c CMD a b` assigns `a` to `$0` and `b` onward to `$@`. The
        // shell path therefore travels as `$0`, and a `--session <id>` the
        // caller appends reaches the shell through `$@` instead of being
        // consumed as the wrapper's own operands.
        "exec \"$0\" \"$@\"".to_string(),
        jsh_path.to_string(),
    ])
}

/// Build the local argv that connects to a remote host via ssh.
/// Produces e.g. `["ssh", "-t", "-p", "2222", "mm@100.x.x.x", "jsh --session home-main"]`.
pub(crate) fn checked_remote_argv(host: &RemoteHost) -> Result<Vec<String>, &'static str> {
    validate_remote_host(host)?;
    Ok(build_remote_argv(host))
}

/// Low-level argv construction. Production consumers use
/// [`checked_remote_argv`] so the gate is immediately adjacent to execution.
pub(crate) fn build_remote_argv(host: &RemoteHost) -> Vec<String> {
    if host.deploy.is_enabled() {
        match jterm_core::jsh_remote::publish_launcher() {
            Ok(script) => return build_deployed_argv(host, &script),
            // Publishing the launcher is the only thing that can fail here, and
            // it fails for reasons that have nothing to do with the host. Plain
            // ssh still reaches the machine, so degrade to it rather than
            // refusing to open the tab at all.
            Err(err) => log::warn!(
                "Cannot publish jsh-remote.sh for {}: {err}; connecting without deployment",
                host.name
            ),
        }
    }
    if host.docker {
        return build_docker_argv(host);
    }
    let control_dir = host.multiplex.then(control_socket_dir).flatten();
    build_remote_argv_with_control_dir(host, control_dir.as_deref())
}

/// argv for a tab that deploys jsh, given a launcher already on disk. Split out
/// from [`build_remote_argv`] so it can be asserted without publishing anything.
fn build_deployed_argv(host: &RemoteHost, script: &std::path::Path) -> Vec<String> {
    // A container takes its user through `--docker-user`, not through an
    // `user@host` destination that `docker exec` would read as a container
    // name nobody has.
    let target = match (&host.user, host.docker) {
        (Some(u), false) => format!("{u}@{}", host.host),
        _ => host.host.clone(),
    };
    jterm_core::jsh_remote::launch_argv_with_script(
        script,
        &jterm_core::jsh_remote::RemoteTarget {
            destination: &target,
            docker: host.docker,
            docker_user: host.docker.then_some(host.user.as_deref()).flatten(),
            artifact: host.deploy_artifact.as_deref().map(Path::new),
            session: host.session.as_deref(),
            ssh_args: &host.ssh_args,
            deploy: host.deploy,
        },
    )
}

/// argv for a container tab that deploys nothing, for an image that already
/// carries the shell. The ssh path's counterpart is
/// [`build_remote_argv_with_control_dir`]; there is no connection to multiplex
/// and no login shell to wrap, because `docker exec` starts a process rather
/// than a session.
fn build_docker_argv(host: &RemoteHost) -> Vec<String> {
    let mut argv = vec!["docker".to_string(), "exec".to_string(), "-it".to_string()];
    if let Some(user) = &host.user {
        argv.push("-u".to_string());
        argv.push(user.clone());
    }
    argv.push(host.host.clone());
    argv.push(host.remote_shell.clone());
    if let Some(sid) = host
        .session
        .as_deref()
        .filter(|sid| jterm_core::execution_journal::is_valid_jsh_session_id(sid))
    {
        argv.push("--session".to_string());
        argv.push(sid.to_string());
    }
    argv
}

fn build_remote_argv_with_control_dir(
    host: &RemoteHost,
    control_dir: Option<&Path>,
) -> Vec<String> {
    let target = match &host.user {
        Some(u) => format!("{u}@{}", host.host),
        None => host.host.clone(),
    };
    let mut remote_cmd = host.remote_shell.clone();
    if let Some(sid) = host
        .session
        .as_deref()
        .filter(|sid| jterm_core::execution_journal::is_valid_jsh_session_id(sid))
    {
        remote_cmd.push_str(" --session ");
        remote_cmd.push_str(sid);
    }
    if host.login_shell {
        remote_cmd = wrap_exec_in_login_bash(&remote_cmd);
    }
    let mut argv = vec!["ssh".to_string(), "-t".to_string()];
    if let Some(dir) = control_dir.filter(|_| host.multiplex) {
        // %C is ssh's hash of (local user, host, port, user) — a safe filename.
        let ctl_path = dir.join("cm-%C");
        argv.push("-o".to_string());
        argv.push("ControlMaster=auto".to_string());
        argv.push("-o".to_string());
        argv.push("ControlPersist=120".to_string());
        argv.push("-o".to_string());
        argv.push(format!("ControlPath={}", ctl_path.display()));
    }
    argv.extend(host.ssh_args.iter().cloned());
    argv.push("--".to_string());
    argv.push(target);
    argv.push(remote_cmd);
    argv
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

pub(crate) const MIN_TAB_WIDTH: u32 = 80;
pub(crate) const MAX_TAB_WIDTH: u32 = 480;
pub(crate) const DEFAULT_TAB_WIDTH: u32 = 180;

// A Nerd Font by default: a plain `Monospace` covers no icon glyph, and the
// per-glyph fallback that follows is fontconfig's global sort, which hands
// prompt and statusline icons to whichever font claims the Private Use Area.
pub(crate) const DEFAULT_FONT_DESC: &str = "JetBrainsMono Nerd Font Mono 14";

#[derive(Clone)]
pub struct Config {
    pub(crate) window_opacity: f64,
    pub(crate) terminal_scrollback_lines: u32,
    pub(crate) font_desc: String,
    /// Family Pango falls back to for icon glyphs `font_desc` does not
    /// cover. `None` auto-detects an installed Nerd Font; `"none"` leaves
    /// the glyphs to fontconfig's global sort.
    pub(crate) icon_font: Option<String>,
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
    /// When to look for a newer jsh. Installing always stays an explicit
    /// choice: this only governs whether the offer appears.
    pub(crate) jsh_update_check: JshUpdateCheck,
    /// Whether the left sidebar is visible. When absent from an older config,
    /// startup derives the default from tab placement: open for sidebar tabs,
    /// closed for top-bar tabs.
    pub(crate) sidebar_visible: bool,
    /// Sidebar width in pixels (resizable divider position).
    pub(crate) sidebar_width: u32,
    /// Width of each tab in the horizontal top tab strip, in pixels.
    pub(crate) tab_width: u32,
    // Block view optimizations
    pub(crate) max_visible_blocks: u32,
    pub(crate) lazy_load_threshold: u32,
    pub(crate) truncation_threshold_lines: u32,
    /// Output rows shown before a finished block is considered long and gains
    /// top/bottom navigation controls.
    pub(crate) finished_block_viewport_rows: u32,
    /// Maximum rows allocated when a long finished block is expanded. Output
    /// beyond this remains reachable through the block's inner scrollbar.
    pub(crate) finished_block_max_expanded_rows: u32,
    #[allow(dead_code)]
    pub(crate) max_collapsed_output_lines: u32,
    pub(crate) virtual_scroll_margin: u32,
    /// Lightweight JSONL command index. Unlike block snapshots this stores no
    /// terminal output, only command metadata used by history/palette UIs.
    pub(crate) command_history_enabled: bool,
    /// Absolute after configuration loading; `~/` never reaches consumers.
    pub(crate) command_history_path: Option<String>,
    pub(crate) command_history_max_entries: u32,
    /// Absolute after configuration loading; `~/` never reaches consumers.
    pub(crate) block_history_path: Option<String>,
    pub(crate) block_history_compress: bool,
    /// Use anvil/Warp-style denser block spacing.
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
    /// Show the experimental no-LLM ASCII organism in Block panes. The widget
    /// reacts only to Forge's local command lifecycle events and never runs a
    /// command or sends terminal contents elsewhere.
    pub(crate) ascii_organism_enabled: bool,
    /// Explicit motion level for the organism body; `None` follows the
    /// desktop animation preference.
    pub(crate) ascii_organism_motion: Option<OrganismMotion>,
    /// Master switch for every network-backed AI feature.
    pub(crate) ai_enabled: bool,
    /// Agent mode can be disabled independently while leaving chat and
    /// natural-language command generation available.
    pub(crate) agent_enabled: bool,
    /// Allow the native Codex agent Tasks panel (isolated-worktree tasks
    /// driven through the contained app-server). Default off, matching
    /// ember's experimental rollout posture: a security surface this size
    /// stays opt-in until it has seen real-world mileage. File-only toggle.
    pub(crate) agent_tasks_enabled: bool,
    /// Explicit consent to attach command/terminal evidence to provider
    /// prompts. Required (together with `ai_enabled`) before a native Codex
    /// task may start or receive follow-up turns; default off so no terminal
    /// content leaves the machine without an explicit opt-in. File-only.
    pub(crate) ai_share_command_context: bool,
    /// Maximum number of model replies in one Agent session.
    pub(crate) agent_max_turns: u32,
    /// Retired compatibility setting. It is still parsed so old configs remain
    /// readable, but runtime loading always normalizes it to false: command
    /// text alone cannot prove what aliases, functions, helpers, or flags will
    /// actually execute.
    pub(crate) agent_auto_approve_readonly: bool,
    /// Offer an editable, review-first correction when a Block command fails
    /// with a narrow typo-shaped error. Nothing is inserted or run
    /// automatically.
    pub(crate) command_correction_enabled: bool,
    /// Provider wire protocol: anthropic, openai-compatible, or ollama.
    pub(crate) ai_provider: String,
    /// Provider API root. Endpoint suffixes are added by the AI client.
    pub(crate) ai_base_url: String,
    /// Optional owner-only file used when no AI API key environment variable
    /// is present. This is the effective path after environment overrides.
    pub(crate) ai_api_key_file: Option<String>,
    /// File-configured key path used as the Settings write target and persisted
    /// back to TOML. Keeping it separate prevents an environment-managed
    /// secret path from accidentally becoming writable UI state.
    pub(crate) ai_api_key_file_configured: Option<String>,
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
    /// Optional sampling temperature (0.0..=2.0); None keeps the provider default.
    pub(crate) ai_temperature: Option<f32>,
    /// Stream AI chat replies into the panel as they arrive instead of
    /// waiting for the complete response. Only the conversational chat panel
    /// streams; strict-JSON surfaces (Agent, command generation, correction)
    /// always wait for the full reply.
    pub(crate) ai_stream: bool,
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
    /// Show the window-global bottom status bar (cwd, git, last command,
    /// grid size, tab position). Family-wide key from
    /// `jterm_core::bottom_bar`; every jterm spells it `bottom_bar`.
    pub(crate) bottom_bar: bool,
    /// A plain click in the live prompt places the shell's edit cursor there.
    /// Family-wide key from `jterm_core::click_cursor`; every jterm spells it
    /// `click_moves_cursor`.
    pub(crate) click_moves_cursor: bool,
    /// Exact disk revision this loaded configuration is allowed to replace.
    /// Clones from one window share the revision and advance it only after a
    /// durable save; independently loaded windows retain their own revisions.
    pub(crate) persistence_revision:
        std::sync::Arc<std::sync::Mutex<Option<crate::config_store::ConfigRevision>>>,
}

impl Config {
    /// Replace the complete configuration with an isolated, built-in VTE
    /// profile. This deliberately ignores both the user's file and FORGE_*
    /// appearance/behavior overrides, making safe mode useful for diagnosis.
    #[cfg(test)]
    pub(crate) fn apply_safe_mode(&mut self) {
        *self = Self::safe_defaults();
    }

    pub(crate) fn safe_defaults() -> Self {
        let themes = builtin_themes();
        let theme = &themes[0];
        Self {
            window_opacity: 0.95,
            terminal_scrollback_lines: 5_000,
            font_desc: DEFAULT_FONT_DESC.to_string(),
            icon_font: None,
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
            // Recovery mode must never initiate update/network work.
            jsh_update_check: JshUpdateCheck::Never,
            sidebar_visible: true,
            sidebar_width: 220,
            tab_width: DEFAULT_TAB_WIDTH,
            max_visible_blocks: 200,
            lazy_load_threshold: 1_000,
            truncation_threshold_lines: 50_000,
            finished_block_viewport_rows: 24,
            finished_block_max_expanded_rows: 5_000,
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
            ascii_organism_enabled: false,
            ascii_organism_motion: None,
            ai_enabled: false,
            agent_enabled: false,
            agent_tasks_enabled: false,
            ai_share_command_context: false,
            agent_max_turns: 20,
            agent_auto_approve_readonly: false,
            command_correction_enabled: false,
            ai_provider: "anthropic".to_string(),
            ai_base_url: "https://api.anthropic.com".to_string(),
            ai_api_key_file: None,
            ai_api_key_file_configured: None,
            ai_panel_visible: false,
            ai_panel_width: 360,
            ai_model: "claude-sonnet-4-6".to_string(),
            ai_max_tokens: 1_024,
            ai_temperature: None,
            ai_stream: true,
            ai_redact_secrets: true,
            allow_remote_clipboard_write: false,
            notify_long_blocks: false,
            notify_long_block_threshold_ms: 10_000,
            bottom_bar: true,
            click_moves_cursor: jterm_core::click_cursor::ENABLED_BY_DEFAULT,
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

/// WCAG contrast target for ordinary UI text. ANSI colors inside the terminal
/// remain byte-for-byte theme colors; this applies only when those hues are
/// reused as labels, badges, and status indicators on Forge chrome.
pub(crate) const UI_TEXT_MIN_CONTRAST: f64 = 4.5;

fn relative_luminance(color: &RGBA) -> f64 {
    fn linear(channel: f32) -> f64 {
        let channel = f64::from(channel.clamp(0.0, 1.0));
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }

    0.2126 * linear(color.red()) + 0.7152 * linear(color.green()) + 0.0722 * linear(color.blue())
}

pub(crate) fn contrast_ratio(first: &RGBA, second: &RGBA) -> f64 {
    let first = relative_luminance(first);
    let second = relative_luminance(second);
    let (lighter, darker) = if first >= second {
        (first, second)
    } else {
        (second, first)
    };
    (lighter + 0.05) / (darker + 0.05)
}

fn mix_color(from: &RGBA, to: &RGBA, amount: f32) -> RGBA {
    let amount = amount.clamp(0.0, 1.0);
    let mix = |start: f32, end: f32| start + (end - start) * amount;
    RGBA::new(
        mix(from.red(), to.red()),
        mix(from.green(), to.green()),
        mix(from.blue(), to.blue()),
        1.0,
    )
}

/// Preserve a preferred semantic hue when it is readable, otherwise move the
/// smallest sampled distance toward the theme foreground. A black/white
/// fallback guarantees a result even for a user-supplied foreground that is
/// itself indistinguishable from the background.
pub(crate) fn accessible_text_color(
    preferred: &RGBA,
    background: &RGBA,
    foreground: &RGBA,
) -> RGBA {
    if contrast_ratio(preferred, background) >= UI_TEXT_MIN_CONTRAST {
        return RGBA::new(preferred.red(), preferred.green(), preferred.blue(), 1.0);
    }

    let black = RGBA::new(0.0, 0.0, 0.0, 1.0);
    let white = RGBA::new(1.0, 1.0, 1.0, 1.0);
    let fallback = if contrast_ratio(foreground, background) >= UI_TEXT_MIN_CONTRAST {
        RGBA::new(foreground.red(), foreground.green(), foreground.blue(), 1.0)
    } else if contrast_ratio(&black, background) >= contrast_ratio(&white, background) {
        black
    } else {
        white
    };

    // Sampling is deterministic, cheap (five colors on a theme change), and
    // does not assume luminance is monotonic when two colored endpoints have
    // channels moving in opposite directions.
    for step in 1..=512 {
        let candidate = mix_color(preferred, &fallback, step as f32 / 512.0);
        if contrast_ratio(&candidate, background) >= UI_TEXT_MIN_CONTRAST {
            return candidate;
        }
    }
    fallback
}

/// Derive a visually quieter foreground without using alpha transparency.
///
/// Alpha-blending ordinary text into the background makes contrast depend on
/// the surface underneath it and caused several light themes to fall well
/// below the WCAG target. Instead, walk an opaque readable foreground toward
/// the background and keep the most-muted sampled color that still passes.
pub(crate) fn accessible_muted_text_color(foreground: &RGBA, background: &RGBA) -> RGBA {
    let readable = accessible_text_color(foreground, background, foreground);
    let mut muted = readable;
    for step in 1..=512 {
        let candidate = mix_color(&readable, background, step as f32 / 512.0);
        if contrast_ratio(&candidate, background) >= UI_TEXT_MIN_CONTRAST {
            muted = candidate;
        }
    }
    muted
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SemanticColors {
    pub(crate) foreground: RGBA,
    pub(crate) muted: RGBA,
    pub(crate) success: RGBA,
    pub(crate) error: RGBA,
    pub(crate) warning: RGBA,
    pub(crate) accent: RGBA,
    pub(crate) info: RGBA,
}

impl Config {
    pub(crate) fn semantic_colors(&self) -> SemanticColors {
        SemanticColors {
            foreground: accessible_text_color(&self.foreground, &self.background, &self.foreground),
            muted: accessible_muted_text_color(&self.foreground, &self.background),
            success: accessible_text_color(&self.palette[2], &self.background, &self.foreground),
            error: accessible_text_color(&self.palette[1], &self.background, &self.foreground),
            warning: accessible_text_color(&self.palette[3], &self.background, &self.foreground),
            accent: accessible_text_color(&self.palette[6], &self.background, &self.foreground),
            info: accessible_text_color(&self.palette[4], &self.background, &self.foreground),
        }
    }
}

// ---------------------------------------------------------------------------
// Env helpers
// ---------------------------------------------------------------------------

fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

fn env_u32(name: &str) -> Option<u32> {
    std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok())
}

fn env_f32(name: &str) -> Option<f32> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|value| value.is_finite())
}

fn finite_clamp_f64(value: Option<f64>, fallback: f64, min: f64, max: f64) -> f64 {
    value
        .filter(|value| value.is_finite())
        .unwrap_or(fallback)
        .clamp(min, max)
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
    if let Some(path) = std::env::var_os("FORGE_CONFIG").filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    glib::user_config_dir().join("forge").join("config.toml")
}

pub(crate) fn default_ai_api_key_path() -> String {
    glib::user_config_dir()
        .join("forge")
        .join("ai.key")
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn ai_api_key_file_env_override() -> Option<String> {
    env_string("FORGE_AI_API_KEY_FILE").filter(|path| configured_path_is_safe(path, true))
}

pub(crate) fn default_command_history_path() -> String {
    xdg_state_home()
        .join("forge")
        .join("history.jsonl")
        .to_string_lossy()
        .into_owned()
}

/// Private, local state used by the experimental native ASCII organism.
///
/// This is intentionally not configurable: command metadata must never make
/// an arbitrary user-selected path writable. Tests and isolated launches can
/// still redirect it through the standard `XDG_STATE_HOME` environment.
pub(crate) fn default_ascii_organism_memory_path() -> PathBuf {
    // Keep the native schema isolated from the standalone prototype, whose
    // historical `ascii-organism.json` uses an incompatible version-1 shape.
    xdg_state_home()
        .join("forge")
        .join("ascii-organism-native.json")
}

/// GLib only exposes `g_get_user_state_dir()` behind a newer API feature than
/// forge currently requires, so implement the XDG Base Directory rule
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
    "jsh_update_check",
    "sidebar_visible",
    "sidebar_width",
    "tab_width",
    "max_visible_blocks",
    "lazy_load_threshold",
    "truncation_threshold_lines",
    "finished_block_viewport_rows",
    "finished_block_max_expanded_rows",
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
    "ascii_organism_enabled",
    "ascii_organism_motion",
    "ai_enabled",
    "agent_enabled",
    "agent_tasks_enabled",
    "ai_share_command_context",
    "agent_max_turns",
    "agent_auto_approve_readonly",
    "command_correction_enabled",
    "ai_provider",
    "ai_base_url",
    "ai_api_key_file",
    "ai_panel_visible",
    "ai_panel_width",
    "ai_model",
    "ai_max_tokens",
    "ai_temperature",
    "ai_stream",
    "ai_redact_secrets",
    "allow_remote_clipboard_write",
    "notify_long_blocks",
    "notify_long_block_threshold_ms",
    "bottom_bar",
    "click_moves_cursor",
];

const REMOTE_HOST_CONFIG_KEYS: &[&str] = &[
    "name",
    "host",
    "user",
    "remote_shell",
    "session",
    "ssh_args",
    "login_shell",
    "multiplex",
    "deploy",
    "docker",
    "deploy_artifact",
];

fn config_issue(
    issues: &mut Vec<ConfigIssue>,
    level: ConfigIssueLevel,
    path: impl Into<String>,
    message: impl Into<String>,
) {
    issues.push(ConfigIssue {
        level,
        path: jterm_core::review_input::safe_inline_display(&path.into(), 512),
        message: jterm_core::review_input::safe_inline_display(&message.into(), 2 * 1024),
    });
}

fn validate_remote_host_string(
    issues: &mut Vec<ConfigIssue>,
    value: Option<&toml::Value>,
    path: &str,
    required: bool,
) {
    let Some(value) = value else {
        if required {
            config_issue(
                issues,
                ConfigIssueLevel::Error,
                path,
                "missing required string",
            );
        }
        return;
    };
    let Some(value) = value.as_str() else {
        config_issue(issues, ConfigIssueLevel::Error, path, "expected a string");
        return;
    };
    if value.trim().is_empty() {
        config_issue(issues, ConfigIssueLevel::Error, path, "must not be empty");
    } else if value.len() > MAX_REMOTE_HOST_FIELD_BYTES {
        config_issue(
            issues,
            ConfigIssueLevel::Error,
            path,
            format!("must not exceed {MAX_REMOTE_HOST_FIELD_BYTES} bytes"),
        );
    } else if value.chars().any(char::is_control) {
        config_issue(
            issues,
            ConfigIssueLevel::Error,
            path,
            "must not contain control characters",
        );
    } else if jterm_core::review_input::contains_visual_spoofing(value) {
        config_issue(
            issues,
            ConfigIssueLevel::Error,
            path,
            "must not contain invisible or bidirectional formatting characters",
        );
    }
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
        "ascii_organism_motion",
        "jsh_update_check",
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
        "tab_width",
        "max_visible_blocks",
        "lazy_load_threshold",
        "truncation_threshold_lines",
        "finished_block_viewport_rows",
        "finished_block_max_expanded_rows",
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
        "ascii_organism_enabled",
        "sidebar_visible",
        "ai_enabled",
        "agent_enabled",
        "agent_tasks_enabled",
        "ai_share_command_context",
        "agent_auto_approve_readonly",
        "command_correction_enabled",
        "ai_panel_visible",
        "ai_stream",
        "ai_redact_secrets",
        "allow_remote_clipboard_write",
        "notify_long_blocks",
        "bottom_bar",
        "click_moves_cursor",
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
            if !value.is_finite() {
                config_issue(
                    issues,
                    Warning,
                    key,
                    "must be finite; the safe default will be used",
                );
            } else if !(min..=max).contains(&value) {
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
    warn_int_range(
        &mut issues,
        "tab_width",
        MIN_TAB_WIDTH as i64,
        MAX_TAB_WIDTH as i64,
    );
    warn_int_range(&mut issues, "max_visible_blocks", 1, 100_000);
    warn_int_range(&mut issues, "lazy_load_threshold", 1, 10_000_000);
    warn_int_range(&mut issues, "truncation_threshold_lines", 1, 10_000_000);
    warn_int_range(&mut issues, "finished_block_viewport_rows", 3, 5_000);
    warn_int_range(&mut issues, "finished_block_max_expanded_rows", 3, 5_000);
    if let (Some(viewport), Some(expanded)) = (
        table
            .get("finished_block_viewport_rows")
            .and_then(toml::Value::as_integer),
        table
            .get("finished_block_max_expanded_rows")
            .and_then(toml::Value::as_integer),
    ) {
        if expanded < viewport {
            config_issue(
                &mut issues,
                Warning,
                "finished_block_max_expanded_rows",
                "must be at least finished_block_viewport_rows; it will be clamped",
            );
        }
    }
    warn_int_range(&mut issues, "max_collapsed_output_lines", 1, 1_000_000);
    warn_int_range(&mut issues, "virtual_scroll_margin", 0, 10_000);
    warn_int_range(&mut issues, "command_history_max_entries", 100, 1_000_000);
    warn_int_range(&mut issues, "agent_max_turns", 1, 100);
    warn_int_range(&mut issues, "ai_panel_width", 240, 1200);
    warn_int_range(&mut issues, "ai_max_tokens", 64, 32_768);
    warn_int_range(&mut issues, "notify_long_block_threshold_ms", 0, i64::MAX);

    if table
        .get("agent_auto_approve_readonly")
        .and_then(toml::Value::as_bool)
        == Some(true)
    {
        config_issue(
            &mut issues,
            Warning,
            "agent_auto_approve_readonly",
            "retired for safety; every Agent proposal now requires explicit approval",
        );
    }

    for (key, max_bytes) in [
        ("font", MAX_FONT_DESC_BYTES),
        ("shell", MAX_CONFIG_PATH_BYTES),
        ("startup_commands", MAX_STARTUP_COMMANDS_BYTES),
        ("ai_model", MAX_AI_IDENTIFIER_BYTES),
    ] {
        if let Some(value) = table.get(key).and_then(toml::Value::as_str) {
            if !setting_text_is_safe(value, max_bytes) {
                config_issue(
                    &mut issues,
                    Error,
                    key,
                    format!(
                        "must be non-empty, at most {max_bytes} bytes, and contain no control or invisible formatting characters"
                    ),
                );
            }
        }
    }

    for key in ["command_history_path", "block_history_path"] {
        if let Some(value) = table.get(key).and_then(toml::Value::as_str) {
            if !configured_path_is_safe(value, true) {
                config_issue(
                    &mut issues,
                    Error,
                    key,
                    format!(
                        "must be an absolute or ~/ path, at most {MAX_CONFIG_PATH_BYTES} bytes, and contain no control or invisible formatting characters"
                    ),
                );
            }
        }
    }

    if let Some(mode) = table.get("terminal_mode").and_then(toml::Value::as_str) {
        if TerminalMode::parse(mode).is_none() {
            config_issue(
                &mut issues,
                Error,
                "terminal_mode",
                "expected 'block', 'vte' or 'unified'",
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
    if let Some(url) = table.get("ai_base_url").and_then(toml::Value::as_str) {
        let provider = table
            .get("ai_provider")
            .and_then(toml::Value::as_str)
            .unwrap_or("anthropic");
        if !ai_base_url_is_safe(provider, url) {
            config_issue(
                &mut issues,
                Error,
                "ai_base_url",
                "expected a bounded absolute HTTPS URL without credentials or ambiguous components; plain HTTP is allowed only for a loopback endpoint",
            );
        }
    }
    if let Some(path) = table.get("ai_api_key_file").and_then(toml::Value::as_str) {
        if !configured_path_is_safe(path, true) {
            config_issue(
                &mut issues,
                Error,
                "ai_api_key_file",
                "expected a bounded absolute path or ~/ path without control or invisible formatting characters",
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
    if let Some(value) = table
        .get("ascii_organism_motion")
        .and_then(toml::Value::as_str)
    {
        if OrganismMotion::parse(value).is_none() {
            config_issue(
                &mut issues,
                Error,
                "ascii_organism_motion",
                "expected 'full', 'calm', or 'static'",
            );
        }
    }
    if let Some(value) = table.get("jsh_update_check").and_then(toml::Value::as_str) {
        if !matches!(
            value.to_ascii_lowercase().as_str(),
            "startup" | "launch" | "always" | "daily" | "never" | "off" | "disabled"
        ) {
            config_issue(
                &mut issues,
                Error,
                "jsh_update_check",
                "expected 'startup', 'daily' or 'never'",
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
            // Validate the complete effective map through the exact atomic path
            // runtime loading uses. This catches collisions with unchanged
            // defaults while still permitting explicit swaps/reassignments.
            if let Err(binding_issues) = KeybindingMap::from_user_overrides(bindings) {
                for issue in binding_issues {
                    config_issue(
                        &mut issues,
                        Error,
                        format!("keybindings.{}", issue.key),
                        issue.message,
                    );
                }
            }
        } else {
            config_issue(&mut issues, Error, "keybindings", "expected a table");
        }
    }

    if let Some(hosts) = table.get("remote_hosts") {
        if let Some(hosts) = hosts.as_array() {
            if hosts.len() > MAX_REMOTE_HOSTS {
                config_issue(
                    &mut issues,
                    Error,
                    "remote_hosts",
                    format!("must not contain more than {MAX_REMOTE_HOSTS} entries"),
                );
            }
            for (index, host) in hosts.iter().take(MAX_REMOTE_HOSTS).enumerate() {
                let path = format!("remote_hosts[{index}]");
                let Some(host) = host.as_table() else {
                    config_issue(&mut issues, Error, path, "expected a table");
                    continue;
                };
                for key in host.keys() {
                    if !REMOTE_HOST_CONFIG_KEYS.contains(&key.as_str()) {
                        config_issue(
                            &mut issues,
                            Warning,
                            format!("{path}.{key}"),
                            "unknown remote host option; it will be ignored",
                        );
                    }
                }
                validate_remote_host_string(
                    &mut issues,
                    host.get("host"),
                    &format!("{path}.host"),
                    true,
                );
                for key in ["name", "user", "remote_shell", "session"] {
                    let field_path = format!("{path}.{key}");
                    validate_remote_host_string(&mut issues, host.get(key), &field_path, false);
                }
                if let Some(session) = host.get("session").and_then(toml::Value::as_str) {
                    if !jterm_core::execution_journal::is_valid_jsh_session_id(session) {
                        config_issue(
                            &mut issues,
                            Error,
                            format!("{path}.session"),
                            "must be 1-128 ASCII letters, digits, '_' or '-'",
                        );
                    }
                }
                if let Some(target) = host.get("host").and_then(toml::Value::as_str) {
                    if target.starts_with('-') || target.chars().any(char::is_whitespace) {
                        config_issue(
                            &mut issues,
                            Error,
                            format!("{path}.host"),
                            "must not begin with '-' or contain whitespace",
                        );
                    }
                }
                if let Some(user) = host.get("user").and_then(toml::Value::as_str) {
                    if user.contains('@') || user.chars().any(char::is_whitespace) {
                        config_issue(
                            &mut issues,
                            Error,
                            format!("{path}.user"),
                            "must not contain '@' or whitespace",
                        );
                    }
                }
                if let Some(args) = host.get("ssh_args") {
                    match args.as_array() {
                        Some(values) => {
                            if values.len() > MAX_REMOTE_SSH_ARGS {
                                config_issue(
                                    &mut issues,
                                    Error,
                                    format!("{path}.ssh_args"),
                                    format!(
                                        "must not contain more than {MAX_REMOTE_SSH_ARGS} entries"
                                    ),
                                );
                            }
                            for (arg_index, value) in
                                values.iter().take(MAX_REMOTE_SSH_ARGS).enumerate()
                            {
                                validate_remote_host_string(
                                    &mut issues,
                                    Some(value),
                                    &format!("{path}.ssh_args[{arg_index}]"),
                                    true,
                                );
                            }
                        }
                        None => config_issue(
                            &mut issues,
                            Error,
                            format!("{path}.ssh_args"),
                            "expected an array of strings",
                        ),
                    }
                }
                for key in ["login_shell", "multiplex", "docker"] {
                    if host.get(key).is_some_and(|value| !value.is_bool()) {
                        config_issue(
                            &mut issues,
                            Error,
                            format!("{path}.{key}"),
                            "expected true or false",
                        );
                    }
                }
                if let Some(value) = host.get("deploy_artifact") {
                    let field_path = format!("{path}.deploy_artifact");
                    match value.as_str() {
                        None => config_issue(
                            &mut issues,
                            Error,
                            &field_path,
                            "expected a path to a jsh binary",
                        ),
                        Some(text) if !std::path::Path::new(text).is_absolute() => config_issue(
                            &mut issues,
                            Error,
                            &field_path,
                            "must be an absolute path; a relative one would \
                             resolve against whatever directory the tab starts in",
                        ),
                        Some(text) => {
                            validate_remote_host_string(
                                &mut issues,
                                Some(value),
                                &field_path,
                                true,
                            );
                            // Reported rather than refused: the file can appear
                            // later, and a host that is otherwise fine should
                            // still open. Left silent, a missing artifact looks
                            // like deployment simply not working.
                            if !std::path::Path::new(text).is_file() {
                                config_issue(
                                    &mut issues,
                                    Warning,
                                    &field_path,
                                    "no such file; deployment will fall back to fetching a release",
                                );
                            }
                            if !host
                                .get("deploy")
                                .and_then(toml::Value::as_str)
                                .and_then(jterm_core::jsh_remote::Deploy::parse)
                                .is_some_and(|deploy| deploy.is_enabled())
                            {
                                config_issue(
                                    &mut issues,
                                    Warning,
                                    &field_path,
                                    "has no effect without deploy = \"persist\" or \"incognito\"",
                                );
                            }
                        }
                    }
                }
                if host.get("docker").and_then(toml::Value::as_bool) == Some(true) {
                    // Warned about rather than refused: they are inert for a
                    // container, and a host that was converted from ssh should
                    // open rather than fail on a leftover key.
                    for key in ["ssh_args", "multiplex", "login_shell"] {
                        if host.contains_key(key) {
                            config_issue(
                                &mut issues,
                                Warning,
                                format!("{path}.{key}"),
                                "has no effect on a docker host; it will be ignored",
                            );
                        }
                    }
                }
                if let Some(value) = host.get("deploy") {
                    let understood = value
                        .as_str()
                        .is_some_and(|text| jterm_core::jsh_remote::Deploy::parse(text).is_some());
                    if !understood {
                        // Naming the accepted values matters more here than
                        // usual: the difference between them is whether the
                        // destination's home directory gets written to.
                        config_issue(
                            &mut issues,
                            Error,
                            format!("{path}.deploy"),
                            "expected \"off\", \"persist\", or \"incognito\"",
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

/// A content-free TOML syntax diagnostic suitable for CLI output, logs, and
/// toast notifications. `toml::de::Error`'s Display implementation may embed a
/// source excerpt; keep only its parser message and derive line/column from the
/// byte span ourselves so diagnostics never echo configuration values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfigSyntaxDiagnostic {
    pub(crate) line: Option<usize>,
    pub(crate) column: Option<usize>,
    pub(crate) message: String,
}

impl std::fmt::Display for ConfigSyntaxDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.line, self.column) {
            (Some(line), Some(column)) => {
                write!(formatter, "line {line}, column {column}: {}", self.message)
            }
            _ => formatter.write_str(&self.message),
        }
    }
}

pub(crate) fn config_syntax_diagnostic(
    contents: &str,
    error: &toml::de::Error,
) -> ConfigSyntaxDiagnostic {
    let location = error.span().map(|span| {
        let mut offset = span.start.min(contents.len());
        while offset > 0 && !contents.is_char_boundary(offset) {
            offset -= 1;
        }
        let prefix = &contents[..offset];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let column = prefix.rsplit_once('\n').map_or(prefix, |(_, tail)| tail);
        (line, column.chars().count() + 1)
    });
    ConfigSyntaxDiagnostic {
        line: location.map(|(line, _)| line),
        column: location.map(|(_, column)| column),
        message: jterm_core::review_input::safe_inline_display(error.message(), 512),
    }
}

/// Parsed TOML config file structure.
#[derive(Default)]
struct FileConfig {
    opacity: Option<f64>,
    scrollback: Option<u32>,
    font: Option<String>,
    icon_font: Option<String>,
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
    jsh_update_check: Option<String>,
    sidebar_visible: Option<bool>,
    sidebar_width: Option<u32>,
    tab_width: Option<u32>,
    // Block view optimizations
    max_visible_blocks: Option<u32>,
    lazy_load_threshold: Option<u32>,
    truncation_threshold_lines: Option<u32>,
    finished_block_viewport_rows: Option<u32>,
    finished_block_max_expanded_rows: Option<u32>,
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
    ascii_organism_enabled: Option<bool>,
    ascii_organism_motion: Option<String>,
    ai_enabled: Option<bool>,
    agent_enabled: Option<bool>,
    agent_tasks_enabled: Option<bool>,
    ai_share_command_context: Option<bool>,
    agent_max_turns: Option<u32>,
    agent_auto_approve_readonly: Option<bool>,
    command_correction_enabled: Option<bool>,
    ai_provider: Option<String>,
    ai_base_url: Option<String>,
    ai_api_key_file: Option<String>,
    ai_panel_visible: Option<bool>,
    ai_panel_width: Option<u32>,
    ai_model: Option<String>,
    ai_max_tokens: Option<u32>,
    ai_temperature: Option<f32>,
    ai_stream: Option<bool>,
    ai_redact_secrets: Option<bool>,
    allow_remote_clipboard_write: Option<bool>,
    notify_long_blocks: Option<bool>,
    notify_long_block_threshold_ms: Option<u64>,
    bottom_bar: Option<bool>,
    click_moves_cursor: Option<bool>,
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

/// Why the configuration file was not read, when it was not.
///
/// Falling back to the built-in defaults is the right thing to do — a terminal
/// that refuses to open because of its config is a terminal you cannot fix the
/// config with — but doing it quietly is not: every setting in the file is
/// gone and nothing on screen says why.
static CONFIG_LOAD_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn record_load_error(message: String) {
    if let Ok(mut slot) = CONFIG_LOAD_ERROR.lock() {
        *slot = Some(message);
    }
}

fn clear_load_error() {
    if let Ok(mut slot) = CONFIG_LOAD_ERROR.lock() {
        *slot = None;
    }
}

/// The most recent reason the configuration file could not be read.
pub(crate) fn load_error() -> Option<String> {
    CONFIG_LOAD_ERROR.lock().ok().and_then(|slot| slot.clone())
}

fn load_file_config() -> (FileConfig, Option<crate::config_store::ConfigRevision>) {
    clear_load_error();
    let path = config_file_path();
    let display_path =
        || jterm_core::review_input::safe_inline_display(&path.to_string_lossy(), 2 * 1024);
    let bytes = match crate::config_store::read_config_bytes(&path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return (
                FileConfig::default(),
                Some(crate::config_store::ConfigRevision::missing()),
            );
        }
        Err(error) => {
            // A log line is invisible to someone whose theme, keybindings and
            // remote hosts have all silently reverted to the defaults. Keep
            // the reason where the window can put it on screen.
            record_load_error(format!("{}: {error}", display_path()));
            log::warn!("Failed to read config file {}: {error}", display_path());
            return (FileConfig::default(), None);
        }
    };
    let revision = crate::config_store::ConfigRevision::from_bytes(&bytes);
    let Ok(contents) = std::str::from_utf8(&bytes) else {
        let reason = format!(
            "{}: not valid UTF-8; run: forge --check-config",
            display_path()
        );
        record_load_error(reason);
        log::warn!("Config file {} is not valid UTF-8", display_path());
        return (FileConfig::default(), Some(revision));
    };
    let table = match contents.parse::<toml::Table>() {
        Ok(table) => table,
        Err(error) => {
            let diagnostic = config_syntax_diagnostic(contents, &error);
            record_load_error(format!(
                "{}: {diagnostic}; run: forge --check-config",
                display_path()
            ));
            log::warn!(
                "Failed to parse config file {}: {diagnostic}",
                display_path()
            );
            return (FileConfig::default(), Some(revision));
        }
    };
    let issues = validate_config_table(&table);
    for issue in &issues {
        match issue.level {
            ConfigIssueLevel::Warning => log::warn!("Config {issue}"),
            ConfigIssueLevel::Error => log::error!("Config {issue}"),
        }
    }
    let error_count = issues.iter().filter(|issue| issue.is_error()).count();
    if error_count > 0 {
        let first_path = issues
            .iter()
            .find(|issue| issue.is_error())
            .map(|issue| issue.path.as_str())
            .unwrap_or("unknown setting");
        record_load_error(format!(
            "{}: {error_count} configuration error{} (first at {}); run: forge --check-config",
            display_path(),
            if error_count == 1 { "" } else { "s" },
            jterm_core::review_input::safe_inline_display(first_path, 512)
        ));
        log::warn!(
            "Ignoring semantically invalid config file {} ({error_count} errors)",
            display_path()
        );
        return (FileConfig::default(), Some(revision));
    }

    let colors = table.get("colors").and_then(|v| v.as_table());
    // Remote targets are always opt-in. A missing section, an unreadable
    // configuration, and an invalid configuration must all resolve to an empty
    // list; silently inventing a network destination is not a safe fallback.
    let remote_hosts = parse_remote_hosts(&table);

    let file_config = FileConfig {
        opacity: table.get("opacity").and_then(|v| v.as_float()),
        scrollback: table_u32(&table, "scrollback"),
        font: table
            .get("font")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        icon_font: table
            .get("icon_font")
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
        jsh_update_check: table
            .get("jsh_update_check")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        sidebar_visible: table.get("sidebar_visible").and_then(|v| v.as_bool()),
        sidebar_width: table_u32(&table, "sidebar_width"),
        tab_width: table_u32(&table, "tab_width"),
        max_visible_blocks: table_u32(&table, "max_visible_blocks"),
        lazy_load_threshold: table_u32(&table, "lazy_load_threshold"),
        truncation_threshold_lines: table_u32(&table, "truncation_threshold_lines"),
        finished_block_viewport_rows: table_u32(&table, "finished_block_viewport_rows"),
        finished_block_max_expanded_rows: table_u32(&table, "finished_block_max_expanded_rows"),
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
        ascii_organism_enabled: table
            .get("ascii_organism_enabled")
            .and_then(|v| v.as_bool()),
        ascii_organism_motion: table
            .get("ascii_organism_motion")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        ai_enabled: table.get("ai_enabled").and_then(|v| v.as_bool()),
        agent_enabled: table.get("agent_enabled").and_then(|v| v.as_bool()),
        agent_tasks_enabled: table.get("agent_tasks_enabled").and_then(|v| v.as_bool()),
        ai_share_command_context: table
            .get("ai_share_command_context")
            .and_then(|v| v.as_bool()),
        agent_max_turns: table_u32(&table, "agent_max_turns"),
        agent_auto_approve_readonly: table
            .get("agent_auto_approve_readonly")
            .and_then(|v| v.as_bool()),
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
        ai_temperature: table
            .get("ai_temperature")
            .and_then(toml::Value::as_float)
            .map(|value| value as f32),
        ai_stream: table.get("ai_stream").and_then(|v| v.as_bool()),
        ai_redact_secrets: table.get("ai_redact_secrets").and_then(|v| v.as_bool()),
        allow_remote_clipboard_write: table
            .get("allow_remote_clipboard_write")
            .and_then(|v| v.as_bool()),
        notify_long_blocks: table.get("notify_long_blocks").and_then(|v| v.as_bool()),
        notify_long_block_threshold_ms: table_u64(&table, "notify_long_block_threshold_ms"),
        bottom_bar: table.get("bottom_bar").and_then(|v| v.as_bool()),
        click_moves_cursor: table.get("click_moves_cursor").and_then(|v| v.as_bool()),
    };
    (file_config, Some(revision))
}

/// Parse `[[remote_hosts]]` array-of-tables. Entries missing a `host` are skipped.
pub(crate) fn parse_remote_hosts(table: &toml::Table) -> Vec<RemoteHost> {
    let Some(arr) = table.get("remote_hosts").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .take(MAX_REMOTE_HOSTS)
        .filter_map(|v| v.as_table())
        .filter_map(|t| {
            let host = t.get("host").and_then(|v| v.as_str())?.to_string();
            if !remote_field_is_safe(&host)
                || host.starts_with('-')
                || host.chars().any(char::is_whitespace)
            {
                return None;
            }
            let name = t
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|name| remote_field_is_safe(name))
                .map(str::to_string)
                .unwrap_or_else(|| host.clone());
            let user = match t.get("user") {
                Some(value) => {
                    let user = value.as_str()?;
                    if !remote_field_is_safe(user)
                        || user.chars().any(char::is_whitespace)
                        || user.contains('@')
                    {
                        return None;
                    }
                    Some(user.to_string())
                }
                None => None,
            };
            let remote_shell = t
                .get("remote_shell")
                .and_then(|v| v.as_str())
                .unwrap_or("jsh")
                .to_string();
            if !remote_field_is_safe(&remote_shell) {
                return None;
            }
            let session = match t.get("session") {
                Some(value) => {
                    let session = value.as_str()?;
                    if !jterm_core::execution_journal::is_valid_jsh_session_id(session) {
                        return None;
                    }
                    Some(session.to_string())
                }
                None => None,
            };
            let ssh_args = match t.get("ssh_args") {
                Some(value) => {
                    let values = value.as_array()?;
                    if values.len() > MAX_REMOTE_SSH_ARGS {
                        return None;
                    }
                    let mut args = Vec::with_capacity(values.len());
                    for value in values {
                        let value = value.as_str()?;
                        if !remote_field_is_safe(value) {
                            return None;
                        }
                        args.push(value.to_string());
                    }
                    args
                }
                None => Vec::new(),
            };
            let login_shell = t
                .get("login_shell")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let multiplex = t.get("multiplex").and_then(|v| v.as_bool()).unwrap_or(true);
            let docker = t.get("docker").and_then(|v| v.as_bool()).unwrap_or(false);
            // Rejected rather than dropped, for the same reason a `deploy`
            // spelling this build does not understand rejects the host: a path
            // that is quietly ignored looks exactly like deployment working,
            // right up until the tab is a bash prompt with none of jsh in it.
            let deploy_artifact = match t.get("deploy_artifact") {
                None => None,
                Some(toml::Value::String(value)) => {
                    if !remote_field_is_safe(value) || !std::path::Path::new(value).is_absolute() {
                        return None;
                    }
                    Some(value.to_string())
                }
                Some(_) => return None,
            };
            // A spelling this build does not understand rejects the host rather
            // than falling back to `off`. Silently downgrading `incognito` would
            // write jsh's dot-files into an account the user asked to leave
            // untouched, which is the one outcome the mode exists to prevent.
            let deploy = match t.get("deploy") {
                None => jterm_core::jsh_remote::Deploy::Off,
                Some(toml::Value::String(value)) => {
                    match jterm_core::jsh_remote::Deploy::parse(value) {
                        Some(deploy) => deploy,
                        None => return None,
                    }
                }
                Some(_) => return None,
            };
            let host = RemoteHost {
                name,
                host,
                user,
                remote_shell,
                session,
                ssh_args,
                login_shell,
                multiplex,
                deploy,
                docker,
                deploy_artifact,
            };
            validate_remote_host(&host).ok()?;
            Some(host)
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
    if h.docker {
        // Same rule as `deploy`: written only when on, so an ssh host does not
        // grow the key on a round trip.
        t.insert("docker".into(), toml::Value::Boolean(true));
    }
    if let Some(artifact) = &h.deploy_artifact {
        t.insert(
            "deploy_artifact".into(),
            toml::Value::String(artifact.clone()),
        );
    }
    if h.deploy.is_enabled() {
        // Only written when it is on, so a config file that never asked for
        // deployment does not grow a key after a round trip.
        t.insert(
            "deploy".into(),
            toml::Value::String(h.deploy.as_str().to_string()),
        );
    }
    toml::Value::Table(t)
}

// ---------------------------------------------------------------------------
// load_config
// ---------------------------------------------------------------------------

pub(crate) fn load_config() -> (Config, Vec<Theme>, KeybindingMap) {
    let (fc, persistence_revision) = load_file_config();
    let themes = builtin_themes();

    // Resolve active theme
    let theme_name = resolve_setting_text(env_string("FORGE_THEME"), fc.theme, 256)
        .unwrap_or_else(|| "default".to_string());
    let theme = themes
        .iter()
        .find(|t| t.name == theme_name)
        .unwrap_or(&themes[0]);

    // Priority: env var > config file > theme default
    let window_opacity = finite_clamp_f64(env_f64("FORGE_OPACITY").or(fc.opacity), 0.95, 0.01, 1.0);
    let terminal_scrollback_lines = env_u32("FORGE_SCROLLBACK")
        .or(fc.scrollback)
        .unwrap_or(5000)
        .min(1_000_000);
    let default_font_scale = finite_clamp_f64(
        env_f64("FORGE_FONT_SCALE").or(fc.font_scale),
        1.0,
        0.1,
        10.0,
    );
    let font_desc = resolve_setting_text(env_string("FORGE_FONT"), fc.font, MAX_FONT_DESC_BYTES)
        .unwrap_or_else(|| DEFAULT_FONT_DESC.to_string());
    // Left unset so `crate::font` can pick whichever icon font is installed.
    let icon_font = resolve_setting_text(
        env_string("FORGE_ICON_FONT"),
        fc.icon_font,
        MAX_FONT_DESC_BYTES,
    );

    let foreground = env_rgba("FORGE_FG")
        .or_else(|| fc.foreground.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(theme.foreground);
    let background = env_rgba("FORGE_BG")
        .or_else(|| fc.background.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(theme.background);
    let cursor = env_rgba("FORGE_CURSOR")
        .or_else(|| fc.cursor.as_deref().and_then(|v| RGBA::parse(v).ok()))
        .unwrap_or(theme.cursor);
    let cursor_foreground = env_rgba("FORGE_CURSOR_FG")
        .or_else(|| {
            fc.cursor_foreground
                .as_deref()
                .and_then(|v| RGBA::parse(v).ok())
        })
        .unwrap_or(theme.cursor_foreground);

    // Block view optimization settings
    let max_visible_blocks = env_u32("FORGE_MAX_BLOCKS")
        .or(fc.max_visible_blocks)
        .unwrap_or(200)
        .clamp(1, 100_000);
    let lazy_load_threshold = env_u32("FORGE_LAZY_LINES")
        .or(fc.lazy_load_threshold)
        .unwrap_or(1000)
        .clamp(1, 10_000_000);
    let truncation_threshold_lines = env_u32("FORGE_TRUNCATION_LINES")
        .or(fc.truncation_threshold_lines)
        .unwrap_or(50000)
        .clamp(1, 10_000_000);
    let finished_block_viewport_rows = env_u32("FORGE_FINISHED_VIEWPORT_ROWS")
        .or(fc.finished_block_viewport_rows)
        .unwrap_or(24)
        .clamp(3, 5_000);
    let finished_block_max_expanded_rows = env_u32("FORGE_FINISHED_MAX_EXPANDED_ROWS")
        .or(fc.finished_block_max_expanded_rows)
        .unwrap_or(5_000)
        .clamp(finished_block_viewport_rows, 5_000);
    let max_collapsed_output_lines = env_u32("FORGE_MAX_COLLAPSED_LINES")
        .or(fc.max_collapsed_output_lines)
        .unwrap_or(25)
        .clamp(1, 1_000_000);
    let virtual_scroll_margin = env_u32("FORGE_VSCROLL_MARGIN")
        .or(fc.virtual_scroll_margin)
        .unwrap_or(1)
        .min(10_000);
    let command_history_enabled = fc.command_history_enabled.unwrap_or(true);
    let command_history_path = if command_history_enabled {
        let requested = env_string("FORGE_COMMAND_HISTORY_PATH")
            .or(fc.command_history_path)
            .unwrap_or_else(default_command_history_path);
        normalize_history_path(Some(requested), "command_history_path")
    } else {
        None
    };
    let command_history_max_entries = fc
        .command_history_max_entries
        .unwrap_or(10_000)
        .clamp(100, 1_000_000);
    let block_history_path = normalize_history_path(
        env_string("FORGE_HISTORY_PATH").or(fc.block_history_path),
        "block_history_path",
    );
    let block_history_compress = fc.block_history_compress.unwrap_or(true);
    let block_compact = match std::env::var("FORGE_BLOCK_COMPACT").ok().as_deref() {
        Some("1") | Some("true") => Some(true),
        Some("0") | Some("false") => Some(false),
        _ => None,
    }
    .or(fc.block_compact)
    .unwrap_or(false);
    let shell = resolve_setting_text(env_string("FORGE_SHELL"), fc.shell, MAX_CONFIG_PATH_BYTES);
    let startup_commands = fc
        .startup_commands
        .filter(|commands| setting_text_is_safe(commands, MAX_STARTUP_COMMANDS_BYTES));

    // Block-first like anvil; VTE remains available for compatibility and
    // safe mode.
    let terminal_mode_str = resolve_setting_text(env_string("FORGE_MODE"), fc.terminal_mode, 64)
        .unwrap_or_else(|| "block".to_string());
    let terminal_mode = TerminalMode::parse(&terminal_mode_str).unwrap_or_else(|| {
        log::warn!("Unknown terminal_mode '{terminal_mode_str}', using block");
        TerminalMode::Block
    });

    let tab_placement = TabPlacement::parse(
        &resolve_setting_text(env_string("FORGE_TAB_PLACEMENT"), fc.tab_placement, 64)
            .unwrap_or_else(|| "sidebar".to_string()),
    );
    let sidebar_visible = resolve_sidebar_visibility(fc.sidebar_visible, tab_placement);

    let ai_enabled = env_bool("FORGE_AI_ENABLED")
        .or(fc.ai_enabled)
        .unwrap_or(true);
    let agent_enabled = env_bool("FORGE_AGENT_ENABLED")
        .or(fc.agent_enabled)
        .unwrap_or(true);
    // Fail-closed by design: the contained-agent surface and the sharing
    // of terminal evidence with a provider both stay off until the user
    // opts in (ember's `experimental_task_sidebar` /
    // `ai_share_command_context` defaults).
    let agent_tasks_enabled = env_bool("FORGE_AGENT_TASKS_ENABLED")
        .or(fc.agent_tasks_enabled)
        .unwrap_or(false);
    let ai_share_command_context = env_bool("FORGE_AI_SHARE_COMMAND_CONTEXT")
        .or(fc.ai_share_command_context)
        .unwrap_or(false);
    let agent_max_turns = env_u32("FORGE_AGENT_MAX_TURNS")
        .or(fc.agent_max_turns)
        .unwrap_or(20)
        .clamp(1, 100);
    let requested_agent_auto_approve = env_bool("FORGE_AGENT_AUTO_APPROVE_READONLY")
        .or(fc.agent_auto_approve_readonly)
        .unwrap_or(false);
    if requested_agent_auto_approve {
        log::warn!(
            "agent_auto_approve_readonly is retired; every Agent proposal requires explicit approval"
        );
    }
    let agent_auto_approve_readonly = false;
    let command_correction_enabled = env_bool("FORGE_COMMAND_CORRECTION_ENABLED")
        .or(fc.command_correction_enabled)
        .unwrap_or(true);
    let ascii_organism_enabled = env_bool("FORGE_ASCII_ORGANISM_ENABLED")
        .or(fc.ascii_organism_enabled)
        .unwrap_or(false);
    let ascii_organism_motion = resolve_setting_text(
        env_string("FORGE_ASCII_ORGANISM_MOTION"),
        fc.ascii_organism_motion,
        64,
    )
    .as_deref()
    .and_then(OrganismMotion::parse);
    let requested_provider =
        resolve_setting_text(env_string("FORGE_AI_PROVIDER"), fc.ai_provider, 64)
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
    let ai_model = resolve_setting_text(
        env_string("FORGE_AI_MODEL"),
        fc.ai_model,
        MAX_AI_IDENTIFIER_BYTES,
    )
    .unwrap_or_else(|| default_ai_model.to_string());
    let ai_base_url = resolve_ai_base_url(
        env_string("FORGE_AI_BASE_URL").or(fc.ai_base_url),
        default_ai_base_url,
    );
    let ai_api_key_file_configured = fc
        .ai_api_key_file
        .filter(|path| configured_path_is_safe(path, true));
    let ai_api_key_file =
        ai_api_key_file_env_override().or_else(|| ai_api_key_file_configured.clone());

    let config = Config {
        window_opacity,
        terminal_scrollback_lines,
        font_desc,
        icon_font,
        default_font_scale,
        theme_name: theme.name.clone(),
        foreground,
        background,
        cursor,
        cursor_foreground,
        palette: theme.palette,
        shell,
        startup_commands,
        terminal_mode,
        tab_placement,
        sidebar_view: SidebarView::parse(
            &fc.sidebar_view
                .filter(|value| setting_text_is_safe(value, 64))
                .unwrap_or_else(|| "tabs".to_string()),
        ),
        jsh_update_check: JshUpdateCheck::parse(
            &fc.jsh_update_check
                .filter(|value| setting_text_is_safe(value, 64))
                .unwrap_or_else(|| "daily".to_string()),
        ),
        sidebar_visible,
        sidebar_width: fc.sidebar_width.unwrap_or(220).clamp(120, 800),
        tab_width: fc
            .tab_width
            .unwrap_or(DEFAULT_TAB_WIDTH)
            .clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH),
        max_visible_blocks,
        lazy_load_threshold,
        truncation_threshold_lines,
        finished_block_viewport_rows,
        finished_block_max_expanded_rows,
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
        ascii_organism_enabled,
        ascii_organism_motion,
        ai_enabled,
        agent_enabled,
        agent_tasks_enabled,
        ai_share_command_context,
        agent_max_turns,
        agent_auto_approve_readonly,
        command_correction_enabled,
        ai_provider,
        ai_base_url,
        ai_api_key_file,
        ai_api_key_file_configured,
        ai_panel_visible: fc.ai_panel_visible.unwrap_or(false),
        ai_panel_width: fc.ai_panel_width.unwrap_or(360).clamp(240, 1200),
        ai_model,
        ai_temperature: env_f32("FORGE_AI_TEMPERATURE")
            .or(fc.ai_temperature)
            .filter(|t| t.is_finite() && (0.0..=2.0).contains(t)),
        ai_max_tokens: env_u32("FORGE_AI_MAX_TOKENS")
            .or(fc.ai_max_tokens)
            .unwrap_or(1024)
            .clamp(64, 32_768),
        ai_stream: env_bool("FORGE_AI_STREAM").or(fc.ai_stream).unwrap_or(true),
        ai_redact_secrets: env_bool("FORGE_AI_REDACT_SECRETS")
            .or(fc.ai_redact_secrets)
            .unwrap_or(true),
        allow_remote_clipboard_write: fc.allow_remote_clipboard_write.unwrap_or(false),
        notify_long_blocks: fc.notify_long_blocks.unwrap_or(true),
        notify_long_block_threshold_ms: fc.notify_long_block_threshold_ms.unwrap_or(10_000),
        bottom_bar: fc.bottom_bar.unwrap_or(true),
        click_moves_cursor: fc
            .click_moves_cursor
            .unwrap_or(jterm_core::click_cursor::ENABLED_BY_DEFAULT),
        persistence_revision: std::sync::Arc::new(std::sync::Mutex::new(persistence_revision)),
    };

    let keybinding_map =
        fc.keybindings
            .as_ref()
            .map_or_else(KeybindingMap::from_defaults, |table| {
                match KeybindingMap::from_user_overrides(table) {
                    Ok(map) => map,
                    Err(issues) => {
                        // `load_file_config` rejects these tables before constructing
                        // `FileConfig`; reaching this branch is an internal invariant
                        // violation. Preserve one coherent default map rather than ever
                        // partially applying the invalid table.
                        for issue in issues {
                            log::error!(
                                "Validated keybinding table became invalid at keybindings.{}: {}",
                                issue.key,
                                issue.message
                            );
                        }
                        KeybindingMap::from_defaults()
                    }
                }
            });

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
    if safe_mode_persistence_disabled(std::env::var_os("FORGE_SAFE_MODE").as_deref()) {
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

fn choose_flatpak_host_shell_argv(configured_shell: Option<&str>) -> Vec<String> {
    if let Some(shell) = configured_shell.filter(|value| !value.trim().is_empty()) {
        let shell_name = Path::new(shell)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if shell_name == "jsh" && crate::host::command_available("bash") {
            return vec![
                "bash".to_string(),
                "-ic".to_string(),
                crate::process::build_jsh_exec_command(shell, None),
            ];
        }
        return vec![shell.to_string()];
    }

    if crate::host::command_available("jsh") {
        if crate::host::command_available("bash") {
            return vec![
                "bash".to_string(),
                "-ic".to_string(),
                "exec jsh".to_string(),
            ];
        }
        return vec!["jsh".to_string()];
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
    if let Some(token) = configured_shell {
        // A bare name is a PATH lookup and never an implicit `./name`: a pane
        // opens in whatever directory the user is browsing, so a checkout
        // containing an executable called `bash` must not hijack
        // `shell = "bash"`. The resolved absolute path is what we spawn, which
        // also survives a child that starts in another directory.
        match crate::host::resolve_configured_program(token, std::env::var_os("PATH").as_deref()) {
            Some(resolved) => {
                let path = resolved.to_string_lossy().to_string();
                if resolved.file_name().and_then(|name| name.to_str()) == Some("jsh") {
                    if let Some(argv) = wrap_jsh_argv_in_interactive_bash(&path) {
                        return argv;
                    }
                }
                return vec![path];
            }
            None => log::warn!(
                "Configured shell '{token}' is not an executable file, falling back to auto-detection"
            ),
        }
    }

    // Prefer jsh when it's on PATH.
    if let Some(jsh_path) = crate::host::find_executable_in_path("jsh") {
        if let Some(argv) = wrap_jsh_argv_in_interactive_bash(&jsh_path.to_string_lossy()) {
            return argv;
        }
        return vec![jsh_path.to_string_lossy().to_string()];
    }

    // Fallback: bash. This one *is* the pane's shell rather than a wrapper
    // that execs itself away, so resolving it off the inherited PATH would
    // leave the user in a bash their rc was not written for.
    if let Some(bash_path) = crate::host::interactive_bash_path() {
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
            user: Some("alice".into()),
            remote_shell: "/home/tester/.cargo/bin/jsh".into(),
            session: Some("cloud-test".into()),
            ssh_args: Vec::new(),
            login_shell: true,
            // Off by default in tests so exact-argv assertions stay deterministic
            // (multiplex injects an env-dependent ControlPath).
            multiplex: false,
            // Likewise: deployment publishes a script and its path depends on
            // the cache directory. The deploy tests below opt in explicitly.
            deploy: jterm_core::jsh_remote::Deploy::Off,
            docker: false,
            deploy_artifact: None,
        }
    }

    /// Updated in round 8: shell selection goes through the shared
    /// `host::resolve_configured_program`, so a configured path that is not an
    /// executable file falls back to auto-detection instead of being spawned
    /// into an exec failure — and the chosen argv is the resolved program, not
    /// the raw token.
    #[test]
    fn a_missing_configured_shell_falls_back_to_auto_detection() {
        if crate::host::is_flatpak() {
            return;
        }
        let missing = "/definitely/missing/forge-shell";
        let argv = choose_shell_argv(Some(missing));
        assert!(!argv.is_empty());
        assert_ne!(argv.first().map(String::as_str), Some(missing));
    }

    #[test]
    fn deploy_routes_through_the_remote_launcher_and_keeps_ssh_arguments() {
        let mut h = host();
        h.deploy = jterm_core::jsh_remote::Deploy::Incognito;
        h.ssh_args = vec!["-p".into(), "2222".into()];
        // A fixed path, not the published one: publishing writes into the real
        // cache directory, and on a machine where that fails this test would
        // silently assert the plain-ssh fallback instead.
        let argv = build_deployed_argv(&h, std::path::Path::new("/c/jsh-remote.sh"));

        assert_eq!(argv[0], "/bin/sh");
        assert_eq!(argv[1], "/c/jsh-remote.sh");
        assert!(argv.contains(&"--incognito".to_string()), "{argv:?}");
        let expected_target = format!("{}@{}", h.user.as_deref().unwrap_or_default(), h.host);
        assert!(argv.contains(&expected_target), "{argv:?}");
        // The remote shell is chosen by the launcher, not by remote_shell: the
        // whole point is that the destination has no jsh to name.
        assert!(
            !argv.iter().any(|a| a.contains("/.local/bin/jsh")),
            "{argv:?}"
        );
        let separator = argv.iter().position(|a| a == "--").expect("ssh separator");
        assert_eq!(&argv[separator + 1..], ["-p", "2222"]);
    }

    #[test]
    fn deploy_off_is_byte_for_byte_the_old_ssh_command() {
        let h = host();
        let control_dir = h.multiplex.then(control_socket_dir).flatten();
        assert_eq!(
            build_remote_argv(&h),
            build_remote_argv_with_control_dir(&h, control_dir.as_deref())
        );
    }

    #[test]
    fn a_deploy_mode_this_build_cannot_parse_rejects_the_host() {
        // Not "falls back to off": a typo in `incognito` must never resolve to a
        // mode that writes into a shared account's home directory.
        let bad: toml::Table = toml::from_str(
            "[[remote_hosts]]\nname = \"h\"\nhost = \"example.test\"\ndeploy = \"incognito!\"\n",
        )
        .expect("toml");
        assert!(parse_remote_hosts(&bad).is_empty());

        let ok: toml::Table = toml::from_str(
            "[[remote_hosts]]\nname = \"h\"\nhost = \"example.test\"\ndeploy = \"incognito\"\n",
        )
        .expect("toml");
        let hosts = parse_remote_hosts(&ok);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].deploy, jterm_core::jsh_remote::Deploy::Incognito);
    }

    #[test]
    fn deploy_survives_a_config_round_trip_and_is_absent_when_off() {
        let mut h = host();
        h.deploy = jterm_core::jsh_remote::Deploy::Persist;
        let value = remote_host_to_toml(&h);
        assert_eq!(
            value.get("deploy").and_then(toml::Value::as_str),
            Some("persist")
        );

        h.deploy = jterm_core::jsh_remote::Deploy::Off;
        assert!(remote_host_to_toml(&h).get("deploy").is_none());
    }

    #[test]
    fn login_shell_wraps_in_bash_lc() {
        let argv = build_remote_argv(&host());
        assert_eq!(
            argv,
            vec![
                "ssh",
                "-t",
                "--",
                "alice@1.2.3.4",
                "bash -lc 'exec /home/tester/.cargo/bin/jsh --session cloud-test'",
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
            "/home/tester/.cargo/bin/jsh --session cloud-test"
        );
    }

    #[test]
    fn invalid_session_ids_are_never_interpolated_into_remote_shell_code() {
        let mut h = host();
        h.session = Some("it's".into());
        let argv = build_remote_argv(&h);
        assert_eq!(
            argv.last().unwrap(),
            "bash -lc 'exec /home/tester/.cargo/bin/jsh'"
        );
    }

    #[test]
    fn local_jsh_is_wrapped_in_interactive_bash() {
        let argv = wrap_jsh_argv_in_interactive_bash("/home/tester/.cargo/bin/jsh")
            .expect("bash should be available on the test runner");
        // The wrapper sources the user's rc, so it must be the system's
        // interactive bash whenever one exists — never whichever bash the
        // inherited PATH names first.
        if crate::host::is_executable_file(Path::new("/usr/bin/bash")) {
            assert_eq!(argv[0], "/usr/bin/bash");
        }
        assert_eq!(argv[1], "-ic");
        // The shell travels as `$0` and appended operands as `$@`, so a
        // `--session <id>` the spawn path adds reaches the shell.
        assert_eq!(argv[2], "exec \"$0\" \"$@\"");
        assert_eq!(argv[3], "/home/tester/.cargo/bin/jsh");
    }

    /// A session id appended by the spawn path must land on the shell's own
    /// argv, not be eaten as the wrapper's `$0`/`$1`.
    #[test]
    fn a_session_id_appended_after_the_wrapper_reaches_the_shell() {
        let mut argv = wrap_jsh_argv_in_interactive_bash("/home/tester/.cargo/bin/jsh")
            .expect("bash should be available on the test runner");
        argv.push("--session".to_string());
        argv.push("home-main".to_string());
        assert_eq!(
            &argv[2..],
            &[
                "exec \"$0\" \"$@\"".to_string(),
                "/home/tester/.cargo/bin/jsh".to_string(),
                "--session".to_string(),
                "home-main".to_string(),
            ]
        );
    }

    #[test]
    fn multiplex_injects_controlmaster_flags() {
        let mut h = host();
        h.multiplex = true;
        let argv = build_remote_argv_with_control_dir(&h, Some(Path::new("/run/user/1000/forge")));
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
        let target_idx = argv.iter().position(|a| a == "alice@1.2.3.4").unwrap();
        let cm_idx = argv.iter().position(|a| a == "ControlMaster=auto").unwrap();
        assert!(cm_idx < target_idx);
    }

    #[cfg(unix)]
    #[test]
    fn control_socket_namespace_is_private_and_never_follows_a_link() {
        use std::os::unix::fs::{symlink, DirBuilderExt, PermissionsExt};

        let root = std::env::temp_dir().join(format!(
            "forge-control-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let parent = open_owned_directory(&root).unwrap();
        let (child_path, child) =
            ensure_owned_child_directory(&parent, &root, "forge", true).unwrap();
        assert_eq!(
            child.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(child);
        fs::remove_dir(&child_path).unwrap();
        symlink(&root, &child_path).unwrap();
        assert!(ensure_owned_child_directory(&parent, &root, "forge", true).is_err());
        fs::remove_file(&child_path).unwrap();
        drop(parent);
        fs::remove_dir(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn control_socket_namespace_rejects_nonsticky_writable_ancestors() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        let root = std::env::temp_dir().join(format!(
            "forge-control-parent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let shared = root.join("shared");
        fs::DirBuilder::new().mode(0o700).create(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        let runtime = shared.join("runtime");
        fs::DirBuilder::new().mode(0o700).create(&runtime).unwrap();
        assert!(open_trusted_owned_directory(&runtime).is_err());
        fs::remove_dir(&runtime).unwrap();
        fs::remove_dir(&shared).unwrap();
        fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn control_socket_path_rejects_openssh_expansion_and_hidden_text() {
        assert!(control_socket_path_is_safe(Path::new(
            "/run/user/1000/forge"
        )));
        assert!(!control_socket_path_is_safe(Path::new("/tmp/%h/forge")));
        assert!(!control_socket_path_is_safe(Path::new(
            "/tmp/safe\u{202e}fake"
        )));
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

    /// One vocabulary for the backend across the config file, `FORGE_MODE`,
    /// `--mode`, the settings dialog and the saved config: every one of them
    /// goes through this pair, so a mode that parses must also serialize back
    /// to the same word.
    #[test]
    fn terminal_mode_parses_and_round_trips_every_backend() {
        for (text, mode) in [
            ("block", TerminalMode::Block),
            ("vte", TerminalMode::Vte),
            ("unified", TerminalMode::Unified),
        ] {
            assert_eq!(TerminalMode::parse(text).as_ref(), Some(&mode));
            assert_eq!(
                TerminalMode::parse(&text.to_uppercase()).as_ref(),
                Some(&mode)
            );
            assert_eq!(mode.as_str(), text);
            assert_eq!(TerminalMode::parse(mode.as_str()).as_ref(), Some(&mode));
        }
        assert_eq!(TerminalMode::parse("warp"), None);
        // Unified is rendered by TermView, so it takes the Block leaf/pane
        // path everywhere a mode is dispatched on.
        assert!(TerminalMode::Unified.uses_term_view());
        assert!(TerminalMode::Block.uses_term_view());
        assert!(!TerminalMode::Vte.uses_term_view());
    }

    #[test]
    fn config_validator_accepts_unified_terminal_mode() {
        let issues = validate_config_contents("terminal_mode = 'unified'\n").unwrap();
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
    }

    #[test]
    fn config_validator_accepts_sibling_keybinding_aliases() {
        let input = r#"
[keybindings]
open_ai_panel = "F5"
open_history_palette = "F6"
open_workflows = "F7"
open_palette = "F8"
"#;
        let issues = validate_config_contents(input).unwrap();
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
    }

    #[test]
    fn config_validator_accepts_indexed_remote_keybindings() {
        let issues = validate_config_contents(
            "[keybindings]\nconnect_remote_1 = 'F4'\nconnect_remote_9 = 'F11'\n",
        )
        .unwrap();
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
    }

    #[test]
    fn config_validator_rejects_collision_with_unchanged_default() {
        let issues =
            validate_config_contents("[keybindings]\nfont_increase = 'Ctrl+Shift+T'\n").unwrap();

        assert!(issues.iter().any(|issue| {
            issue.level == ConfigIssueLevel::Error
                && issue.path == "keybindings.font_increase"
                && issue.message.contains("keybindings.new_tab")
                && issue.message.contains("rebind or disable")
        }));
    }

    #[test]
    fn config_validator_accepts_nonconflicting_override_and_atomic_swap() {
        let nonconflicting =
            validate_config_contents("[keybindings]\nfont_increase = 'F10'\n").unwrap();
        assert!(
            nonconflicting.is_empty(),
            "unexpected issues: {nonconflicting:?}"
        );

        let swap = validate_config_contents(
            "[keybindings]\nnew_tab = 'Ctrl+Shift+W'\nclose_pane_or_tab = 'Ctrl+Shift+T'\n",
        )
        .unwrap();
        assert!(swap.is_empty(), "unexpected issues: {swap:?}");

        let reassignment = validate_config_contents(
            "[keybindings]\nnew_tab = false\nfont_increase = 'Ctrl+Shift+T'\n",
        )
        .unwrap();
        assert!(
            reassignment.is_empty(),
            "unexpected issues: {reassignment:?}"
        );
    }

    #[test]
    fn tab_and_finished_block_height_limits_are_validated() {
        let issues = validate_config_contents(
            "tab_width = 79\nfinished_block_viewport_rows = 40\nfinished_block_max_expanded_rows = 30\n",
        )
        .unwrap();
        assert!(issues.iter().any(|issue| issue.path == "tab_width"));
        assert!(issues.iter().any(|issue| {
            issue.path == "finished_block_max_expanded_rows"
                && issue.message.contains("finished_block_viewport_rows")
        }));

        let (config, _, _) = load_safe_config();
        assert_eq!(config.tab_width, DEFAULT_TAB_WIDTH);
        assert_eq!(config.finished_block_max_expanded_rows, 5_000);
    }

    #[test]
    fn ai_panel_settings_have_stable_defaults_and_width_limits() {
        for width in [239, 1201] {
            let issues = validate_config_contents(&format!("ai_panel_width = {width}\n")).unwrap();
            assert!(issues.iter().any(|issue| {
                issue.path == "ai_panel_width" && issue.level == ConfigIssueLevel::Warning
            }));
        }
        for width in [240, 1200] {
            let issues = validate_config_contents(&format!("ai_panel_width = {width}\n")).unwrap();
            assert!(
                issues.iter().all(|issue| issue.path != "ai_panel_width"),
                "unexpected issues for {width}: {issues:?}"
            );
        }

        let (config, _, _) = load_safe_config();
        assert!(!config.ai_panel_visible);
        assert_eq!(config.ai_panel_width, 360);
    }

    #[test]
    fn ascii_organism_motion_accepts_only_the_three_levels() {
        assert_eq!(OrganismMotion::parse("full"), Some(OrganismMotion::Full));
        assert_eq!(OrganismMotion::parse("Calm"), Some(OrganismMotion::Calm));
        assert_eq!(
            OrganismMotion::parse("STATIC"),
            Some(OrganismMotion::Static)
        );
        assert_eq!(OrganismMotion::parse("off"), None);
        assert_eq!(OrganismMotion::Full.as_str(), "full");
        assert_eq!(OrganismMotion::Calm.as_str(), "calm");
        assert_eq!(OrganismMotion::Static.as_str(), "static");

        let valid = validate_config_contents("ascii_organism_motion = 'calm'\n").unwrap();
        assert!(valid
            .iter()
            .all(|issue| issue.path != "ascii_organism_motion"));
        let invalid = validate_config_contents("ascii_organism_motion = 'sleepy'\n").unwrap();
        assert!(invalid.iter().any(|issue| {
            issue.path == "ascii_organism_motion" && issue.level == ConfigIssueLevel::Error
        }));
    }

    #[test]
    fn ascii_organism_is_a_boolean_opt_in_config_key() {
        let valid = validate_config_contents("ascii_organism_enabled = true\n").unwrap();
        assert!(valid.is_empty(), "unexpected issues: {valid:?}");

        let invalid = validate_config_contents("ascii_organism_enabled = 'yes'\n").unwrap();
        assert!(invalid.iter().any(|issue| {
            issue.path == "ascii_organism_enabled" && issue.level == ConfigIssueLevel::Error
        }));
    }

    #[test]
    fn executable_and_network_settings_reject_hidden_or_unbounded_text() {
        let oversized_model = "x".repeat(MAX_AI_IDENTIFIER_BYTES + 1);
        let input = format!(
            "startup_commands = \"echo safe\\u202efake\"\nshell = \"/bin/sh\\n--bad\"\nai_model = \"{oversized_model}\"\nai_base_url = \"https://example.com/\\uFE0F\"\nai_api_key_file = \"relative.key\"\n"
        );
        let issues = validate_config_contents(&input).unwrap();
        for path in [
            "startup_commands",
            "shell",
            "ai_model",
            "ai_base_url",
            "ai_api_key_file",
        ] {
            assert!(
                issues
                    .iter()
                    .any(|issue| issue.path == path && issue.is_error()),
                "missing error for {path}: {issues:?}"
            );
        }
    }

    #[test]
    fn ai_base_url_matches_the_provider_transport_contract() {
        assert!(ai_base_url_is_safe(
            "openai-compatible",
            "https://api.example.com/v1"
        ));
        assert!(ai_base_url_is_safe(
            "openai-compatible",
            "https://localhost:8000/v1"
        ));
        for provider in ["anthropic", "openai-compatible", "ollama"] {
            for endpoint in [
                "http://localhost:8000/v1",
                "http://127.0.0.1:8000/v1",
                "http://[::1]:8000/v1",
            ] {
                assert!(
                    ai_base_url_is_safe(provider, endpoint),
                    "{provider} should accept loopback endpoint {endpoint}"
                );
            }
            assert!(
                !ai_base_url_is_safe(provider, "http://models.example.com:8000/v1"),
                "{provider} must reject remote clear-text transport"
            );
        }
        for value in [
            "https://user:secret@example.com/v1",
            "https://example.com/v1?key=secret",
            "https://example.com/v1#fragment",
            "https://example.com\\@attacker.invalid/v1",
            "https:///missing-authority",
        ] {
            assert!(
                !ai_base_url_is_safe("openai-compatible", value),
                "accepted {value:?}"
            );
        }
        assert!(!ai_base_url_is_safe(
            "openai-compatible",
            &format!(
                "https://example.com/{}",
                "x".repeat(jagent::provider::MAX_BASE_URL_BYTES)
            )
        ));
    }

    #[test]
    fn explicit_invalid_ai_endpoint_never_drifts_to_a_public_default() {
        let remote_cleartext = "http://models.example.com:8000/v1";
        let resolved = resolve_ai_base_url(
            Some(remote_cleartext.to_string()),
            "https://api.openai.com/v1",
        );
        assert_eq!(
            resolved, remote_cleartext,
            "the runtime validator must reject the requested destination itself"
        );
        let mut config = Config::safe_defaults();
        config.ai_enabled = true;
        config.ai_provider = "openai-compatible".to_string();
        config.ai_base_url = resolved.clone();
        assert!(
            matches!(
                crate::ai::client_from_config(&config),
                Err(crate::ai::AiError::InvalidConfiguration(_))
            ),
            "the client boundary must reject the unsafe endpoint before any network I/O"
        );
        assert_eq!(
            resolve_ai_base_url(
                Some("https://user:secret@example.com/v1".to_string()),
                "https://api.openai.com/v1"
            ),
            "",
            "credential-bearing text must fail closed without being retained"
        );
        assert_eq!(
            resolve_ai_base_url(None, "https://api.openai.com/v1"),
            "https://api.openai.com/v1",
            "only an absent endpoint may select the provider default"
        );
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
    fn syntax_diagnostic_reports_location_without_source_excerpt() {
        let contents = "theme = 'private-value'\nopacity = [\n";
        let error = contents.parse::<toml::Table>().unwrap_err();
        let diagnostic = config_syntax_diagnostic(contents, &error);

        assert_eq!(diagnostic.line, Some(2));
        assert!(diagnostic.column.is_some());
        assert!(!diagnostic.message.contains("private-value"));
        assert!(!diagnostic.message.contains('\n'));
        assert!(diagnostic.message.len() <= 512);
    }

    #[test]
    fn semantic_ui_colors_meet_text_contrast_in_every_builtin_theme() {
        for theme in builtin_themes() {
            let chrome_foreground =
                accessible_text_color(&theme.foreground, &theme.background, &theme.foreground);
            assert!(
                contrast_ratio(&chrome_foreground, &theme.background) + f64::EPSILON
                    >= UI_TEXT_MIN_CONTRAST,
                "{} chrome foreground is not readable",
                theme.name
            );
            let muted = accessible_muted_text_color(&theme.foreground, &theme.background);
            assert!(
                contrast_ratio(&muted, &theme.background) + f64::EPSILON >= UI_TEXT_MIN_CONTRAST,
                "{} muted foreground is not readable",
                theme.name
            );
            for (name, index) in [
                ("error", 1usize),
                ("success", 2),
                ("warning", 3),
                ("info", 4),
                ("accent", 6),
            ] {
                let adjusted = accessible_text_color(
                    &theme.palette[index],
                    &theme.background,
                    &theme.foreground,
                );
                let ratio = contrast_ratio(&adjusted, &theme.background);
                assert!(
                    ratio + f64::EPSILON >= UI_TEXT_MIN_CONTRAST,
                    "{} {name} contrast is only {ratio:.3}",
                    theme.name
                );
            }
        }

        let light = builtin_themes()
            .into_iter()
            .find(|theme| theme.name == "light")
            .unwrap();
        assert!(
            contrast_ratio(&light.palette[2], &light.background) < UI_TEXT_MIN_CONTRAST,
            "the regression fixture should exercise an actual adjustment"
        );
    }

    #[test]
    fn remote_hosts_are_strictly_opt_in() {
        assert!(FileConfig::default().remote_hosts.is_empty());
        assert!(parse_remote_hosts(&toml::Table::new()).is_empty());

        let unrelated = "opacity = 0.9\ntheme = 'default'"
            .parse::<toml::Table>()
            .unwrap();
        assert!(
            parse_remote_hosts(&unrelated).is_empty(),
            "an unrelated or partial config must not invent a network target"
        );
    }

    #[test]
    fn remote_host_config_accepts_complete_and_minimal_entries() {
        let issues = validate_config_contents(
            r#"
[[remote_hosts]]
name = "开发机"
host = "dev.example.com"
user = "alice"
remote_shell = "/opt/tools/jsh --resume"
session = "dev-main"
ssh_args = ["-p", "2222", "-o", "ProxyCommand=ssh bastion -W %h:%p"]
login_shell = false
multiplex = true

[[remote_hosts]]
host = "backup.example.com"
"#,
        )
        .unwrap();
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn a_container_tab_runs_docker_exec_rather_than_ssh() {
        let mut h = host();
        h.host = "devbox".into();
        h.user = Some("devuser".into());
        h.docker = true;
        // Inert for a container, and set here to prove they stay inert.
        h.ssh_args = vec!["-p".into(), "2222".into()];
        h.login_shell = true;

        let argv = build_remote_argv(&h);
        assert_eq!(
            argv,
            [
                "docker",
                "exec",
                "-it",
                "-u",
                "devuser",
                "devbox",
                "/home/tester/.cargo/bin/jsh",
                "--session",
                "cloud-test",
            ]
        );
        assert!(!argv.iter().any(|a| a == "ssh"), "{argv:?}");
        // `user@host` would be read as a container name nobody has.
        assert!(!argv.iter().any(|a| a.contains('@')), "{argv:?}");
    }

    #[test]
    fn a_deployed_container_tab_names_the_container_and_its_user_separately() {
        let mut h = host();
        h.host = "devbox".into();
        h.user = Some("devuser".into());
        h.docker = true;
        h.deploy = jterm_core::jsh_remote::Deploy::Persist;

        let argv = build_deployed_argv(&h, std::path::Path::new("/c/jsh-remote.sh"));

        let container = argv.iter().position(|a| a == "--docker").expect("--docker");
        assert_eq!(argv[container + 1], "devbox");
        let user = argv
            .iter()
            .position(|a| a == "--docker-user")
            .expect("--docker-user");
        assert_eq!(argv[user + 1], "devuser");
        assert!(!argv.iter().any(|a| a.contains('@')), "{argv:?}");
        // Deployment is the whole point of this path: the container is not
        // assumed to have a shell already.
        assert!(argv.contains(&"--persist".to_string()), "{argv:?}");
    }

    #[test]
    fn a_host_can_name_the_jsh_it_deploys() {
        let mut h = host();
        h.deploy = jterm_core::jsh_remote::Deploy::Incognito;
        h.deploy_artifact = Some("/home/tester/projects/jsh/target/release/jsh".into());

        let argv = build_deployed_argv(&h, std::path::Path::new("/c/jsh-remote.sh"));

        let artifact = argv
            .iter()
            .position(|a| a == "--artifact")
            .expect("--artifact");
        assert_eq!(
            argv[artifact + 1],
            "/home/tester/projects/jsh/target/release/jsh"
        );
    }

    #[test]
    fn an_artifact_that_could_be_read_as_an_option_or_a_relative_path_rejects_the_host() {
        // Silently ignoring it would look exactly like deployment working,
        // right up to the moment the tab is a bash prompt.
        for artifact in ["target/release/jsh", "-artifact", ""] {
            let table = toml::toml! {
                remote_hosts = [{ host = "h", deploy = "persist", deploy_artifact = (artifact) }]
            };
            assert!(
                parse_remote_hosts(&table).is_empty(),
                "accepted deploy_artifact {artifact:?}"
            );
        }
    }

    #[test]
    fn a_named_artifact_is_reported_when_it_cannot_be_deployed() {
        let issues = validate_config_contents(
            r#"
[[remote_hosts]]
host = "devbox"
deploy = "persist"
deploy_artifact = "/definitely/missing/jsh"

[[remote_hosts]]
name = "no-deploy"
host = "other"
deploy_artifact = "/definitely/missing/jsh"
"#,
        )
        .unwrap();
        let for_host = |index: usize| {
            issues
                .iter()
                .filter(|i| i.path == format!("remote_hosts[{index}].deploy_artifact"))
                .map(|i| (i.level, i.message.as_str()))
                .collect::<Vec<_>>()
        };
        let missing = for_host(0);
        assert_eq!(missing.len(), 1, "{issues:?}");
        assert_eq!(missing[0].0, ConfigIssueLevel::Warning);
        assert!(missing[0].1.contains("no such file"), "{issues:?}");
        // The second host names one *and* never deploys, so it hears about both.
        assert_eq!(for_host(1).len(), 2, "{issues:?}");
        assert!(
            for_host(1).iter().any(|(_, m)| m.contains("no effect")),
            "{issues:?}"
        );
        assert!(
            !issues.iter().any(|i| i.level == ConfigIssueLevel::Error),
            "{issues:?}"
        );
    }

    #[test]
    fn a_docker_host_round_trips_through_toml_and_ssh_hosts_do_not_grow_the_key() {
        let mut h = host();
        h.docker = true;
        h.deploy_artifact = Some("/opt/jsh".into());
        let toml_value = remote_host_to_toml(&h);
        let table = toml::toml! {
            remote_hosts = [(toml_value.clone())]
        };
        let parsed = parse_remote_hosts(&table);
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].docker);
        assert_eq!(parsed[0].deploy_artifact.as_deref(), Some("/opt/jsh"));

        let ssh_host = remote_host_to_toml(&host());
        assert!(
            ssh_host.as_table().is_some_and(|t| {
                !t.contains_key("docker") && !t.contains_key("deploy_artifact")
            }),
            "{ssh_host:?}"
        );
    }

    #[test]
    fn keys_that_do_nothing_for_a_container_are_flagged_without_refusing_the_host() {
        let issues = validate_config_contents(
            r#"
[[remote_hosts]]
host = "devbox"
docker = true
deploy = "persist"
ssh_args = ["-p", "2222"]
multiplex = true
"#,
        )
        .unwrap();
        for key in ["ssh_args", "multiplex"] {
            let issue = issues
                .iter()
                .find(|i| i.path == format!("remote_hosts[0].{key}"))
                .unwrap_or_else(|| panic!("no issue for {key}: {issues:?}"));
            assert_eq!(issue.level, ConfigIssueLevel::Warning, "{issue:?}");
        }
        assert!(
            !issues.iter().any(|i| i.level == ConfigIssueLevel::Error),
            "{issues:?}"
        );
    }

    #[test]
    fn remote_host_config_validates_every_nested_field_type_and_unknown_key() {
        let issues = validate_config_contents(
            r#"
[[remote_hosts]]
name = 42
host = 42
user = false
remote_shell = []
session = { value = "dev" }
ssh_args = "not-an-array"
login_shell = "yes"
multiplex = 1
unexpected = true

[[remote_hosts]]
name = "missing host"
"#,
        )
        .unwrap();

        for key in [
            "name",
            "host",
            "user",
            "remote_shell",
            "session",
            "ssh_args",
            "login_shell",
            "multiplex",
        ] {
            let path = format!("remote_hosts[0].{key}");
            assert!(
                issues
                    .iter()
                    .any(|issue| issue.path == path && issue.is_error()),
                "missing error for {path}: {issues:?}"
            );
        }
        assert!(issues
            .iter()
            .any(|issue| issue.path == "remote_hosts[1].host" && issue.is_error()));
        assert!(issues.iter().any(|issue| {
            issue.path == "remote_hosts[0].unexpected" && issue.level == ConfigIssueLevel::Warning
        }));
    }

    #[test]
    fn remote_host_config_rejects_empty_and_control_character_strings() {
        let issues = validate_config_contents(
            r#"
[[remote_hosts]]
name = " "
host = "example.com\u001b"
user = ""
remote_shell = "jsh\u0007"
session = "\t"
ssh_args = ["", "ok\u007f"]
"#,
        )
        .unwrap();

        for path in [
            "remote_hosts[0].name",
            "remote_hosts[0].host",
            "remote_hosts[0].user",
            "remote_hosts[0].remote_shell",
            "remote_hosts[0].session",
            "remote_hosts[0].ssh_args[0]",
            "remote_hosts[0].ssh_args[1]",
        ] {
            assert!(
                issues
                    .iter()
                    .any(|issue| issue.path == path && issue.is_error()),
                "missing error for {path}: {issues:?}"
            );
        }
    }

    #[test]
    fn remote_host_config_rejects_visual_spoofing_and_unsafe_wire_ids() {
        let issues = validate_config_contents(
            r#"
[[remote_hosts]]
name = "safe\u202efake"
host = "-oProxyCommand=bad"
user = "bad user"
remote_shell = "jsh\uFE0F"
session = "bad/session"
"#,
        )
        .unwrap();
        for path in [
            "remote_hosts[0].name",
            "remote_hosts[0].host",
            "remote_hosts[0].user",
            "remote_hosts[0].remote_shell",
            "remote_hosts[0].session",
        ] {
            assert!(
                issues
                    .iter()
                    .any(|issue| issue.path == path && issue.is_error()),
                "missing error for {path}: {issues:?}"
            );
        }
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
    fn history_paths_require_absolute_or_home_relative_safe_text() {
        for value in ["/tmp/history.jsonl", "~/.local/state/forge/history.jsonl"] {
            let input =
                format!("command_history_path = {value:?}\nblock_history_path = {value:?}\n");
            let issues = validate_config_contents(&input).unwrap();
            assert!(
                issues.iter().all(|issue| !issue.is_error()),
                "rejected {value:?}: {issues:?}"
            );
        }

        let exact_limit = format!("/{}", "x".repeat(MAX_CONFIG_PATH_BYTES - 1));
        assert_eq!(exact_limit.len(), MAX_CONFIG_PATH_BYTES);
        assert!(configured_path_is_safe(&exact_limit, true));
        let oversized = format!("/tmp/{}", "x".repeat(MAX_CONFIG_PATH_BYTES));
        let invalid = [
            "history.jsonl".to_string(),
            "~".to_string(),
            "~//etc/passwd".to_string(),
            "/tmp/bad\nname".to_string(),
            "/tmp/visual\u{202e}spoof".to_string(),
            oversized,
        ];
        for key in ["command_history_path", "block_history_path"] {
            for value in &invalid {
                assert!(!configured_path_is_safe(value, true), "accepted {value:?}");
                let mut table = toml::Table::new();
                table.insert(key.to_string(), toml::Value::String(value.clone()));
                let issues = validate_config_table(&table);
                assert!(
                    issues.iter().any(|issue| {
                        issue.path == key && issue.level == ConfigIssueLevel::Error
                    }),
                    "validator accepted {key}={value:?}: {issues:?}"
                );
            }
        }
    }

    #[test]
    fn history_paths_expand_once_and_missing_home_fails_closed() {
        let home = Path::new("/home/tester");
        assert_eq!(
            expand_configured_path_with("/tmp/history.jsonl", None),
            Some("/tmp/history.jsonl".to_string())
        );
        assert_eq!(
            expand_configured_path_with("~/.local/state/forge/history.jsonl", Some(home)),
            Some("/home/tester/.local/state/forge/history.jsonl".to_string())
        );
        assert!(expand_configured_path_with("~/history", None).is_none());
        assert!(expand_configured_path_with("~/history", Some(Path::new("relative"))).is_none());
        assert!(
            expand_configured_path_with("~/history", Some(Path::new("/home/bad\nname"))).is_none()
        );
    }

    #[test]
    fn ai_and_agent_config_is_semantically_validated() {
        let valid = validate_config_contents(
            "ai_enabled = true\nagent_enabled = true\nagent_max_turns = 20\nagent_auto_approve_readonly = false\ncommand_correction_enabled = true\nai_provider = 'openai-compatible'\nai_base_url = 'https://localhost:8000/v1'\nai_api_key_file = '~/.config/forge/ai.key'\nai_model = 'local-model'\nai_max_tokens = 4096\nai_redact_secrets = true\n",
        )
        .unwrap();
        assert!(valid.is_empty(), "{valid:?}");

        let keyless_https_openai = validate_config_contents(
            "ai_provider = 'openai-compatible'\nai_base_url = 'https://localhost:8000/v1'\n",
        )
        .unwrap();
        assert!(keyless_https_openai.is_empty(), "{keyless_https_openai:?}");

        let local_openai = validate_config_contents(
            "ai_provider = 'openai-compatible'\nai_base_url = 'http://127.0.0.1:8000/v1'\n",
        )
        .unwrap();
        assert!(local_openai.is_empty(), "{local_openai:?}");

        let remote_cleartext = validate_config_contents(
            "ai_provider = 'openai-compatible'\nai_base_url = 'http://models.example.com:8000/v1'\n",
        )
        .unwrap();
        assert!(remote_cleartext.iter().any(|issue| {
            issue.path == "ai_base_url"
                && issue.level == ConfigIssueLevel::Error
                && issue.message.contains("loopback")
        }));

        let local_ollama = validate_config_contents(
            "ai_provider = 'ollama'\nai_base_url = 'http://localhost:11434'\n",
        )
        .unwrap();
        assert!(local_ollama.is_empty(), "{local_ollama:?}");

        let retired = validate_config_contents("agent_auto_approve_readonly = true\n").unwrap();
        assert!(retired.iter().any(|issue| {
            issue.path == "agent_auto_approve_readonly"
                && issue.level == ConfigIssueLevel::Warning
                && issue.message.contains("retired")
        }));

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
        config.bottom_bar = false;
        config.shell = Some("/custom/shell".into());
        config.startup_commands = Some("touch /tmp/should-not-run".into());
        config.command_history_enabled = true;
        config.command_history_path = Some("/tmp/history".into());
        config.block_history_path = Some("/tmp/blocks".into());
        config.ai_enabled = true;
        config.ai_api_key_file = Some("/tmp/ai-key".into());
        config.ai_api_key_file_configured = Some("/tmp/ai-key".into());
        config.agent_enabled = true;
        config.agent_auto_approve_readonly = true;
        config.command_correction_enabled = true;
        config.ai_panel_visible = true;
        config.notify_long_blocks = true;
        config.allow_remote_clipboard_write = true;
        config.remote_hosts.push(host());

        config.apply_safe_mode();

        assert!(matches!(config.terminal_mode, TerminalMode::Vte));
        assert_eq!(config.window_opacity, 0.95);
        assert_eq!(config.terminal_scrollback_lines, 5_000);
        assert_eq!(config.font_desc, DEFAULT_FONT_DESC);
        assert_eq!(config.default_font_scale, 1.0);
        assert_eq!(config.theme_name, "default");
        assert_eq!(config.tab_placement, TabPlacement::Sidebar);
        assert_eq!(config.sidebar_view, SidebarView::Tabs);
        assert!(config.sidebar_visible);
        assert!(config.mouse_reporting_enabled);
        assert!(config.bottom_bar);
        assert!(config.shell.is_none());
        assert!(config.startup_commands.is_none());
        assert!(!config.command_history_enabled);
        assert!(config.command_history_path.is_none());
        assert!(config.block_history_path.is_none());
        assert!(!config.ai_enabled);
        assert!(config.ai_api_key_file.is_none());
        assert!(config.ai_api_key_file_configured.is_none());
        assert!(!config.agent_enabled);
        assert!(!config.agent_auto_approve_readonly);
        assert!(!config.command_correction_enabled);
        assert!(!config.ai_panel_visible);
        assert_eq!(config.jsh_update_check, JshUpdateCheck::Never);
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
    fn non_finite_presentation_numbers_fall_back_before_clamping() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(finite_clamp_f64(Some(value), 0.95, 0.01, 1.0), 0.95);
        }
        assert_eq!(finite_clamp_f64(Some(-1.0), 0.95, 0.01, 1.0), 0.01);
        assert_eq!(finite_clamp_f64(Some(4.0), 0.95, 0.01, 1.0), 1.0);
        assert_eq!(finite_clamp_f64(Some(0.7), 0.95, 0.01, 1.0), 0.7);

        let issues = validate_config_contents("opacity = nan\nfont_scale = inf\n").unwrap();
        for key in ["opacity", "font_scale"] {
            assert!(issues.iter().any(|issue| {
                issue.path == key
                    && issue.level == ConfigIssueLevel::Warning
                    && issue.message.contains("finite")
            }));
        }
    }

    #[test]
    fn invalid_text_override_does_not_erase_a_safe_file_setting() {
        let configured = Some("  Monospace 15  ".to_string());
        assert_eq!(
            resolve_setting_text(
                Some("spoof\u{202e}font".to_string()),
                configured.clone(),
                MAX_FONT_DESC_BYTES,
            )
            .as_deref(),
            Some("Monospace 15")
        );
        assert_eq!(
            resolve_setting_text(
                Some("  JetBrains Mono  ".to_string()),
                configured,
                MAX_FONT_DESC_BYTES,
            )
            .as_deref(),
            Some("JetBrains Mono")
        );
        assert!(resolve_setting_text(
            Some("x".repeat(MAX_FONT_DESC_BYTES + 1)),
            Some("bad\nfont".to_string()),
            MAX_FONT_DESC_BYTES,
        )
        .is_none());
    }

    #[test]
    fn example_config_mentions_every_supported_top_level_key() {
        let example = include_str!("../config.toml.example");
        for key in KNOWN_CONFIG_KEYS {
            let assignment = format!("{key} =");
            let table = format!("[{key}]");
            let array_table = format!("[[{key}]]");
            let present = example.lines().any(|line| {
                let line = line.trim_start().trim_start_matches('#').trim_start();
                line.starts_with(&assignment) || line == table || line == array_table
            });
            assert!(present, "config.toml.example is missing `{key}`");
        }
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

    #[test]
    fn execution_gate_rejects_spoofing_unstructured_ssh_args_and_high_indexes() {
        let valid = host();
        assert!(checked_remote_argv(&valid).is_ok());

        let mut spoofed = valid.clone();
        spoofed.name = "trusted\u{202e}host".into();
        assert!(checked_remote_argv(&spoofed).is_err());

        let mut second_destination = valid.clone();
        second_destination.ssh_args = vec!["attacker.example".into()];
        assert!(checked_remote_argv(&second_destination).is_err());

        let mut premature_separator = valid.clone();
        premature_separator.ssh_args = vec!["--".into()];
        assert!(checked_remote_argv(&premature_separator).is_err());

        let hosts = vec![valid; MAX_REMOTE_HOSTS + 1];
        assert!(checked_remote_host(&hosts, MAX_REMOTE_HOSTS - 1).is_ok());
        assert!(checked_remote_host(&hosts, MAX_REMOTE_HOSTS).is_err());
    }
}
