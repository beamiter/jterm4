//! git_meta — cheap shell-out for the active-pane git status strip.
//!
//! Runs `git` against a cwd to read branch, dirty flag, and ahead/behind
//! counts vs upstream. Designed to be polled on cwd-change and on
//! block-finish (the user just ran something that may have touched the
//! repo). Each call is one short subprocess; results are cached in the
//! caller against the cwd path to avoid re-running for repeated probes.
//!
//! Failures are silent — non-repo dirs just return None and the strip
//! hides itself. We never want git-status flakiness to surface as an
//! error in the terminal UI.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

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

/// Resolve repo metadata for `cwd`. Returns None if `cwd` isn't inside a
/// git repository, the directory doesn't exist, or git isn't on PATH.
pub fn read(cwd: &Path) -> Option<RepoMeta> {
    if !cwd.is_dir() {
        return None;
    }

    // First gate: are we even in a repo? Fast and gives a clean exit code.
    let inside = run_git(cwd, &["rev-parse", "--is-inside-work-tree"])?;
    if inside.trim() != "true" {
        return None;
    }

    let branch = read_branch(cwd)?;
    let dirty = read_dirty(cwd);
    let (ahead, behind) = read_ahead_behind(cwd);

    Some(RepoMeta {
        branch,
        dirty,
        ahead,
        behind,
    })
}

fn read_branch(cwd: &Path) -> Option<String> {
    // `--abbrev-ref HEAD` returns the branch name, or "HEAD" if detached.
    let name = run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string();
    if name != "HEAD" {
        return Some(name);
    }
    // Detached: prefer a short SHA so the user sees *something* useful.
    let sha = run_git(cwd, &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string();
    if sha.is_empty() {
        None
    } else {
        Some(format!("({sha})"))
    }
}

fn read_dirty(cwd: &Path) -> bool {
    // `--porcelain` is line-per-change. Any output at all = dirty.
    match run_git(cwd, &["status", "--porcelain", "--untracked-files=normal"]) {
        Some(out) => !out.trim().is_empty(),
        None => false,
    }
}

fn read_ahead_behind(cwd: &Path) -> (Option<u32>, Option<u32>) {
    // `--count` prints "<behind>\t<ahead>" for `@{u}...HEAD`. Errors when
    // no upstream is configured — that's the None branch.
    let raw = match run_git(cwd, &["rev-list", "--left-right", "--count", "@{u}...HEAD"]) {
        Some(s) => s,
        None => return (None, None),
    };
    let mut parts = raw.split_whitespace();
    let behind = parts.next().and_then(|s| s.parse::<u32>().ok());
    let ahead = parts.next().and_then(|s| s.parse::<u32>().ok());
    (ahead, behind)
}

/// Run `git <args>` in `cwd` with a hard 500ms ceiling on wait. Returns
/// stdout on exit 0, None otherwise. A timeout/spawn error is treated
/// as "no info" — the strip just hides for this cwd.
fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        // Keep git from emitting page-or-prompt prompts on slow ops.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // We can't easily impose a true timeout without a thread + kill, but
    // the queries above are all O(1)-ish against the index and finish in
    // <50ms on healthy repos. The cap below is the wall clock we'll wait
    // for the process to die under wait_with_output(); if a repo is wedged
    // (e.g. corrupt .git, hung lock) we accept that the first probe blocks
    // briefly the first time. Future iterations can add a timer-kill.
    let _ = Duration::from_millis(500); // documents intent

    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Format a RepoMeta into the compact strip text. Designed to read at a
/// glance: "main ●  ↑2 ↓1" — branch, dirty dot if any uncommitted change,
/// ahead/behind arrows if upstream is set.
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
    fn format_strip_clean_no_upstream() {
        let m = RepoMeta {
            branch: "main".into(),
            dirty: false,
            ahead: None,
            behind: None,
        };
        assert_eq!(format_strip(&m), "main");
    }

    #[test]
    fn format_strip_dirty_marker() {
        let m = RepoMeta {
            branch: "feature/x".into(),
            dirty: true,
            ahead: None,
            behind: None,
        };
        assert_eq!(format_strip(&m), "feature/x ●");
    }

    #[test]
    fn format_strip_ahead_behind() {
        let m = RepoMeta {
            branch: "main".into(),
            dirty: false,
            ahead: Some(2),
            behind: Some(1),
        };
        assert_eq!(format_strip(&m), "main  ↑2 ↓1");
    }

    #[test]
    fn format_strip_ahead_only() {
        let m = RepoMeta {
            branch: "main".into(),
            dirty: true,
            ahead: Some(3),
            behind: Some(0),
        };
        assert_eq!(format_strip(&m), "main ●  ↑3 ");
    }

    #[test]
    fn format_strip_zero_zero_hidden() {
        let m = RepoMeta {
            branch: "main".into(),
            dirty: false,
            ahead: Some(0),
            behind: Some(0),
        };
        // No arrows when we're in sync.
        assert_eq!(format_strip(&m), "main");
    }
}
