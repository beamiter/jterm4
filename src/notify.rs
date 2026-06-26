//! notify — fire-and-forget desktop notification for long-running blocks.
//!
//! Shells out to `notify-send` rather than wiring `gio::Notification`. The
//! TermView block-finished callback runs without a window/application
//! handle in scope (would require threading one through `TermView::new`),
//! and notify-send is universally available on Linux desktops (libnotify
//! is a near-mandatory dep of every major DE). The subprocess cost is one
//! fork+exec per long-running command — negligible compared to whatever
//! the command itself just spent doing.
//!
//! Errors are intentionally swallowed: if notify-send is missing or
//! D-Bus is broken, the user shouldn't see a stack trace from a feature
//! that's meant to be unobtrusive.

use std::process::{Command, Stdio};

/// Post a desktop notification for a command that just finished. `cmd` is
/// the displayed command (truncated to keep the toast readable);
/// `exit_code` drives the urgency hint (non-zero → critical, since failed
/// long builds are the case users most want to come back to).
///
/// `duration_ms` shows up in the body so the user knows whether they
/// have time to refill their coffee.
pub fn long_block_finished(cmd: &str, exit_code: i32, duration_ms: u64) {
    // Truncate the cmd so the notification title stays one line.
    let title_cmd = cmd.lines().next().unwrap_or(cmd);
    let title_cmd = if title_cmd.len() > 60 {
        let mut s = title_cmd[..60].to_string();
        s.push('…');
        s
    } else {
        title_cmd.to_string()
    };

    let status = if exit_code == 0 { "✓" } else { "✗" };
    let title = format!("{status} {title_cmd}");
    let body = format!("Exit {exit_code} after {}", humanize_duration(duration_ms));

    let urgency = if exit_code == 0 { "normal" } else { "critical" };

    // -t 0 = sticky-until-dismissed by some servers; -t 8000 = 8s. We pick a
    // mid value (5s) so success toasts decay quickly but failures still get
    // a moment of attention.
    let timeout_ms = if exit_code == 0 { "5000" } else { "10000" };

    let _ = Command::new("notify-send")
        .args([
            "--app-name=jterm4",
            "--icon=utilities-terminal",
            "--urgency",
            urgency,
            "--expire-time",
            timeout_ms,
            "--",
            &title,
            &body,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Render a millisecond count as a short human string. Used in the
/// notification body so "exit 0 after 12m 4s" reads naturally instead of
/// "exit 0 after 724000ms".
fn humanize_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m {s}s")
        }
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_seconds_only() {
        assert_eq!(humanize_duration(0), "0s");
        assert_eq!(humanize_duration(7_500), "7s");
        assert_eq!(humanize_duration(59_999), "59s");
    }

    #[test]
    fn humanize_minutes_round() {
        assert_eq!(humanize_duration(60_000), "1m");
        assert_eq!(humanize_duration(120_000), "2m");
    }

    #[test]
    fn humanize_minutes_and_seconds() {
        assert_eq!(humanize_duration(125_000), "2m 5s");
        assert_eq!(humanize_duration(3_599_000), "59m 59s");
    }

    #[test]
    fn humanize_hours() {
        assert_eq!(humanize_duration(3_600_000), "1h");
        assert_eq!(humanize_duration(3_660_000), "1h 1m");
        assert_eq!(humanize_duration(7_200_000), "2h");
    }
}
