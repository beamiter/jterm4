//! history — extracted from block_view (mechanical split, no logic changes)
//!
//! Persist the in-memory `block_data` deque to/from disk as length-prefixed
//! rkyv records (optional zstd). Truncate-on-save (not append) keeps the file
//! bounded, since the deque was already seeded from this file on startup.

use super::{BlockData, TermView};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Expand the shell-style `~/` prefix used in configuration, but leave every
/// other tilde form alone (`~`, `~user/...`, and embedded tildes are literal).
fn expand_home_prefix_with(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

fn history_path(path: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    expand_home_prefix_with(path, home.as_deref())
}

fn temp_file_name(target: &Path) -> io::Result<OsString> {
    let file_name = target.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("history path has no file name: {}", target.display()),
        )
    })?;
    let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = OsString::from(".");
    name.push(file_name);
    name.push(format!(".tmp-{}-{sequence}", std::process::id()));
    Ok(name)
}

/// Write a replacement beside `target`, sync it, then atomically rename it over
/// the old file. Keeping the temporary file in the same directory guarantees
/// that the rename cannot cross filesystems. A failed encoder leaves the old
/// history intact and removes its incomplete temporary file.
fn atomic_write(
    target: &Path,
    write_contents: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let temp_path = parent.join(temp_file_name(target)?);
    let result = (|| {
        let mut temp = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        write_contents(&mut temp)?;
        temp.flush()?;
        temp.sync_all()?;
        drop(temp);
        fs::rename(&temp_path, target)?;

        // Persist the directory entry as well as the file contents. Directory
        // syncing is supported on the Unix platforms jterm4 targets.
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn push_bounded_back<T>(items: &mut VecDeque<T>, item: T, limit: usize) {
    if limit == 0 {
        return;
    }
    if items.len() == limit {
        items.pop_front();
    }
    items.push_back(item);
}

#[allow(dead_code)]
impl TermView {
    /// Save block history to file (if configured).
    pub fn save_history(&self) -> std::io::Result<()> {
        let (path_opt, compress) = {
            let config = self.config.borrow();
            (
                config.block_history_path.as_ref().cloned(),
                config.block_history_compress,
            )
        };
        if path_opt.is_none() {
            return Ok(());
        }

        let path = history_path(&path_opt.unwrap());
        let blocks = self.block_data.borrow();

        // Overwrite (do NOT append). The in-memory deque was itself seeded from
        // this file at startup, so appending it re-wrote every loaded block on
        // each session. Encode into a sibling temp file first so a crash or
        // serialization error never truncates the last good history.
        atomic_write(&path, |file| {
            for block in blocks.iter() {
                let serialized = rkyv::to_bytes::<rkyv::rancor::Error>(block)
                    .map_err(|e| io::Error::other(e.to_string()))?;

                let record: Cow<[u8]> = if compress {
                    Cow::Owned(
                        zstd::encode_all(serialized.as_slice(), 3)
                            .map_err(|e| io::Error::other(e.to_string()))?,
                    )
                } else {
                    Cow::Borrowed(serialized.as_slice())
                };

                // The length prefix is a u32; silently truncating it would corrupt
                // every following frame boundary. Skip a pathological record rather
                // than writing a bad prefix.
                if record.len() > u32::MAX as usize {
                    log::warn!(
                        "save_history: skipping block of {} bytes (exceeds u32 frame limit)",
                        record.len()
                    );
                    continue;
                }
                file.write_all(&(record.len() as u32).to_le_bytes())?;
                file.write_all(record.as_ref())?;
            }
            Ok(())
        })
    }

    /// Load block history from file (if configured).
    pub fn load_history(&self) -> std::io::Result<()> {
        let (path_opt, compress, lazy_load_threshold) = {
            let config = self.config.borrow();
            (
                config.block_history_path.as_ref().cloned(),
                config.block_history_compress,
                config.lazy_load_threshold as usize,
            )
        };
        if path_opt.is_none() {
            return Ok(());
        }

        let path = history_path(&path_opt.unwrap());
        if !path.exists() {
            return Ok(());
        }

        use std::io::Read;
        let mut file = File::open(path)?;
        let mut recent_blocks = VecDeque::new();
        let mut total_loaded = 0usize;

        loop {
            let mut len_bytes = [0u8; 4];
            if file.read_exact(&mut len_bytes).is_err() {
                break;
            }

            let len = u32::from_le_bytes(len_bytes) as usize;
            // Guard against a corrupt/misaligned length causing a giant allocation.
            const MAX_RECORD_BYTES: usize = 256 * 1024 * 1024;
            if len > MAX_RECORD_BYTES {
                log::warn!("load_history: record length {} exceeds {} — treating file as corrupt, stopping", len, MAX_RECORD_BYTES);
                break;
            }
            let mut data = vec![0u8; len];
            file.read_exact(&mut data)?;

            let decoded = if compress {
                zstd::decode_all(data.as_slice())
                    .map_err(|e| std::io::Error::other(e.to_string()))?
            } else {
                data
            };

            if let Ok(block) = rkyv::from_bytes::<BlockData, rkyv::rancor::Error>(&decoded) {
                total_loaded += 1;
                push_bounded_back(&mut recent_blocks, block, lazy_load_threshold);
            }
        }

        if total_loaded > lazy_load_threshold {
            log::info!("Lazy loading history: keeping {} recent blocks out of {} total (skipping {} old blocks)",
                lazy_load_threshold, total_loaded, total_loaded - lazy_load_threshold);
        }

        let mut blocks = self.block_data.borrow_mut();
        let start_idx = total_loaded.saturating_sub(recent_blocks.len());
        for (offset, block) in recent_blocks.into_iter().enumerate() {
            let idx = start_idx + offset;
            log::debug!(
                "Loaded historical block #{}: prompt={:?}, cmd={:?}, output_len={}, exit_code={}",
                idx,
                &block.prompt,
                &block.cmd,
                block.output.len(),
                block.exit_code
            );
            blocks.push_back(block);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{atomic_write, expand_home_prefix_with, push_bounded_back};
    use std::collections::VecDeque;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "jterm4-history-{name}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn push_bounded_back_keeps_only_recent_items() {
        let mut items = VecDeque::new();

        for item in 0..5 {
            push_bounded_back(&mut items, item, 3);
        }

        assert_eq!(items.into_iter().collect::<Vec<_>>(), vec![2, 3, 4]);
    }

    #[test]
    fn push_bounded_back_honors_zero_limit() {
        let mut items = VecDeque::new();

        push_bounded_back(&mut items, 1, 0);

        assert!(items.is_empty());
    }

    #[test]
    fn expands_only_home_slash_prefix() {
        let home = Path::new("/home/tester");
        assert_eq!(
            expand_home_prefix_with("~/.local/share/jterm4/history", Some(home)),
            home.join(".local/share/jterm4/history")
        );
        assert_eq!(expand_home_prefix_with("~", Some(home)), PathBuf::from("~"));
        assert_eq!(
            expand_home_prefix_with("~other/history", Some(home)),
            PathBuf::from("~other/history")
        );
        assert_eq!(
            expand_home_prefix_with("cache/~/history", Some(home)),
            PathBuf::from("cache/~/history")
        );
        assert_eq!(
            expand_home_prefix_with("~/history", None),
            PathBuf::from("~/history")
        );
    }

    #[test]
    fn atomic_write_creates_parent_directories_and_replaces_file() {
        let dir = TestDir::new("replace");
        let target = dir.path().join("nested/deeper/history.bin");

        atomic_write(&target, |file| {
            use std::io::Write as _;
            file.write_all(b"first")
        })
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"first");

        atomic_write(&target, |file| {
            use std::io::Write as _;
            file.write_all(b"second")
        })
        .unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"second");
    }

    #[test]
    fn failed_atomic_write_preserves_previous_file_and_cleans_temp() {
        let dir = TestDir::new("failure");
        let target = dir.path().join("history.bin");
        fs::write(&target, b"last-good").unwrap();

        let err = atomic_write(&target, |file| {
            use std::io::Write as _;
            file.write_all(b"partial")?;
            Err(io::Error::other("simulated encoder failure"))
        })
        .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&target).unwrap(), b"last-good");
        let entries = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![target.file_name().unwrap()]);
    }
}
