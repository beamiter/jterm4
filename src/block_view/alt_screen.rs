//! alt — extracted from block_view (mechanical split, no logic changes)
use gtk4::gdk::RGBA;
use gtk4::pango::FontDescription;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::{CursorBlinkMode, CursorShape, Terminal};
use vte4::{TerminalExt, TerminalExtManual};
use crate::config::Config;

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

/// True if the byte stream contains a *real* bell (BEL, 0x07) — i.e. one that is
/// NOT acting as the string terminator of an OSC sequence (`ESC ] … BEL`). A
/// naive `bytes.contains(&7)` fires spuriously on every OSC 0/2 title update that
/// uses the BEL terminator form.
pub(crate) fn contains_bell(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b']' {
            // OSC: skip to its terminator (BEL or ESC \), consuming the BEL.
            i += 2;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    i += 1;
                    break;
                }
                if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if bytes[i] == 0x07 {
            return true;
        }
        i += 1;
    }
    false
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

#[cfg(test)]
mod contains_bell_tests {
    use super::contains_bell;

    #[test]
    fn detects_real_bell() {
        assert!(contains_bell(b"abc\x07def"));
        assert!(contains_bell(b"\x07"));
    }

    #[test]
    fn no_bell_in_plain_text() {
        assert!(!contains_bell(b"hello world"));
        assert!(!contains_bell(b""));
    }

    #[test]
    fn ignores_bel_terminating_osc() {
        // OSC title set: ESC ] 0 ; title BEL — the trailing BEL is a string
        // terminator, not an audible bell, so it must not count.
        assert!(!contains_bell(b"\x1b]0;my title\x07"));
        assert!(!contains_bell(b"before\x1b]0;t\x07after"));
    }

    #[test]
    fn osc_terminated_by_st_then_real_bell() {
        // OSC closed with ESC \ (ST), followed by a genuine bell afterwards.
        assert!(contains_bell(b"\x1b]0;t\x1b\\\x07"));
    }

    #[test]
    fn real_bell_before_osc() {
        assert!(contains_bell(b"\x07\x1b]0;t\x07"));
    }
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
        cols.saturating_sub(1),
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

        // A page can only be a contiguous sub-window of `merged` if its first
        // line already appears in `merged`. Testing that membership first turns
        // the common forward-scroll case (genuinely new content) from an
        // O(merged_len * page_len) windows scan into an O(merged_len) lookup,
        // while preserving the exact dedup result.
        let first_line = &page_lines[0];
        if merged.iter().any(|line| line == first_line)
            && merged
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

pub(crate) fn record_pager_snapshot(
    vte: &Terminal,
    snapshots: &Rc<RefCell<Vec<String>>>,
    pre_clear: &Rc<RefCell<String>>,
) {
    let raw_text = visible_vte_text(vte);
    log::debug!("record_pager_snapshot: raw_text len={}, first 100 chars: {:?}",
               raw_text.len(), raw_text.chars().take(100).collect::<String>());

    let snapshot = normalize_pager_snapshot(&raw_text);
    if snapshot.is_empty() {
        log::debug!("record_pager_snapshot: snapshot is empty after normalization");
        return;
    }

    // The shared alt VTE is reset+cleared asynchronously on each new alt-screen
    // entry. Until that clear actually renders, the VTE still shows the previous
    // command's last frame. Drop any snapshot that matches that baseline so the
    // previous command's content cannot leak into this command's block.
    if pre_clear.borrow().as_str() == snapshot {
        log::debug!("record_pager_snapshot: snapshot matches pre-clear baseline (stale render), skipping");
        return;
    }

    let mut snapshots = snapshots.borrow_mut();
    if snapshots.last().map(|last| last == &snapshot).unwrap_or(false) {
        log::debug!("record_pager_snapshot: snapshot is duplicate, skipping");
        return;
    }
    log::debug!("record_pager_snapshot: adding snapshot #{}, len={}", snapshots.len() + 1, snapshot.len());
    snapshots.push(snapshot);
    // A genuine new-content frame has rendered; the baseline is no longer needed
    // and keeping it could wrongly drop a legitimately recurring frame.
    pre_clear.borrow_mut().clear();
}

pub(crate) fn schedule_pager_snapshot(
    vte: &Terminal,
    snapshots: &Rc<RefCell<Vec<String>>>,
    generation: &Rc<Cell<u64>>,
    pre_clear: &Rc<RefCell<String>>,
) {
    let token = generation.get();

    let vte = vte.clone();
    let snapshots = snapshots.clone();
    let generation = generation.clone();
    let pre_clear = pre_clear.clone();
    glib::idle_add_local_once(move || {
        if generation.get() == token {
            record_pager_snapshot(&vte, &snapshots, &pre_clear);
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

/// The single persistent live VTE for block mode. It keeps `input_enabled(true)`
/// so the VTE translates keypresses into terminal byte sequences and emits them
/// via its `commit` signal (which we forward to our PTY). It also owns IME
/// natively, so there is no separate IMMulticontext to fight for fcitx/ibus focus.
pub(crate) fn create_active_terminal(config: &Config) -> Terminal {
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(true)
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(true)
        .scrollback_lines(config.terminal_scrollback_lines)
        .cursor_blink_mode(CursorBlinkMode::On)
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

