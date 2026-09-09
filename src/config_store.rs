//! Transactional configuration persistence and backup recovery.
//!
//! A loaded [`Config`] carries the exact on-disk
//! revision it was created from.  Saving is serialized with an advisory file
//! lock and rejects stale revisions, so a window cannot silently overwrite an
//! edit made by another window, process, or editor.  Replacement files and
//! backups are private, durable, and atomically renamed into place.

use std::ffi::{CString, OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::config::{self, Config};

const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
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

fn validate_private_regular_file(file: &fs::File, path: &Path) -> io::Result<fs::Metadata> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular configuration file", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} must have exactly one hard link", path.display()),
            ));
        }
        // SAFETY: geteuid has no preconditions and only reads process state.
        if metadata.uid() != unsafe { nix::libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is not owned by the current user", path.display()),
            ));
        }
        // The parent-directory guard below stops an attacker who can create or
        // rename entries in the config dir; it says nothing about a config file
        // that is itself mode 0664, which is what a permissive umask or a
        // `chmod -R` leaves behind. This config names the shell to spawn, the
        // remote hosts, and the agent settings, so a group-writable one is
        // process-launch injection. anvil refuses it; so does forge now.
        if metadata.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} must not be writable by group or other users (run: chmod g-w,o-w {})",
                    path.display(),
                    path.display()
                ),
            ));
        }
    }
    Ok(metadata)
}

#[cfg(unix)]
fn path_name_cstring(path: &Path, label: &str) -> io::Result<CString> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} path has no file name: {}", path.display()),
        )
    })?;
    CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} file name contains NUL"),
        )
    })
}

#[cfg(unix)]
fn os_name_cstring(name: &OsStr, label: &str) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} file name contains NUL"),
        )
    })
}

#[cfg(unix)]
fn open_relative_file(
    directory: &fs::File,
    path: &Path,
    flags: i32,
    mode: nix::libc::mode_t,
) -> io::Result<fs::File> {
    let name = path_name_cstring(path, "configuration")?;
    // SAFETY: `name` and the retained directory descriptor remain live for
    // the call; ownership of a successful descriptor is transferred once.
    let descriptor =
        unsafe { nix::libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, mode) };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: `descriptor` is newly returned and uniquely owned.
        Ok(unsafe { fs::File::from_raw_fd(descriptor) })
    }
}

#[cfg(unix)]
fn existing_parent_directory(path: &Path) -> io::Result<Option<fs::File>> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let directory = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(parent)
    {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = directory.metadata()?;
    // SAFETY: geteuid has no preconditions and only reads process state.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { nix::libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "configuration parent must be current-user owned and not \
                 group/world writable: {} (run: chmod g-w,o-w {})",
                parent.display(),
                parent.display()
            ),
        ));
    }
    Ok(Some(directory))
}

#[cfg(unix)]
fn read_private_bytes_in_directory(
    directory: &fs::File,
    path: &Path,
    max_bytes: u64,
    owner_only: bool,
) -> io::Result<Option<Vec<u8>>> {
    let file = match open_relative_file(
        directory,
        path,
        nix::libc::O_RDONLY | nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC,
        0,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = validate_private_regular_file(&file, path)?;
    if owner_only {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} must have owner-only permissions", path.display()),
            ));
        }
    }
    if metadata.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("{} exceeds {max_bytes} bytes", path.display()),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("{} exceeds {max_bytes} bytes", path.display()),
        ));
    }
    Ok(Some(bytes))
}

pub(crate) fn read_private_bytes(path: &Path, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
    #[cfg(unix)]
    {
        let Some(directory) = existing_parent_directory(path)? else {
            return Ok(None);
        };
        read_private_bytes_in_directory(&directory, path, max_bytes, false)
    }
    #[cfg(not(unix))]
    {
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC);
        }
        let file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata = validate_private_regular_file(&file, path)?;
        if metadata.len() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!("{} exceeds {max_bytes} bytes", path.display()),
            ));
        }

        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!("{} exceeds {max_bytes} bytes", path.display()),
            ));
        }
        Ok(Some(bytes))
    }
}

/// Read a credential-shaped file through the same descriptor-relative,
/// nonblocking boundary as configuration, additionally requiring Unix mode
/// 0600/0400-style owner-only access.
pub(crate) fn read_secret_bytes(path: &Path, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
    #[cfg(unix)]
    {
        let Some(directory) = existing_parent_directory(path)? else {
            return Ok(None);
        };
        read_private_bytes_in_directory(&directory, path, max_bytes, true)
    }
    #[cfg(not(unix))]
    {
        read_private_bytes(path, max_bytes)
    }
}

pub(crate) fn read_config_bytes(path: &Path) -> io::Result<Option<Vec<u8>>> {
    read_private_bytes(path, MAX_CONFIG_BYTES)
}

pub(crate) fn read_config_text(path: &Path) -> io::Result<Option<String>> {
    read_config_bytes(path)?.map_or(Ok(None), |bytes| {
        String::from_utf8(bytes).map(Some).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not valid UTF-8", path.display()),
            )
        })
    })
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, ConfigWriteError> {
    read_config_bytes(path).map_err(|error| io_error("read", path, error))
}

#[cfg(unix)]
fn read_optional_in_directory(
    directory: &fs::File,
    path: &Path,
) -> Result<Option<Vec<u8>>, ConfigWriteError> {
    read_private_bytes_in_directory(directory, path, MAX_CONFIG_BYTES, false)
        .map_err(|error| io_error("read", path, error))
}

fn revision_from_content(content: Option<&[u8]>) -> ConfigRevision {
    content.map_or_else(ConfigRevision::missing, ConfigRevision::from_bytes)
}

fn revision_at(path: &Path) -> Result<ConfigRevision, ConfigWriteError> {
    Ok(revision_from_content(read_optional(path)?.as_deref()))
}

#[cfg(unix)]
fn revision_at_in_directory(
    directory: &fs::File,
    path: &Path,
) -> Result<ConfigRevision, ConfigWriteError> {
    Ok(revision_from_content(
        read_optional_in_directory(directory, path)?.as_deref(),
    ))
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

#[cfg(unix)]
fn validate_existing_parent(parent: &Path) -> Result<fs::File, ConfigWriteError> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(parent)
        .map_err(|error| io_error("open directory", parent, error))?;
    let metadata = directory
        .metadata()
        .map_err(|error| io_error("inspect directory", parent, error))?;
    if !metadata.is_dir() {
        return Err(ConfigWriteError::Io(format!(
            "{} is not a configuration directory",
            parent.display()
        )));
    }
    // A lock path only protects its namespace while another uid cannot replace
    // directory entries behind the descriptor. Allow owner-readable shared
    // directories such as 0755 project roots, but reject group/world-writable
    // parents such as /tmp for configuration persistence.
    // SAFETY: geteuid has no preconditions and only reads process state.
    if metadata.uid() != unsafe { nix::libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(ConfigWriteError::Io(format!(
            "{} must be owned by the current user and not group/world writable",
            parent.display()
        )));
    }
    Ok(directory)
}

#[cfg(not(unix))]
fn validate_existing_parent(parent: &Path) -> Result<fs::File, ConfigWriteError> {
    let directory =
        fs::File::open(parent).map_err(|error| io_error("open directory", parent, error))?;
    if directory
        .metadata()
        .map_err(|error| io_error("inspect directory", parent, error))?
        .is_dir()
    {
        Ok(directory)
    } else {
        Err(ConfigWriteError::Io(format!(
            "{} is not a configuration directory",
            parent.display()
        )))
    }
}

/// Create a missing configuration parent privately, or validate an existing
/// final directory entry without following a symlink. Existing shared parents
/// (for an explicit FORGE_CONFIG path) are never chmodded.
pub(crate) fn ensure_config_parent(path: &Path) -> Result<(), ConfigWriteError> {
    if path.file_name().is_none() {
        return Err(ConfigWriteError::Io(format!(
            "{} has no file name",
            path.display()
        )));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match fs::symlink_metadata(parent) {
        Ok(_) => {
            validate_existing_parent(parent)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)
                    .map_err(|error| io_error("create directory", parent, error))?;
            }
            #[cfg(not(unix))]
            fs::create_dir_all(parent)
                .map_err(|error| io_error("create directory", parent, error))?;
            validate_existing_parent(parent)?;
        }
        Err(error) => return Err(io_error("inspect directory", parent, error)),
    }
    Ok(())
}

fn open_lock_file_in_directory(
    directory: &fs::File,
    path: &Path,
    create: bool,
    tighten_permissions: bool,
) -> Result<fs::File, ConfigWriteError> {
    let mut flags =
        nix::libc::O_RDONLY | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC;
    if create {
        flags = nix::libc::O_RDWR
            | nix::libc::O_CREAT
            | nix::libc::O_NOFOLLOW
            | nix::libc::O_NONBLOCK
            | nix::libc::O_CLOEXEC;
    }
    let file = open_relative_file(directory, path, flags, 0o600)
        .map_err(|error| io_error("open lock", path, error))?;
    validate_private_regular_file(&file, path)
        .map_err(|error| io_error("inspect lock", path, error))?;
    if tighten_permissions {
        set_private_permissions(&file, path)?;
    }
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
    // forge's supported GTK targets are Unix.  Keeping this fallback makes
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
    let parent = config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let directory = match validate_existing_parent(parent) {
        Ok(directory) => directory,
        Err(_)
            if fs::symlink_metadata(parent)
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound) =>
        {
            return ConfigLockStatus::Clear
        }
        Err(_) => return ConfigLockStatus::Unavailable,
    };
    match try_lock_exclusive(&directory) {
        Ok(false) => return ConfigLockStatus::Active,
        Err(_) => return ConfigLockStatus::Unavailable,
        Ok(true) => {}
    }

    // Diagnostics must not create the lock file or tighten its permissions as
    // a side effect. The final entry is opened no-follow/nonblocking and its
    // descriptor is validated before probing the advisory file lock.
    let file = match open_lock_file_in_directory(&directory, &path, false, false) {
        Ok(file) => file,
        Err(_)
            if fs::symlink_metadata(&path)
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound) =>
        {
            unlock(&directory);
            return ConfigLockStatus::Clear;
        }
        Err(_) => return ConfigLockStatus::Unavailable,
    };
    let status = match try_lock_exclusive(&file) {
        Ok(true) => {
            unlock(&file);
            ConfigLockStatus::Clear
        }
        Ok(false) => ConfigLockStatus::Active,
        Err(_) => ConfigLockStatus::Unavailable,
    };
    unlock(&directory);
    status
}

pub fn lock_status() -> ConfigLockStatus {
    lock_status_for(&config::config_file_path())
}

/// Advisory exclusive lock on a private file's parent directory.
///
/// Agent snapshot claims and writes hold this so concurrent forge processes
/// serialize namespace mutations (claim renames, quarantines, replacements)
/// against each other and against configuration saves, which take the same
/// directory lock through `ConfigFileLock`.
pub(crate) struct PrivateParentLock {
    directory: fs::File,
}

impl PrivateParentLock {
    pub(crate) fn acquire(target: &Path) -> Result<Self, ConfigWriteError> {
        ensure_config_parent(target)?;
        let parent = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let directory = validate_existing_parent(parent)?;
        let started = Instant::now();
        loop {
            match try_lock_exclusive(&directory) {
                Ok(true) => return Ok(Self { directory }),
                Ok(false) if started.elapsed() < LOCK_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(false) => {
                    return Err(ConfigWriteError::Locked {
                        path: parent.to_path_buf(),
                    });
                }
                Err(error) => return Err(io_error("lock directory", parent, error)),
            }
        }
    }
}

impl Drop for PrivateParentLock {
    fn drop(&mut self) {
        unlock(&self.directory);
    }
}

struct ConfigFileLock {
    directory: fs::File,
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
        ensure_config_parent(config_path)?;
        let parent = config_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let directory = validate_existing_parent(parent)?;
        let path = lock_path_for(config_path);
        let start = Instant::now();
        loop {
            match try_lock_exclusive(&directory) {
                Ok(true) => break,
                Ok(false) if start.elapsed() < timeout => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(false) => return Err(ConfigWriteError::Locked { path }),
                Err(error) => return Err(io_error("lock directory", parent, error)),
            }
        }

        let file = open_lock_file_in_directory(&directory, &path, true, true)?;
        loop {
            match try_lock_exclusive(&file) {
                Ok(true) => return Ok(Self { directory, file }),
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
        unlock(&self.directory);
    }
}

fn unique_sibling(target: &Path, label: &str) -> Result<PathBuf, ConfigWriteError> {
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .ok_or_else(|| ConfigWriteError::Io(format!("{} has no file name", target.display())))?;
    let nonce = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut staged_name = std::ffi::OsString::from(".");
    staged_name.push(name);
    staged_name.push(format!(".{label}.{}.{nonce}", std::process::id()));
    Ok(parent.join(staged_name))
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

fn unlink_relative(directory: &fs::File, name: &OsStr) {
    let Ok(name) = os_name_cstring(name, "configuration temporary") else {
        return;
    };
    // SAFETY: the name is relative to a retained live directory descriptor.
    unsafe {
        nix::libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0);
    }
}

fn stage_private_file_in_directory(
    directory: &fs::File,
    target: &Path,
    label: &str,
    contents: &[u8],
) -> Result<OsString, ConfigWriteError> {
    for _ in 0..16 {
        let path = unique_sibling(target, label)?;
        let name = path
            .file_name()
            .ok_or_else(|| ConfigWriteError::Io(format!("{} has no file name", path.display())))?;
        let mut file = match open_relative_file(
            directory,
            &path,
            nix::libc::O_WRONLY
                | nix::libc::O_CREAT
                | nix::libc::O_EXCL
                | nix::libc::O_NOFOLLOW
                | nix::libc::O_CLOEXEC,
            0o600,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error("create temporary file", &path, error)),
        };
        if let Err(error) = set_private_permissions(&file, &path) {
            drop(file);
            unlink_relative(directory, name);
            return Err(error);
        }
        if let Err(error) = file.write_all(contents).and_then(|_| file.sync_all()) {
            drop(file);
            unlink_relative(directory, name);
            return Err(io_error("write", &path, error));
        }
        return Ok(name.to_os_string());
    }
    Err(ConfigWriteError::Io(format!(
        "could not allocate a unique temporary file beside {}",
        target.display()
    )))
}

fn sync_parent(path: &Path) -> Result<(), ConfigWriteError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    validate_existing_parent(parent)?
        .sync_all()
        .map_err(|error| io_error("sync directory", parent, error))
}

pub(crate) fn sync_config_parent(path: &Path) -> Result<(), ConfigWriteError> {
    sync_parent(path)
}

fn replace_with_staged_in_directory(
    directory: &fs::File,
    staged_name: &OsStr,
    target: &Path,
) -> Result<(), ConfigWriteError> {
    let staged_name = os_name_cstring(staged_name, "staged configuration")
        .map_err(|error| io_error("inspect staged name for", target, error))?;
    let target_name = path_name_cstring(target, "configuration")
        .map_err(|error| io_error("inspect name for", target, error))?;
    // SAFETY: both names are relative to the same retained directory and no
    // pointers survive the call.
    if unsafe {
        nix::libc::renameat(
            directory.as_raw_fd(),
            staged_name.as_ptr(),
            directory.as_raw_fd(),
            target_name.as_ptr(),
        )
    } != 0
    {
        return Err(io_error("replace", target, io::Error::last_os_error()));
    }
    directory
        .sync_all()
        .map_err(|error| io_error("sync directory for", target, error))
}

fn atomic_replace_in_directory(
    directory: &fs::File,
    target: &Path,
    contents: &[u8],
) -> Result<(), ConfigWriteError> {
    if contents.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigWriteError::Io(format!(
            "refusing to write {} bytes to {}; limit is {MAX_CONFIG_BYTES}",
            contents.len(),
            target.display()
        )));
    }
    let staged = stage_private_file_in_directory(directory, target, "tmp", contents)?;
    if let Err(error) = replace_with_staged_in_directory(directory, &staged, target) {
        unlink_relative(directory, &staged);
        return Err(error);
    }
    Ok(())
}

fn atomic_replace(target: &Path, contents: &[u8]) -> Result<(), ConfigWriteError> {
    ensure_config_parent(target)?;
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let directory = validate_existing_parent(parent)?;
    atomic_replace_in_directory(&directory, target, contents)
}

pub(crate) fn write_private_bytes(
    target: &Path,
    contents: &[u8],
    max_bytes: usize,
) -> Result<(), ConfigWriteError> {
    if contents.len() > max_bytes {
        return Err(ConfigWriteError::Io(format!(
            "refusing to write {} bytes to {}; limit is {max_bytes}",
            contents.len(),
            target.display()
        )));
    }
    atomic_replace(target, contents)
}

fn rotate_backups_in_directory(
    directory: &fs::File,
    config_path: &Path,
    current: &[u8],
) -> Result<(), ConfigWriteError> {
    let primary = backup_path_for(config_path);
    let secondary = secondary_backup_path_for(config_path);
    if let Some(previous_primary) = valid_config_bytes_in_directory(directory, &primary)? {
        atomic_replace_in_directory(directory, &secondary, &previous_primary)?;
    }
    atomic_replace_in_directory(directory, &primary, current)
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
        toml::Value::String(config.terminal_mode.as_str().to_string()),
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
        "jsh_update_check".into(),
        toml::Value::String(config.jsh_update_check.as_str().to_string()),
    );
    table.insert(
        "sidebar_visible".into(),
        toml::Value::Boolean(config.sidebar_visible),
    );
    table.insert(
        "sidebar_width".into(),
        toml::Value::Integer(config.sidebar_width as i64),
    );
    table.insert(
        "tab_width".into(),
        toml::Value::Integer(config.tab_width as i64),
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
        "agent_auto_approve_readonly".into(),
        // Retained as an explicit compatibility tombstone. Never persist a
        // transient/programmatic true value for a capability that no longer
        // exists at runtime.
        toml::Value::Boolean(false),
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
    if let Some(temperature) = config.ai_temperature {
        table.insert(
            "ai_temperature".into(),
            toml::Value::Float(temperature as f64),
        );
    } else {
        table.remove("ai_temperature");
    }
    table.insert("ai_stream".into(), toml::Value::Boolean(config.ai_stream));
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
        "finished_block_max_expanded_rows".into(),
        toml::Value::Integer(config.finished_block_max_expanded_rows as i64),
    );
    table.insert(
        "block_compact".into(),
        toml::Value::Boolean(config.block_compact),
    );
    table.insert(
        "ascii_organism_enabled".into(),
        toml::Value::Boolean(config.ascii_organism_enabled),
    );
    if let Some(motion) = config.ascii_organism_motion {
        table.insert(
            "ascii_organism_motion".into(),
            toml::Value::String(motion.as_str().to_string()),
        );
    } else {
        table.remove("ascii_organism_motion");
    }
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
    table.insert("bottom_bar".into(), toml::Value::Boolean(config.bottom_bar));
    table.insert(
        "click_moves_cursor".into(),
        toml::Value::Boolean(config.click_moves_cursor),
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

    // The in-memory list is authoritative once the file has a remote_hosts
    // key or the list is non-empty, so hosts added or removed in the settings
    // panel actually reach disk.  A fresh install with no hosts keeps no key:
    // do not invent personal network targets.
    if table.contains_key("remote_hosts") || !config.remote_hosts.is_empty() {
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

/// Where a save learns the revision it must still be replacing, and where it
/// publishes the revision it wrote.
///
/// A live save shares one slot with every reader of that [`Config`] — the GTK
/// thread reads it on each reload decision. Both accesses happen inside the
/// advisory file lock and hold the slot's mutex only for the move itself; the
/// two-second lock spin, the backup rotation and every `fsync` stay outside it.
/// Holding it across that I/O is what made a settings change on a slow or
/// contended filesystem freeze the window for as long as the write took.
///
/// Reading `expected` *after* the file lock is what keeps that safe: writers
/// are still serialized by the lock, so the revision this save compares against
/// is the one the previous writer published, not one read seconds earlier.
enum RevisionBinding<'a> {
    /// A fixed expectation supplied by the caller; the result is returned
    /// rather than published.
    Fixed(Option<&'a ConfigRevision>),
    /// The slot shared with everything holding a clone of this `Config`.
    Shared(&'a std::sync::Mutex<Option<ConfigRevision>>),
}

impl RevisionBinding<'_> {
    fn expected(&self) -> Result<Option<ConfigRevision>, ConfigWriteError> {
        match self {
            Self::Fixed(revision) => Ok(revision.map(Clone::clone)),
            Self::Shared(slot) => slot
                .lock()
                .map(|revision| revision.clone())
                .map_err(|_| ConfigWriteError::Io(REVISION_LOCK_POISONED.into())),
        }
    }

    fn publish(&self, revision: &ConfigRevision) -> Result<(), ConfigWriteError> {
        match self {
            Self::Fixed(_) => Ok(()),
            Self::Shared(slot) => slot
                .lock()
                .map(|mut slot| *slot = Some(revision.clone()))
                .map_err(|_| ConfigWriteError::Io(REVISION_LOCK_POISONED.into())),
        }
    }
}

const REVISION_LOCK_POISONED: &str = "configuration revision lock is poisoned";

fn save_config_to_path(
    path: &Path,
    config: &Config,
    expected: Option<&ConfigRevision>,
) -> Result<ConfigRevision, ConfigWriteError> {
    save_config_bound(path, config, &RevisionBinding::Fixed(expected))
}

fn save_config_bound(
    path: &Path,
    config: &Config,
    binding: &RevisionBinding<'_>,
) -> Result<ConfigRevision, ConfigWriteError> {
    ensure_config_parent(path)?;

    let lock = ConfigFileLock::acquire(path)?;
    let expected = binding.expected()?;
    let revision = write_config_under_lock(path, config, &lock, expected.as_ref())?;
    binding.publish(&revision)?;
    Ok(revision)
}

fn write_config_under_lock(
    path: &Path,
    config: &Config,
    lock: &ConfigFileLock,
    expected: Option<&ConfigRevision>,
) -> Result<ConfigRevision, ConfigWriteError> {
    let current = read_optional_in_directory(&lock.directory, path)?;
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
    if rendered.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigWriteError::Io(format!(
            "refusing to save {} because the rendered configuration is {} bytes; limit is {MAX_CONFIG_BYTES}",
            path.display(),
            rendered.len()
        )));
    }
    if current.as_deref() == Some(rendered.as_slice()) {
        let file = open_relative_file(
            &lock.directory,
            path,
            nix::libc::O_RDWR
                | nix::libc::O_NONBLOCK
                | nix::libc::O_NOFOLLOW
                | nix::libc::O_CLOEXEC,
            0,
        )
        .map_err(|error| io_error("open", path, error))?;
        validate_private_regular_file(&file, path)
            .map_err(|error| io_error("inspect", path, error))?;
        set_private_permissions(&file, path)?;
        file.sync_all()
            .map_err(|error| io_error("sync", path, error))?;
        return Ok(actual_revision);
    }

    let staged = stage_private_file_in_directory(&lock.directory, path, "next", &rendered)?;
    if revision_at_in_directory(&lock.directory, path)? != actual_revision {
        unlink_relative(&lock.directory, &staged);
        return Err(ConfigWriteError::Conflict {
            path: path.to_path_buf(),
        });
    }
    if let Some(current) = current.as_deref() {
        if let Err(error) = rotate_backups_in_directory(&lock.directory, path, current) {
            unlink_relative(&lock.directory, &staged);
            return Err(error);
        }
    }
    // Non-cooperating editors do not take our advisory lock. Re-check after
    // the potentially slow backup fsyncs so their latest bytes are not silently
    // replaced by a revision validated several I/O operations ago.
    if revision_at_in_directory(&lock.directory, path)? != actual_revision {
        unlink_relative(&lock.directory, &staged);
        return Err(ConfigWriteError::Conflict {
            path: path.to_path_buf(),
        });
    }
    if let Err(error) = replace_with_staged_in_directory(&lock.directory, &staged, path) {
        unlink_relative(&lock.directory, &staged);
        return Err(error);
    }
    Ok(ConfigRevision::from_bytes(&rendered))
}

/// Save using the revision carried by `config`, updating that revision only
/// after the durable rename succeeds.
fn save_config_with_path(path: &Path, config: &Config) -> Result<ConfigRevision, ConfigWriteError> {
    save_config_bound(
        path,
        config,
        &RevisionBinding::Shared(&config.persistence_revision),
    )
}

pub(crate) fn save_config(config: &Config) -> Result<ConfigRevision, ConfigWriteError> {
    save_config_with_path(&config::config_file_path(), config)
}

fn valid_config_bytes_in_directory(
    directory: &fs::File,
    path: &Path,
) -> Result<Option<Vec<u8>>, ConfigWriteError> {
    let bytes = match read_optional_in_directory(directory, path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(error) => {
            log::warn!(
                "Ignoring unreadable configuration backup {}: {error}",
                path.display()
            );
            return Ok(None);
        }
    };
    match parse_valid_table(path, &bytes) {
        Ok(_) => Ok(Some(bytes)),
        Err(ConfigWriteError::InvalidConfig { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

fn restore_backup_to_path(path: &Path) -> Result<(PathBuf, ConfigRevision), ConfigWriteError> {
    ensure_config_parent(path)?;
    let lock = ConfigFileLock::acquire(path)?;

    let primary = backup_path_for(path);
    let secondary = secondary_backup_path_for(path);
    let (source, bytes) =
        if let Some(bytes) = valid_config_bytes_in_directory(&lock.directory, &primary)? {
            (primary, bytes)
        } else if let Some(bytes) = valid_config_bytes_in_directory(&lock.directory, &secondary)? {
            (secondary, bytes)
        } else {
            return Err(ConfigWriteError::BackupUnavailable {
                path: path.to_path_buf(),
            });
        };

    let current = read_optional_in_directory(&lock.directory, path)?;
    let expected_revision = revision_from_content(current.as_deref());
    let staged = stage_private_file_in_directory(&lock.directory, path, "restore", &bytes)?;
    if revision_at_in_directory(&lock.directory, path)? != expected_revision {
        unlink_relative(&lock.directory, &staged);
        return Err(ConfigWriteError::Conflict {
            path: path.to_path_buf(),
        });
    }
    if let Some(current) = current {
        if let Err(error) =
            atomic_replace_in_directory(&lock.directory, &before_restore_path_for(path), &current)
        {
            unlink_relative(&lock.directory, &staged);
            return Err(error);
        }
    }
    if revision_at_in_directory(&lock.directory, path)? != expected_revision {
        unlink_relative(&lock.directory, &staged);
        return Err(ConfigWriteError::Conflict {
            path: path.to_path_buf(),
        });
    }
    if let Err(error) = replace_with_staged_in_directory(&lock.directory, &staged, path) {
        unlink_relative(&lock.directory, &staged);
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
            "forge-config-store-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
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

    #[cfg(unix)]
    #[test]
    fn config_reads_reject_links_fifo_and_oversize_files() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;

        let directory = temporary_directory("hostile-reads");
        let target = directory.join("target.toml");
        let symbolic = directory.join("symbolic.toml");
        let hard = directory.join("hard.toml");
        let fifo = directory.join("fifo.toml");
        let oversized = directory.join("oversized.toml");
        fs::write(&target, b"opacity = 0.5\n").unwrap();
        symlink(&target, &symbolic).unwrap();
        fs::hard_link(&target, &hard).unwrap();
        let fifo_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_path is a live NUL-terminated path and mkfifo retains
        // no pointer after returning.
        assert_eq!(unsafe { nix::libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        let file = fs::File::create(&oversized).unwrap();
        file.set_len(MAX_CONFIG_BYTES + 1).unwrap();

        assert!(read_config_bytes(&symbolic).is_err());
        assert!(read_config_bytes(&target).is_err());
        assert!(read_config_bytes(&hard).is_err());
        assert!(read_config_bytes(&fifo).is_err());
        assert_eq!(
            read_config_bytes(&oversized).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
        assert_eq!(fs::read(&target).unwrap(), b"opacity = 0.5\n");
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_links_and_fifo_never_touch_or_block_on_their_targets() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{symlink, PermissionsExt};

        for kind in ["symlink", "hardlink"] {
            let directory = temporary_directory(&format!("hostile-lock-{kind}"));
            let config_path = directory.join("config.toml");
            let lock_path = lock_path_for(&config_path);
            let victim = directory.join("victim");
            fs::write(&victim, b"unchanged").unwrap();
            fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
            if kind == "symlink" {
                symlink(&victim, &lock_path).unwrap();
            } else {
                fs::hard_link(&victim, &lock_path).unwrap();
            }

            assert!(
                ConfigFileLock::acquire_with_timeout(&config_path, Duration::from_millis(20))
                    .is_err()
            );
            assert_eq!(fs::read(&victim).unwrap(), b"unchanged");
            assert_eq!(
                fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
                0o644
            );
            fs::remove_dir_all(directory).unwrap();
        }

        let directory = temporary_directory("hostile-lock-fifo");
        let config_path = directory.join("config.toml");
        let lock_path = lock_path_for(&config_path);
        let lock_name = CString::new(lock_path.as_os_str().as_bytes()).unwrap();
        // SAFETY: lock_name is a live NUL-terminated path for this call.
        assert_eq!(unsafe { nix::libc::mkfifo(lock_name.as_ptr(), 0o600) }, 0);
        assert_eq!(lock_status_for(&config_path), ConfigLockStatus::Unavailable);
        assert!(
            ConfigFileLock::acquire_with_timeout(&config_path, Duration::from_millis(20)).is_err()
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn directory_lock_survives_lock_entry_replacement() {
        let directory = temporary_directory("lock-replacement");
        let path = directory.join("config.toml");
        let guard = ConfigFileLock::acquire(&path).unwrap();
        let lock_path = lock_path_for(&path);
        fs::rename(&lock_path, lock_path.with_extension("old")).unwrap();

        let contender = path.clone();
        let error = std::thread::spawn(move || {
            ConfigFileLock::acquire_with_timeout(&contender, Duration::from_millis(30))
                .err()
                .expect("replacement must not bypass the directory lock")
        })
        .join()
        .unwrap();
        assert!(matches!(error, ConfigWriteError::Locked { .. }));
        drop(guard);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn held_directory_descriptor_prevents_parent_namespace_redirection() {
        use std::os::unix::fs::PermissionsExt;

        let root = temporary_directory("parent-swap");
        let live = root.join("live");
        let displaced = root.join("displaced");
        fs::create_dir(&live).unwrap();
        fs::set_permissions(&live, fs::Permissions::from_mode(0o700)).unwrap();
        let path = live.join("config.toml");
        let guard = ConfigFileLock::acquire(&path).unwrap();

        fs::rename(&live, &displaced).unwrap();
        fs::create_dir(&live).unwrap();
        fs::set_permissions(&live, fs::Permissions::from_mode(0o700)).unwrap();
        atomic_replace_in_directory(&guard.directory, &path, b"opacity = 0.5\n").unwrap();

        assert_eq!(
            fs::read_to_string(displaced.join("config.toml")).unwrap(),
            "opacity = 0.5\n"
        );
        assert!(!live.join("config.toml").exists());
        drop(guard);
        fs::remove_dir_all(root).unwrap();
    }

    /// The revision slot is shared with the GTK thread, which reads it on every
    /// reload decision. A save must therefore take the two-second advisory file
    /// lock *before* it touches that slot, and must never hold the slot across
    /// the lock spin, the backup rotation, or an `fsync` — a settings change on
    /// a contended or slow filesystem would otherwise freeze the window for as
    /// long as the whole write took.
    ///
    /// Deterministic without sleeping on the writer: this test holds the slot,
    /// so a correctly ordered save parks inside the file lock and the existing
    /// read-only lock probe reports an active writer. The previous ordering
    /// took the slot first and would still be waiting with the file untouched.
    #[test]
    fn a_save_takes_the_file_lock_before_the_revision_slot() {
        let directory = temporary_directory("revision-slot-order");
        let path = directory.join("config.toml");
        let mut config = default_config();
        config.window_opacity = 0.42;
        config.persistence_revision =
            std::sync::Arc::new(std::sync::Mutex::new(Some(ConfigRevision::missing())));
        let slot = std::sync::Arc::clone(&config.persistence_revision);

        let held = slot.lock().unwrap();
        let save_path = path.clone();
        let saver = std::thread::spawn(move || save_config_with_path(&save_path, &config));

        let deadline = Instant::now() + LOCK_TIMEOUT;
        while lock_status_for(&path) != ConfigLockStatus::Active {
            assert!(
                Instant::now() < deadline,
                "the save never reached the file lock, so it was still waiting on the revision slot"
            );
            std::thread::yield_now();
        }

        drop(held);
        let revision = saver
            .join()
            .unwrap()
            .expect("the save completes once the slot is free");
        assert_eq!(
            slot.lock().unwrap().as_ref(),
            Some(&revision),
            "the written revision is published back into the shared slot"
        );
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("opacity = 0.42"));
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
        config.command_history_path = Some("/tmp/forge-history.jsonl".into());
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
            Some("/tmp/forge-history.jsonl")
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
    fn tab_width_and_finished_block_expansion_limit_are_persisted() {
        let directory = temporary_directory("tab-and-block-sizing");
        let path = directory.join("config.toml");
        let mut config = default_config();
        config.tab_width = 264;
        config.finished_block_max_expanded_rows = 1_234;
        save_config_to_path(&path, &config, Some(&ConfigRevision::missing())).unwrap();
        let table = fs::read_to_string(&path)
            .unwrap()
            .parse::<toml::Table>()
            .unwrap();
        assert_eq!(
            table.get("tab_width").and_then(toml::Value::as_integer),
            Some(264)
        );
        assert_eq!(
            table
                .get("finished_block_max_expanded_rows")
                .and_then(toml::Value::as_integer),
            Some(1_234)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ascii_organism_settings_are_written_transactionally() {
        let directory = temporary_directory("ascii-organism");
        let path = directory.join("config.toml");
        let mut config = default_config();
        config.ascii_organism_enabled = true;
        config.ascii_organism_motion = Some(config::OrganismMotion::Calm);

        let mut revision =
            save_config_to_path(&path, &config, Some(&ConfigRevision::missing())).unwrap();
        let table = fs::read_to_string(&path)
            .unwrap()
            .parse::<toml::Table>()
            .unwrap();
        assert_eq!(
            table
                .get("ascii_organism_enabled")
                .and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            table
                .get("ascii_organism_motion")
                .and_then(toml::Value::as_str),
            Some("calm")
        );

        config.ascii_organism_motion = None;
        revision = save_config_to_path(&path, &config, Some(&revision)).unwrap();
        let table = fs::read_to_string(&path)
            .unwrap()
            .parse::<toml::Table>()
            .unwrap();
        assert!(!table.contains_key("ascii_organism_motion"));
        assert_eq!(revision, revision_at(&path).unwrap());
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
        config.agent_auto_approve_readonly = true;
        config.command_correction_enabled = false;
        config.ai_panel_visible = true;
        config.ai_panel_width = 840;
        config.ai_provider = "ollama".into();
        config.ai_base_url = "http://localhost:11434".into();
        config.ai_api_key_file = Some("/run/secrets/provider-api-key".into());
        config.ai_api_key_file_configured = Some("~/.config/forge/ai.key".into());
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
                .get("agent_auto_approve_readonly")
                .and_then(toml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            table
                .get("command_correction_enabled")
                .and_then(toml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            table.get("ai_panel_visible").and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            table
                .get("ai_panel_width")
                .and_then(toml::Value::as_integer),
            Some(840)
        );
        assert_eq!(
            table.get("ai_base_url").and_then(toml::Value::as_str),
            Some("http://localhost:11434")
        );
        assert_eq!(
            table.get("ai_api_key_file").and_then(toml::Value::as_str),
            Some("~/.config/forge/ai.key")
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

    #[cfg(unix)]
    #[test]
    fn unreadable_primary_backup_falls_back_to_valid_secondary() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let directory = temporary_directory("restore-unreadable-primary");
        let path = directory.join("config.toml");
        fs::write(&path, "not valid toml = [\n").unwrap();
        let primary = backup_path_for(&path);
        let primary_name = CString::new(primary.as_os_str().as_bytes()).unwrap();
        // SAFETY: primary_name is a live NUL-terminated path for this call.
        assert_eq!(
            unsafe { nix::libc::mkfifo(primary_name.as_ptr(), 0o600) },
            0
        );
        fs::write(secondary_backup_path_for(&path), "opacity = 0.7\n").unwrap();

        let (source, _) = restore_backup_to_path(&path).unwrap();
        assert_eq!(source, secondary_backup_path_for(&path));
        assert_eq!(fs::read_to_string(&path).unwrap(), "opacity = 0.7\n");
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn save_rejects_unsafe_parent_entries_without_chmod_or_write_through() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = temporary_directory("unsafe-parent");
        let victim = root.join("victim");
        fs::create_dir(&victim).unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o755)).unwrap();
        let linked = root.join("linked");
        symlink(&victim, &linked).unwrap();
        assert!(save_config_to_path(
            &linked.join("config.toml"),
            &default_config(),
            Some(&ConfigRevision::missing())
        )
        .is_err());
        assert!(!victim.join("config.toml").exists());
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o755
        );

        let writable = root.join("world-writable");
        fs::create_dir(&writable).unwrap();
        fs::set_permissions(&writable, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(save_config_to_path(
            &writable.join("config.toml"),
            &default_config(),
            Some(&ConfigRevision::missing())
        )
        .is_err());
        assert!(!writable.join("config.toml").exists());
        assert_eq!(
            fs::metadata(&writable).unwrap().permissions().mode() & 0o777,
            0o777
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// A private directory is not enough: the file's own mode decides who can
    /// rewrite the shell we spawn, the remote hosts and the agent settings.
    /// anvil has refused a group/other-writable config all along.
    #[test]
    fn a_group_or_world_writable_config_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("forge-cfg-mode-{}-{}", std::process::id(), line!()));
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("config.toml");
        fs::write(&path, "font = \"Monospace 14\"\n").unwrap();

        for mode in [0o600, 0o640, 0o644] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(
                read_private_bytes(&path, 64 * 1024).is_ok(),
                "mode {mode:o} should be readable"
            );
        }
        for mode in [0o660, 0o666, 0o622] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            let error =
                read_private_bytes(&path, 64 * 1024).expect_err("a writable mode must be refused");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rendered_config_cannot_exceed_the_reader_budget() {
        let directory = temporary_directory("render-budget");
        let path = directory.join("config.toml");
        let original = b"opacity = 0.5\n";
        fs::write(&path, original).unwrap();
        let expected = revision_at(&path).unwrap();
        let mut config = default_config();
        config.ai_model = "x".repeat(MAX_CONFIG_BYTES as usize);

        assert!(save_config_to_path(&path, &config, Some(&expected)).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!backup_path_for(&path).exists());
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

    fn sample_ssh_host() -> config::RemoteHost {
        config::RemoteHost {
            name: "dev".into(),
            host: "dev.example.com".into(),
            user: Some("alice".into()),
            docker: false,
            deploy_artifact: None,
            remote_shell: "jsh".into(),
            session: None,
            ssh_args: vec!["-p".into(), "2222".into()],
            login_shell: true,
            multiplex: true,
            deploy: jterm_core::jsh_remote::Deploy::Persist,
        }
    }

    fn sample_docker_host() -> config::RemoteHost {
        config::RemoteHost {
            name: "build container".into(),
            host: "my-service".into(),
            user: Some("devuser".into()),
            docker: true,
            deploy_artifact: None,
            remote_shell: "jsh".into(),
            session: None,
            ssh_args: Vec::new(),
            login_shell: true,
            multiplex: true,
            deploy: jterm_core::jsh_remote::Deploy::Off,
        }
    }

    #[test]
    fn remote_hosts_added_in_memory_reach_a_file_that_had_none() {
        let directory = temporary_directory("remote-hosts-add");
        let path = directory.join("config.toml");
        fs::write(&path, "opacity = 0.5\n").unwrap();
        let expected = revision_at(&path).unwrap();
        let mut config = default_config();
        config.remote_hosts = vec![sample_ssh_host(), sample_docker_host()];
        save_config_to_path(&path, &config, Some(&expected)).unwrap();
        let table = fs::read_to_string(&path)
            .unwrap()
            .parse::<toml::Table>()
            .unwrap();
        assert_eq!(config::parse_remote_hosts(&table), config.remote_hosts);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn emptied_remote_hosts_list_deletes_the_on_disk_entries() {
        let directory = temporary_directory("remote-hosts-delete");
        let path = directory.join("config.toml");
        fs::write(
            &path,
            "[[remote_hosts]]\nname = \"old\"\nhost = \"old.example.com\"\n",
        )
        .unwrap();
        let expected = revision_at(&path).unwrap();
        let mut config = default_config();
        config.remote_hosts = Vec::new();
        save_config_to_path(&path, &config, Some(&expected)).unwrap();
        let table = fs::read_to_string(&path)
            .unwrap()
            .parse::<toml::Table>()
            .unwrap();
        assert_eq!(
            table
                .get("remote_hosts")
                .and_then(toml::Value::as_array)
                .map(Vec::len),
            Some(0)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn fresh_file_with_no_hosts_gains_no_remote_hosts_key() {
        let directory = temporary_directory("remote-hosts-fresh");
        let path = directory.join("config.toml");
        let mut config = default_config();
        config.remote_hosts = Vec::new();
        save_config_to_path(&path, &config, Some(&ConfigRevision::missing())).unwrap();
        let table = fs::read_to_string(&path)
            .unwrap()
            .parse::<toml::Table>()
            .unwrap();
        assert!(!table.contains_key("remote_hosts"));
        fs::remove_dir_all(directory).unwrap();
    }
}
