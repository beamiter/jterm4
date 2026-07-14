#!/usr/bin/env python3
"""Replace the single last-writer-wins tabs.state file with per-window snapshots."""

from __future__ import annotations

from pathlib import Path


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected one match in {path}, found {count}: {old[:100]!r}")
    path.write_text(text.replace(old, new, 1))


root = Path(__file__).resolve().parents[1]
state = root / "src/state.rs"
main = root / "src/main.rs"
cli = root / "src/cli.rs"
readme = root / "README.md"
guide = root / "docs/USER_GUIDE.md"

replace_once(
    state,
    '''use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};''',
    '''use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};''',
)

replace_once(
    state,
    '''pub(crate) fn tabs_state_file_path() -> PathBuf {
    glib::user_config_dir().join("jterm4").join("tabs.state")
}

/// Generate a unique session ID for rsh session persistence.
pub(crate) fn generate_session_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{}", std::process::id(), ts)
}
''',
    '''const MAX_READY_WINDOW_STATES: usize = 32;
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
            log::debug!("Failed to prune old window snapshot {}: {error}", stale.display());
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
''',
)

replace_once(
    state,
    '''pub(crate) fn load_tabs_state() -> (Option<u32>, Vec<(Option<String>, PaneLayout)>) {
    let path = tabs_state_file_path();
    log::info!("Loading tabs state from: {}", path.display());

    let Ok(contents) = fs::read_to_string(&path) else {
        log::info!("No tabs state file found (first run or no previous state)");
        return (None, Vec::new());
    };

    let (current_page, tabs) = parse_tabs_state(&contents);
    log::info!("Loaded {} tabs from state file", tabs.len());

    // Consume-on-start: delete after read so only one instance restores this snapshot.
    // Each instance writes its own state on close; the last one closed wins.
    if let Err(err) = fs::remove_file(&path) {
        log::debug!("Failed to remove tabs state {}: {err}", path.display());
    }

    (current_page, tabs)
}
''',
    '''pub(crate) fn load_tabs_state() -> (Option<u32>, Vec<(Option<String>, PaneLayout)>) {
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
''',
)

replace_once(
    state,
    '''pub(crate) fn save_tabs_state(notebook: &Notebook, session_ids: &HashMap<u32, String>) {
    let path = tabs_state_file_path();''',
    '''pub(crate) fn save_tabs_state(notebook: &Notebook, session_ids: &HashMap<u32, String>) {
    if WINDOW_STATE_FINALIZED.load(Ordering::Acquire) {
        return;
    }
    let path = tabs_state_file_path();''',
)

state_text = state.read_text()
if "#[cfg(test)]\nmod tests" in state_text:
    raise SystemExit("src/state.rs unexpectedly already contains a tests module")
state.write_text(
    state_text
    + r'''

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_state_dir(test_name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "jterm4-{test_name}-{}",
            generate_session_id()
        ));
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
        assert!(claim_ready_snapshot_in(
            &directory,
            &directory.join("window-12-12.active")
        )
        .is_none());

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
            fs::write(directory.join(format!("window-{index}-{index}.state")), index.to_string())
                .unwrap();
        }
        prune_ready_snapshots_in(&directory, 2);
        assert_eq!(ready_snapshots_in(&directory).len(), 2);
        fs::remove_dir_all(directory).unwrap();
    }
}
'''
)

replace_once(
    main,
    '''use crate::state::{kill_all_terminal_children, load_tabs_state, save_tabs_state};''',
    '''use crate::state::{
    finalize_tabs_state, kill_all_terminal_children, load_tabs_state, save_tabs_state,
};''',
)
replace_once(
    main,
    '''        // Restore tabs from last session snapshot (and delete it immediately).
        // Each instance saves its own state on close; the last one closed wins.''',
    '''        // Atomically claim one ready window snapshot. Other running instances
        // keep separate active files, so concurrent windows cannot overwrite or
        // restore one another's state.''',
)
replace_once(
    main,
    '''            while notebook_for_close_request.n_pages() > 0 {
                notebook_for_close_request.remove_page(Some(0));
            }

            // Directly quit the application''',
    '''            while notebook_for_close_request.n_pages() > 0 {
                notebook_for_close_request.remove_page(Some(0));
            }
            // Make the final snapshot visible only after this window is fully
            // quiesced. Any queued auto-save callbacks become no-ops.
            finalize_tabs_state();

            // Directly quit the application''',
)

replace_once(
    cli,
    '''    let (config, _, _) = load_config();
    let shell = choose_shell_argv(config.shell.as_deref());''',
    '''    let (ready_snapshots, active_snapshots) = crate::state::session_snapshot_counts();
    println!("session snapshots: {ready_snapshots} ready, {active_snapshots} active");

    let (config, _, _) = load_config();
    let shell = choose_shell_argv(config.shell.as_deref());''',
)

replace_once(
    readme,
    '''- 标签页、VTE 分屏、方向导航、缩放与会话恢复''',
    '''- 标签页、VTE 分屏、方向导航、缩放与多窗口独立会话恢复''',
)
replace_once(
    readme,
    '''- 标签状态与 Block 历史使用临时文件加原子替换，降低中断时损坏风险。''',
    '''- 每个窗口使用独立的原子会话快照；并发窗口互不覆盖，崩溃遗留快照会在下次启动回收。''',
)
replace_once(
    readme,
    '''jterm4 --doctor
jterm4 --print-config-path''',
    '''jterm4 --doctor              # 同时报告 ready / active 会话快照数量
jterm4 --print-config-path''',
)

replace_once(
    guide,
    '''标签支持拖放排序、双击重命名、固定、标记、复制以及右键菜单。侧栏可在 Tabs 与 Files 之间切换；标签移到顶栏时，过滤动作仍会显示可见的搜索输入框。
''',
    '''标签支持拖放排序、双击重命名、固定、标记、复制以及右键菜单。侧栏可在 Tabs 与 Files 之间切换；标签移到顶栏时，过滤动作仍会显示可见的搜索输入框。

每个 jterm4 窗口维护独立的活动快照。正常关闭后，快照才会发布给未来窗口；同时运行的窗口不会读取或覆盖彼此状态。多个窗口关闭后，后续启动会逐个原子领取最近的快照。异常退出留下的活动快照会在确认原进程已结束后自动回收，旧版 `tabs.state` 也会在首次启动时无损迁移。`jterm4 --doctor` 只报告 ready / active 数量，不暴露路径或标签内容。
''',
)

for path in (state, main, cli, readme, guide):
    text = path.read_text()
    if "last one closed wins" in text or "最后关闭者" in text:
        raise SystemExit(f"stale last-writer session wording remains in {path}")

print("multi-window session snapshot migration applied")
