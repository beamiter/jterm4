//! alt — extracted from block_view (mechanical split, no logic changes)
use gtk4::gdk::RGBA;
use gtk4::pango::FontDescription;
use gtk4::prelude::*;
use gtk4::{glib, ScrolledWindow};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::{CursorBlinkMode, CursorShape, Terminal};
use vte4::{TerminalExt, TerminalExtManual};
use crate::config::Config;
use super::*;



// ─── Mouse Reporting Mode ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(Default)]
pub(crate) enum MouseReportingMode {
    /// No mouse reporting (CSI ?1000l, etc.)
    #[default]
    None,
    /// Basic click reporting (CSI ?1000h)
    Click,
    /// Button press/release/drag (CSI ?1002h)
    Button,
    /// All mouse motion (CSI ?1003h)
    Motion,
    /// SGR-style reporting (CSI ?1006h) - modern format
    Sgr,
}

pub(crate) fn is_alt_screen_mode(params: &[u8]) -> bool {
    matches!(params, b"?47" | b"?1047" | b"?1049")
}

pub(crate) fn is_application_cursor_mode(params: &[u8]) -> bool {
    params == b"?1"
}

/// Private modes that only a full-screen / interactive TUI sets, and that
/// line-oriented progress output (git, npm, cargo) never does. Modern TUIs such
/// as the Claude CLI never enter the alt-screen and never use absolute cursor
/// positioning — they repaint in place — so the alt-screen / app-cursor checks
/// miss them. These extra DECSET modes are the reliable tell:
///   ?2026 — synchronized output (atomic frame begin)
///   ?1004 — focus-event reporting
///   ?2031 — color-scheme / theme change notifications
pub(crate) fn is_interactive_app_mode(params: &[u8]) -> bool {
    matches!(params, b"?2026" | b"?1004" | b"?2031")
}

pub(crate) fn contains_interactive_screen_enter(bytes: &[u8]) -> bool {
    let mut i = 0;

    while i + 1 < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }

        match bytes[i + 1] {
            b'[' => {
                i += 2;
                let params_start = i;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                if i >= bytes.len() {
                    break;
                }

                let final_byte = bytes[i];
                let params = &bytes[params_start..i];
                i += 1;

                if final_byte == b'h'
                    && (is_alt_screen_mode(params)
                        || is_application_cursor_mode(params)
                        || is_interactive_app_mode(params))
                {
                    return true;
                }
            }
            _ => {
                i = skip_escape_sequence(bytes, i);
            }
        }
    }

    false
}

/// Detect a CSI sequence that repaints the screen using absolute positioning:
/// cursor-position (`H`/`f`) or erase-display (`J`). Full-screen TUIs (vim,
/// htop, Claude CLI) use these; line-oriented progress output (git, npm, cargo)
/// only uses `\r` + erase-line (`K`) + cursor-up (`A`), so it does NOT match.
/// This is the signal that separates a real TUI from a cursor-hiding progress
/// bar — see [`contains_cursor_hide`].
pub(crate) fn contains_full_screen_redraw(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        if bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            if matches!(bytes[i], b'H' | b'f' | b'J') {
                return true;
            }
            i += 1;
        } else {
            i = skip_escape_sequence(bytes, i);
        }
    }
    false
}

/// True if the byte stream contains `ESC[?25l` (hide cursor).
pub(crate) fn contains_cursor_hide(bytes: &[u8]) -> bool {
    contains_private_mode(bytes, b'l')
}

/// True if the byte stream contains `ESC[?25h` (show cursor).
pub(crate) fn contains_cursor_show(bytes: &[u8]) -> bool {
    contains_private_mode(bytes, b'h')
}

fn contains_private_mode(bytes: &[u8], want_final: u8) -> bool {
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        if bytes[i + 1] == b'[' {
            i += 2;
            let params_start = i;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            if bytes[i] == want_final && &bytes[params_start..i] == b"?25" {
                return true;
            }
            i += 1;
        } else {
            i = skip_escape_sequence(bytes, i);
        }
    }
    false
}

/// Detect a readline reverse/forward incremental-search prompt in already
/// ANSI-stripped text. Ctrl-R/Ctrl-S put the shell into a search mode whose
/// echoed line bears no relation to the normal prompt, so the custom
/// command-line renderer cannot parse it. When this returns true we route the
/// raw bytes to the output VTE instead (same fallback as the completion menu).
///
/// Covers bash/readline `(reverse-i-search)`/`(i-search)`/`(failed ...)` and
/// zsh `bck-i-search:`/`fwd-i-search:` forms.
pub(crate) fn detect_isearch_marker(stripped: &str) -> bool {
    stripped.contains("reverse-i-search")
        || stripped.contains("i-search)`")
        || stripped.contains("bck-i-search:")
        || stripped.contains("fwd-i-search:")
        || stripped.contains("failed reverse-i-search")
        || stripped.contains("failed i-search")
}

#[cfg(test)]
mod isearch_tests {
    use super::detect_isearch_marker;

    #[test]
    pub(crate) fn detects_bash_reverse_isearch() {
        assert!(detect_isearch_marker("(reverse-i-search)`gi': git status"));
    }

    #[test]
    fn detects_bash_forward_isearch() {
        assert!(detect_isearch_marker("(i-search)`gi': git status"));
    }

    #[test]
    fn detects_failed_reverse_isearch() {
        assert!(detect_isearch_marker("(failed reverse-i-search)`zzz': "));
    }

    #[test]
    fn detects_zsh_bck_isearch() {
        assert!(detect_isearch_marker("bck-i-search: git status"));
    }

    #[test]
    fn detects_zsh_fwd_isearch() {
        assert!(detect_isearch_marker("fwd-i-search: git status"));
    }

    #[test]
    fn ignores_normal_prompt() {
        assert!(!detect_isearch_marker("user@host ~/projects ❯ git status"));
    }

    #[test]
    fn ignores_command_containing_search_word() {
        assert!(!detect_isearch_marker("❯ grep -r search ."));
    }
}

#[cfg(test)]
mod interactive_screen_tests {
    use super::contains_interactive_screen_enter;

    #[test]
    pub(crate) fn detects_alt_screen_enter() {
        assert!(contains_interactive_screen_enter(b"\x1b[?1049h"));
    }

    #[test]
    fn detects_less_application_cursor_enter() {
        assert!(contains_interactive_screen_enter(b"\x1b[?1h\x1b="));
    }

    #[test]
    fn detects_modern_tui_private_modes() {
        // The Claude CLI never enters the alt-screen; it announces itself with
        // synchronized-output / focus / theme DECSET modes instead.
        assert!(contains_interactive_screen_enter(b"\x1b[?2026h"));
        assert!(contains_interactive_screen_enter(b"\x1b[?1004h"));
        assert!(contains_interactive_screen_enter(b"\x1b[?2031h"));
        // Claude's actual startup preamble.
        assert!(contains_interactive_screen_enter(
            b"\x1b7\x1b[r\x1b8\x1b[?25h\x1b[?25l\x1b[?2004h\x1b[?1004h\x1b[?2031h\x1b[?2026h"
        ));
    }

    #[test]
    fn cursor_hide_alone_is_not_interactive() {
        // A bare cursor-hide is ambiguous (git/npm/cargo progress all hide the
        // cursor). It must be paired with a full-screen redraw — handled
        // statefully by the caller — so it does NOT trigger on its own.
        assert!(!contains_interactive_screen_enter(b"\x1b[?25l"));
    }

    #[test]
    fn ignores_cursor_show() {
        assert!(!contains_interactive_screen_enter(b"\x1b[?25h"));
    }

    #[test]
    fn ignores_leave_sequences() {
        assert!(!contains_interactive_screen_enter(b"\x1b[?1049l\x1b[?1l\x1b>"));
    }
}

#[cfg(test)]
mod cursor_and_redraw_tests {
    use super::{contains_cursor_hide, contains_cursor_show, contains_full_screen_redraw};

    #[test]
    fn detects_cursor_hide_and_show() {
        assert!(contains_cursor_hide(b"\x1b[?25l"));
        assert!(contains_cursor_show(b"\x1b[?25h"));
        assert!(!contains_cursor_hide(b"\x1b[?25h"));
        assert!(!contains_cursor_show(b"\x1b[?25l"));
    }

    #[test]
    fn full_screen_redraw_detects_positioning_and_erase_display() {
        assert!(contains_full_screen_redraw(b"\x1b[2J"));
        assert!(contains_full_screen_redraw(b"\x1b[H"));
        assert!(contains_full_screen_redraw(b"\x1b[10;5H"));
        assert!(contains_full_screen_redraw(b"\x1b[3;1f"));
    }

    #[test]
    fn git_progress_is_not_a_full_screen_redraw() {
        // git push: hide cursor, rewrite a line with \r + erase-to-line-end (K),
        // optional cursor-up (A). None of these are full-screen positioning.
        let git = b"\x1b[?25lCompressing objects:  50% (10/20)\r\x1b[KCompressing objects: 100% (20/20), done.\n";
        assert!(contains_cursor_hide(git));
        assert!(!contains_full_screen_redraw(git));
        assert!(!contains_full_screen_redraw(b"\x1b[A\x1b[K"));
    }
}

#[cfg(test)]
mod alt_screen_pty_size_tests {
    use super::alt_screen_pty_size;

    // Regression guard: the PTY must be sized from VTE's OWN grid
    // (column_count/row_count), passed through unchanged. If someone reverts to
    // pixel math (vte_w / char_w), the grid values would no longer pass through
    // and these assertions would fail. A mismatch between PTY size and the grid
    // VTE renders corrupts box-drawing characters on sidebar toggle.
    #[test]
    pub(crate) fn passes_grid_dimensions_through_unchanged() {
        assert_eq!(alt_screen_pty_size(212, 50), Some((212, 50)));
        assert_eq!(alt_screen_pty_size(80, 24), Some((80, 24)));
    }

    #[test]
    fn returns_none_when_grid_not_ready() {
        assert_eq!(alt_screen_pty_size(0, 24), None);
        assert_eq!(alt_screen_pty_size(80, 0), None);
        assert_eq!(alt_screen_pty_size(0, 0), None);
        assert_eq!(alt_screen_pty_size(-1, 24), None);
        assert_eq!(alt_screen_pty_size(80, -5), None);
    }
}

#[cfg(test)]
mod pager_snapshot_tests {
    use super::{merge_pager_snapshots, normalize_pager_snapshot};

    #[test]
    pub(crate) fn filters_less_status_lines() {
        let snapshot = normalize_pager_snapshot("\ncommit a\nAuthor: me\n:\n");
        assert_eq!(snapshot, "commit a\nAuthor: me");
    }

    #[test]
    fn merges_viewed_pages_by_overlap() {
        let merged = merge_pager_snapshots(vec![
            "commit a\nAuthor: me\nDate: today\n\n    first".to_string(),
            "Date: today\n\n    first\ncommit b\nAuthor: you".to_string(),
            "commit b\nAuthor: you\nDate: yesterday".to_string(),
        ]);

        assert_eq!(
            merged,
            "commit a\nAuthor: me\nDate: today\n\n    first\ncommit b\nAuthor: you\nDate: yesterday"
        );
    }

    #[test]
    fn skips_duplicate_pages() {
        let merged = merge_pager_snapshots(vec![
            "commit a\nAuthor: me".to_string(),
            "commit a\nAuthor: me".to_string(),
        ]);

        assert_eq!(merged, "commit a\nAuthor: me");
    }
}

/// Decide the PTY size for the alt-screen VTE from VTE's OWN grid
/// (`column_count`/`row_count`) — never from pixel math (vte_w / char_w).
///
/// VTE derives its grid from its allocation minus internal padding/border with
/// its own rounding, so a pixel-derived count can disagree by a column or two.
/// Feeding the PTY a size that differs from the grid VTE actually renders makes
/// the child (e.g. Claude Code) draw box-border lines at the wrong width, which
/// wrap and corrupt the box-drawing characters when the sidebar toggles.
///
/// Returns `None` when the grid is not ready yet (non-positive dimensions).
pub(crate) fn alt_screen_pty_size(grid_cols: i64, grid_rows: i64) -> Option<(u16, u16)> {
    if grid_cols <= 0 || grid_rows <= 0 {
        return None;
    }
    Some((grid_cols as u16, grid_rows as u16))
}

pub(crate) fn show_alt_screen(
    block_scroll: &ScrolledWindow,
    vte_box: &gtk4::Box,
    vte: &Terminal,
    initial_bytes: Option<&[u8]>,
) {
    block_scroll.set_visible(false);
    block_scroll.set_vexpand(false);
    vte_box.set_vexpand(true);
    vte_box.set_visible(true);

    // The PTY is resized to match the VTE grid by the root tick callback once
    // the freshly-shown VTE has been allocated. We deliberately do NOT resize
    // here: tick callbacks run in the frame clock's UPDATE phase, before the
    // LAYOUT phase that allocates the just-shown VTE, so column_count/row_count
    // are still 0 (first entry) or stale (re-entry) at this point.

    if let Some(bytes) = initial_bytes {
        vte.feed(bytes);
    }

    vte.grab_focus();
}

pub(crate) fn hide_alt_screen(block_scroll: &ScrolledWindow, vte_box: &gtk4::Box) {
    block_scroll.set_vexpand(true);
    block_scroll.set_visible(true);
    // Grab focus on block_scroll BEFORE hiding vte_box.  When vte_box is
    // hidden GTK moves focus automatically; if the sidebar is visible it can
    // land there, leaving the terminal unable to receive key events.  By
    // grabbing first we prevent the unwanted focus migration entirely.
    block_scroll.grab_focus();
    vte_box.set_visible(false);
    vte_box.set_vexpand(false);
}

pub(crate) fn visible_vte_text(vte: &Terminal) -> String {
    let rows = vte.row_count();
    let cols = vte.column_count();
    if rows <= 0 || cols <= 0 {
        return String::new();
    }

    let (text, _) = vte.text_range_format(
        vte4::Format::Text,
        0,
        0,
        rows.saturating_sub(1),
        cols,
    );

    text.map(|s| s.to_string()).unwrap_or_default()
}

pub(crate) fn normalize_pager_snapshot(text: &str) -> String {
    let lines: Vec<String> = text
        .lines()
        .map(|line| line.trim_end().to_string())
        .filter(|line| !is_pager_chrome_line(line))
        .collect();

    if lines
        .iter()
        .any(|line| line.trim().contains("...skipping..."))
    {
        return String::new();
    }

    let first = lines.iter().position(|line| !line.trim().is_empty());
    let last = lines.iter().rposition(|line| !line.trim().is_empty());

    match (first, last) {
        (Some(start), Some(end)) if start <= end => lines[start..=end].join("\n"),
        _ => String::new(),
    }
}

pub(crate) fn is_pager_chrome_line(line: &str) -> bool {
    matches!(line.trim(), ":" | "(END)" | "END")
}

pub(crate) fn overlap_line_count(existing: &[String], next: &[String]) -> usize {
    let max_overlap = existing.len().min(next.len());
    for count in (1..=max_overlap).rev() {
        if existing[existing.len() - count..] == next[..count]
            && (count > 1 || !existing[existing.len() - count].trim().is_empty()) {
                return count;
            }
    }
    0
}

pub(crate) fn merge_pager_snapshots(pages: Vec<String>) -> String {
    let mut merged: Vec<String> = Vec::new();

    for page in pages {
        let page_lines: Vec<String> = page.lines().map(|line| line.to_string()).collect();
        if page_lines.is_empty() {
            continue;
        }

        if merged.is_empty() {
            merged = page_lines;
            continue;
        }

        if merged
            .windows(page_lines.len())
            .any(|window| window == page_lines.as_slice())
        {
            continue;
        }

        let overlap = overlap_line_count(&merged, &page_lines);
        merged.extend(page_lines.into_iter().skip(overlap));
    }

    merged.join("\n")
}

pub(crate) fn contains_clear_screen(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            let mut params = Vec::new();
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                params.push(bytes[i]);
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'J' {
                // CSI 2J or CSI 3J = clear screen
                if params == b"2" || params == b"3" {
                    return true;
                }
            }
        } else {
            i += 1;
        }
    }
    false
}

pub(crate) fn record_pager_snapshot(vte: &Terminal, snapshots: &Rc<RefCell<Vec<String>>>) {
    let raw_text = visible_vte_text(vte);
    log::debug!("record_pager_snapshot: raw_text len={}, first 100 chars: {:?}",
               raw_text.len(), raw_text.chars().take(100).collect::<String>());

    let snapshot = normalize_pager_snapshot(&raw_text);
    if snapshot.is_empty() {
        log::debug!("record_pager_snapshot: snapshot is empty after normalization");
        return;
    }

    let mut snapshots = snapshots.borrow_mut();
    if snapshots.last().map(|last| last == &snapshot).unwrap_or(false) {
        log::debug!("record_pager_snapshot: snapshot is duplicate, skipping");
        return;
    }
    log::debug!("record_pager_snapshot: adding snapshot #{}, len={}", snapshots.len() + 1, snapshot.len());
    snapshots.push(snapshot);
}

pub(crate) fn schedule_pager_snapshot(
    vte: &Terminal,
    snapshots: &Rc<RefCell<Vec<String>>>,
    generation: &Rc<Cell<u64>>,
) {
    let token = generation.get();

    let vte = vte.clone();
    let snapshots = snapshots.clone();
    let generation = generation.clone();
    glib::idle_add_local_once(move || {
        if generation.get() == token {
            record_pager_snapshot(&vte, &snapshots);
        }
    });
}

pub(crate) fn drain_pager_snapshots(snapshots: &Rc<RefCell<Vec<String>>>) -> String {
    let pages = std::mem::take(&mut *snapshots.borrow_mut());
    log::debug!("drain_pager_snapshots: draining {} snapshots", pages.len());
    for (i, page) in pages.iter().enumerate() {
        log::debug!("  snapshot #{}: {} lines, {} chars", i + 1, page.lines().count(), page.len());
    }
    let merged = merge_pager_snapshots(pages);
    log::debug!("drain_pager_snapshots: merged result: {} lines, {} chars", merged.lines().count(), merged.len());
    merged
}

// ─── VTE builder ─────────────────────────────────────────────────────────────

pub(crate) fn build_vte(config: &Config) -> Terminal {
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(true)
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        // Display-only: all keystrokes are intercepted by the root key controller
        // and written to the PTY ourselves (see comment at the IM setup). Leaving
        // input enabled makes the VTE activate its own IMContext when it grabs
        // focus in show_alt_screen, stealing the fcitx/ibus input focus from our
        // IMMulticontext — which broke the Shift Chinese/English toggle inside
        // alt-screen apps like Claude Code.
        .input_enabled(false)
        .scrollback_lines(config.terminal_scrollback_lines)
        .cursor_blink_mode(CursorBlinkMode::Off)
        .cursor_shape(CursorShape::Block)
        .font_scale(config.default_font_scale)
        .opacity(1.0)
        .pointer_autohide(true)
        .enable_sixel(true)
        .build();
    terminal.set_mouse_autohide(true);
    let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
    terminal.set_colors(
        Some(&config.foreground),
        Some(&config.background),
        &palette_refs,
    );
    terminal.set_color_cursor(Some(&config.cursor));
    terminal.set_color_cursor_foreground(Some(&config.cursor_foreground));
    let font_desc = FontDescription::from_string(&config.font_desc);
    terminal.set_font(Some(&font_desc));
    terminal
}

pub(crate) fn build_output_vte(config: &Config) -> Terminal {
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(false)
        .can_focus(false)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(false)
        .scrollback_lines(MAX_INLINE_OUTPUT_ROWS as u32)
        .cursor_blink_mode(CursorBlinkMode::Off)
        .cursor_shape(CursorShape::Block)
        .font_scale(config.default_font_scale)
        .opacity(1.0)
        .pointer_autohide(true)
        .enable_sixel(true)
        .build();
    terminal.set_mouse_autohide(true);
    let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
    terminal.set_colors(
        Some(&config.foreground),
        Some(&config.background),
        &palette_refs,
    );
    terminal.set_color_cursor(Some(&config.cursor));
    terminal.set_color_cursor_foreground(Some(&config.cursor_foreground));
    let font_desc = FontDescription::from_string(&config.font_desc);
    terminal.set_font(Some(&font_desc));
    terminal
}
