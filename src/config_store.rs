//! Transactional configuration persistence and backup recovery.
//!
//! A loaded [`Config`] carries the exact on-disk
//! revision it was created from.  Saving is serialized with an advisory file
//! lock and rejects stale revisions, so a window cannot silently overwrite an
//! edit made by another window, process, or editor.  Replacement files and
//! backups are private, durable, and atomically renamed into place.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::config::{self, Config, TerminalMode};

const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Exact content revision used for optimistic concurrency checks.
///
/// The content is retained as well as a compact hash so equality does not
/// depend on hash collision resistance.  Its `Debug` output intentionally
/// never includes configuration bytes, which may contain secrets.
#[derive(Clone, PartialEq, Eq)]
pub struct ConfigRevision(RevisionState);

#[derive(Clone, PartialEq, Eq)]
enum RevisionState {
    Missing,
    Present { content: Box<[u8]>, hash: u64 },
}

impl ConfigRevision {
    pub(crate) fn missing() -> Self {
        Self(RevisionState::Missing)
    }

    pub(crate) fn from_bytes(bytes: &[u8]) -> Self {
        let mut hash = FNV_OFFSET;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        Self(RevisionState::Present {
            content: bytes.to_vec().into_boxed_slice(),
            hash,
        })
    }
}

impl fmt::Debug for ConfigRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            RevisionState::Missing => f.write_str("Missing"),
            RevisionState::Present { content, hash } => f
                .debug_struct("Present")
                .field("bytes", &content.len())
                .field("hash", hash)
                .finish(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigWriteError {
    Conflict { path: PathBuf },
    Locked { path: PathBuf },
    RevisionUnavailable { path: PathBuf },
    InvalidConfig { path: PathBuf, errors: usize },
    BackupUnavailable { path: PathBuf },
    Io(String),
}

impl ConfigWriteError {
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }
}

impl std::error::Error for ConfigWriteError {}

impl fmt::Display for ConfigWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { path } => write!(
                f,
                "{} changed in another window or editor; reload it before saving",
                path.display()
            ),
            Self::Locked { path } => write!(
                f,
                "timed out waiting for the configuration write lock {}",
                path.display()
            ),
            Self::RevisionUnavailable { path } => write!(
                f,
                "cannot safely save {} because its starting revision is unavailable",
                path.display()
            ),
            Self::InvalidConfig { path, errors } => write!(
                f,
                "refusing to overwrite {} because validation found {errors} error(s)",
                path.display()
            ),
            Self::BackupUnavailable { path } => write!(
                f,
                "no valid configuration backup is available for {}",
                path.display()
            ),
            Self::Io(message) => f.write_str(message),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigLockStatus {
    Clear,
    Active,
    Unavailable,
}

fn io_error(operation: &str, path: &Path, error: impl fmt::Display) -> ConfigWriteError {
    ConfigWriteError::Io(format!("{operation} {}: {error}", path.display()))
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, ConfigWriteError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("read", path, error)),
    }
}

fn revision_from_content(content: Option<&[u8]>) -> ConfigRevision {
    content.map_or_else(ConfigRevision::missing, ConfigRevision::from_bytes)
}

fn revision_at(path: &Path) -> Result<ConfigRevision, ConfigWriteError> {
    Ok(revision_from_content(read_optional(path)?.as_deref()))
}

pub fn current_revision() -> Result<ConfigRevision, ConfigWriteError> {
    revision_at(&config::config_file_path())
}

fn backup_path_for(path: &Path) -> PathBuf {
    path.with_extension("toml.bak")
}

fn secondary_backup_path_for(path: &Path) -> PathBuf {
    path.with_extension("toml.bak.1")
}

fn before_restore_path_for(path: &Path) -> PathBuf {
    path.with_extension("toml.before-restore")
}

fn lock_path_for(path: &Path) -> PathBuf {
    path.with_extension("toml.lock")
}

pub fn backup_paths() -> [PathBuf; 2] {
    let path = config::config_file_path();
    [backup_path_for(&path), secondary_backup_path_for(&path)]
}

fn open_lock_file(path: &Path) -> Result<fs::File, ConfigWriteError> {
    let mut options = fs::OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|error| io_error("open lock", path, error))?;
    set_private_permissions(&file, path)?;
    Ok(file)
}

#[cfg(unix)]
fn try_lock_exclusive(file: &fs::File) -> io::Result<bool> {
    // SAFETY: `file` owns a live descriptor for the duration of this call and
    // `flock` neither retains the pointer nor accesses Rust memory.
    let result =
        unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| code == nix::libc::EAGAIN || code == nix::libc::EWOULDBLOCK)
    {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(not(unix))]
fn try_lock_exclusive(_file: &fs::File) -> io::Result<bool> {
    // jterm4's supported GTK targets are Unix.  Keeping this fallback makes
    // the persistence code type-check on other targets without pretending an
    // unsupported platform has a process-safe advisory lock.
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "configuration locking is only supported on Unix",
    ))
}

#[cfg(unix)]
fn unlock(file: &fs::File) {
    // SAFETY: see `try_lock_exclusive`; the descriptor remains live here.
    if unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_UN) } != 0 {
        log::warn!(
            "Failed to release configuration write lock: {}",
            io::Error::last_os_error()
        );
    }
}

#[cfg(not(unix))]
fn unlock(_file: &fs::File) {}

fn lock_status_for(config_path: &Path) -> ConfigLockStatus {
    let path = lock_path_for(config_path);
    if !path.exists() {
        return ConfigLockStatus::Clear;
    }
    // Diagnostics must not create the lock file or tighten its permissions as
    // a side effect. Open the existing inode read-only and probe its advisory
    // lock state; the writer path remains responsible for owner-only mode.
    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ConfigLockStatus::Clear;
        }
        Err(_) => return ConfigLockStatus::Unavailable,
    };
    match try_lock_exclusive(&file) {
        Ok(true) => {
            unlock(&file);
            ConfigLockStatus::Clear
        }
        Ok(false) => ConfigLockStatus::Active,
        Err(_) => ConfigLockStatus::Unavailable,
    }
}

pub fn lock_status() -> ConfigLockStatus {
    lock_status_for(&config::config_file_path())
}

struct ConfigFileLock {
    file: fs::File,
}

impl ConfigFileLock {
    fn acquire(config_path: &Path) -> Result<Self, ConfigWriteError> {
        Self::acquire_with_timeout(config_path, LOCK_TIMEOUT)
    }

    fn acquire_with_timeout(
        config_path: &Path,
        timeout: Duration,
    ) -> Result<Self, ConfigWriteError> {
        let path = lock_path_for(config_path);
        let file = open_lock_file(&path)?;
        let start = Instant::now();
        loop {
            match try_lock_exclusive(&file) {
                Ok(true) => return Ok(Self { file }),
                Ok(false) if start.elapsed() < timeout => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(false) => return Err(ConfigWriteError::Locked { path }),
                Err(error) => return Err(io_error("lock", &path, error)),
            }
        }
    }
}

impl Drop for ConfigFileLock {
    fn drop(&mut self) {
        unlock(&self.file);
    }
}

fn unique_sibling(target: &Path, label: &str) -> Result<PathBuf, ConfigWriteError> {
    let parent = target.parent().ok_or_else(|| {
        ConfigWriteError::Io(format!("{} has no parent directory", target.display()))
    })?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ConfigWriteError::Io(format!("{} has no file name", target.display())))?;
    let nonce = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(".{name}.{label}.{}.{}", std::process::id(), nonce)))
}

fn set_private_permissions(file: &fs::File, path: &Path) -> Result<(), ConfigWriteError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| io_error("set permissions on", path, error))?;
    }
    Ok(())
}

fn stage_private_file(
    target: &Path,
    label: &str,
    contents: &[u8],
) -> Result<PathBuf, ConfigWriteError> {
    for _ in 0..16 {
        let path = unique_sibling(target, label)?;
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error("create temporary file", &path, error)),
        };
        if let Err(error) = set_private_permissions(&file, &path) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        if let Err(error) = file.write_all(contents).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(io_error("write", &path, error));
        }
        return Ok(path);
    }
    Err(ConfigWriteError::Io(format!(
        "could not allocate a unique temporary file beside {}",
        target.display()
    )))
}

fn sync_parent(path: &Path) -> Result<(), ConfigWriteError> {
    let parent = path.parent().ok_or_else(|| {
        ConfigWriteError::Io(format!("{} has no parent directory", path.display()))
    })?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io_error("sync directory", parent, error))
}

fn replace_with_staged(staged: &Path, target: &Path) -> Result<(), ConfigWriteError> {
    fs::rename(staged, target).map_err(|error| io_error("replace", target, error))?;
    sync_parent(target)
}

fn atomic_replace(target: &Path, contents: &[u8]) -> Result<(), ConfigWriteError> {
    let staged = stage_private_file(target, "tmp", contents)?;
    if let Err(error) = replace_with_staged(&staged, target) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    Ok(())
}

fn rotate_backups(config_path: &Path, current: &[u8]) -> Result<(), ConfigWriteError> {
    let primary = backup_path_for(config_path);
    let secondary = secondary_backup_path_for(config_path);
    if let Some(previous_primary) = valid_config_bytes(&primary)? {
        atomic_replace(&secondary, &previous_primary)?;
    }
    atomic_replace(&primary, current)
}

fn apply_config_to_table(config: &Config, table: &mut toml::Table) {
    table.insert("opacity".into(), toml::Value::Float(config.window_opacity));
    table.insert(
        "scrollback".into(),
        toml::Value::Integer(config.terminal_scrollback_lines as i64),
    );
    table.insert("font".into(), toml::Value::String(config.font_desc.clone()));
    table.insert(
        "font_scale".into(),
        toml::Value::Float(config.default_font_scale),
    );
    table.insert(
        "theme".into(),
        toml::Value::String(config.theme_name.clone()),
    );
    table.insert(
        "terminal_mode".into(),
        toml::Value::String(
            match config.terminal_mode {
                TerminalMode::Block => "block",
                TerminalMode::Vte => "vte",
            }
            .to_string(),
        ),
    );
    table.insert(
        "tab_placement".into(),
        toml::Value::String(config.tab_placement.as_str().to_string()),
    );
    table.insert(
        "sidebar_view".into(),
        toml::Value::String(config.sidebar_view.as_str().to_string()),
    );
    table.insert(
        "sidebar_visible".into(),
        toml::Value::Boolean(config.sidebar_visible),
    );
    table.insert(
        "sidebar_width".into(),
        toml::Value::Integer(config.sidebar_width as i64),
    );
    table.insert("ai_enabled".into(), toml::Value::Boolean(config.ai_enabled));
    table.insert(
        "agent_enabled".into(),
        toml::Value::Boolean(config.agent_enabled),
    );
    table.insert(
        "agent_max_turns".into(),
        toml::Value::Integer(config.agent_max_turns as i64),
    );
    table.insert(
        "command_correction_enabled".into(),
        toml::Value::Boolean(config.command_correction_enabled),
    );
    table.insert(
        "ai_provider".into(),
        toml::Value::String(config.ai_provider.clone()),
    );
    table.insert(
        "ai_base_url".into(),
        toml::Value::String(config.ai_base_url.clone()),
    );
    if let Some(path) = &config.ai_api_key_file_configured {
        table.insert("ai_api_key_file".into(), toml::Value::String(path.clone()));
    } else {
        table.remove("ai_api_key_file");
    }
    table.insert(
        "ai_panel_visible".into(),
        toml::Value::Boolean(config.ai_panel_visible),
    );
    table.insert(
        "ai_panel_width".into(),
        toml::Value::Integer(config.ai_panel_width as i64),
    );
    table.insert(
        "ai_model".into(),
        toml::Value::String(config.ai_model.clone()),
    );
    table.insert(
        "ai_max_tokens".into(),
        toml::Value::Integer(config.ai_max_tokens as i64),
    );
    table.insert(
        "ai_redact_secrets".into(),
        toml::Value::Boolean(config.ai_redact_secrets),
    );
    table.insert(
        "allow_remote_clipboard_write".into(),
        toml::Value::Boolean(config.allow_remote_clipboard_write),
    );
    table.insert(
        "notify_long_blocks".into(),
        toml::Value::Boolean(config.notify_long_blocks),
    );
    table.insert(
        "notify_long_block_threshold_ms".into(),
        toml::Value::Integer(config.notify_long_block_threshold_ms as i64),
    );
    table.insert(
        "finished_block_viewport_rows".into(),
        toml::Value::Integer(config.finished_block_viewport_rows as i64),
    );
    table.insert(
        "block_compact".into(),
        toml::Value::Boolean(config.block_compact),
    );
    table.insert(
        "command_history_enabled".into(),
        toml::Value::Boolean(config.command_history_enabled),
    );
    if let Some(path) = &config.command_history_path {
        table.insert(
            "command_history_path".into(),
            toml::Value::String(path.clone()),
        );
    }
    table.insert(
        "command_history_max_entries".into(),
        toml::Value::Integer(config.command_history_max_entries as i64),
    );
    table.insert(
        "show_repo_strip".into(),
        toml::Value::Boolean(config.show_repo_strip),
    );

    // Preserve unknown/user-authored color keys just like other sections.
    let mut colors = table
        .remove("colors")
        .and_then(|value| value.as_table().cloned())
        .unwrap_or_default();
    colors.insert(
        "foreground".into(),
        toml::Value::String(config::rgba_to_hex(&config.foreground)),
    );
    colors.insert(
        "background".into(),
        toml::Value::String(config::rgba_to_hex(&config.background)),
    );
    colors.insert(
        "cursor".into(),
        toml::Value::String(config::rgba_to_hex(&config.cursor)),
    );
    colors.insert(
        "cursor_foreground".into(),
        toml::Value::String(config::rgba_to_hex(&config.cursor_foreground)),
    );
    table.insert("colors".into(), toml::Value::Table(colors));

    // Do not invent personal network targets on a fresh install.  If hosts
    // were explicitly loaded, preserve them when another setting first causes
    // the file to be materialized.
    if !table.contains_key("remote_hosts") && !config.remote_hosts.is_empty() {
        table.insert(
            "remote_hosts".into(),
            toml::Value::Array(
                config
                    .remote_hosts
                    .iter()
                    .map(config::remote_host_to_toml)
                    .collect(),
            ),
        );
    }
}

fn parse_valid_table(path: &Path, bytes: &[u8]) -> Result<toml::Table, ConfigWriteError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ConfigWriteError::InvalidConfig {
        path: path.to_path_buf(),
        errors: 1,
    })?;
    let issues =
        config::validate_config_contents(text).map_err(|_| ConfigWriteError::InvalidConfig {
            path: path.to_path_buf(),
            errors: 1,
        })?;
    let errors = issues.iter().filter(|issue| issue.is_error()).count();
    if errors > 0 {
        return Err(ConfigWriteError::InvalidConfig {
            path: path.to_path_buf(),
            errors,
        });
    }
    text.parse::<toml::Table>()
        .map_err(|_| ConfigWriteError::InvalidConfig {
            path: path.to_path_buf(),
            errors: 1,
        })
}

fn save_config_to_path(
    path: &Path,
    config: &Config,
    expected: Option<&ConfigRevision>,
) -> Result<ConfigRevision, ConfigWriteError> {
    let parent = path.parent().ok_or_else(|| {
        ConfigWriteError::Io(format!("{} has no parent directory", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|error| io_error("create directory", parent, error))?;

    let _lock = ConfigFileLock::acquire(path)?;
    let current = read_optional(path)?;
    let actual_revision = revision_from_content(current.as_deref());
    let Some(expected_revision) = expected else {
        return Err(ConfigWriteError::RevisionUnavailable {
            path: path.to_path_buf(),
        });
    };
    if &actual_revision != expected_revision {
        return Err(ConfigWriteError::Conflict {
            path: path.to_path_buf(),
        });
    }

    let mut table = match current.as_deref() {
        Some(bytes) => parse_valid_table(path, bytes)?,
        None => toml::Table::new(),
    };
    apply_config_to_table(config, &mut table);
    let mut rendered = table.to_string();
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    let rendered = rendered.into_bytes();
    if current.as_deref() == Some(rendered.as_slice()) {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| io_error("open", path, error))?;
        set_private_permissions(&file, path)?;
        file.sync_all()
            .map_err(|error| io_error("sync", path, error))?;
        return Ok(actual_revision);
    }

    let staged = stage_private_file(path, "next", &rendered)?;
    if let Some(current) = current.as_deref() {
        if let Err(error) = rotate_backups(path, current) {
            let _ = fs::remove_file(&staged);
            return Err(error);
        }
    }
    if let Err(error) = replace_with_staged(&staged, path) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    Ok(ConfigRevision::from_bytes(&rendered))
}

/// Save using the revision carried by `config`, updating that revision only
/// after the durable rename succeeds.
fn save_config_with_path(path: &Path, config: &Config) -> Result<ConfigRevision, ConfigWriteError> {
    let mut expected = config
        .persistence_revision
        .lock()
        .map_err(|_| ConfigWriteError::Io("configuration revision lock is poisoned".into()))?;
    let revision = save_config_to_path(path, config, expected.as_ref())?;
    *expected = Some(revision.clone());
    Ok(revision)
}

pub(crate) fn save_config(config: &Config) -> Result<ConfigRevision, ConfigWriteError> {
    save_config_with_path(&config::config_file_path(), config)
}

fn valid_config_bytes(path: &Path) -> Result<Option<Vec<u8>>, ConfigWriteError> {
    let Some(bytes) = read_optional(path)? else {
        return Ok(None);
    };
    match parse_valid_table(path, &bytes) {
        Ok(_) => Ok(Some(bytes)),
        Err(ConfigWriteError::InvalidConfig { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

fn restore_backup_to_path(path: &Path) -> Result<(PathBuf, ConfigRevision), ConfigWriteError> {
    let parent = path.parent().ok_or_else(|| {
        ConfigWriteError::Io(format!("{} has no parent directory", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|error| io_error("create directory", parent, error))?;
    let _lock = ConfigFileLock::acquire(path)?;

    let primary = backup_path_for(path);
    let secondary = secondary_backup_path_for(path);
    let (source, bytes) = if let Some(bytes) = valid_config_bytes(&primary)? {
        (primary, bytes)
    } else if let Some(bytes) = valid_config_bytes(&secondary)? {
        (secondary, bytes)
    } else {
        return Err(ConfigWriteError::BackupUnavailable {
            path: path.to_path_buf(),
        });
    };

    let staged = stage_private_file(path, "restore", &bytes)?;
    if let Some(current) = read_optional(path)? {
        if let Err(error) = atomic_replace(&before_restore_path_for(path), &current) {
            let _ = fs::remove_file(&staged);
            return Err(error);
        }
    }
    if let Err(error) = replace_with_staged(&staged, path) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    Ok((source, ConfigRevision::from_bytes(&bytes)))
}

/// Restore the newest semantically valid rotating backup.  The replaced file,
/// even when corrupt, is retained as `config.toml.before-restore`.
pub fn restore_backup() -> Result<(PathBuf, ConfigRevision), ConfigWriteError> {
    restore_backup_to_path(&config::config_file_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_directory(label: &str) -> PathBuf {
        let nonce = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "jterm4-config-store-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn default_config() -> Config {
        // No environment mutation is required because the path-specific test
        // helpers receive their target explicitly.
        config::load_config().0
    }

    #[test]
    fn revisions_detect_external_changes_without_exposing_contents() {
        let directory = temporary_directory("revision");
        let path = directory.join("config.toml");
        fs::write(&path, "ai_model = 'secret-model-name'\n").unwrap();
        let first = revision_at(&path).unwrap();
        fs::write(&path, "ai_model = 'new-secret-model-name'\n").unwrap();
        let second = revision_at(&path).unwrap();
        assert_ne!(first, second);
        assert!(!format!("{first:?}").contains("secret-model-name"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stale_writer_is_rejected_without_touching_disk() {
        let directory = temporary_directory("conflict");
        let path = directory.join("config.toml");
        fs::write(&path, "opacity = 0.5\n").unwrap();
        let expected = revision_at(&path).unwrap();
        fs::write(&path, "opacity = 0.6\n").unwrap();
        let error = save_config_to_path(&path, &default_config(), Some(&expected)).unwrap_err();
        assert!(error.is_conflict());
        assert_eq!(fs::read_to_string(&path).unwrap(), "opacity = 0.6\n");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_existing_toml_is_never_overwritten() {
        let directory = temporary_directory("invalid");
        let path = directory.join("config.toml");
        let invalid = b"opacity = [ definitely not toml\n";
        fs::write(&path, invalid).unwrap();
        let expected = revision_at(&path).unwrap();
        let error = save_config_to_path(&path, &default_config(), Some(&expected)).unwrap_err();
        assert!(matches!(error, ConfigWriteError::InvalidConfig { .. }));
        assert_eq!(fs::read(&path).unwrap(), invalid);
        assert!(!backup_path_for(&path).exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn semantically_invalid_existing_config_is_never_overwritten() {
        let directory = temporary_directory("semantic-invalid");
        let path = directory.join("config.toml");
        let invalid = b"terminal_mode = 'not-a-mode'\n";
        fs::write(&path, invalid).unwrap();
        let expected = revision_at(&path).unwrap();
        let error = save_config_to_path(&path, &default_config(), Some(&expected)).unwrap_err();
        assert!(matches!(error, ConfigWriteError::InvalidConfig { .. }));
        assert_eq!(fs::read(&path).unwrap(), invalid);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn active_writer_lock_blocks_a_concurrent_writer() {
        let directory = temporary_directory("lock");
        let path = directory.join("config.toml");
        let guard = ConfigFileLock::acquire(&path).unwrap();
        let contender_path = path.clone();
        let error = std::thread::spawn(move || {
            ConfigFileLock::acquire_with_timeout(&contender_path, Duration::from_millis(30))
                .err()
                .expect("second writer must not acquire an active lock")
        })
        .join()
        .unwrap();
        assert!(matches!(error, ConfigWriteError::Locked { .. }));
        drop(guard);
        ConfigFileLock::acquire_with_timeout(&path, Duration::from_millis(30)).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn lock_status_probe_is_read_only_and_detects_an_active_writer() {
        use std::os::unix::fs::PermissionsExt;

        let directory = temporary_directory("lock-status");
        let path = directory.join("config.toml");
        assert_eq!(lock_status_for(&path), ConfigLockStatus::Clear);

        let guard = ConfigFileLock::acquire(&path).unwrap();
        let lock_path = lock_path_for(&path);
        let original_mode = fs::metadata(&lock_path).unwrap().permissions().mode();
        assert_eq!(lock_status_for(&path), ConfigLockStatus::Active);
        assert_eq!(
            fs::metadata(&lock_path).unwrap().permissions().mode(),
            original_mode
        );

        drop(guard);
        assert_eq!(lock_status_for(&path), ConfigLockStatus::Clear);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn config_clones_share_revision_but_independent_loads_stay_stale() {
        let directory = temporary_directory("lineage");
        let path = directory.join("config.toml");

        let mut first_window = default_config();
        first_window.persistence_revision =
            std::sync::Arc::new(std::sync::Mutex::new(Some(ConfigRevision::missing())));
        let mut same_window_clone = first_window.clone();
        let mut independently_loaded = default_config();
        independently_loaded.persistence_revision =
            std::sync::Arc::new(std::sync::Mutex::new(Some(ConfigRevision::missing())));

        first_window.window_opacity = 0.5;
        save_config_with_path(&path, &first_window).unwrap();

        // A clone shares the successfully advanced revision and can perform a
        // later save without falsely conflicting with itself.
        same_window_clone.window_opacity = 0.6;
        save_config_with_path(&path, &same_window_clone).unwrap();

        // An independently loaded window still expects a missing file and is
        // correctly rejected instead of overwriting the newer value.
        independently_loaded.window_opacity = 0.7;
        let error = save_config_with_path(&path, &independently_loaded).unwrap_err();
        assert!(error.is_conflict());
        assert!(fs::read_to_string(&path).unwrap().contains("opacity = 0.6"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn command_history_settings_are_written_transactionally() {
        let directory = temporary_directory("command-history");
        let path = directory.join("config.toml");
        let mut config = default_config();
        config.command_history_enabled = true;
        config.command_history_path = Some("/tmp/jterm4-history.jsonl".into());
        config.command_history_max_entries = 42_000;
        save_config_to_path(&path, &config, Some(&ConfigRevision::missing())).unwrap();
        let table = fs::read_to_string(&path)
            .unwrap()
            .parse::<toml::Table>()
            .unwrap();
        assert_eq!(
            table
                .get("command_history_enabled")
                .and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            table
                .get("command_history_path")
                .and_then(toml::Value::as_str),
            Some("/tmp/jterm4-history.jsonl")
        );
        assert_eq!(
            table
                .get("command_history_max_entries")
                .and_then(toml::Value::as_integer),
            Some(42_000)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ai_provider_settings_round_trip_without_persisting_credentials() {
        let directory = temporary_directory("ai-provider");
        let path = directory.join("config.toml");
        let mut config = default_config();
        config.ai_enabled = true;
        config.agent_enabled = true;
        config.agent_max_turns = 17;
        config.command_correction_enabled = false;
        config.ai_provider = "ollama".into();
        config.ai_base_url = "http://localhost:11434".into();
        config.ai_api_key_file = Some("/run/secrets/provider-api-key".into());
        config.ai_api_key_file_configured = Some("~/.config/jterm4/ai.key".into());
        config.ai_model = "qwen2.5-coder:7b".into();
        config.ai_max_tokens = 2048;
        save_config_to_path(&path, &config, Some(&ConfigRevision::missing())).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        let table = contents.parse::<toml::Table>().unwrap();
        assert_eq!(
            table.get("ai_provider").and_then(toml::Value::as_str),
            Some("ollama")
        );
        assert_eq!(
            table
                .get("agent_max_turns")
                .and_then(toml::Value::as_integer),
            Some(17)
        );
        assert_eq!(
            table
                .get("command_correction_enabled")
                .and_then(toml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            table.get("ai_base_url").and_then(toml::Value::as_str),
            Some("http://localhost:11434")
        );
        assert_eq!(
            table.get("ai_api_key_file").and_then(toml::Value::as_str),
            Some("~/.config/jterm4/ai.key")
        );
        assert!(!contents.contains("sk-test-secret"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn committed_files_backups_and_staging_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let directory = temporary_directory("permissions");
        let path = directory.join("config.toml");
        fs::write(&path, "opacity = 0.5\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let expected = revision_at(&path).unwrap();
        save_config_to_path(&path, &default_config(), Some(&expected)).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(backup_path_for(&path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let leftovers: Vec<_> = fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".config.toml")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "leftover staging files: {leftovers:?}"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rolling_backups_restore_secondary_when_primary_is_invalid() {
        let directory = temporary_directory("restore");
        let path = directory.join("config.toml");
        fs::write(&path, "not valid toml = [\n").unwrap();
        fs::write(backup_path_for(&path), "also invalid = [\n").unwrap();
        fs::write(secondary_backup_path_for(&path), "opacity = 0.7\n").unwrap();
        let (source, _) = restore_backup_to_path(&path).unwrap();
        assert_eq!(source, secondary_backup_path_for(&path));
        assert_eq!(fs::read_to_string(&path).unwrap(), "opacity = 0.7\n");
        assert_eq!(
            fs::read_to_string(before_restore_path_for(&path)).unwrap(),
            "not valid toml = [\n"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn two_successful_updates_rotate_known_good_backups() {
        let directory = temporary_directory("rotation");
        let path = directory.join("config.toml");
        fs::write(&path, "opacity = 0.5\n").unwrap();
        let mut expected = revision_at(&path).unwrap();
        let mut config = default_config();
        config.window_opacity = 0.6;
        expected = save_config_to_path(&path, &config, Some(&expected)).unwrap();
        config.window_opacity = 0.7;
        save_config_to_path(&path, &config, Some(&expected)).unwrap();
        assert!(fs::read_to_string(backup_path_for(&path))
            .unwrap()
            .contains("opacity = 0.6"));
        assert_eq!(
            fs::read_to_string(secondary_backup_path_for(&path)).unwrap(),
            "opacity = 0.5\n"
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
