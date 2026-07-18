//! history — extracted from block_view (mechanical split, no logic changes)
//!
//! Persist the in-memory `block_data` deque to/from disk as length-prefixed
//! rkyv records (optional zstd). Truncate-on-save (not append) keeps the file
//! bounded, since the deque was already seeded from this file on startup.

use super::{next_block_id, BlockData, TermView};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_ENCODED_RECORD_BYTES: usize = 256 * 1024 * 1024;
const MAX_DECODED_RECORD_BYTES: u64 = 256 * 1024 * 1024;

fn decode_zstd_bounded(data: &[u8], max_decoded_bytes: u64) -> io::Result<Vec<u8>> {
    let decoder = zstd::Decoder::new(data).map_err(|error| io::Error::other(error.to_string()))?;
    let mut decoded = Vec::new();
    decoder
        .take(max_decoded_bytes + 1)
        .read_to_end(&mut decoded)?;
    if decoded.len() as u64 > max_decoded_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("block history record expands beyond {max_decoded_bytes} bytes"),
        ));
    }
    Ok(decoded)
}

fn decode_rkyv_block(data: &[u8]) -> Option<BlockData> {
    rkyv::from_bytes::<BlockData, rkyv::rancor::Error>(data).ok()
}

/// History frames predate an on-disk codec marker. Try the configured codec
/// first, then the alternate representation so toggling compression never makes
/// the previous session look corrupt.
fn decode_block_record(data: &[u8], prefer_compressed: bool) -> Option<BlockData> {
    let decode_compressed = || {
        decode_zstd_bounded(data, MAX_DECODED_RECORD_BYTES)
            .ok()
            .and_then(|decoded| decode_rkyv_block(&decoded))
    };
    if prefer_compressed {
        decode_compressed().or_else(|| decode_rkyv_block(data))
    } else {
        decode_rkyv_block(data).or_else(decode_compressed)
    }
}

fn history_load_limit(lazy_load_threshold: usize, max_visible_blocks: usize) -> usize {
    lazy_load_threshold.min(max_visible_blocks)
}

/// Persisted IDs are process-local implementation details. Reusing them after a
/// restart collides with the global allocator (which starts from zero again), so
/// restore every record with a fresh runtime ID before exposing it to selection,
/// deletion, bookmarks, search, and export.
fn refresh_loaded_block_ids(blocks: &mut VecDeque<BlockData>) {
    for block in blocks {
        block.id = next_block_id();
    }
}

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
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder.create(parent)?;

    let temp_path = parent.join(temp_file_name(target)?);
    let result = (|| {
        let mut temp = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp_path)?;
        temp.set_permissions(fs::Permissions::from_mode(0o600))?;
        write_contents(&mut temp)?;
        temp.flush()?;
        temp.sync_all()?;
        drop(temp);
        fs::rename(&temp_path, target)?;
        fs::set_permissions(target, fs::Permissions::from_mode(0o600))?;

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
        let (path_opt, compress, load_limit) = {
            let config = self.config.borrow();
            (
                config.block_history_path.as_ref().cloned(),
                config.block_history_compress,
                history_load_limit(
                    config.lazy_load_threshold as usize,
                    config.max_visible_blocks as usize,
                ),
            )
        };
        if path_opt.is_none() {
            return Ok(());
        }

        let path = history_path(&path_opt.unwrap());
        if !path.exists() {
            return Ok(());
        }

        let mut file = File::open(path)?;
        let mut recent_blocks = VecDeque::new();
        let mut total_loaded = 0usize;
        let mut frame_index = 0usize;

        loop {
            let mut len_bytes = [0u8; 4];
            match file.read_exact(&mut len_bytes) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error),
            }

            let len = u32::from_le_bytes(len_bytes) as usize;
            if len > MAX_ENCODED_RECORD_BYTES {
                log::warn!(
                    "load_history: record length {} exceeds {} — treating file as corrupt, stopping",
                    len,
                    MAX_ENCODED_RECORD_BYTES
                );
                break;
            }
            let mut data = vec![0u8; len];
            match file.read_exact(&mut data) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                    log::warn!(
                        "load_history: truncated final frame #{frame_index}; preserving earlier records"
                    );
                    break;
                }
                Err(error) => return Err(error),
            }

            if let Some(block) = decode_block_record(&data, compress) {
                total_loaded += 1;
                push_bounded_back(&mut recent_blocks, block, load_limit);
            } else {
                log::warn!(
                    "load_history: skipping undecodable frame #{frame_index} ({} bytes)",
                    data.len()
                );
            }
            frame_index += 1;
        }

        if total_loaded > load_limit {
            log::info!(
                "Loading block history: keeping {} recent blocks out of {} total (skipping {} old blocks)",
                load_limit,
                total_loaded,
                total_loaded - load_limit
            );
        }

        refresh_loaded_block_ids(&mut recent_blocks);
        let mut blocks = self.block_data.borrow_mut();
        let start_idx = total_loaded.saturating_sub(recent_blocks.len());
        for (offset, block) in recent_blocks.into_iter().enumerate() {
            let idx = start_idx + offset;
            log::debug!(
                "Loaded historical block #{}: prompt={:?}, cmd={:?}, output_len={}, exit_code={}",
                idx,
                block.prompt,
                block.cmd,
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
    use super::{
        atomic_write, decode_block_record, decode_zstd_bounded, expand_home_prefix_with,
        history_load_limit, push_bounded_back, refresh_loaded_block_ids, BlockData,
    };
    use std::collections::VecDeque;
    use std::fs;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
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

    fn sample_block(id: u64, command: &str) -> BlockData {
        BlockData {
            id,
            prompt: "prompt".to_string(),
            cmd: command.to_string(),
            cmd_markup: None,
            output: "output".to_string(),
            exit_code: 0,
            estimated_height: 32,
            line_count: 1,
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: None,
            cwd: None,
            cols: 80,
        }
    }

    #[test]
    fn history_load_limit_never_exceeds_runtime_block_cap() {
        assert_eq!(history_load_limit(1_000, 200), 200);
        assert_eq!(history_load_limit(100, 200), 100);
        assert_eq!(history_load_limit(0, 200), 0);
    }

    #[test]
    fn loaded_blocks_receive_unique_runtime_ids() {
        let mut blocks = VecDeque::from([sample_block(0, "first"), sample_block(0, "second")]);
        refresh_loaded_block_ids(&mut blocks);
        assert_ne!(blocks[0].id, blocks[1].id);
    }

    #[test]
    fn history_decoder_accepts_raw_and_compressed_records_after_config_toggle() {
        let block = sample_block(7, "printf hello");
        let raw = rkyv::to_bytes::<rkyv::rancor::Error>(&block).unwrap();
        let compressed = zstd::encode_all(raw.as_slice(), 1).unwrap();

        for prefer_compressed in [false, true] {
            assert_eq!(
                decode_block_record(raw.as_slice(), prefer_compressed)
                    .unwrap()
                    .cmd,
                "printf hello"
            );
            assert_eq!(
                decode_block_record(&compressed, prefer_compressed)
                    .unwrap()
                    .cmd,
                "printf hello"
            );
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
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(target.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

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

    #[test]
    fn compressed_record_decode_enforces_output_limit() {
        let compressed = zstd::encode_all(&b"0123456789abcdef"[..], 1).unwrap();

        let error = decode_zstd_bounded(&compressed, 8).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
