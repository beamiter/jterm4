//! Lightweight, privacy-conscious command history.
//!
//! Block snapshots are optional because they include terminal output.  This
//! JSONL index intentionally stores only the command, cwd, exit status and
//! completion time, so palette history works without persisting output.

use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, OnceLock};
use std::time::Duration;

const COMPACT_EVERY: u64 = 128;
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 1024 * 1024;
static APPEND_COUNT: AtomicU64 = AtomicU64::new(0);
static HISTORY_WORKER: OnceLock<mpsc::SyncSender<HistoryMessage>> = OnceLock::new();

struct HistoryLock {
    file: File,
}

impl HistoryLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(lock_path(path))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }

        loop {
            if unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX) } == 0 {
                return Ok(Self { file });
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        unsafe {
            nix::libc::flock(self.file.as_raw_fd(), nix::libc::LOCK_UN);
        }
    }
}

struct AppendRequest {
    path: PathBuf,
    max_entries: usize,
    command: String,
    cwd: Option<String>,
    exit_code: i32,
    end_time_ms: Option<u64>,
}

enum HistoryMessage {
    Append(AppendRequest),
    Flush(mpsc::Sender<()>),
}

fn lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".lock");
    PathBuf::from(value)
}

fn history_worker() -> &'static mpsc::SyncSender<HistoryMessage> {
    HISTORY_WORKER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel(1024);
        let spawn_result = std::thread::Builder::new()
            .name("jterm4-command-history".to_string())
            .spawn(move || {
                for message in receiver {
                    match message {
                        HistoryMessage::Append(request) => {
                            if let Err(error) = append(
                                &request.path,
                                request.max_entries,
                                &request.command,
                                request.cwd.as_deref(),
                                request.exit_code,
                                request.end_time_ms,
                            ) {
                                log::warn!("failed to append command history: {error}");
                            }
                        }
                        HistoryMessage::Flush(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            });
        if let Err(error) = spawn_result {
            log::error!("failed to start command-history worker: {error}");
        }
        sender
    })
}

pub(crate) fn append_async(
    path: &Path,
    max_entries: usize,
    command: &str,
    cwd: Option<&str>,
    exit_code: i32,
    end_time_ms: Option<u64>,
) -> io::Result<()> {
    if command.trim().is_empty() {
        return Ok(());
    }
    let request = AppendRequest {
        path: path.to_path_buf(),
        max_entries,
        command: command.to_string(),
        cwd: cwd.map(str::to_string),
        exit_code,
        end_time_ms,
    };
    history_worker()
        .try_send(HistoryMessage::Append(request))
        .map_err(|error| match error {
            mpsc::TrySendError::Full(_) => {
                io::Error::new(io::ErrorKind::WouldBlock, "command-history queue is full")
            }
            mpsc::TrySendError::Disconnected(_) => io::Error::new(
                io::ErrorKind::BrokenPipe,
                "command-history worker is unavailable",
            ),
        })
}

/// Wait for all records queued before this call to reach durable storage.
pub(crate) fn flush_async(timeout: Duration) -> bool {
    let Some(worker) = HISTORY_WORKER.get() else {
        return true;
    };
    let (done_tx, done_rx) = mpsc::channel();
    if worker.try_send(HistoryMessage::Flush(done_tx)).is_err() {
        return false;
    }
    done_rx.recv_timeout(timeout).is_ok()
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct CommandHistoryRecord {
    pub(crate) command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cwd: Option<String>,
    pub(crate) exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) end_time_ms: Option<u64>,
}

pub(crate) fn append(
    path: &Path,
    max_entries: usize,
    command: &str,
    cwd: Option<&str>,
    exit_code: i32,
    end_time_ms: Option<u64>,
) -> io::Result<()> {
    if command.trim().is_empty() {
        return Ok(());
    }
    crate::review_input::validate(command).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("command is unsafe for review-only history: {error}"),
        )
    })?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    let record = CommandHistoryRecord {
        command: command.to_string(),
        cwd: cwd.map(str::to_string),
        exit_code,
        end_time_ms,
    };
    let encoded = serde_json::to_vec(&record).map_err(io::Error::other)?;
    if encoded.len() > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "command history record exceeds 1 MiB",
        ));
    }

    // The same history path can be shared by multiple jterm4 windows or
    // processes. Hold one advisory lock across append and any compaction so a
    // rename can never discard another process's freshly appended record.
    let _lock = HistoryLock::acquire(path)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let mut line = encoded;
    line.push(b'\n');
    file.write_all(&line)?;
    file.flush()?;

    let append_number = APPEND_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let oversized = file.metadata()?.len() > MAX_FILE_BYTES;
    drop(file);
    if oversized || append_number.is_multiple_of(COMPACT_EVERY) {
        compact_unlocked(path, max_entries.max(1))?;
    }
    Ok(())
}

/// Read newest-first, deduplicating repeated commands while retaining the
/// newest metadata. Corrupt or oversized records are ignored.
pub(crate) fn read_recent(
    path: &Path,
    max_entries: usize,
) -> io::Result<Vec<CommandHistoryRecord>> {
    let input = File::open(path)?;
    let mut reader = BufReader::new(input);
    let mut records = VecDeque::new();
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader
            .by_ref()
            .take((MAX_RECORD_BYTES + 1) as u64)
            .read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        if bytes > MAX_RECORD_BYTES || !line.ends_with('\n') {
            if !line.ends_with('\n') {
                let mut discard = Vec::new();
                reader.read_until(b'\n', &mut discard)?;
            }
            continue;
        }
        if let Ok(record) = serde_json::from_str::<CommandHistoryRecord>(line.trim_end()) {
            if crate::review_input::validate(&record.command).is_ok() {
                records.push_front(record);
            }
        }
    }

    let mut seen = HashSet::new();
    Ok(records
        .into_iter()
        .filter(|record| seen.insert(record.command.clone()))
        .take(max_entries)
        .collect())
}

fn compact(path: &Path, max_entries: usize) -> io::Result<()> {
    let _lock = HistoryLock::acquire(path)?;
    compact_unlocked(path, max_entries)
}

fn compact_unlocked(path: &Path, max_entries: usize) -> io::Result<()> {
    let input = File::open(path)?;
    let mut reader = BufReader::new(input);
    let mut recent = VecDeque::with_capacity(max_entries.min(16_384));
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader
            .by_ref()
            .take((MAX_RECORD_BYTES + 1) as u64)
            .read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        if bytes > MAX_RECORD_BYTES || !line.ends_with('\n') {
            if !line.ends_with('\n') {
                let mut discard = Vec::new();
                reader.read_until(b'\n', &mut discard)?;
            }
            continue;
        }
        if serde_json::from_str::<serde_json::Value>(line.trim_end()).is_err() {
            continue;
        }
        if recent.len() == max_entries {
            recent.pop_front();
        }
        recent.push_back(line.clone());
    }

    let tmp = path.with_extension(format!("jsonl.tmp.{}", std::process::id()));
    {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let output = options.open(&tmp)?;
        let mut writer = BufWriter::new(output);
        for record in recent {
            writer.write_all(record.as_bytes())?;
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;
    }
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "jterm4-command-history-{name}-{}-{}.jsonl",
            std::process::id(),
            APPEND_COUNT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn remove_history_files(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(lock_path(path));
    }

    #[test]
    fn append_writes_private_palette_compatible_jsonl() {
        let path = temp_path("append");
        append(&path, 100, "cargo test", Some("/tmp/project"), 0, Some(42)).unwrap();
        let records = read_recent(&path, 10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].command, "cargo test");
        assert_eq!(records[0].cwd.as_deref(), Some("/tmp/project"));
        assert_eq!(records[0].exit_code, 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        remove_history_files(&path);
    }

    #[test]
    fn read_recent_deduplicates_and_skips_corruption() {
        let path = temp_path("read");
        fs::write(
            &path,
            "{\"command\":\"one\",\"exit_code\":1}\nnot-json\n{\"command\":\"two\",\"exit_code\":0}\n{\"command\":\"one\",\"exit_code\":0}\n",
        )
        .unwrap();
        let records = read_recent(&path, 2).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|r| r.command.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two"]
        );
        assert_eq!(records[0].exit_code, 0);
        remove_history_files(&path);
    }

    #[test]
    fn unsafe_control_characters_never_reach_the_palette() {
        let path = temp_path("control");
        let error = append(&path, 100, "echo one\necho two", Some("/tmp"), 0, None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        fs::write(
            &path,
            "{\"command\":\"safe\",\"exit_code\":0}\n{\"command\":\"echo one\\necho two\",\"exit_code\":0}\n{\"command\":\"nul\\u0000byte\",\"exit_code\":0}\n",
        )
        .unwrap();
        let records = read_recent(&path, 10).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.command.as_str())
                .collect::<Vec<_>>(),
            vec!["safe"]
        );
        remove_history_files(&path);
    }

    #[test]
    fn compact_keeps_only_recent_valid_records() {
        let path = temp_path("compact");
        fs::write(
            &path,
            "{\"command\":\"one\"}\nnot-json\n{\"command\":\"two\"}\n{\"command\":\"three\"}\n",
        )
        .unwrap();
        compact(&path, 2).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("one"));
        assert!(!text.contains("not-json"));
        assert!(text.contains("two"));
        assert!(text.contains("three"));
        remove_history_files(&path);
    }

    #[test]
    fn concurrent_append_preserves_every_jsonl_record() {
        let path = std::sync::Arc::new(temp_path("concurrent"));
        let mut threads = Vec::new();
        for worker in 0..8 {
            let path = path.clone();
            threads.push(std::thread::spawn(move || {
                for entry in 0..20 {
                    append(
                        &path,
                        1_000,
                        &format!("echo worker-{worker}-{entry}"),
                        Some("/tmp"),
                        0,
                        None,
                    )
                    .unwrap();
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }

        let records = read_recent(&path, 1_000).unwrap();
        assert_eq!(records.len(), 160);
        remove_history_files(&path);
    }

    #[test]
    fn async_append_flushes_without_blocking_the_caller_on_io() {
        let path = temp_path("async");
        append_async(&path, 100, "cargo check", Some("/tmp"), 0, Some(42)).unwrap();
        assert!(flush_async(Duration::from_secs(5)));
        let records = read_recent(&path, 10).unwrap();
        assert_eq!(records[0].command, "cargo check");
        remove_history_files(&path);
    }
}
