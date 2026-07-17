use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Label, Notebook, Paned};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use vte4::Terminal;
use vte4::TerminalExt;

use crate::terminal::{find_first_terminal, terminal_working_directory};
use crate::ui::{PaneLeaf, PaneNode};

const MAX_READY_WINDOW_STATES: usize = 32;
const MAX_WINDOW_STATE_BYTES: usize = 20 * 1024 * 1024;
const READY_STATE_EXTENSION: &str = "state";
const ACTIVE_STATE_EXTENSION: &str = "active";
const AI_CONVERSATION_PREFIX: &str = "ai_conversation=";
const MAX_AI_CONVERSATION_LINE_BYTES: usize = crate::ai::MAX_CONVERSATION_SNAPSHOT_JSON_BYTES * 2;

#[derive(Debug)]
struct WindowStatePaths {
    directory: PathBuf,
    active: PathBuf,
    ready: PathBuf,
}

static WINDOW_STATE_PATHS: OnceLock<WindowStatePaths> = OnceLock::new();
static WINDOW_STATE_FINALIZED: AtomicBool = AtomicBool::new(false);
static WINDOW_STATE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static AI_CONVERSATION_SNAPSHOT: OnceLock<Mutex<Option<crate::ai::ConversationSnapshot>>> =
    OnceLock::new();

fn ai_conversation_slot() -> &'static Mutex<Option<crate::ai::ConversationSnapshot>> {
    AI_CONVERSATION_SNAPSHOT.get_or_init(|| Mutex::new(None))
}

/// Return the complete, bounded AI conversation associated with this process's
/// active window snapshot.
pub(crate) fn get_ai_conversation_snapshot() -> Option<crate::ai::ConversationSnapshot> {
    ai_conversation_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Replace the AI conversation that the next window-state save will embed.
/// Passing `None` makes Clear durable without manufacturing an empty record.
pub(crate) fn set_ai_conversation_snapshot(snapshot: Option<crate::ai::ConversationSnapshot>) {
    *ai_conversation_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = snapshot;
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder.create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn make_file_private(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

fn write_private_file(path: &Path, payload: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(payload)?;
    file.sync_all()
}

fn unique_state_temp_path(target: &Path) -> io::Result<PathBuf> {
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("state path has no parent: {}", target.display()),
        )
    })?;
    let file_name = target.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("state path has no file name: {}", target.display()),
        )
    })?;
    let sequence = WINDOW_STATE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut temp_name = OsString::from(".");
    temp_name.push(file_name);
    temp_name.push(format!(".tmp-{}-{sequence}", std::process::id()));
    Ok(parent.join(temp_name))
}

/// Durably replace a private state file without ever truncating the last good
/// snapshot. The sibling temporary file also prevents cross-filesystem rename.
fn atomic_write_private_file(target: &Path, payload: &[u8]) -> io::Result<()> {
    let temp_path = unique_state_temp_path(target)?;
    let result = (|| {
        write_private_file(&temp_path, payload)?;
        fs::rename(&temp_path, target)?;
        make_file_private(target)?;
        sync_parent_directory(target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::File::open(parent)?.sync_all()
}

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
            Ok(()) => {
                if let Err(error) = make_file_private(&ready) {
                    log::warn!(
                        "Failed to tighten snapshot permissions {}: {error}",
                        ready.display()
                    );
                }
                log::info!("Recovered interrupted window snapshot {}", ready.display());
            }
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
    if let Err(error) = ensure_private_directory(&paths.directory) {
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
                if let Err(error) = make_file_private(&paths.active) {
                    log::warn!(
                        "Failed to tighten legacy snapshot permissions {}: {error}",
                        paths.active.display()
                    );
                }
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
        if let Err(error) = make_file_private(&paths.active) {
            log::warn!(
                "Failed to tighten claimed snapshot permissions {}: {error}",
                paths.active.display()
            );
        }
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
        let pane_leaf = PaneLeaf::from_widget(widget);
        let dir = pane_leaf
            .as_ref()
            .and_then(PaneLeaf::block_view)
            .map(|view| view.cwd())
            .filter(|cwd| !cwd.is_empty())
            .or_else(|| terminal_working_directory(&terminal))
            .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".to_string()));

        // Prefer the identity attached to this exact leaf. The tab-number map
        // remains a compatibility fallback for older/top-level pages.
        let widget_name = widget.widget_name();
        let sid = pane_leaf
            .as_ref()
            .and_then(PaneLeaf::session_id)
            .unwrap_or_else(|| {
                if let Some(tab_str) = widget_name.to_string().strip_prefix("tab-") {
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
                }
            });

        let cmds = pane_leaf
            .as_ref()
            .and_then(PaneLeaf::restorable_command)
            .or_else(|| get_restorable_commands(&terminal));

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

fn ai_conversation_state_line(
    snapshot: &crate::ai::ConversationSnapshot,
) -> Result<String, crate::ai::ConversationSnapshotError> {
    let encoded = snapshot.to_json()?;
    let escaped = escape_tab_state(&encoded);
    if escaped.len() > MAX_AI_CONVERSATION_LINE_BYTES {
        return Err(crate::ai::ConversationSnapshotError::EncodedTooLarge);
    }
    Ok(format!("{AI_CONVERSATION_PREFIX}{escaped}"))
}

/// Encode line-oriented window state within its hard limit. If the optional
/// line (the AI transcript) is what tips the payload over the boundary, omit
/// it so tabs and pane layouts still reach disk.
fn bounded_window_state_payload(
    lines: &mut Vec<String>,
    optional_line_index: Option<usize>,
    max_bytes: usize,
) -> Option<(String, bool)> {
    let mut payload = lines.join("\n") + "\n";
    if payload.len() <= max_bytes {
        return Some((payload, false));
    }

    let optional_line_index = optional_line_index.filter(|index| *index < lines.len())?;
    lines.remove(optional_line_index);
    payload = lines.join("\n") + "\n";
    (payload.len() <= max_bytes).then_some((payload, true))
}

/// When the current workspace itself is too large to replace, preserve the
/// previous tab/pane payload but still refresh its optional AI line. This
/// keeps New chat and newly enabled redaction durable even at the workspace
/// size boundary.
fn rewrite_existing_ai_conversation(
    path: &Path,
    snapshot: Option<&crate::ai::ConversationSnapshot>,
) -> io::Result<bool> {
    let contents = read_window_state_bounded(path)?;
    let mut lines = Vec::new();
    let mut had_ai_line = false;
    for line in contents.lines() {
        if line.trim().starts_with(AI_CONVERSATION_PREFIX) {
            had_ai_line = true;
        } else {
            lines.push(line.to_string());
        }
    }

    if snapshot.is_none() && !had_ai_line {
        return Ok(false);
    }

    let mut ai_line_index = None;
    if let Some(snapshot) = snapshot {
        let line = ai_conversation_state_line(snapshot)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let insertion = usize::from(
            lines
                .first()
                .is_some_and(|line| line.starts_with("current_page=")),
        );
        lines.insert(insertion, line);
        ai_line_index = Some(insertion);
    }

    let (payload, omitted_ai) =
        bounded_window_state_payload(&mut lines, ai_line_index, MAX_WINDOW_STATE_BYTES)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "previous workspace snapshot cannot be safely rewritten",
                )
            })?;
    atomic_write_private_file(path, payload.as_bytes())?;
    Ok(omitted_ai)
}

/// Parse the AI payload independently from tabs. Any malformed, duplicated,
/// unsupported, or oversized value is ignored without affecting tab recovery.
fn parse_ai_conversation(contents: &str) -> Option<crate::ai::ConversationSnapshot> {
    let mut parsed = None;
    let mut found = false;
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        let Some(value) = line.strip_prefix(AI_CONVERSATION_PREFIX) else {
            continue;
        };
        if found {
            log::warn!("Ignoring duplicated AI conversation in window snapshot");
            return None;
        }
        found = true;
        if value.len() > MAX_AI_CONVERSATION_LINE_BYTES {
            log::warn!("Ignoring oversized AI conversation in window snapshot");
            return None;
        }
        let encoded = unescape_tab_state(value);
        match crate::ai::ConversationSnapshot::from_json(&encoded) {
            Ok(snapshot) => parsed = Some(snapshot),
            Err(error) => {
                log::warn!("Ignoring invalid AI conversation in window snapshot: {error}");
                return None;
            }
        }
    }
    parsed
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
        // Parsed separately so a damaged or future AI payload cannot create a
        // bogus legacy path tab or interfere with workspace recovery.
        if line.starts_with(AI_CONVERSATION_PREFIX) {
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

fn read_window_state_bounded(path: &Path) -> io::Result<String> {
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_WINDOW_STATE_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "window snapshot exceeds the {} byte limit",
                MAX_WINDOW_STATE_BYTES
            ),
        ));
    }
    let mut contents = String::new();
    file.take((MAX_WINDOW_STATE_BYTES + 1) as u64)
        .read_to_string(&mut contents)?;
    if contents.len() > MAX_WINDOW_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "window snapshot exceeds the {} byte limit",
                MAX_WINDOW_STATE_BYTES
            ),
        ));
    }
    Ok(contents)
}

pub(crate) fn load_tabs_state() -> (Option<u32>, Vec<(Option<String>, PaneLayout)>) {
    let path = prepare_active_tabs_state_path();
    log::info!("Loading tabs state from: {}", path.display());

    let contents = match read_window_state_bounded(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            set_ai_conversation_snapshot(None);
            log::info!("No window snapshot found (first run or a new window)");
            return (None, Vec::new());
        }
        Err(error) => {
            set_ai_conversation_snapshot(None);
            log::warn!(
                "Ignoring unreadable window snapshot {}: {error}",
                path.display()
            );
            return (None, Vec::new());
        }
    };

    set_ai_conversation_snapshot(parse_ai_conversation(&contents));
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
            if let Err(error) = make_file_private(&paths.ready) {
                log::warn!(
                    "Failed to tighten published snapshot permissions {}: {error}",
                    paths.ready.display()
                );
            }
            if let Err(error) = sync_parent_directory(&paths.ready) {
                log::debug!(
                    "Failed to sync window-state directory {}: {error}",
                    paths.directory.display()
                );
            }
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
    if let Some(node) = PaneNode::from_widget(widget) {
        for leaf in node.leaves() {
            leaf.kill();
        }
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
            let _ = kill_widget_child_processes(&page_widget);
        }
    }
}

/// Read `/proc/<pid>/cmdline` and return the argv as a `Vec<String>`.
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

/// Read the parent PID from `/proc/<pid>/stat`.
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

/// Foreground process group for a PTY, excluding the pane's ordinary shell.
fn foreground_pgid(pty_fd: i32, shell_pid: i32) -> Option<i32> {
    if pty_fd < 0 || shell_pid <= 0 {
        return None;
    }
    let fg_pgid = unsafe { tcgetpgrp(pty_fd) };
    if fg_pgid <= 0 || fg_pgid == shell_pid {
        return None;
    }
    Some(fg_pgid)
}

/// Detect a restorable interactive command by inspecting a real PTY master fd
/// and walking from its foreground process group back toward the pane shell.
/// This backend-neutral entry point is used by both VTE and Block panes.
pub(crate) fn restorable_command_for_pty(pty_fd: i32, shell_pid: i32) -> Option<String> {
    let fg_pgid = foreground_pgid(pty_fd, shell_pid)?;

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

/// Name of the foreground process on a PTY, or `None` while the normal shell
/// owns the foreground process group.
pub(crate) fn foreground_process_name_for_pty(pty_fd: i32, shell_pid: i32) -> Option<String> {
    let fg_pgid = foreground_pgid(pty_fd, shell_pid)?;
    let args = read_proc_cmdline(fg_pgid)?;
    Path::new(args.first()?)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}

/// Conventional-VTE compatibility wrapper. Block panes must use their
/// `PaneLeaf` probe because their custom PTY is intentionally not VTE-owned.
pub(crate) fn get_restorable_commands(terminal: &Terminal) -> Option<String> {
    let shell_pid: i32 = unsafe { *terminal.data::<i32>("child-pid")?.as_ref() };
    let pty_fd = terminal.pty()?.fd().as_raw_fd();
    restorable_command_for_pty(pty_fd, shell_pid)
}

/// Conventional-VTE compatibility wrapper for tooltip callers.
pub(crate) fn get_foreground_process_name(terminal: &Terminal) -> Option<String> {
    let shell_pid: i32 = unsafe { *terminal.data::<i32>("child-pid")?.as_ref() };
    let pty_fd = terminal.pty()?.fd().as_raw_fd();
    foreground_process_name_for_pty(pty_fd, shell_pid)
}

pub(crate) fn save_tabs_state(notebook: &Notebook, session_ids: &HashMap<u32, String>) {
    if WINDOW_STATE_FINALIZED.load(Ordering::Acquire) {
        return;
    }
    let path = tabs_state_file_path();
    log::info!("Saving tabs state to: {}", path.display());

    if let Some(parent) = path.parent() {
        if let Err(err) = ensure_private_directory(parent) {
            log::error!("Failed to create state dir {}: {err}", parent.display());
            return;
        }
    }

    let _home = std::env::var("HOME").ok();
    let n_pages = notebook.n_pages();
    log::info!("Saving {} tabs", n_pages);
    let mut lines: Vec<String> = Vec::with_capacity((n_pages as usize) + 2);
    if let Some(current) = notebook.current_page() {
        lines.push(format!("current_page={current}"));
    }
    let ai_snapshot = get_ai_conversation_snapshot();
    let mut ai_line_index = None;
    if let Some(snapshot) = ai_snapshot.as_ref() {
        match ai_conversation_state_line(snapshot) {
            Ok(line) => {
                ai_line_index = Some(lines.len());
                lines.push(line);
            }
            Err(error) => {
                // AI state is optional. A malformed in-memory payload must
                // never prevent newer tabs/panes from reaching durable state.
                log::error!(
                    "Omitting unserializable AI conversation from window snapshot: {error}"
                );
            }
        }
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

    let Some((payload, omitted_ai)) =
        bounded_window_state_payload(&mut lines, ai_line_index, MAX_WINDOW_STATE_BYTES)
    else {
        log::error!(
            "Refusing to save window snapshot because tabs and panes exceed the {} byte limit",
            MAX_WINDOW_STATE_BYTES
        );
        match rewrite_existing_ai_conversation(&path, ai_snapshot.as_ref()) {
            Ok(true) => log::warn!(
                "Preserved the previous workspace snapshot but omitted its AI conversation"
            ),
            Ok(false) => log::warn!(
                "Preserved the previous workspace snapshot and refreshed its AI conversation"
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                log::debug!("No previous workspace snapshot exists to refresh")
            }
            Err(error) => log::error!(
                "Failed to refresh AI state in previous workspace snapshot {}: {error}",
                path.display()
            ),
        }
        return;
    };
    if omitted_ai {
        log::warn!(
            "Omitting AI conversation so the window snapshot stays within the {} byte limit",
            MAX_WINDOW_STATE_BYTES
        );
    }

    // Write, fsync, atomically replace, then fsync the directory. A failure at
    // any earlier stage leaves the last good active snapshot untouched.
    if let Err(err) = atomic_write_private_file(&path, payload.as_bytes()) {
        log::error!(
            "Failed to atomically save state file {}: {err}",
            path.display()
        );
        return;
    }
    log::info!("Successfully saved tabs state to {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation_snapshot(question: &str, answer: &str) -> crate::ai::ConversationSnapshot {
        let history = vec![
            crate::ai::Turn {
                role: crate::ai::Role::User,
                text: question.into(),
            },
            crate::ai::Turn {
                role: crate::ai::Role::Assistant,
                text: answer.into(),
            },
        ];
        crate::ai::ConversationSnapshot::from_completed_history(&history, None).unwrap()
    }

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

    #[test]
    fn private_state_storage_uses_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = temporary_state_dir("private-permissions");
        let directory = root.join("windows");
        let snapshot = directory.join("window-1-1.active");
        ensure_private_directory(&directory).unwrap();
        write_private_file(&snapshot, b"state").unwrap();

        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&snapshot).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ai_conversation_line_round_trips_without_becoming_a_tab() {
        let snapshot = conversation_snapshot("为什么？\n第二行", "因为 C:\\tmp");
        let line = ai_conversation_state_line(&snapshot).unwrap();
        let contents = format!("current_page=0\n{line}\ntab=/tmp\n");

        assert_eq!(parse_ai_conversation(&contents), Some(snapshot));
        let (current_page, tabs) = parse_tabs_state(&contents);
        assert_eq!(current_page, Some(0));
        assert_eq!(tabs.len(), 1);
    }

    #[test]
    fn invalid_ai_payload_does_not_prevent_tab_recovery() {
        let contents = concat!(
            "ai_conversation={not-json}\n",
            "current_page=0\n",
            "tab=Terminal 1\t/tmp\t123-456\n"
        );

        assert!(parse_ai_conversation(contents).is_none());
        let (current_page, tabs) = parse_tabs_state(contents);
        assert_eq!(current_page, Some(0));
        assert_eq!(tabs.len(), 1);
    }

    #[test]
    fn duplicate_or_future_ai_payload_is_ignored() {
        let snapshot = conversation_snapshot("q", "a");
        let line = ai_conversation_state_line(&snapshot).unwrap();
        assert!(parse_ai_conversation(&format!("{line}\n{line}\n")).is_none());

        let future = r#"ai_conversation={"version":2,"turns":[{"role":"user","text":"q"},{"role":"assistant","text":"a"}]}"#;
        assert!(parse_ai_conversation(future).is_none());
    }

    #[test]
    fn stale_active_recovery_keeps_conversation_with_its_window() {
        let directory = temporary_state_dir("recover-ai");
        let snapshot = conversation_snapshot("crash question", "last complete answer");
        let line = ai_conversation_state_line(&snapshot).unwrap();
        let stale = directory.join(format!("window-{}-1.active", i32::MAX));
        fs::write(&stale, format!("{line}\ntab=/tmp\n")).unwrap();

        recover_stale_active_snapshots(&directory);
        let ready = stale.with_extension(READY_STATE_EXTENSION);
        assert!(ready.exists());
        let claimed = directory.join("window-10-10.active");
        assert!(claim_ready_snapshot_in(&directory, &claimed).is_some());
        let contents = fs::read_to_string(claimed).unwrap();
        assert_eq!(parse_ai_conversation(&contents), Some(snapshot));
        assert_eq!(parse_tabs_state(&contents).1.len(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn atomic_private_replace_is_durable_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = temporary_state_dir("atomic-replace");
        let directory = root.join("windows");
        ensure_private_directory(&directory).unwrap();
        let target = directory.join("window-1-1.active");
        atomic_write_private_file(&target, b"first").unwrap();
        atomic_write_private_file(&target, b"second").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"second");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(fs::read_dir(&directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bounded_reader_rejects_pathological_snapshot_before_parsing() {
        let root = temporary_state_dir("bounded-read");
        let path = root.join("oversized.state");
        let file = fs::File::create(&path).unwrap();
        file.set_len((MAX_WINDOW_STATE_BYTES + 1) as u64).unwrap();
        drop(file);

        let error = read_window_state_bounded(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_optional_ai_line_never_blocks_workspace_payload() {
        let mut lines = vec![
            "current_page=0".to_string(),
            "ai_conversation=xxxxxxxxxxxxxxxx".to_string(),
            "tab=workspace".to_string(),
        ];
        let (payload, omitted) = bounded_window_state_payload(&mut lines, Some(1), 40).unwrap();

        assert!(omitted);
        assert_eq!(payload, "current_page=0\ntab=workspace\n");
        assert!(!payload.contains(AI_CONVERSATION_PREFIX));
    }

    #[test]
    fn fallback_rewrites_ai_without_touching_previous_workspace() {
        let root = temporary_state_dir("rewrite-ai-only");
        let path = root.join("window-1-1.active");
        let original = conversation_snapshot("old question", "old answer");
        let replacement = conversation_snapshot("new question", "new answer");
        let original_line = ai_conversation_state_line(&original).unwrap();
        fs::write(
            &path,
            format!("current_page=0\n{original_line}\ntab=/tmp\n"),
        )
        .unwrap();

        assert!(!rewrite_existing_ai_conversation(&path, Some(&replacement)).unwrap());
        let replaced = fs::read_to_string(&path).unwrap();
        assert_eq!(parse_ai_conversation(&replaced), Some(replacement));
        assert_eq!(parse_tabs_state(&replaced).1.len(), 1);

        assert!(!rewrite_existing_ai_conversation(&path, None).unwrap());
        let cleared = fs::read_to_string(&path).unwrap();
        assert!(parse_ai_conversation(&cleared).is_none());
        assert_eq!(parse_tabs_state(&cleared).1.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }
}
