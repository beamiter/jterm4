//! Coalesced background Git metadata for the active-pane strip.
//!
//! Callers run on the GTK main thread, so they never execute Git directly. A
//! single worker owns all `git status --porcelain=v2 --branch` processes,
//! coalesces concurrent requests for the same directory, and caches the latest
//! result. Slow repositories return a stale value after a short UI wait while
//! the refresh continues in the background.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const GIT_STATUS_TIMEOUT: Duration = Duration::from_millis(500);
const GIT_WAIT_POLL: Duration = Duration::from_millis(5);
/// Keep first paint responsive on slow disks, remote mounts, and large repos.
const UI_WAIT_BUDGET: Duration = Duration::from_millis(12);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoMeta {
    /// Short branch name, or detached-HEAD short sha.
    pub branch: String,
    /// True if there are any uncommitted changes (tracked or untracked).
    pub dirty: bool,
    /// Commits on local branch not yet on upstream. None if no upstream.
    pub ahead: Option<u32>,
    /// Commits on upstream not yet locally. None if no upstream.
    pub behind: Option<u32>,
}

type ProbeResult = Option<RepoMeta>;
type ReplySender = mpsc::SyncSender<ProbeResult>;

/// One worker serializes probes and prevents a process storm when several panes
/// finish output together. Requests for the same cwd share one in-flight probe.
struct GitMetaService {
    request_tx: mpsc::Sender<PathBuf>,
    cache: Arc<Mutex<HashMap<PathBuf, ProbeResult>>>,
    pending: Arc<Mutex<HashMap<PathBuf, Vec<ReplySender>>>>,
}

impl GitMetaService {
    fn new() -> Option<Self> {
        let (request_tx, request_rx) = mpsc::channel::<PathBuf>();
        let cache = Arc::new(Mutex::new(HashMap::new()));
        let pending = Arc::new(Mutex::new(HashMap::<PathBuf, Vec<ReplySender>>::new()));

        let cache_worker = cache.clone();
        let pending_worker = pending.clone();
        thread::Builder::new()
            .name("jterm4-git-meta-worker".to_string())
            .spawn(move || worker_loop(request_rx, &cache_worker, &pending_worker))
            .ok()?;

        Some(Self {
            request_tx,
            cache,
            pending,
        })
    }

    fn cached(&self, path: &Path) -> Option<ProbeResult> {
        self.cache.lock().ok()?.get(path).cloned()
    }

    fn request(&self, path: &Path) -> Option<mpsc::Receiver<ProbeResult>> {
        let path = path.to_path_buf();
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);

        {
            let mut pending = self.pending.lock().ok()?;
            if let Some(waiters) = pending.get_mut(&path) {
                waiters.push(reply_tx);
                return Some(reply_rx);
            }
            pending.insert(path.clone(), vec![reply_tx]);
        }

        if self.request_tx.send(path.clone()).is_err() {
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&path);
            }
            return None;
        }
        Some(reply_rx)
    }
}

fn service() -> Option<&'static GitMetaService> {
    static SERVICE: OnceLock<Option<GitMetaService>> = OnceLock::new();
    SERVICE.get_or_init(GitMetaService::new).as_ref()
}

fn worker_loop(
    request_rx: mpsc::Receiver<PathBuf>,
    cache: &Mutex<HashMap<PathBuf, ProbeResult>>,
    pending: &Mutex<HashMap<PathBuf, Vec<ReplySender>>>,
) {
    for path in request_rx {
        let result = read_uncached(&path);

        if let Ok(mut cache) = cache.lock() {
            cache.insert(path.clone(), result.clone());
        }

        let waiters = pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&path))
            .unwrap_or_default();
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
    }
}

/// Resolve repo metadata without running Git on the caller's thread.
///
/// A healthy local repository usually completes inside `UI_WAIT_BUDGET`. A slow
/// refresh returns the cached value while the worker continues in the background.
pub fn read(cwd: &Path) -> Option<RepoMeta> {
    if !cwd.is_dir() {
        return None;
    }

    let service = service()?;
    let stale = service.cached(cwd).flatten();
    let Some(reply) = service.request(cwd) else {
        return stale;
    };

    match reply.recv_timeout(UI_WAIT_BUDGET) {
        Ok(fresh) => fresh,
        Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => stale,
    }
}

fn read_uncached(cwd: &Path) -> ProbeResult {
    let output = run_git_status(cwd)?;
    parse_porcelain_v2(&output)
}

/// Parse stable porcelain-v2 branch headers and worktree records.
fn parse_porcelain_v2(output: &str) -> Option<RepoMeta> {
    let mut oid: Option<&str> = None;
    let mut head: Option<&str> = None;
    let mut ahead_behind: Option<(u32, u32)> = None;
    let mut dirty = false;

    for line in output.lines() {
        if let Some(value) = line.strip_prefix("# branch.oid ") {
            oid = Some(value.trim());
            continue;
        }
        if let Some(value) = line.strip_prefix("# branch.head ") {
            head = Some(value.trim());
            continue;
        }
        if let Some(value) = line.strip_prefix("# branch.ab ") {
            ahead_behind = parse_ahead_behind(value);
            continue;
        }

        if matches!(line.as_bytes().first(), Some(b'1' | b'2' | b'u' | b'?')) {
            dirty = true;
        }
    }

    let head = head?;
    let branch = if head == "(detached)" {
        let oid = oid.filter(|value| *value != "(initial)")?;
        format!("({})", &oid[..oid.len().min(7)])
    } else {
        head.to_string()
    };
    let (ahead, behind) = match ahead_behind {
        Some((ahead, behind)) => (Some(ahead), Some(behind)),
        None => (None, None),
    };

    Some(RepoMeta {
        branch,
        dirty,
        ahead,
        behind,
    })
}

fn parse_ahead_behind(value: &str) -> Option<(u32, u32)> {
    let mut fields = value.split_whitespace();
    let ahead = fields.next()?.strip_prefix('+')?.parse().ok()?;
    let behind = fields.next()?.strip_prefix('-')?.parse().ok()?;
    Some((ahead, behind))
}

/// Run one bounded Git process. A helper drains stdout so a very dirty worktree
/// cannot fill the pipe and deadlock before the child exits.
fn run_git_status(cwd: &Path) -> Option<String> {
    let mut command = crate::host::command_with_cwd("git", cwd);
    let mut child = command
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let reader = match thread::Builder::new()
        .name("jterm4-git-status-reader".to_string())
        .spawn(move || {
            let mut output = String::new();
            stdout.read_to_string(&mut output).ok()?;
            Some(output)
        }) {
        Ok(reader) => reader,
        Err(_) => {
            terminate_child(&mut child);
            return None;
        }
    };

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < GIT_STATUS_TIMEOUT => thread::sleep(GIT_WAIT_POLL),
            Ok(None) | Err(_) => {
                terminate_child(&mut child);
                let _ = reader.join();
                return None;
            }
        }
    };

    let output = reader.join().ok().flatten()?;
    status.success().then_some(output)
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Format a RepoMeta into the compact status-strip text.
pub fn format_strip(meta: &RepoMeta) -> String {
    let mut s = String::new();
    s.push_str(&meta.branch);
    if meta.dirty {
        s.push_str(" ●");
    }
    match (meta.ahead, meta.behind) {
        (Some(a), Some(b)) if a > 0 || b > 0 => {
            s.push_str("  ");
            if a > 0 {
                s.push_str(&format!("↑{a} "));
            }
            if b > 0 {
                s.push_str(&format!("↓{b}"));
            }
        }
        _ => {}
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_branch_with_upstream() {
        let output = concat!(
            "# branch.oid 0123456789abcdef\n",
            "# branch.head feature/cache\n",
            "# branch.upstream origin/feature/cache\n",
            "# branch.ab +2 -1\n",
        );
        assert_eq!(
            parse_porcelain_v2(output),
            Some(RepoMeta {
                branch: "feature/cache".into(),
                dirty: false,
                ahead: Some(2),
                behind: Some(1),
            })
        );
    }

    #[test]
    fn parses_dirty_records_and_no_upstream() {
        let output = concat!(
            "# branch.oid 0123456789abcdef\n",
            "# branch.head main\n",
            "1 .M N... 100644 100644 100644 abc abc src/main.rs\n",
            "? scratch.txt\n",
        );
        assert_eq!(
            parse_porcelain_v2(output),
            Some(RepoMeta {
                branch: "main".into(),
                dirty: true,
                ahead: None,
                behind: None,
            })
        );
    }

    #[test]
    fn parses_detached_head_as_short_oid() {
        let output = concat!(
            "# branch.oid 89abcdef01234567\n",
            "# branch.head (detached)\n",
        );
        assert_eq!(
            parse_porcelain_v2(output).map(|meta| meta.branch),
            Some("(89abcde)".into())
        );
    }

    #[test]
    fn rejects_missing_or_initial_detached_oid() {
        assert_eq!(parse_porcelain_v2("# branch.oid abc\n"), None);
        assert_eq!(
            parse_porcelain_v2("# branch.oid (initial)\n# branch.head (detached)\n"),
            None
        );
    }

    #[test]
    fn coalesces_waiters_for_the_same_path() {
        let (request_tx, request_rx) = mpsc::channel();
        let service = GitMetaService {
            request_tx,
            cache: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
        };
        let path = Path::new("/tmp/repo");

        let _first = service.request(path).expect("first request");
        let _second = service.request(path).expect("coalesced request");

        assert_eq!(request_rx.try_iter().count(), 1);
        assert_eq!(
            service.pending.lock().unwrap().get(path).map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn format_strip_clean_no_upstream() {
        let meta = RepoMeta {
            branch: "main".into(),
            dirty: false,
            ahead: None,
            behind: None,
        };
        assert_eq!(format_strip(&meta), "main");
    }

    #[test]
    fn format_strip_dirty_marker() {
        let meta = RepoMeta {
            branch: "feature/x".into(),
            dirty: true,
            ahead: None,
            behind: None,
        };
        assert_eq!(format_strip(&meta), "feature/x ●");
    }

    #[test]
    fn format_strip_ahead_behind() {
        let meta = RepoMeta {
            branch: "main".into(),
            dirty: false,
            ahead: Some(2),
            behind: Some(1),
        };
        assert_eq!(format_strip(&meta), "main  ↑2 ↓1");
    }

    #[test]
    fn format_strip_ahead_only() {
        let meta = RepoMeta {
            branch: "main".into(),
            dirty: true,
            ahead: Some(3),
            behind: Some(0),
        };
        assert_eq!(format_strip(&meta), "main ●  ↑3 ");
    }

    #[test]
    fn format_strip_zero_zero_hidden() {
        let meta = RepoMeta {
            branch: "main".into(),
            dirty: false,
            ahead: Some(0),
            behind: Some(0),
        };
        assert_eq!(format_strip(&meta), "main");
    }
}
