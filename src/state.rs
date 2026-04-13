use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Label, Notebook};
use std::collections::HashMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use vte4::Terminal;
use vte4::TerminalExt;

use crate::terminal::{collect_terminals, find_first_terminal, terminal_working_directory};

pub(crate) fn tabs_state_file_path() -> PathBuf {
    glib::user_config_dir()
        .join("jterm4")
        .join("tabs.state")
}

/// Generate a unique session ID for rsh session persistence.
pub(crate) fn generate_session_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{}", std::process::id(), ts)
}

pub(crate) fn escape_tab_state(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

pub(crate) fn unescape_tab_state(value: &str) -> String {
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

pub(crate) fn parse_tabs_state(
    contents: &str,
) -> (
    Option<u32>,
    Vec<(Option<String>, String, Option<String>, Option<String>)>,
) {
    let mut current_page: Option<u32> = None;
    let mut tabs: Vec<(Option<String>, String, Option<String>, Option<String>)> = Vec::new();

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
            // Split into all tab-separated fields
            let fields: Vec<&str> = rest.splitn(4, '\t').collect();
            match fields.len() {
                1 => {
                    // Just dir (legacy)
                    let dir = unescape_tab_state(fields[0]);
                    tabs.push((None, dir, None, None));
                }
                2 => {
                    // name + dir
                    let name = unescape_tab_state(fields[0]);
                    let dir = unescape_tab_state(fields[1]);
                    tabs.push((Some(name), dir, None, None));
                }
                3 => {
                    // name + dir + session_id
                    let name = unescape_tab_state(fields[0]);
                    let dir = unescape_tab_state(fields[1]);
                    let sid = unescape_tab_state(fields[2]);
                    let effective_sid = if sid.is_empty() { None } else { Some(sid) };
                    tabs.push((Some(name), dir, effective_sid, None));
                }
                4 => {
                    // name + dir + session_id + commands
                    let name = unescape_tab_state(fields[0]);
                    let dir = unescape_tab_state(fields[1]);
                    let sid = unescape_tab_state(fields[2]);
                    let cmds = unescape_tab_state(fields[3]);
                    let effective_sid = if sid.is_empty() { None } else { Some(sid) };
                    let effective_cmds = if cmds.is_empty() { None } else { Some(cmds) };
                    tabs.push((Some(name), dir, effective_sid, effective_cmds));
                }
                _ => {}
            }
            continue;
        }
        // Legacy: bare path line
        tabs.push((None, line.to_string(), None, None));
    }

    (current_page, tabs)
}

pub(crate) fn load_tabs_state() -> (
    Option<u32>,
    Vec<(Option<String>, String, Option<String>, Option<String>)>,
) {
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

pub(crate) fn tab_label_text(notebook: &Notebook, widget: &gtk4::Widget) -> Option<String> {
    let tab_label = notebook.tab_label(widget)?;
    let tab_box = tab_label.downcast::<gtk4::Box>().ok()?;
    let first_child = tab_box.first_child()?;
    let label = first_child.downcast::<Label>().ok()?;
    Some(label.text().to_string())
}

extern "C" {
    fn tcgetpgrp(fd: std::ffi::c_int) -> std::ffi::c_int;
    fn kill(pid: std::ffi::c_int, sig: std::ffi::c_int) -> std::ffi::c_int;
}

/// Send SIGHUP to a terminal's child process group so the shell exits cleanly.
pub(crate) fn kill_terminal_child(terminal: &Terminal) {
    let pid: i32 = unsafe {
        match terminal.data::<i32>("child-pid") {
            Some(p) => { let v: &i32 = p.as_ref(); *v }
            None => return,
        }
    };
    if pid > 0 {
        // Negative PID signals the entire process group
        unsafe {
            kill(-pid, 1 /* SIGHUP */);
        }
    }
}

/// Send SIGHUP to all child process groups across every terminal in the notebook.
pub(crate) fn kill_all_terminal_children(notebook: &Notebook) {
    for i in 0..notebook.n_pages() {
        if let Some(page_widget) = notebook.nth_page(Some(i)) {
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

pub(crate) fn save_tabs_state(notebook: &Notebook, session_ids: &HashMap<u32, String>) {
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

        // Extract tab_num from widget name (format: "tab-N")
        let sid = widget
            .widget_name()
            .as_str()
            .strip_prefix("tab-")
            .and_then(|n: &str| n.parse::<u32>().ok())
            .and_then(|tab_num| session_ids.get(&tab_num))
            .map(|s| escape_tab_state(s))
            .unwrap_or_default();

        let commands = get_restorable_commands(&terminal)
            .map(|c| escape_tab_state(&c))
            .unwrap_or_default();

        let line = format!(
            "tab={}\t{}\t{}\t{}",
            escape_tab_state(&label_text),
            escape_tab_state(&dir),
            sid,
            commands
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
