use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Label, Notebook, Paned};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use vte4::Terminal;
use vte4::TerminalExt;

use crate::block_view::TermView;
use crate::terminal::{collect_terminals, find_first_terminal, terminal_working_directory};

const MAX_READY_WINDOW_STATES: usize = 32;
const READY_STATE_EXTENSION: &str = "state";
const ACTIVE_STATE_EXTENSION: &str = "active";

#[derive(Debug)]
struct WindowStatePaths {
    directory: PathBuf,
    active: PathBuf,
    ready: PathBuf,
}

static WINDOW_STATE_PATHS: OnceLock<WindowStatePaths> = OnceLock::new();
static WINDOW_STATE_FINALIZED: AtomicBool = AtomicBool::new(false);

fn window_state_directory() -> PathBuf {
    glib::user_config_dir().join("jterm4").join("windows")
}

fn legacy_tabs_state_file_path() -> PathBuf {
    glib::user_config_dir().join("jterm4").join("tabs.state")
}

fn window_state_paths() -> &'static WindowStatePaths {
    WINDOW_STATE_PATHS.get_or_init(|| {
        let directory = window_state_directory();
        let id = generate_session_id();
        WindowStatePaths {
            active: directory.join(format!("window-{id}.{ACTIVE_STATE_EXTENSION}")),
            ready: directory.join(format!("window-{id}.{READY_STATE_EXTENSION}")),
            directory,
        }
    })
}

pub(crate) fn tabs_state_file_path() -> PathBuf {
    window_state_paths().active.clone()
}

/// Generate a unique session ID for rsh session persistence and window-state files.
pub(crate) fn generate_session_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{}", std::process::id(), ts)
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension().and_then(|value| value.to_str()) == Some(extension)
}

fn modified_time(path: &Path) -> SystemTime {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(UNIX_EPOCH)
}

fn snapshots_with_extension(directory: &Path, extension: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut snapshots: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && has_extension(path, extension))
        .collect();
    snapshots.sort_by(|left, right| {
        modified_time(right)
            .cmp(&modified_time(left))
            .then_with(|| right.cmp(left))
    });
    snapshots
}

fn ready_snapshots_in(directory: &Path) -> Vec<PathBuf> {
    snapshots_with_extension(directory, READY_STATE_EXTENSION)
}

fn snapshot_owner_pid(path: &Path) -> Option<i32> {
    path.file_stem()?
        .to_str()?
        .strip_prefix("window-")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

fn recover_stale_active_snapshots(directory: &Path) {
    for active in snapshots_with_extension(directory, ACTIVE_STATE_EXTENSION) {
        if snapshot_owner_pid(&active).is_some_and(process_exists) {
            continue;
        }
        let ready = active.with_extension(READY_STATE_EXTENSION);
        match fs::rename(&active, &ready) {
            Ok(()) => log::info!("Recovered interrupted window snapshot {}", ready.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => log::warn!(
                "Failed to recover interrupted window snapshot {}: {error}",
                active.display()
            ),
        }
    }
}

fn claim_ready_snapshot_in(directory: &Path, active: &Path) -> Option<PathBuf> {
    for candidate in ready_snapshots_in(directory) {
        match fs::rename(&candidate, active) {
            Ok(()) => return Some(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => log::debug!(
                "Failed to claim window snapshot {}: {error}",
                candidate.display()
            ),
        }
    }
    None
}

fn prune_ready_snapshots_in(directory: &Path, keep: usize) {
    for stale in ready_snapshots_in(directory).into_iter().skip(keep) {
        if let Err(error) = fs::remove_file(&stale) {
            log::debug!(
                "Failed to prune old window snapshot {}: {error}",
                stale.display()
            );
        }
    }
}

fn prepare_active_tabs_state_path() -> PathBuf {
    let paths = window_state_paths();
    if let Err(error) = fs::create_dir_all(&paths.directory) {
        log::warn!(
            "Failed to create window-state directory {}: {error}",
            paths.directory.display()
        );
        return paths.active.clone();
    }

    recover_stale_active_snapshots(&paths.directory);
    if paths.active.exists() {
        return paths.active.clone();
    }

    // Upgrade the old single-file format first. Atomic rename means concurrent
    // launches cannot restore the same legacy snapshot.
    let legacy = legacy_tabs_state_file_path();
    if legacy.exists() {
        match fs::rename(&legacy, &paths.active) {
            Ok(()) => {
                log::info!("Claimed legacy tabs snapshot {}", legacy.display());
                return paths.active.clone();
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => log::warn!(
                "Failed to claim legacy tabs snapshot {}: {error}",
                legacy.display()
            ),
        }
    }

    if let Some(claimed) = claim_ready_snapshot_in(&paths.directory, &paths.active) {
        log::info!("Claimed window snapshot {}", claimed.display());
    }
    prune_ready_snapshots_in(&paths.directory, MAX_READY_WINDOW_STATES);
    paths.active.clone()
}

/// Report saved and currently active window snapshots without exposing paths.
pub(crate) fn session_snapshot_counts() -> (usize, usize) {
    let directory = window_state_directory();
    (
        ready_snapshots_in(&directory).len(),
        snapshots_with_extension(&directory, ACTIVE_STATE_EXTENSION).len(),
    )
}

/// Pane layout structure for serialization
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PaneLayout {
    Leaf {
        dir: String,
        sid: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cmds: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pinned: Option<bool>,
    },
    Split {
        orientation: char, // 'h' or 'v'
        position: i32,
        start: Box<PaneLayout>,
        end: Box<PaneLayout>,
    },
}

/// Serialize a pane layout tree from a GTK widget
pub(crate) fn serialize_pane_layout(
    widget: &gtk4::Widget,
    session_ids: &HashMap<u32, String>,
) -> PaneLayout {
    if let Some(paned) = widget.downcast_ref::<Paned>() {
        let orientation = match paned.orientation() {
            gtk4::Orientation::Horizontal => 'h',
            gtk4::Orientation::Vertical => 'v',
            _ => 'h',
        };

        let start = paned.start_child().expect("Paned must have start child");
        let end = paned.end_child().expect("Paned must have end child");

        PaneLayout::Split {
            orientation,
            position: paned.position(),
            start: Box::new(serialize_pane_layout(&start, session_ids)),
            end: Box::new(serialize_pane_layout(&end, session_ids)),
        }
    } else {
        // Leaf terminal
        let terminal = find_first_terminal(widget).expect("Leaf must contain terminal");
        let dir = unsafe {
            widget
                .data::<std::rc::Rc<TermView>>("term-view")
                .map(|tv| tv.as_ref().cwd())
                .filter(|s| !s.is_empty())
        }
        .unwrap_or_else(|| {
            terminal_working_directory(&terminal)
                .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".to_string()))
        });

        // Extract tab number from widget name "tab-N" and lookup session_id
        let widget_name = widget.widget_name();
        let sid = if let Some(tab_str) = widget_name.to_string().strip_prefix("tab-") {
            if let Ok(tab_num) = tab_str.parse::<u32>() {
                session_ids
                    .get(&tab_num)
                    .cloned()
                    .unwrap_or_else(generate_session_id)
            } else {
                generate_session_id()
            }
        } else {
            generate_session_id()
        };

        let cmds = get_restorable_commands(&terminal);

        // Check if this tab is pinned
        let pinned = unsafe { widget.data::<bool>("pinned").map(|p| *p.as_ref()) };

        PaneLayout::Leaf {
            dir,
            sid,
            cmds,
            pinned,
        }
    }
}

pub fn escape_tab_state(value: &str) -> String {
    // Optimized: single pass instead of multiple replace() calls
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn unescape_tab_state(value: &str) -> String {
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

pub fn parse_tabs_state(contents: &str) -> (Option<u32>, Vec<(Option<String>, PaneLayout)>) {
    let mut current_page: Option<u32> = None;
    let mut tabs: Vec<(Option<String>, PaneLayout)> = Vec::new();

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
            // Split into fields
            let fields: Vec<&str> = rest.splitn(4, '\t').collect();
            match fields.len() {
                1 => {
                    // Just dir (legacy)
                    let dir = unescape_tab_state(fields[0]);
                    let layout = PaneLayout::Leaf {
                        dir,
                        sid: generate_session_id(),
                        cmds: None,
                        pinned: None,
                    };
                    tabs.push((None, layout));
                }
                2 => {
                    // New format: name + layout_json OR legacy: name + dir
                    let name = unescape_tab_state(fields[0]);
                    let data = unescape_tab_state(fields[1]);

                    // Try parsing as JSON first (new format)
                    if let Ok(layout) = serde_json::from_str::<PaneLayout>(&data) {
                        tabs.push((Some(name), layout));
                    } else {
                        // Legacy: treat as directory
                        let layout = PaneLayout::Leaf {
                            dir: data,
                            sid: generate_session_id(),
                            cmds: None,
                            pinned: None,
                        };
                        tabs.push((Some(name), layout));
                    }
                }
                3 => {
                    // Legacy: name + dir + session_id
                    let name = unescape_tab_state(fields[0]);
                    let dir = unescape_tab_state(fields[1]);
                    let sid = unescape_tab_state(fields[2]);
                    let effective_sid = if sid.is_empty() {
                        generate_session_id()
                    } else {
                        sid
                    };
                    let layout = PaneLayout::Leaf {
                        dir,
                        sid: effective_sid,
                        cmds: None,
                        pinned: None,
                    };
                    tabs.push((Some(name), layout));
                }
                4 => {
                    // Legacy: name + dir + session_id + commands
                    let name = unescape_tab_state(fields[0]);
                    let dir = unescape_tab_state(fields[1]);
                    let sid = unescape_tab_state(fields[2]);
                    let cmds = unescape_tab_state(fields[3]);
                    let effective_sid = if sid.is_empty() {
                        generate_session_id()
                    } else {
                        sid
                    };
                    let effective_cmds = if cmds.is_empty() { None } else { Some(cmds) };
                    let layout = PaneLayout::Leaf {
                        dir,
                        sid: effective_sid,
                        cmds: effective_cmds,
                        pinned: None,
                    };
                    tabs.push((Some(name), layout));
                }
                _ => {}
            }
            continue;
        }
        // Legacy: bare path line
        let layout = PaneLayout::Leaf {
            dir: line.to_string(),
            sid: generate_session_id(),
            cmds: None,
            pinned: None,
        };
        tabs.push((None, layout));
    }

    (current_page, tabs)
}

pub(crate) fn load_tabs_state() -> (Option<u32>, Vec<(Option<String>, PaneLayout)>) {
    let path = prepare_active_tabs_state_path();
    log::info!("Loading tabs state from: {}", path.display());

    let Ok(contents) = fs::read_to_string(&path) else {
        log::info!("No window snapshot found (first run or a new window)");
        return (None, Vec::new());
    };

    let (current_page, tabs) = parse_tabs_state(&contents);
    log::info!("Loaded {} tabs from window snapshot", tabs.len());
    (current_page, tabs)
}

/// Publish this process's active snapshot for a future jterm4 window. Active
/// snapshots are deliberately invisible to other running instances.
pub(crate) fn finalize_tabs_state() {
    if WINDOW_STATE_FINALIZED.swap(true, Ordering::AcqRel) {
        return;
    }

    let paths = window_state_paths();
    if !paths.active.exists() {
        return;
    }
    match fs::rename(&paths.active, &paths.ready) {
        Ok(()) => {
            prune_ready_snapshots_in(&paths.directory, MAX_READY_WINDOW_STATES);
            log::info!("Published window snapshot {}", paths.ready.display());
        }
        Err(error) => log::error!(
            "Failed to publish window snapshot {}: {error}",
            paths.active.display()
        ),
    }
}

pub(crate) fn tab_label_text(notebook: &Notebook, widget: &gtk4::Widget) -> Option<String> {
    let tab_label = notebook.tab_label(widget)?;
    let tab_box = tab_label.downcast::<gtk4::Box>().ok()?;
    let first_child = tab_box.first_child()?;
    let label = first_child.downcast::<Label>().ok()?;
    Some(label.text().to_string())
}

extern "C" {
    fn tcgetpgrp(fd: std::ffi::c_int) -> std::ffi::c_int;
}

fn process_exists(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }

    let rc = unsafe { nix::libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }

    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(nix::libc::EPERM)
    )
}

fn get_process_group_id(pid: i32) -> Option<i32> {
    if pid <= 0 {
        return None;
    }

    let path = format!("/proc/{}/stat", pid);
    let contents = fs::read_to_string(path).ok()?;

    // The stat file format: pid (comm) state ppid pgrp ...
    // comm is in parentheses and may contain spaces, so we need to find the last ')'
    let rparen_pos = contents.rfind(')')?;
    let after_comm = &contents[rparen_pos + 1..];

    // After ')' we have: state ppid pgrp ...
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    if fields.len() >= 3 {
        // fields[0] = state, fields[1] = ppid, fields[2] = pgrp
        fields[2].parse().ok()
    } else {
        None
    }
}

fn signal_pid_and_group(pid: i32, sig: std::ffi::c_int) {
    if pid <= 0 {
        return;
    }

    // First, send signal to the main process
    let rc = unsafe { nix::libc::kill(pid, sig) };
    if rc < 0 {
        // Process doesn't exist or we don't have permission, skip process group signal
        return;
    }

    // Verify the process group leader is the process we want to kill
    // This prevents accidentally killing processes from other sessions if PID was reused
    if let Some(pgid) = get_process_group_id(pid) {
        // Only send signal to process group if this process is the group leader
        // (pgid == pid) or explicitly belongs to a group we created
        if pgid == pid {
            unsafe {
                nix::libc::kill(-pid, sig);
            }
        }
    }
}

fn wait_for_process_exit(pid: i32, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while process_exists(pid) {
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    true
}

pub(crate) fn terminate_terminal_process(pid: i32) {
    if pid <= 0 {
        return;
    }

    // Send initial SIGHUP immediately (non-blocking)
    signal_pid_and_group(pid, nix::libc::SIGHUP);

    // Spawn background thread for escalation to avoid blocking the GTK main thread
    std::thread::spawn(move || {
        if wait_for_process_exit(pid, Duration::from_millis(120)) {
            return;
        }

        signal_pid_and_group(pid, nix::libc::SIGTERM);
        if wait_for_process_exit(pid, Duration::from_millis(250)) {
            return;
        }

        signal_pid_and_group(pid, nix::libc::SIGKILL);
        let _ = wait_for_process_exit(pid, Duration::from_millis(150));
    });
}

pub(crate) fn kill_widget_child_processes(widget: &gtk4::Widget) -> bool {
    let term_view = unsafe {
        widget
            .data::<std::rc::Rc<TermView>>("term-view")
            .map(|ptr| ptr.as_ref().clone())
    };

    if let Some(term_view) = term_view {
        term_view.kill();
        return true;
    }

    false
}

/// Terminate a terminal child process and its process group before the UI tears down.
pub(crate) fn kill_terminal_child(terminal: &Terminal) {
    let pid: i32 = unsafe {
        match terminal.data::<i32>("child-pid") {
            Some(p) => {
                let v: &i32 = p.as_ref();
                *v
            }
            None => return,
        }
    };
    terminate_terminal_process(pid);
}

/// Send SIGHUP to all child process groups across every terminal in the notebook.
pub(crate) fn kill_all_terminal_children(notebook: &Notebook) {
    for i in 0..notebook.n_pages() {
        if let Some(page_widget) = notebook.nth_page(Some(i)) {
            if kill_widget_child_processes(&page_widget) {
                continue;
            }
            let mut terms = Vec::new();
            collect_terminals(&page_widget, &mut terms);
            for term in &terms {
                kill_terminal_child(term);
            }
        }
    }
}

/// Read /proc/<pid>/cmdline and return the argv as a Vec<String>.
pub(crate) fn read_proc_cmdline(pid: i32) -> Option<Vec<String>> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let args: Vec<String> = bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect();
    if args.is_empty() {
        None
    } else {
        Some(args)
    }
}

/// Read the parent PID from /proc/<pid>/stat.
pub(crate) fn read_ppid(pid: i32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Format: "<pid> (<comm>) <state> <ppid> ..."
    // comm may contain spaces/parens, so find the last ')' first.
    let after_comm = stat.rsplit_once(')')?.1;
    let mut fields = after_comm.split_whitespace();
    fields.next(); // state
    fields.next()?.parse::<i32>().ok()
}

/// Check if an argv matches a known restorable command pattern.
/// Returns the command string to replay, or None.
pub(crate) fn match_restorable_command(args: &[String]) -> Option<String> {
    if args.is_empty() {
        return None;
    }
    let bin = Path::new(&args[0])
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    match bin.as_str() {
        "nix" => {
            // e.g. nix develop, nix develop /path/to/flake
            if args.len() >= 2 && args[1] == "develop" {
                Some(args.join(" "))
            } else {
                None
            }
        }
        "bash" | "zsh" | "fish" => {
            // nix develop execs into: bash --rcfile /tmp/nix-shell.XXXXX
            // Detect this pattern and restore as "nix develop" using the CWD's flake.
            for arg in &args[1..] {
                if arg.starts_with("/tmp/nix-shell.") || arg.starts_with("/tmp/nix-shell-") {
                    return Some("nix develop".to_string());
                }
            }
            None
        }
        "ssh" | "mosh" => Some(args.join(" ")),
        "docker" | "podman" => {
            if args.len() >= 2
                && (args[1] == "exec"
                    || (args[1] == "compose" && args.len() >= 3 && args[2] == "exec"))
            {
                Some(args.join(" "))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Detect restorable interactive commands running in a terminal by inspecting the
/// foreground process group and walking up the process tree to the shell.
pub(crate) fn get_restorable_commands(terminal: &Terminal) -> Option<String> {
    let shell_pid: i32 = unsafe { *terminal.data::<i32>("child-pid")?.as_ref() };

    // Find the foreground process group via the PTY fd.
    let pty = terminal.pty()?;
    let raw_fd = pty.fd().as_raw_fd();
    let fg_pgid = unsafe { tcgetpgrp(raw_fd) };
    if fg_pgid <= 0 || fg_pgid == shell_pid {
        return None; // shell itself is foreground — nothing to restore
    }

    // Walk from the foreground process up to the shell, checking each level.
    // This handles cases like: rsh -> nix develop -> bash (fg)
    // as well as: rsh -> bash --rcfile /tmp/nix-shell.* (fg, nix exec'd)
    let mut pid = fg_pgid;
    let mut visited = 0;
    while pid != shell_pid && pid > 1 && visited < 16 {
        if let Some(args) = read_proc_cmdline(pid) {
            if let Some(cmd) = match_restorable_command(&args) {
                return Some(cmd);
            }
        }
        pid = match read_ppid(pid) {
            Some(ppid) => ppid,
            None => break,
        };
        visited += 1;
    }
    None
}

/// Get the name of the foreground process in a terminal, or None if the shell itself is foreground.
pub(crate) fn get_foreground_process_name(terminal: &Terminal) -> Option<String> {
    let shell_pid: i32 = unsafe { *terminal.data::<i32>("child-pid")?.as_ref() };

    let pty = terminal.pty()?;
    let raw_fd = pty.fd().as_raw_fd();
    let fg_pgid = unsafe { tcgetpgrp(raw_fd) };
    if fg_pgid <= 0 || fg_pgid == shell_pid {
        return None;
    }

    if let Some(args) = read_proc_cmdline(fg_pgid) {
        if !args.is_empty() {
            return Path::new(&args[0])
                .file_name()
                .and_then(|name| name.to_str())
                .map(|s| s.to_string());
        }
    }
    None
}

pub(crate) fn save_tabs_state(notebook: &Notebook, session_ids: &HashMap<u32, String>) {
    if WINDOW_STATE_FINALIZED.load(Ordering::Acquire) {
        return;
    }
    let path = tabs_state_file_path();
    log::info!("Saving tabs state to: {}", path.display());

    if let Some(parent) = path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            log::error!("Failed to create state dir {}: {err}", parent.display());
            return;
        }
    }

    let _home = std::env::var("HOME").ok();
    let n_pages = notebook.n_pages();
    log::info!("Saving {} tabs", n_pages);
    let mut lines: Vec<String> = Vec::with_capacity((n_pages as usize) + 1);
    if let Some(current) = notebook.current_page() {
        lines.push(format!("current_page={current}"));
    }

    for i in 0..n_pages {
        let Some(widget) = notebook.nth_page(Some(i)) else {
            continue;
        };

        let label_text =
            tab_label_text(notebook, &widget).unwrap_or_else(|| format!("Terminal {}", i + 1));

        // Serialize the pane layout (supports splits)
        let layout = serialize_pane_layout(&widget, session_ids);
        let layout_json = serde_json::to_string(&layout).unwrap_or_else(|e| {
            log::error!("Failed to serialize layout: {}", e);
            "{}".to_string()
        });

        let line = format!(
            "tab={}\t{}",
            escape_tab_state(&label_text),
            escape_tab_state(&layout_json)
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
        log::error!(
            "Failed to write temp state file {}: {err}",
            tmp_path.display()
        );
        return;
    }

    if let Err(err) = fs::rename(&tmp_path, &path) {
        // On some platforms rename may fail if the destination exists; fall back to remove+rename.
        let _ = fs::remove_file(&path);
        if let Err(err2) = fs::rename(&tmp_path, &path) {
            log::error!(
                "Failed to move temp state file {} into place {}: {err} / {err2}",
                tmp_path.display(),
                path.display()
            );
            let _ = fs::remove_file(&tmp_path);
            return;
        }
    }

    log::info!("Successfully saved tabs state to {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_state_dir(test_name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("jterm4-{test_name}-{}", generate_session_id()));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn parses_snapshot_owner_pid() {
        assert_eq!(
            snapshot_owner_pid(Path::new("window-123-456.active")),
            Some(123)
        );
        assert_eq!(snapshot_owner_pid(Path::new("other.active")), None);
    }

    #[test]
    fn claims_each_ready_snapshot_at_most_once() {
        let directory = temporary_state_dir("claim-ready");
        fs::write(directory.join("window-1-1.state"), "one").unwrap();
        fs::write(directory.join("window-2-2.state"), "two").unwrap();

        let active_one = directory.join("window-10-10.active");
        let active_two = directory.join("window-11-11.active");
        assert!(claim_ready_snapshot_in(&directory, &active_one).is_some());
        assert!(claim_ready_snapshot_in(&directory, &active_two).is_some());
        assert!(
            claim_ready_snapshot_in(&directory, &directory.join("window-12-12.active")).is_none()
        );

        let mut payloads = vec![
            fs::read_to_string(active_one).unwrap(),
            fs::read_to_string(active_two).unwrap(),
        ];
        payloads.sort();
        assert_eq!(payloads, ["one", "two"]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn never_claims_an_active_window_snapshot() {
        let directory = temporary_state_dir("ignore-active");
        fs::write(directory.join("window-1-1.active"), "live").unwrap();
        let destination = directory.join("window-2-2.active");
        assert!(claim_ready_snapshot_in(&directory, &destination).is_none());
        assert!(!destination.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn prunes_ready_snapshots_to_retention_limit() {
        let directory = temporary_state_dir("prune-ready");
        for index in 0..5 {
            fs::write(
                directory.join(format!("window-{index}-{index}.state")),
                index.to_string(),
            )
            .unwrap();
        }
        prune_ready_snapshots_in(&directory, 2);
        assert_eq!(ready_snapshots_in(&directory).len(), 2);
        fs::remove_dir_all(directory).unwrap();
    }
}
