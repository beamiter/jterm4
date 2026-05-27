/// TermView — block-based terminal widget.
///
/// # Overview
///
/// TermView implements a "block mode" terminal that displays command history as
/// discrete blocks, each containing a prompt, command, and output. This is in
/// contrast to traditional line-based terminal emulators.
///
/// # Architecture
///
/// ## Widget Hierarchy
///
/// ```text
/// root (gtk4::Box, Vertical)
///   ├── block_scroll (ScrolledWindow)  — shown in block mode
///   │   └── block_list (gtk4::Box, Vertical)
///   │       ├── finished blocks …
///   │       └── active_block (gtk4::Box, Vertical)
///   │           ├── prompt_row (gtk4::Box, Horizontal)
///   │           │   └── prompt_label
///   │           ├── cmd_row (gtk4::Box, Horizontal)
///   │           │   └── cmd_label
///   │           └── live_view (gtk4::TextView) — live output
///   └── vte_box (gtk4::Box)            — shown in alt-screen mode
///       └── vte4::Terminal + Scrollbar
/// ```
///
/// ## State Machine
///
/// The PTY reader processes output through a state machine:
/// - **Idle**: Waiting for prompt
/// - **CollectingPrompt**: Accumulating prompt output (OSC 133;A to OSC 133;B)
/// - **AwaitingCommand**: Prompt complete, waiting for user input
/// - **CollectingOutput**: Executing command, collecting output (OSC 133;C to OSC 133;D)
/// - **AltScreen**: Full-screen application (vim, less, etc.) - switches to VTE fallback
///
/// ## Performance Optimizations
///
/// - **ANSI Cache (LRU)**: Caches ANSI-to-Pango conversions to avoid re-parsing
/// - **Output Batching**: Coalesces rapid output into batches (configurable min/max ms)
/// - **Scroll Debouncing**: Defers scroll updates to avoid cascade of timers
/// - **Widget Pool**: Reuses block widgets to reduce allocation overhead
/// - **Virtual Scrolling**: Only renders visible blocks (future enhancement)
///
/// ## Session Persistence
///
/// - Session ID passed via `--session` flag to rsh
/// - Working directory tracked via OSC 7 or `/proc/<pid>/cwd`
/// - Restorable commands (nix develop, ssh, docker) detected and saved
/// - Commands replayed on PromptEnd events during restoration
///
/// # Module Organization
///
/// - `block_view.rs` - Main TermView implementation (3000+ lines)
/// - `block_view_types.rs` - Type definitions (BlockState, BlockData, etc.)
/// - `parser.rs` - OSC 133 sequence parsing
/// - `pty.rs` - PTY management
///
/// # Layout
///
/// ```text
/// root (gtk4::Box, Vertical)
///   ├── block_scroll (ScrolledWindow)  — shown in block mode
///   │   └── block_list (gtk4::Box, Vertical)
///   │       ├── finished blocks …
///   │       └── active_block (gtk4::Box, Vertical)
///   │           ├── prompt_row (gtk4::Box, Horizontal)
///   │           │   └── prompt_label
///   │           ├── cmd_row (gtk4::Box, Horizontal)
///   │           │   └── cmd_label
///   │           └── live_view (gtk4::TextView) — live output
///   └── vte_box (gtk4::Box)            — shown in alt-screen mode
///       └── vte4::Terminal + Scrollbar
/// ```
use gtk4::gdk::RGBA;
use gtk4::glib::translate::IntoGlib;
use gtk4::pango::FontDescription;
use gtk4::prelude::*;
use gtk4::{glib, EventControllerKey, Orientation, ScrolledWindow, TextBuffer, TextView};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use vte4::{CursorBlinkMode, CursorShape, Terminal};
use vte4::{TerminalExt, TerminalExtManual};

use crate::config::Config;
use crate::parser::{Parser, ParserEvent};
use crate::pty::OwnedPty;
use crate::terminal::open_uri;

// Global block ID counter
static BLOCK_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_block_id() -> u64 {
    BLOCK_ID_COUNTER.fetch_add(1, Ordering::SeqCst)
}

// ─── Cursor Shape ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermCursorShape {
    Block,      // 0 or 1: block cursor
    Underline,  // 3 or 4: underline cursor
    Bar,        // 5 or 6: bar/vertical cursor
}

impl Default for TermCursorShape {
    fn default() -> Self {
        TermCursorShape::Block
    }
}

// ─── Mouse Reporting Mode ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseReportingMode {
    /// No mouse reporting (CSI ?1000l, etc.)
    None,
    /// Basic click reporting (CSI ?1000h)
    Click,
    /// Button press/release/drag (CSI ?1002h)
    Button,
    /// All mouse motion (CSI ?1003h)
    Motion,
    /// SGR-style reporting (CSI ?1006h) - modern format
    SGR,
}

impl Default for MouseReportingMode {
    fn default() -> Self {
        MouseReportingMode::None
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Simple URL detection regex (http/https/file URLs)
fn is_url(text: &str) -> bool {
    text.starts_with("http://") || text.starts_with("https://") || text.starts_with("file://")
}

/// Extract URL at cursor position in a TextView's buffer, returning bounds and text
fn get_url_bounds_at_position(
    buffer: &TextBuffer,
    iter: &gtk4::TextIter,
) -> Option<(gtk4::TextIter, gtk4::TextIter, String)> {
    let mut start = iter.clone();
    let mut end = iter.clone();

    // Expand backwards to find URL start
    while !start.starts_line() {
        let ch = start.char();
        if ch == ' ' || ch == '\n' || ch == '\t' || ch == '<' || ch == '>' {
            start.forward_char();
            break;
        }
        if !start.backward_char() {
            break;
        }
    }

    // Expand forwards to find URL end
    while !end.ends_line() {
        let ch = end.char();
        if ch == ' ' || ch == '\n' || ch == '\t' || ch == '<' || ch == '>' {
            break;
        }
        if !end.forward_char() {
            break;
        }
    }

    let text = buffer.text(&start, &end, false).to_string();
    if is_url(&text) {
        Some((start, end, text))
    } else {
        None
    }
}

fn get_url_at_position(buffer: &TextBuffer, iter: &gtk4::TextIter) -> Option<String> {
    get_url_bounds_at_position(buffer, iter).map(|(_, _, url)| url)
}

/// Format mouse event in SGR mode (CSI <button;x;y M/m)
fn format_mouse_event_sgr(button: u8, x: i32, y: i32, pressed: bool) -> Vec<u8> {
    let event_type = if pressed { 'M' } else { 'm' };
    format!("\x1b[<{};{};{}{}", button, x + 1, y + 1, event_type).into_bytes()
}

/// Coalesces repeated scroll requests into a single scroll event.
/// Eliminates cascade of timers and provides smooth scrolling under rapid output.
struct ScrollDebouncer {
    dirty: Rc<Cell<bool>>,
    pending_handle: Rc<RefCell<Option<glib::source::SourceId>>>,
    user_scrolled_up: Rc<Cell<bool>>,
    programmatic_scroll: Rc<Cell<bool>>,
}

impl ScrollDebouncer {
    fn with_scroll_lock(
        user_scrolled_up: Rc<Cell<bool>>,
        programmatic_scroll: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            dirty: Rc::new(Cell::new(false)),
            pending_handle: Rc::new(RefCell::new(None)),
            user_scrolled_up,
            programmatic_scroll,
        }
    }

    fn mark_dirty(&self, scroll: &ScrolledWindow) {
        if self.dirty.get() || self.user_scrolled_up.get() {
            return;
        }
        self.dirty.set(true);

        let scroll = scroll.clone();
        let dirty = self.dirty.clone();
        let pending = self.pending_handle.clone();
        let programmatic = self.programmatic_scroll.clone();

        if let Some(handle) = pending.borrow_mut().take() {
            let _ = handle.remove();
        }

        let pending_for_clear = pending.clone();
        let handle =
            glib::timeout_add_local_once(std::time::Duration::from_millis(50), move || {
                let adj = scroll.vadjustment();
                let target = adj.upper() - adj.page_size();
                if adj.value() < target {
                    programmatic.set(true);
                    adj.set_value(target);
                    programmatic.set(false);
                }
                dirty.set(false);
                pending_for_clear.borrow_mut().take();
            });
        pending.borrow_mut().replace(handle);
    }

    fn reset_scroll_lock(&self) {
        self.user_scrolled_up.set(false);
    }
}

fn rgba_to_hex(c: &RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c.red() * 255.0) as u8,
        (c.green() * 255.0) as u8,
        (c.blue() * 255.0) as u8,
    )
}

fn shorten_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let display = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    let parts: Vec<&str> = display.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 3 {
        display
    } else {
        format!("…/{}", parts[parts.len()-2..].join("/"))
    }
}

fn chrono_local_offset_secs() -> i64 {
    use nix::libc;
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm.tm_gmtoff as i64
    }
}

fn skip_osc_sequence(bytes: &[u8], mut i: usize) -> usize {
    i += 2;
    while i < bytes.len() {
        if bytes[i] == 0x07 {
            return i + 1;
        }
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
            return i + 2;
        }
        i += 1;
    }
    i
}

fn skip_escape_sequence(bytes: &[u8], i: usize) -> usize {
    if i + 1 >= bytes.len() {
        return i + 1;
    }

    match bytes[i + 1] {
        b'[' => {
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            if j < bytes.len() {
                j += 1;
            }
            j
        }
        b']' => skip_osc_sequence(bytes, i),
        next if (0x20..=0x2f).contains(&next) => {
            let mut j = i + 2;
            while j < bytes.len() && (0x20..=0x2f).contains(&bytes[j]) {
                j += 1;
            }
            if j < bytes.len() && (0x30..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            j
        }
        _ => i + 2,
    }
}

fn is_alt_screen_mode(params: &[u8]) -> bool {
    matches!(params, b"?47" | b"?1047" | b"?1049")
}

fn is_application_cursor_mode(params: &[u8]) -> bool {
    params == b"?1"
}

fn contains_interactive_screen_enter(bytes: &[u8]) -> bool {
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
                    && (is_alt_screen_mode(params) || is_application_cursor_mode(params))
                {
                    return true;
                }
                // ESC[?25l — cursor hide, strong indicator of TUI app (Claude CLI, etc.)
                if final_byte == b'l' && params == b"?25" {
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

#[cfg(test)]
mod interactive_screen_tests {
    use super::contains_interactive_screen_enter;

    #[test]
    fn detects_alt_screen_enter() {
        assert!(contains_interactive_screen_enter(b"\x1b[?1049h"));
    }

    #[test]
    fn detects_less_application_cursor_enter() {
        assert!(contains_interactive_screen_enter(b"\x1b[?1h\x1b="));
    }

    #[test]
    fn detects_cursor_hide() {
        assert!(contains_interactive_screen_enter(b"\x1b[?25l"));
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
mod pager_snapshot_tests {
    use super::{merge_pager_snapshots, normalize_pager_snapshot};

    #[test]
    fn filters_less_status_lines() {
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

fn show_alt_screen(
    block_scroll: &ScrolledWindow,
    vte_box: &gtk4::Box,
    vte: &Terminal,
    pty: Rc<OwnedPty>,
    initial_bytes: Option<&[u8]>,
) {
    block_scroll.set_visible(false);
    block_scroll.set_vexpand(false);
    vte_box.set_vexpand(true);
    vte_box.set_visible(true);

    let pty_resize = pty.clone();
    let vte_for_resize = vte.clone();
    glib::idle_add_local_once(move || {
        let width = vte_for_resize.allocated_width() as i64;
        let height = vte_for_resize.allocated_height() as i64;
        if width > 0 && height > 0 {
            let char_width = vte_for_resize.char_width();
            let char_height = vte_for_resize.char_height();
            if char_width > 0 && char_height > 0 {
                let cols = (width / char_width) as u16;
                let rows = (height / char_height) as u16;
                log::debug!(
                    "Resizing PTY to {}x{} (widget {}x{}, char {}x{})",
                    cols,
                    rows,
                    width,
                    height,
                    char_width,
                    char_height
                );
                pty_resize.resize(cols, rows);
            }
        }
    });

    if let Some(bytes) = initial_bytes {
        vte.feed(bytes);
    }

    vte.grab_focus();
}

fn hide_alt_screen(block_scroll: &ScrolledWindow, vte_box: &gtk4::Box) {
    vte_box.set_visible(false);
    vte_box.set_vexpand(false);
    block_scroll.set_vexpand(true);
    block_scroll.set_visible(true);
}

fn visible_vte_text(vte: &Terminal) -> String {
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

fn normalize_pager_snapshot(text: &str) -> String {
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

fn is_pager_chrome_line(line: &str) -> bool {
    matches!(line.trim(), ":" | "(END)" | "END")
}

fn overlap_line_count(existing: &[String], next: &[String]) -> usize {
    let max_overlap = existing.len().min(next.len());
    for count in (1..=max_overlap).rev() {
        if existing[existing.len() - count..] == next[..count] {
            if count > 1 || !existing[existing.len() - count].trim().is_empty() {
                return count;
            }
        }
    }
    0
}

fn merge_pager_snapshots(pages: Vec<String>) -> String {
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

fn contains_clear_screen(bytes: &[u8]) -> bool {
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

fn record_pager_snapshot(vte: &Terminal, snapshots: &Rc<RefCell<Vec<String>>>) {
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

fn schedule_pager_snapshot(
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

fn drain_pager_snapshots(snapshots: &Rc<RefCell<Vec<String>>>) -> String {
    let pages = std::mem::take(&mut *snapshots.borrow_mut());
    log::debug!("drain_pager_snapshots: draining {} snapshots", pages.len());
    for (i, page) in pages.iter().enumerate() {
        log::debug!("  snapshot #{}: {} lines, {} chars", i + 1, page.lines().count(), page.len());
    }
    let merged = merge_pager_snapshots(pages);
    log::debug!("drain_pager_snapshots: merged result: {} lines, {} chars", merged.lines().count(), merged.len());
    merged
}

fn strip_ansi_with_clear_detect(input: &str) -> (String, bool) {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut should_clear = false;

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    // CSI sequence: collect params and final byte
                    i += 2;
                    let mut params = Vec::new();
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        params.push(bytes[i]);
                        i += 1;
                    }
                    if i < bytes.len() {
                        let final_byte = bytes[i];
                        i += 1;

                        // Detect clear-screen sequences. Cursor movement and partial erase
                        // are common in command progress output, so only full-display erase
                        // should clear the active block's rendered output.
                        match final_byte {
                            b'J' => {
                                // CSI 2J / CSI 3J — erase full display / scrollback.
                                if params == b"2" || params == b"3" {
                                    should_clear = true;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                b']' => {
                    i = skip_osc_sequence(bytes, i);
                }
                _ => {
                    i = skip_escape_sequence(bytes, i);
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    (String::from_utf8_lossy(&out).to_string(), should_clear)
}

fn strip_ansi(input: &str) -> String {
    strip_ansi_with_clear_detect(input).0
}

fn strip_ansi_cached(
    input: &str,
    cache: &std::collections::HashMap<String, String>,
) -> (String, bool) {
    if let Some(cached) = cache.get(input) {
        (cached.clone(), true)
    } else {
        (strip_ansi(input), false)
    }
}

fn ansi256_to_rgb(idx: u8, palette: &[RGBA; 16]) -> (u8, u8, u8) {
    match idx {
        0..=15 => {
            let c = palette[idx as usize];
            (
                (c.red() * 255.0) as u8,
                (c.green() * 255.0) as u8,
                (c.blue() * 255.0) as u8,
            )
        }
        16..=231 => {
            let idx = idx - 16;
            let r = (idx / 36) * 51;
            let g = ((idx % 36) / 6) * 51;
            let b = (idx % 6) * 51;
            (r, g, b)
        }
        232..=255 => {
            let gray = 8 + (idx - 232) * 10;
            (gray, gray, gray)
        }
    }
}

fn skip_ansi_visible_chars(input: &str, mut count: usize) -> String {
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() && count > 0 {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    i = skip_escape_sequence(bytes, i);
                }
                b']' => {
                    i = skip_escape_sequence(bytes, i);
                }
                _ => {
                    i = skip_escape_sequence(bytes, i);
                }
            }
        } else {
            let ch_len = if bytes[i] & 0x80 == 0 {
                1
            } else if bytes[i] & 0xe0 == 0xc0 {
                2
            } else if bytes[i] & 0xf0 == 0xe0 {
                3
            } else if bytes[i] & 0xf8 == 0xf0 {
                4
            } else {
                1
            };
            i += ch_len;
            if count > 0 {
                count -= 1;
            }
        }
    }
    input[i..].to_string()
}

fn separate_input_and_suggestion(input: &str, column_offset: usize) -> (String, String) {
    struct Cell {
        text: String,
        in_dim: bool,
    }

    let mut cells: Vec<Cell> = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut in_dim = false;
    let mut cursor = 0usize;

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            let mut params = Vec::new();

            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                if bytes[i] == b';' {
                    params.push(String::new());
                } else if let Some(last) = params.last_mut() {
                    last.push(bytes[i] as char);
                } else {
                    params.push(String::from(bytes[i] as char));
                }
                i += 1;
            }

            if i < bytes.len() {
                let final_byte = bytes[i];
                i += 1;

                match final_byte {
                    b'm' => {
                        if params.is_empty() || params.iter().any(|p| p.is_empty()) {
                            in_dim = false;
                        }

                        for param in &params {
                            match param.as_str() {
                                "0" | "22" => in_dim = false,
                                "2" => in_dim = true,
                                _ => {}
                            }
                        }
                    }
                    b'D' => {
                        let count = params
                            .first()
                            .and_then(|param| param.parse::<usize>().ok())
                            .unwrap_or(1);
                        cursor = cursor.saturating_sub(count);
                    }
                    b'C' => {
                        let count = params
                            .first()
                            .and_then(|param| param.parse::<usize>().ok())
                            .unwrap_or(1);
                        cursor = (cursor + count).min(cells.len());
                    }
                    b'G' => {
                        let col = params
                            .first()
                            .and_then(|param| param.parse::<usize>().ok())
                            .unwrap_or(1);
                        cursor = if column_offset == 0 {
                            col.saturating_sub(1)
                        } else {
                            col.saturating_sub(column_offset)
                        }
                        .min(cells.len());
                    }
                    b'K' => {
                        let mode = params.first().map(String::as_str).unwrap_or("0");
                        match mode {
                            "" | "0" => cells.truncate(cursor),
                            "1" => {
                                cells.drain(..cursor);
                                cursor = 0;
                            }
                            "2" => {
                                cells.clear();
                                cursor = 0;
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        } else if bytes[i] == 0x1b && i + 1 < bytes.len() {
            i = skip_escape_sequence(bytes, i);
        } else if bytes[i] == b'\r' {
            cursor = 0;
            i += 1;
        } else if bytes[i] == b'\x08' {
            cursor = cursor.saturating_sub(1);
            i += 1;
        } else {
            let ch_start = i;
            i += input[i..]
                .chars()
                .next()
                .map(|ch| ch.len_utf8())
                .unwrap_or(1);

            let ch = String::from_utf8_lossy(&bytes[ch_start..i]).to_string();
            if cursor < cells.len() {
                cells[cursor] = Cell { text: ch, in_dim };
            } else {
                cells.push(Cell { text: ch, in_dim });
            }
            cursor += 1;
        }
    }

    let cursor_split = cursor.min(cells.len());
    let dim_split = cells
        .iter()
        .position(|cell| cell.in_dim)
        .unwrap_or(cells.len());
    let split = cursor_split.min(dim_split);

    let mut user_input = String::new();
    let mut suggestion = String::new();

    for (idx, cell) in cells.into_iter().enumerate() {
        if idx < split {
            user_input.push_str(&cell.text);
        } else {
            suggestion.push_str(&cell.text);
        }
    }

    (user_input, suggestion)
}

fn command_line_plain_text(input: &str) -> String {
    let mut cells: Vec<String> = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut cursor = 0usize;

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            let mut params = Vec::new();

            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                if bytes[i] == b';' {
                    params.push(String::new());
                } else if let Some(last) = params.last_mut() {
                    last.push(bytes[i] as char);
                } else {
                    params.push(String::from(bytes[i] as char));
                }
                i += 1;
            }

            if i < bytes.len() {
                let final_byte = bytes[i];
                i += 1;

                match final_byte {
                    b'D' => {
                        let count = params
                            .first()
                            .and_then(|param| param.parse::<usize>().ok())
                            .unwrap_or(1);
                        cursor = cursor.saturating_sub(count);
                    }
                    b'C' => {
                        let count = params
                            .first()
                            .and_then(|param| param.parse::<usize>().ok())
                            .unwrap_or(1);
                        cursor = (cursor + count).min(cells.len());
                    }
                    b'G' => {
                        let col = params
                            .first()
                            .and_then(|param| param.parse::<usize>().ok())
                            .unwrap_or(1);
                        cursor = col.saturating_sub(1).min(cells.len());
                    }
                    b'K' => {
                        let mode = params.first().map(String::as_str).unwrap_or("0");
                        match mode {
                            "" | "0" => cells.truncate(cursor),
                            "1" => {
                                cells.drain(..cursor);
                                cursor = 0;
                            }
                            "2" => {
                                cells.clear();
                                cursor = 0;
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        } else if bytes[i] == 0x1b && i + 1 < bytes.len() {
            i = skip_escape_sequence(bytes, i);
        } else if bytes[i] == b'\r' {
            cursor = 0;
            i += 1;
        } else if bytes[i] == b'\x08' {
            cursor = cursor.saturating_sub(1);
            i += 1;
        } else {
            let ch_start = i;
            i += input[i..]
                .chars()
                .next()
                .map(|ch| ch.len_utf8())
                .unwrap_or(1);

            let ch = String::from_utf8_lossy(&bytes[ch_start..i]).to_string();
            if cursor < cells.len() {
                cells[cursor] = ch;
            } else {
                cells.push(ch);
            }
            cursor += 1;
        }
    }

    cells.concat()
}

fn plain_text_from_ansi(input: &str) -> String {
    command_line_plain_text(input)
}

#[derive(Clone, Default, PartialEq)]
struct AnsiStyleState {
    foreground: Option<RGBA>,
    background: Option<RGBA>,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    dim: bool,
    reverse: bool,     // SGR 7 - swap fg/bg
    hidden: bool,      // SGR 8 - invisible text
    overline: bool,    // SGR 53 - line above text
}

#[derive(Clone)]
struct AnsiTextRun {
    text: String,
    style: AnsiStyleState,
}

fn ansi_tag_name(style: &AnsiStyleState) -> Option<String> {
    if style.foreground.is_none()
        && style.background.is_none()
        && !style.bold
        && !style.italic
        && !style.underline
        && !style.strikethrough
        && !style.dim
        && !style.reverse
        && !style.hidden
        && !style.overline
    {
        return None;
    }

    let rgba_key = |color: Option<&RGBA>| match color {
        Some(color) => format!(
            "{:03}-{:03}-{:03}-{:03}",
            (color.red() * 255.0).round() as u8,
            (color.green() * 255.0).round() as u8,
            (color.blue() * 255.0).round() as u8,
            (color.alpha() * 255.0).round() as u8,
        ),
        None => "none".to_string(),
    };

    Some(format!(
        "ansi-run-fg:{}-bg:{}-b{}-i{}-u{}-s{}-d{}",
        rgba_key(style.foreground.as_ref()),
        rgba_key(style.background.as_ref()),
        style.bold as u8,
        style.italic as u8,
        style.underline as u8,
        style.strikethrough as u8,
        style.dim as u8,
    ))
}

fn ensure_ansi_text_tag(buffer: &TextBuffer, style: &AnsiStyleState) -> Option<gtk4::TextTag> {
    let tag_name = ansi_tag_name(style)?;
    let tag_table = buffer.tag_table();

    if let Some(tag) = tag_table.lookup(&tag_name) {
        return Some(tag);
    }

    let tag = gtk4::TextTag::new(Some(&tag_name));
    if let Some(mut foreground) = style.foreground {
        if style.dim {
            foreground.set_alpha(0.7);
        }
        tag.set_foreground_rgba(Some(&foreground));
    }
    if let Some(background) = style.background {
        tag.set_background_rgba(Some(&background));
    }
    if style.bold {
        tag.set_weight(gtk4::pango::Weight::Bold.into_glib());
    }
    if style.italic {
        tag.set_style(gtk4::pango::Style::Italic);
    }
    if style.underline {
        tag.set_underline(gtk4::pango::Underline::Single);
    }
    if style.strikethrough {
        tag.set_strikethrough(true);
    }

    tag_table.add(&tag);
    Some(tag)
}

fn flush_ansi_run(runs: &mut Vec<AnsiTextRun>, text: &mut String, style: &AnsiStyleState) {
    if text.is_empty() {
        return;
    }

    runs.push(AnsiTextRun {
        text: std::mem::take(text),
        style: style.clone(),
    });
}

fn parse_sgr_params(style: &mut AnsiStyleState, params: &[String], palette: &[RGBA; 16]) {
    let mut index = 0;
    while index < params.len() {
        let param = if params[index].is_empty() {
            0
        } else {
            params[index].parse::<u32>().unwrap_or(0)
        };

        match param {
            0 => *style = AnsiStyleState::default(),
            1 => style.bold = true,
            2 => style.dim = true,
            3 => style.italic = true,
            4 => style.underline = true,
            9 => style.strikethrough = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => style.underline = false,
            29 => style.strikethrough = false,
            30..=37 => {
                let (r, g, b) = ansi256_to_rgb((param - 30) as u8, palette);
                style.foreground = Some(RGBA::new(
                    r as f32 / 255.0,
                    g as f32 / 255.0,
                    b as f32 / 255.0,
                    1.0,
                ));
            }
            39 => style.foreground = None,
            40..=47 => {
                let (r, g, b) = ansi256_to_rgb((param - 40) as u8, palette);
                style.background = Some(RGBA::new(
                    r as f32 / 255.0,
                    g as f32 / 255.0,
                    b as f32 / 255.0,
                    1.0,
                ));
            }
            49 => style.background = None,
            90..=97 => {
                let (r, g, b) = ansi256_to_rgb((param - 90 + 8) as u8, palette);
                style.foreground = Some(RGBA::new(
                    r as f32 / 255.0,
                    g as f32 / 255.0,
                    b as f32 / 255.0,
                    1.0,
                ));
            }
            100..=107 => {
                let (r, g, b) = ansi256_to_rgb((param - 100 + 8) as u8, palette);
                style.background = Some(RGBA::new(
                    r as f32 / 255.0,
                    g as f32 / 255.0,
                    b as f32 / 255.0,
                    1.0,
                ));
            }
            38 | 48 => {
                let target = if param == 38 {
                    &mut style.foreground
                } else {
                    &mut style.background
                };

                if index + 2 < params.len() && params[index + 1] == "5" {
                    if let Ok(color_index) = params[index + 2].parse::<u8>() {
                        let (r, g, b) = ansi256_to_rgb(color_index, palette);
                        *target = Some(RGBA::new(
                            r as f32 / 255.0,
                            g as f32 / 255.0,
                            b as f32 / 255.0,
                            1.0,
                        ));
                    }
                    index += 2;
                } else if index + 4 < params.len() && params[index + 1] == "2" {
                    if let (Ok(r), Ok(g), Ok(b)) = (
                        params[index + 2].parse::<u8>(),
                        params[index + 3].parse::<u8>(),
                        params[index + 4].parse::<u8>(),
                    ) {
                        *target = Some(RGBA::new(
                            r as f32 / 255.0,
                            g as f32 / 255.0,
                            b as f32 / 255.0,
                            1.0,
                        ));
                    }
                    index += 4;
                }
            }
            7 => style.reverse = true,           // SGR 7: reverse video (swap fg/bg)
            8 => style.hidden = true,            // SGR 8: conceal/hidden
            27 => style.reverse = false,         // SGR 27: disable reverse video
            28 => style.hidden = false,          // SGR 28: disable hidden
            53 => style.overline = true,         // SGR 53: overline
            55 => style.overline = false,        // SGR 55: disable overline
            _ => {}
        }

        index += 1;
    }
}

/// Parse ANSI text with proper cursor movement handling
/// This ensures colors align with the final text after \r and cursor movements
fn ansi_text_runs(input: &str, palette: &[RGBA; 16]) -> Vec<AnsiTextRun> {
    let bytes = input.as_bytes();
    let mut runs: Vec<AnsiTextRun> = Vec::new();
    let mut current_style = AnsiStyleState::default();

    // Track cells with their styles (like command_line_plain_text but with colors)
    let mut cells: Vec<(String, AnsiStyleState)> = Vec::new();
    let mut cursor = 0usize;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            let mut params = Vec::new();
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                if bytes[i] == b';' {
                    params.push(String::new());
                } else if let Some(last) = params.last_mut() {
                    last.push(bytes[i] as char);
                } else {
                    params.push(String::from(bytes[i] as char));
                }
                i += 1;
            }

            if i < bytes.len() {
                let final_byte = bytes[i];
                i += 1;

                match final_byte {
                    b'm' => {
                        // Color change
                        if params.is_empty() || params[0].is_empty() {
                            params = vec!["0".to_string()];
                        }
                        parse_sgr_params(&mut current_style, &params, palette);
                    }
                    b'D' => {
                        // Cursor left
                        let count = params.first().and_then(|p| p.parse::<usize>().ok()).unwrap_or(1);
                        cursor = cursor.saturating_sub(count);
                    }
                    b'C' => {
                        // Cursor right
                        let count = params.first().and_then(|p| p.parse::<usize>().ok()).unwrap_or(1);
                        cursor = (cursor + count).min(cells.len());
                    }
                    b'G' => {
                        // Cursor to column
                        let col = params.first().and_then(|p| p.parse::<usize>().ok()).unwrap_or(1);
                        cursor = col.saturating_sub(1).min(cells.len());
                    }
                    b'K' => {
                        // Erase in line
                        let mode = params.first().map(String::as_str).unwrap_or("0");
                        match mode {
                            "" | "0" => cells.truncate(cursor),
                            "1" => {
                                cells.drain(..cursor);
                                cursor = 0;
                            }
                            "2" => {
                                cells.clear();
                                cursor = 0;
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        } else if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b']' {
            i = skip_osc_sequence(bytes, i);
        } else if bytes[i] == 0x1b && i + 1 < bytes.len() {
            i = skip_escape_sequence(bytes, i);
        } else if bytes[i] == b'\r' {
            // Carriage return - move cursor to start of current line
            cursor = 0;
            i += 1;
        } else if bytes[i] == b'\n' {
            // Newline - flush current line's cells to runs, add newline, start new line
            // Convert current cells to runs
            for (ch, style) in cells.drain(..) {
                if runs.is_empty() || runs.last().unwrap().style != style {
                    runs.push(AnsiTextRun {
                        text: ch,
                        style: style.clone(),
                    });
                } else {
                    runs.last_mut().unwrap().text.push_str(&ch);
                }
            }
            // Add newline as a separate run
            runs.push(AnsiTextRun {
                text: "\n".to_string(),
                style: current_style.clone(),
            });
            cursor = 0;
            i += 1;
        } else if bytes[i] == b'\x08' {
            // Backspace
            cursor = cursor.saturating_sub(1);
            i += 1;
        } else {
            // Regular character - write to cell with current style
            let ch_len = if bytes[i] & 0x80 == 0 {
                1
            } else if bytes[i] & 0xe0 == 0xc0 {
                2
            } else if bytes[i] & 0xf0 == 0xe0 {
                3
            } else if bytes[i] & 0xf8 == 0xf0 {
                4
            } else {
                1
            };
            let end = (i + ch_len).min(bytes.len());
            let ch = String::from_utf8_lossy(&bytes[i..end]).to_string();

            if cursor < cells.len() {
                cells[cursor] = (ch, current_style.clone());
            } else {
                cells.push((ch, current_style.clone()));
            }
            cursor += 1;
            i = end;
        }
    }

    // Convert cells to runs by merging adjacent cells with the same style
    let mut current_run_text = String::new();
    let mut current_run_style = AnsiStyleState::default();
    let mut first = true;

    for (ch, style) in cells {
        if first {
            current_run_text = ch;
            current_run_style = style;
            first = false;
        } else if style == current_run_style {
            current_run_text.push_str(&ch);
        } else {
            if !current_run_text.is_empty() {
                runs.push(AnsiTextRun {
                    text: current_run_text.clone(),
                    style: current_run_style.clone(),
                });
            }
            current_run_text = ch;
            current_run_style = style;
        }
    }

    if !current_run_text.is_empty() {
        runs.push(AnsiTextRun {
            text: current_run_text,
            style: current_run_style,
        });
    }

    runs
}

fn apply_ansi_runs_to_buffer(buffer: &TextBuffer, start_offset: usize, runs: &[AnsiTextRun]) {
    let mut offset = start_offset;
    for run in runs {
        let len = run.text.chars().count();
        if len == 0 {
            continue;
        }

        if let Some(tag) = ensure_ansi_text_tag(buffer, &run.style) {
            let start_iter = buffer.iter_at_offset(offset as i32);
            let end_iter = buffer.iter_at_offset((offset + len) as i32);
            buffer.apply_tag(&tag, &start_iter, &end_iter);
        }
        offset += len;
    }
}

fn set_active_buffer_text(
    buffer: &TextBuffer,
    cmd: &str,
    suggestion: &str,
    output: &str,
    cursor_visible: bool,
    palette: &[RGBA; 16],
) {
    let cursor_char = if output.is_empty() {
        if cursor_visible { "█" } else { " " }
    } else {
        ""
    };
    let text = if output.is_empty() {
        format!("{}{}{}", cmd, cursor_char, suggestion)
    } else {
        // First strip ANSI codes, then handle \r per-line
        let output_no_ansi = strip_ansi(output);
        let output_plain = output_no_ansi
            .lines()
            .map(|line| command_line_plain_text(line))
            .collect::<Vec<_>>()
            .join("\n");
        format!("{}{}\n{}", cmd, cursor_char, output_plain)
    };

    buffer.set_text(&text);

    if !output.is_empty() {
        let output_runs = ansi_text_runs(output, palette);
        let output_start = cmd.chars().count() + cursor_char.chars().count() + 1;
        apply_ansi_runs_to_buffer(buffer, output_start, &output_runs);
        return;
    }

    if suggestion.is_empty() {
        return;
    }

    let tag_table = buffer.tag_table();
    if tag_table.lookup("suggestion").is_none() {
        let tag = gtk4::TextTag::new(Some("suggestion"));
        tag.set_style(gtk4::pango::Style::Italic);
        tag.set_foreground_rgba(Some(&RGBA::new(0.5, 0.5, 0.5, 0.7)));
        tag_table.add(&tag);
    }

    if let Some(tag) = tag_table.lookup("suggestion") {
        let start_pos = cmd.chars().count() + cursor_char.chars().count();
        let end_pos = start_pos + suggestion.chars().count();
        let start_iter = buffer.iter_at_offset(start_pos as i32);
        let end_iter = buffer.iter_at_offset(end_pos as i32);
        buffer.apply_tag(&tag, &start_iter, &end_iter);
    }
}

fn set_active_prompt_buffer(buffer: &TextBuffer, prompt: &str) {
    buffer.set_text(prompt);
}

fn set_active_command_buffer(
    buffer: &TextBuffer,
    cmd: &str,
    preedit: &str,
    cursor_visible: bool,
    suggestion: &str,
    cursor_color: &RGBA,
    cursor_foreground: &RGBA,
) {
    set_active_command_buffer_at(
        buffer, cmd, preedit, cursor_visible, suggestion, cursor_color, cursor_foreground, None,
    );
}

fn set_active_command_buffer_at(
    buffer: &TextBuffer,
    cmd: &str,
    preedit: &str,
    cursor_visible: bool,
    suggestion: &str,
    cursor_color: &RGBA,
    cursor_foreground: &RGBA,
    explicit_cursor_pos: Option<usize>,
) {
    let cursor_pos = explicit_cursor_pos.unwrap_or_else(|| cmd.chars().count() + preedit.chars().count());
    let text = format!("{}{} {}", cmd, preedit, suggestion);
    buffer.set_text(&text);
    let cursor_iter = buffer.iter_at_offset(cursor_pos as i32);
    buffer.place_cursor(&cursor_iter);

    let tag_table = buffer.tag_table();

    if tag_table.lookup("cursor").is_none() {
        let tag = gtk4::TextTag::new(Some("cursor"));
        tag_table.add(&tag);
    }

    if cursor_visible {
        if let Some(tag) = tag_table.lookup("cursor") {
            tag.set_background_rgba(Some(cursor_color));
            tag.set_foreground_rgba(Some(cursor_foreground));
            let start_iter = buffer.iter_at_offset(cursor_pos as i32);
            let end_iter = buffer.iter_at_offset((cursor_pos + 1) as i32);
            buffer.apply_tag(&tag, &start_iter, &end_iter);
        }
    }

    if !preedit.is_empty() {
        if tag_table.lookup("preedit").is_none() {
            let tag = gtk4::TextTag::new(Some("preedit"));
            tag.set_underline(gtk4::pango::Underline::Single);
            tag_table.add(&tag);
        }

        if let Some(tag) = tag_table.lookup("preedit") {
            let start_pos = cmd.chars().count();
            let end_pos = start_pos + preedit.chars().count();
            let start_iter = buffer.iter_at_offset(start_pos as i32);
            let end_iter = buffer.iter_at_offset(end_pos as i32);
            buffer.apply_tag(&tag, &start_iter, &end_iter);
        }
    }

    if suggestion.is_empty() {
        return;
    }

    if tag_table.lookup("suggestion").is_none() {
        let tag = gtk4::TextTag::new(Some("suggestion"));
        tag.set_style(gtk4::pango::Style::Italic);
        tag.set_foreground_rgba(Some(&RGBA::new(0.5, 0.5, 0.5, 0.7)));
        tag_table.add(&tag);
    }

    if let Some(tag) = tag_table.lookup("suggestion") {
        let start_pos = cursor_pos + 1;
        let end_pos = start_pos + suggestion.chars().count();
        let start_iter = buffer.iter_at_offset(start_pos as i32);
        let end_iter = buffer.iter_at_offset(end_pos as i32);
        buffer.apply_tag(&tag, &start_iter, &end_iter);
    }
}

/// Returns (cursor_col_in_last_line, after_newline).
/// after_newline=true means cursor is at start of a new line following \n
/// (buffer may not render that empty trailing line).
fn output_cursor_col(output: &str) -> (usize, bool) {
    let bytes = output.as_bytes();
    let mut cells_len = 0usize;
    let mut cursor = 0usize;
    let mut after_newline = false;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            let mut params: Vec<String> = Vec::new();
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                if bytes[i] == b';' {
                    params.push(String::new());
                } else if let Some(last) = params.last_mut() {
                    last.push(bytes[i] as char);
                } else {
                    params.push(String::from(bytes[i] as char));
                }
                i += 1;
            }
            if i < bytes.len() {
                let final_byte = bytes[i];
                i += 1;
                match final_byte {
                    b'D' => {
                        let count = params.first().and_then(|p| p.parse::<usize>().ok()).unwrap_or(1);
                        cursor = cursor.saturating_sub(count);
                        after_newline = false;
                    }
                    b'C' => {
                        let count = params.first().and_then(|p| p.parse::<usize>().ok()).unwrap_or(1);
                        cursor = (cursor + count).min(cells_len);
                        after_newline = false;
                    }
                    b'G' => {
                        let col = params.first().and_then(|p| p.parse::<usize>().ok()).unwrap_or(1);
                        cursor = col.saturating_sub(1).min(cells_len);
                        after_newline = false;
                    }
                    b'K' => {
                        let mode = params.first().map(String::as_str).unwrap_or("0");
                        match mode {
                            "" | "0" => { cells_len = cursor; }
                            "1" => { cursor = 0; after_newline = false; }
                            "2" => { cells_len = 0; cursor = 0; after_newline = false; }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        } else if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b']' {
            i = skip_osc_sequence(bytes, i);
        } else if bytes[i] == 0x1b {
            i = skip_escape_sequence(bytes, i);
        } else if bytes[i] == b'\n' {
            cursor = 0;
            cells_len = 0;
            after_newline = true;
            i += 1;
        } else if bytes[i] == b'\r' {
            cursor = 0;
            after_newline = false;
            i += 1;
        } else if bytes[i] == b'\x08' {
            cursor = cursor.saturating_sub(1);
            after_newline = false;
            i += 1;
        } else {
            let ch_len = if bytes[i] & 0x80 == 0 { 1 }
                else if bytes[i] & 0xe0 == 0xc0 { 2 }
                else if bytes[i] & 0xf0 == 0xe0 { 3 }
                else if bytes[i] & 0xf8 == 0xf0 { 4 }
                else { 1 };
            i = (i + ch_len).min(bytes.len());
            if cursor >= cells_len { cells_len += 1; }
            cursor += 1;
            after_newline = false;
        }
    }
    (cursor, after_newline)
}

fn apply_output_cursor(
    buffer: &TextBuffer,
    output: &str,
    cursor_color: &RGBA,
    cursor_foreground: &RGBA,
) {
    let (cursor_col, after_newline) = output_cursor_col(output);

    let tag_table = buffer.tag_table();
    if tag_table.lookup("output-cursor").is_none() {
        let tag = gtk4::TextTag::new(Some("output-cursor"));
        tag_table.add(&tag);
    }
    if let Some(tag) = tag_table.lookup("output-cursor") {
        tag.set_background_rgba(Some(cursor_color));
        tag.set_foreground_rgba(Some(cursor_foreground));
    }

    let buffer_len = buffer.char_count() as usize;
    let cursor_abs: usize = if after_newline {
        buffer_len
    } else {
        let last_line = (buffer.line_count() - 1).max(0);
        if let Some(line_start) = buffer.iter_at_line(last_line) {
            line_start.offset() as usize + cursor_col
        } else {
            buffer_len
        }
    };

    if cursor_abs >= buffer.char_count() as usize {
        let mut end_iter = buffer.end_iter();
        buffer.insert(&mut end_iter, " ");
    }

    if let Some(tag) = tag_table.lookup("output-cursor") {
        let start = buffer.iter_at_offset(cursor_abs as i32);
        let end = buffer.iter_at_offset(cursor_abs as i32 + 1);
        buffer.apply_tag(&tag, &start, &end);
    }
}

fn set_active_output_buffer(
    buffer: &TextBuffer,
    output: &str,
    palette: &[RGBA; 16],
    cursor_colors: Option<(&RGBA, &RGBA)>,
) {
    let output_no_ansi = strip_ansi(output);
    let output_plain = output_no_ansi
        .lines()
        .map(|line| command_line_plain_text(line))
        .collect::<Vec<_>>()
        .join("\n");
    buffer.set_text(&output_plain);

    let output_runs = ansi_text_runs(output, palette);
    apply_ansi_runs_to_buffer(buffer, 0, &output_runs);

    if let Some((cursor_color, cursor_foreground)) = cursor_colors {
        apply_output_cursor(buffer, output, cursor_color, cursor_foreground);
    }
}

/// Incrementally append new output to buffer without full rewrite
fn append_active_output_buffer(
    buffer: &TextBuffer,
    full_output: &str,
    last_flushed_size: usize,
    palette: &[RGBA; 16],
    cursor_colors: Option<(&RGBA, &RGBA)>,
) {
    // Extract only the new portion
    if full_output.len() <= last_flushed_size {
        return; // Nothing new to append
    }

    let new_output = &full_output[last_flushed_size..];

    // If new output contains \r, it may need to overwrite previous lines
    // (e.g., progress updates like apt/curl). Fall back to full rewrite.
    if new_output.contains('\r') {
        set_active_output_buffer(buffer, full_output, palette, cursor_colors);
        return;
    }

    // Process new output (strip ANSI and handle carriage returns)
    let new_output_no_ansi = strip_ansi(new_output);
    let new_output_plain = new_output_no_ansi
        .lines()
        .map(|line| command_line_plain_text(line))
        .collect::<Vec<_>>()
        .join("\n");

    // Append the new text to the buffer
    let mut end_iter = buffer.end_iter();
    buffer.insert(&mut end_iter, &new_output_plain);

    // IMPORTANT: Parse ANSI runs from the FULL output, not just the new portion
    // ANSI color codes are stateful (e.g., setting yellow persists until reset)
    // Parsing only the new portion would lose the inherited color state
    let all_output_runs = ansi_text_runs(full_output, palette);

    // Clear all existing tags first to avoid stale coloring
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    buffer.remove_all_tags(&start, &end);

    // Re-apply all ANSI tags from the beginning
    apply_ansi_runs_to_buffer(buffer, 0, &all_output_runs);

    if let Some((cursor_color, cursor_foreground)) = cursor_colors {
        apply_output_cursor(buffer, full_output, cursor_color, cursor_foreground);
    }
}


fn ansi_to_pango(input: &str, palette: &[RGBA; 16]) -> String {
    let bytes = input.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let mut open_spans = 0;

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            let mut params = Vec::new();
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                if bytes[i] == b';' {
                    params.push(String::new());
                } else if let Some(last) = params.last_mut() {
                    last.push(bytes[i] as char);
                } else {
                    params.push(String::from(bytes[i] as char));
                }
                i += 1;
            }

            if i < bytes.len() {
                let final_byte = bytes[i];
                i += 1;

                if final_byte == b'm' {
                    if params.is_empty() || params[0].is_empty() {
                        params = vec!["0".to_string()];
                    }

                    for param_str in &params {
                        if param_str.is_empty() {
                            continue;
                        }
                        match param_str.parse::<u32>() {
                            Ok(0) => {
                                while open_spans > 0 {
                                    out.push_str("</span>");
                                    open_spans -= 1;
                                }
                            }
                            Ok(1) => {
                                out.push_str("<span weight=\"bold\">");
                                open_spans += 1;
                            }
                            Ok(2) => {
                                // Dim - used for shell suggestions/hints, show in italic with reduced opacity
                                out.push_str("<span style=\"italic\" alpha=\"65%\">");
                                open_spans += 1;
                            }
                            Ok(3) => {
                                out.push_str("<span style=\"italic\">");
                                open_spans += 1;
                            }
                            Ok(4) => {
                                out.push_str("<span underline=\"single\">");
                                open_spans += 1;
                            }
                            Ok(5) => {
                                // Blink - map to different opacity/style
                                out.push_str("<span alpha=\"60%\">");
                                open_spans += 1;
                            }
                            Ok(9) => {
                                out.push_str("<span strikethrough=\"true\">");
                                open_spans += 1;
                            }
                            Ok(7) => {
                                // Reverse video - for now use background/foreground swap via CSS
                                out.push_str("<span style=\"reverse\">");
                                open_spans += 1;
                            }
                            Ok(8) => {
                                // Hidden/conceal text - use very low opacity
                                out.push_str("<span alpha=\"5%\">");
                                open_spans += 1;
                            }
                            Ok(27) => {
                                out.push_str("</span>");
                                if open_spans > 0 { open_spans -= 1; }
                            }
                            Ok(28) => {
                                out.push_str("</span>");
                                if open_spans > 0 { open_spans -= 1; }
                            }
                            Ok(53) => {
                                // Overline - use overline attribute (if supported)
                                out.push_str("<span overline=\"single\">");
                                open_spans += 1;
                            }
                            Ok(30..=37) => {
                                let idx = (param_str.parse::<u32>().unwrap() - 30) as usize;
                                let (r, g, b) = ansi256_to_rgb(idx as u8, palette);
                                out.push_str(&format!(
                                    "<span foreground=\"#{:02x}{:02x}{:02x}\">",
                                    r, g, b
                                ));
                                open_spans += 1;
                            }
                            Ok(40..=47) => {
                                let idx = (param_str.parse::<u32>().unwrap() - 40) as usize;
                                let (r, g, b) = ansi256_to_rgb(idx as u8, palette);
                                out.push_str(&format!(
                                    "<span background=\"#{:02x}{:02x}{:02x}\">",
                                    r, g, b
                                ));
                                open_spans += 1;
                            }
                            Ok(90..=97) => {
                                let idx = (param_str.parse::<u32>().unwrap() - 90 + 8) as u8;
                                let (r, g, b) = ansi256_to_rgb(idx, palette);
                                out.push_str(&format!(
                                    "<span foreground=\"#{:02x}{:02x}{:02x}\">",
                                    r, g, b
                                ));
                                open_spans += 1;
                            }
                            Ok(100..=107) => {
                                let idx = (param_str.parse::<u32>().unwrap() - 100 + 8) as u8;
                                let (r, g, b) = ansi256_to_rgb(idx, palette);
                                out.push_str(&format!(
                                    "<span background=\"#{:02x}{:02x}{:02x}\">",
                                    r, g, b
                                ));
                                open_spans += 1;
                            }
                            Ok(38) => {
                                let j = params.iter().position(|p| p == param_str).unwrap_or(0);
                                if j + 2 < params.len() {
                                    if params[j + 1] == "5" {
                                        if let Ok(idx) = params[j + 2].parse::<u8>() {
                                            let (r, g, b) = ansi256_to_rgb(idx, palette);
                                            out.push_str(&format!(
                                                "<span foreground=\"#{:02x}{:02x}{:02x}\">",
                                                r, g, b
                                            ));
                                            open_spans += 1;
                                        }
                                    } else if params[j + 1] == "2" && j + 4 < params.len() {
                                        if let (Ok(r), Ok(g), Ok(b)) = (
                                            params[j + 2].parse::<u8>(),
                                            params[j + 3].parse::<u8>(),
                                            params[j + 4].parse::<u8>(),
                                        ) {
                                            out.push_str(&format!(
                                                "<span foreground=\"#{:02x}{:02x}{:02x}\">",
                                                r, g, b
                                            ));
                                            open_spans += 1;
                                        }
                                    }
                                }
                            }
                            Ok(48) => {
                                let j = params.iter().position(|p| p == param_str).unwrap_or(0);
                                if j + 2 < params.len() {
                                    if params[j + 1] == "5" {
                                        if let Ok(idx) = params[j + 2].parse::<u8>() {
                                            let (r, g, b) = ansi256_to_rgb(idx, palette);
                                            out.push_str(&format!(
                                                "<span background=\"#{:02x}{:02x}{:02x}\">",
                                                r, g, b
                                            ));
                                            open_spans += 1;
                                        }
                                    } else if params[j + 1] == "2" && j + 4 < params.len() {
                                        if let (Ok(r), Ok(g), Ok(b)) = (
                                            params[j + 2].parse::<u8>(),
                                            params[j + 3].parse::<u8>(),
                                            params[j + 4].parse::<u8>(),
                                        ) {
                                            out.push_str(&format!(
                                                "<span background=\"#{:02x}{:02x}{:02x}\">",
                                                r, g, b
                                            ));
                                            open_spans += 1;
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        } else if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b']' {
            i = skip_osc_sequence(bytes, i);
        } else if bytes[i] == 0x1b && i + 1 < bytes.len() {
            i = skip_escape_sequence(bytes, i);
        } else {
            // Collect UTF-8 characters
            let ch_start = i;
            let ch_len = if bytes[i] & 0x80 == 0 {
                1
            } else if bytes[i] & 0xe0 == 0xc0 {
                2
            } else if bytes[i] & 0xf0 == 0xe0 {
                3
            } else if bytes[i] & 0xf8 == 0xf0 {
                4
            } else {
                1
            };
            i += ch_len;

            if i > bytes.len() {
                i = bytes.len();
            }

            let char_bytes = &bytes[ch_start..i];
            match String::from_utf8(char_bytes.to_vec()) {
                Ok(s) => {
                    for ch in s.chars() {
                        match ch {
                            '<' => out.push_str("&lt;"),
                            '>' => out.push_str("&gt;"),
                            '&' => out.push_str("&amp;"),
                            '"' => out.push_str("&quot;"),
                            '\'' => out.push_str("&apos;"),
                            _ => out.push(ch),
                        }
                    }
                }
                Err(_) => {
                    // Replacement character for invalid UTF-8
                    out.push('\u{FFFD}');
                }
            }
        }
    }

    while open_spans > 0 {
        out.push_str("</span>");
        open_spans -= 1;
    }

    out
}

fn ansi_to_pango_cached(
    input: &str,
    palette: &[RGBA; 16],
    cache: &std::collections::HashMap<String, String>,
) -> (String, bool) {
    if let Some(cached) = cache.get(input) {
        (cached.clone(), true)
    } else {
        (ansi_to_pango(input, palette), false)
    }
}

// ─── FinishedBlock ────────────────────────────────────────────────────────────

/// Data for a finished command block (decoupled from widget representation)
#[derive(Clone, Serialize, Deserialize)]
struct BlockData {
    id: u64,
    prompt: String,
    cmd: String,
    cmd_markup: Option<String>,
    output: String,
    exit_code: i32,
    estimated_height: i32,
    line_count: usize,
    #[serde(default)]
    start_time: Option<SystemTime>,
    #[serde(default)]
    end_time: Option<SystemTime>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    cwd: Option<String>,
}

impl BlockData {
    /// Export block to JSON format
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Export block to Markdown format
    pub fn to_markdown(&self) -> String {
        let mut md = String::new();

        md.push_str("## Command Block\n\n");

        if !self.prompt.is_empty() {
            md.push_str(&format!("**Prompt:** `{}`\n\n", self.prompt));
        }

        md.push_str("**Command:**\n```bash\n");
        md.push_str(&self.cmd);
        md.push_str("\n```\n\n");

        if !self.output.is_empty() {
            md.push_str("**Output:**\n```\n");
            md.push_str(&self.output);
            md.push_str("\n```\n\n");
        }

        md.push_str(&format!("**Exit Code:** {}\n\n", self.exit_code));

        if let Some(dur) = self.duration_ms {
            let dur_sec = dur as f64 / 1000.0;
            md.push_str(&format!("**Duration:** {:.3}s\n\n", dur_sec));
        }

        md
    }
}

/// Filters for searching/filtering blocks
#[derive(Clone, Default)]
pub struct BlockFilters {
    /// Filter by exit code (e.g., Some(0) = only successful, Some(1) = only failed)
    pub exit_code: Option<i32>,
    /// Filter by minimum duration in milliseconds
    pub min_duration_ms: Option<u64>,
    /// Filter by maximum duration in milliseconds
    pub max_duration_ms: Option<u64>,
    /// Show only failed commands (exit_code != 0)
    pub failed_only: bool,
    /// Show only slow commands (duration > threshold)
    pub slow_only: bool,
    /// Slow threshold in milliseconds (default 1000ms)
    pub slow_threshold_ms: u64,
}

struct FinishedBlock {
    id: u64,
    widget: gtk4::Box,
    prompt_view: gtk4::TextView,
    prompt_buffer: gtk4::TextBuffer,
    command_view: gtk4::TextView,
    command_buffer: gtk4::TextBuffer,
    output_view: gtk4::TextView,
    output_buffer: gtk4::TextBuffer,
    show_more_btn: Option<gtk4::Button>,
    full_output: Rc<RefCell<String>>,
    cmd_text: String,
}

impl Clone for FinishedBlock {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            widget: self.widget.clone(),
            prompt_view: self.prompt_view.clone(),
            prompt_buffer: self.prompt_buffer.clone(),
            command_view: self.command_view.clone(),
            command_buffer: self.command_buffer.clone(),
            output_view: self.output_view.clone(),
            output_buffer: self.output_buffer.clone(),
            show_more_btn: self.show_more_btn.clone(),
            cmd_text: self.cmd_text.clone(),
            full_output: self.full_output.clone(),
        }
    }
}

impl FinishedBlock {
    fn new(
        prompt: &str,
        cmd: &str,
        _cmd_markup: Option<&str>,
        output: &str,
        exit_code: i32,
        config: &Config,
        duration_ms: Option<u64>,
        end_time: Option<SystemTime>,
        cwd: Option<&str>,
    ) -> Self {
        let view_margin_top = 2;
        let view_margin_bottom = 2;

        let outer = gtk4::Box::new(Orientation::Vertical, 0);
        outer.add_css_class("block-finished");
        outer.set_margin_top(4);
        outer.set_margin_bottom(4);
        outer.set_margin_start(8);
        outer.set_margin_end(8);

        // Add hover highlighting to show block is interactive
        let hover_ctrl = gtk4::EventControllerMotion::new();
        let outer_for_enter = outer.clone();
        hover_ctrl.connect_enter(move |_, _, _| {
            outer_for_enter.add_css_class("block-hovered");
        });
        let outer_for_leave = outer.clone();
        hover_ctrl.connect_leave(move |_| {
            outer_for_leave.remove_css_class("block-hovered");
        });
        outer.add_controller(hover_ctrl);

        // ── Header row ──────────────────────────────────────────────────────
        let header_row = gtk4::Box::new(Orientation::Horizontal, 8);
        header_row.add_css_class("block-header");
        header_row.set_margin_start(12);
        header_row.set_margin_end(8);
        header_row.set_margin_top(6);
        header_row.set_margin_bottom(2);

        // CWD label (shortened to last 2 segments)
        if let Some(cwd_path) = cwd {
            let shortened = shorten_path(cwd_path);
            let cwd_label = gtk4::Label::new(Some(&shortened));
            cwd_label.add_css_class("block-header-label");
            cwd_label.set_halign(gtk4::Align::Start);
            cwd_label.set_ellipsize(gtk4::pango::EllipsizeMode::Start);
            cwd_label.set_max_width_chars(40);
            header_row.append(&cwd_label);
        }

        // Spacer
        let spacer = gtk4::Box::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        header_row.append(&spacer);

        // Timestamp label
        if let Some(et) = end_time {
            if let Ok(duration_since_epoch) = et.duration_since(SystemTime::UNIX_EPOCH) {
                let secs = duration_since_epoch.as_secs();
                let hours = (secs % 86400) / 3600;
                let mins = (secs % 3600) / 60;
                let s = secs % 60;
                // Adjust to local timezone offset (approximate)
                let local_offset = chrono_local_offset_secs();
                let local_secs = secs as i64 + local_offset;
                let local_secs = local_secs.rem_euclid(86400) as u64;
                let h = local_secs / 3600;
                let m = (local_secs % 3600) / 60;
                let sec = local_secs % 60;
                let _ = (hours, mins, s); // suppress warnings
                let ts_label = gtk4::Label::new(Some(&format!("{:02}:{:02}:{:02}", h, m, sec)));
                ts_label.add_css_class("block-header-label");
                header_row.append(&ts_label);
            }
        }

        // Duration badge
        if let Some(dur_ms) = duration_ms {
            let dur_sec = dur_ms as f64 / 1000.0;
            let duration_text = if dur_sec < 1.0 {
                format!("{:.0}ms", dur_ms)
            } else if dur_sec < 60.0 {
                format!("{:.1}s", dur_sec)
            } else {
                let min = dur_sec / 60.0;
                format!("{:.0}m", min)
            };
            let dur_label = gtk4::Label::new(Some(&duration_text));
            dur_label.add_css_class("block-meta-badge");
            header_row.append(&dur_label);
        }

        // Exit code badge
        if exit_code != 0 {
            let badge = gtk4::Label::new(Some(&format!("exit:{}", exit_code)));
            badge.add_css_class("block-exit-bad");
            header_row.append(&badge);
        }

        // Collapse toggle button
        let collapse_btn = gtk4::Button::with_label("\u{25BC}"); // ▼
        collapse_btn.add_css_class("block-collapse-btn");
        collapse_btn.add_css_class("flat");
        header_row.append(&collapse_btn);

        outer.append(&header_row);

        // ── Text Views ──────────────────────────────────────────────────────
        // Helper to create TextView
        let create_textview = |css_class: &str| -> (gtk4::TextView, gtk4::TextBuffer) {
            let buffer = gtk4::TextBuffer::new(None);
            let view = gtk4::TextView::with_buffer(&buffer);
            view.add_css_class(css_class);
            view.set_editable(false);
            view.set_cursor_visible(false);
            view.set_can_focus(true);
            view.set_focusable(true);
            view.set_hexpand(true);
            view.set_vexpand(false);
            view.set_valign(gtk4::Align::Start);
            view.set_wrap_mode(gtk4::WrapMode::Char);
            view.set_left_margin(12);
            view.set_right_margin(8);
            view.set_top_margin(view_margin_top);
            view.set_bottom_margin(view_margin_bottom);
            view.set_monospace(true);
            view.set_accepts_tab(false);
            (view, buffer)
        };

        let (prompt_view, prompt_buffer) = create_textview("block-prompt-view");
        let (command_view, command_buffer) = create_textview("block-command-view");
        let (output_view, output_buffer) = create_textview("block-output-view");

        // Populate buffers
        set_active_prompt_buffer(&prompt_buffer, prompt);

        let cmd_display = if cmd.is_empty() { "(empty)" } else { cmd };
        command_buffer.set_text(cmd_display);

        // Explicitly remove any cursor tags from finished block command buffer
        let tag_table = command_buffer.tag_table();
        if let Some(cursor_tag) = tag_table.lookup("cursor") {
            let start = command_buffer.start_iter();
            let end = command_buffer.end_iter();
            command_buffer.remove_tag(&cursor_tag, &start, &end);
        }

        // Output truncation: show first N lines with "Show more" button for long output
        let threshold = config.max_collapsed_output_lines as usize;
        let output_lines: Vec<&str> = output.lines().collect();
        let total_lines = output_lines.len();
        let is_truncated = total_lines > threshold;
        let full_output: Rc<RefCell<String>> = Rc::new(RefCell::new(output.to_string()));

        let display_output = if is_truncated {
            output_lines[..threshold].join("\n")
        } else {
            output.to_string()
        };
        set_active_output_buffer(&output_buffer, &display_output, &config.palette, None);

        // Add Ctrl+Click handler to open URLs in command and output views
        for (view, buffer) in [(&command_view, &command_buffer), (&output_view, &output_buffer)] {
            let click_controller = gtk4::GestureClick::new();
            click_controller.set_button(1); // left click
            click_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);

            let buffer_clone = buffer.clone();
            let view_clone = view.clone();
            click_controller.connect_pressed(move |controller, n_press, x, y| {
                if n_press == 1 {
                    let state = controller.current_event_state();
                    if state.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
                        let (bx, by) = view_clone.window_to_buffer_coords(
                            gtk4::TextWindowType::Widget,
                            x as i32,
                            y as i32,
                        );
                        if let Some(iter) = view_clone.iter_at_location(bx, by) {
                            if let Some(url) = get_url_at_position(&buffer_clone, &iter) {
                                open_uri(&url);
                                controller.set_state(gtk4::EventSequenceState::Claimed);
                                return;
                            }
                        }
                    }
                }
                controller.set_state(gtk4::EventSequenceState::Denied);
            });

            view.add_controller(click_controller);

            // URL hover: underline + pointer cursor on mouse over
            let url_tag = gtk4::TextTag::new(Some("url-hover"));
            url_tag.set_underline(gtk4::pango::Underline::Single);
            buffer.tag_table().add(&url_tag);

            let motion_ctrl = gtk4::EventControllerMotion::new();
            let view_for_motion = view.clone();
            let buffer_for_motion = buffer.clone();
            let tag_for_motion = url_tag.clone();
            motion_ctrl.connect_motion(move |_ctrl, x, y| {
                let (bx, by) = view_for_motion.window_to_buffer_coords(
                    gtk4::TextWindowType::Widget,
                    x as i32,
                    y as i32,
                );
                let start = buffer_for_motion.start_iter();
                let end = buffer_for_motion.end_iter();
                buffer_for_motion.remove_tag(&tag_for_motion, &start, &end);

                if let Some(iter) = view_for_motion.iter_at_location(bx, by) {
                    if let Some((url_start, url_end, _)) =
                        get_url_bounds_at_position(&buffer_for_motion, &iter)
                    {
                        buffer_for_motion.apply_tag(&tag_for_motion, &url_start, &url_end);
                        view_for_motion.set_cursor(
                            gtk4::gdk::Cursor::from_name("pointer", None).as_ref(),
                        );
                        return;
                    }
                }
                view_for_motion.set_cursor(
                    gtk4::gdk::Cursor::from_name("text", None).as_ref(),
                );
            });

            let view_for_leave = view.clone();
            let buffer_for_leave = buffer.clone();
            let tag_for_leave = url_tag;
            motion_ctrl.connect_leave(move |_| {
                let start = buffer_for_leave.start_iter();
                let end = buffer_for_leave.end_iter();
                buffer_for_leave.remove_tag(&tag_for_leave, &start, &end);
                view_for_leave.set_cursor(
                    gtk4::gdk::Cursor::from_name("text", None).as_ref(),
                );
            });

            view.add_controller(motion_ctrl);
        }

        // Append views to outer box
        outer.append(&command_view);
        outer.append(&output_view);

        // "Show more" button for truncated output
        let show_more_btn = if is_truncated {
            let remaining = total_lines - threshold;
            let btn = gtk4::Button::with_label(&format!("Show more ({} more lines)", remaining));
            btn.add_css_class("block-show-more");
            btn.add_css_class("flat");
            outer.append(&btn);

            let is_expanded = Rc::new(Cell::new(false));
            let output_buffer_clone = output_buffer.clone();
            let palette = config.palette;
            let full_output_clone = full_output.clone();
            let is_expanded_clone = is_expanded.clone();

            btn.connect_clicked(move |btn| {
                let expanded = is_expanded_clone.get();
                if expanded {
                    let full = full_output_clone.borrow();
                    let lines: Vec<&str> = full.lines().collect();
                    let truncated = lines[..threshold].join("\n");
                    set_active_output_buffer(&output_buffer_clone, &truncated, &palette, None);
                    let remaining = lines.len() - threshold;
                    btn.set_label(&format!("Show more ({} more lines)", remaining));
                    is_expanded_clone.set(false);
                } else {
                    let full = full_output_clone.borrow();
                    set_active_output_buffer(&output_buffer_clone, &full, &palette, None);
                    btn.set_label("Show less");
                    is_expanded_clone.set(true);
                }
            });

            Some(btn)
        } else {
            None
        };

        // Wire collapse button to toggle output visibility
        let output_view_for_collapse = output_view.clone();
        let show_more_for_collapse = show_more_btn.clone();
        let has_output = !output.trim().is_empty();
        if !has_output {
            output_view.set_visible(false);
        }
        collapse_btn.connect_clicked(move |btn| {
            let visible = output_view_for_collapse.is_visible();
            output_view_for_collapse.set_visible(!visible);
            if let Some(ref smb) = show_more_for_collapse {
                smb.set_visible(!visible);
            }
            btn.set_label(if visible { "\u{25B6}" } else { "\u{25BC}" }); // ▶ / ▼
        });
        if !has_output {
            collapse_btn.set_label("\u{25B6}"); // ▶
        }

        FinishedBlock {
            id: next_block_id(),
            widget: outer,
            prompt_view,
            prompt_buffer,
            command_view,
            command_buffer,
            output_view,
            output_buffer,
            show_more_btn,
            full_output,
            cmd_text: cmd.to_string(),
        }
    }

    fn widget(&self) -> &gtk4::Box {
        &self.widget
    }
}

// ─── ActiveBlock ──────────────────────────────────────────────────────────────

struct ActiveBlock {
    widget: gtk4::Box,
    prompt_view: gtk4::TextView,
    prompt_buffer: gtk4::TextBuffer,
    command_view: gtk4::TextView,
    command_buffer: gtk4::TextBuffer,
    output_vte: Terminal,
    raw_output: Rc<RefCell<Vec<u8>>>,
    pending_cmd: Rc<RefCell<String>>,        // User input only
    pending_preedit: Rc<RefCell<String>>,    // IME composing text
    pending_suggestion: Rc<RefCell<String>>, // Shell suggestion/autocomplete
    cursor_visible: Rc<Cell<bool>>, // For blinking cursor animation
    cursor_offset: Rc<Cell<usize>>, // Cursor position in chars (editor mode)
    cursor_color: RGBA,
    cursor_foreground: RGBA,
    running_label: gtk4::Label,
    running_timer_handle: Rc<RefCell<Option<glib::SourceId>>>,
}

impl ActiveBlock {
    fn new(_batch_min_ms: u32, _batch_max_ms: u32, config: &Config) -> Self {
        let widget = gtk4::Box::new(Orientation::Vertical, 0);
        widget.add_css_class("block-active");
        widget.set_margin_top(4);
        widget.set_margin_bottom(2);
        widget.set_can_focus(false);
        widget.set_can_target(false);
        widget.set_focusable(false);

        // Helper to create and configure a TextView
        let create_textview = |css_class: &str, editable: bool| -> (gtk4::TextView, gtk4::TextBuffer) {
            let buffer = TextBuffer::new(None);
            let view = TextView::with_buffer(&buffer);
            view.add_css_class(css_class);
            view.set_editable(editable);
            view.set_cursor_visible(editable);
            view.set_can_focus(true);
            view.set_focusable(true);
            view.set_hexpand(true);
            view.set_vexpand(false);
            view.set_wrap_mode(gtk4::WrapMode::Char);
            view.set_left_margin(12);
            view.set_right_margin(8);
            view.set_top_margin(0);
            view.set_bottom_margin(0);
            view.set_monospace(true);

            if !editable {
                let key_controller = EventControllerKey::new();
                key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
                key_controller.connect_key_pressed(|_controller, _key, _code, _modifier| {
                    glib::Propagation::Stop
                });
                view.add_controller(key_controller);
            }

            (view, buffer)
        };

        let (prompt_view, prompt_buffer) = create_textview("block-prompt-view", false);
        let (command_view, command_buffer) = create_textview("block-command-view", false);

        // Running timer label (shown during command execution)
        let running_label = gtk4::Label::new(None);
        running_label.add_css_class("block-running-label");
        running_label.set_halign(gtk4::Align::End);
        running_label.set_hexpand(true);
        running_label.set_visible(false);

        // Prompt row: prompt_view + running_label
        let prompt_row = gtk4::Box::new(Orientation::Horizontal, 4);
        prompt_row.append(&prompt_view);
        prompt_row.append(&running_label);

        // Output: use VTE widget for full terminal compatibility
        let output_vte = build_output_vte(config);
        output_vte.set_visible(false); // Hidden until there's output

        // Append to widget
        widget.append(&prompt_row);
        widget.append(&command_view);
        widget.append(&output_vte);

        // Grab focus on command_view when realized
        let command_view_clone = command_view.clone();
        command_view.connect_realize(move |_| {
            command_view_clone.grab_focus();
        });

        let cursor_visible = Rc::new(Cell::new(true));
        let cursor_offset: Rc<Cell<usize>> = Rc::new(Cell::new(0));
        let pending_cmd = Rc::new(RefCell::new(String::new()));
        let pending_preedit = Rc::new(RefCell::new(String::new()));
        let pending_suggestion = Rc::new(RefCell::new(String::new()));

        {
            // Manual cursor blink animation (both editor and non-editor modes)
            let cursor_visible_clone = cursor_visible.clone();
            let cursor_offset_clone = cursor_offset.clone();
            let command_buffer_clone = command_buffer.clone();
            let pending_cmd_clone = pending_cmd.clone();
            let pending_preedit_clone = pending_preedit.clone();
            let pending_suggestion_clone = pending_suggestion.clone();
            let cursor_color_for_timer = config.cursor;
            let cursor_foreground_for_timer = config.cursor_foreground;

            glib::timeout_add_local(std::time::Duration::from_millis(530), move || {
                cursor_visible_clone.set(!cursor_visible_clone.get());

                let cmd = pending_cmd_clone.borrow();
                let preedit = pending_preedit_clone.borrow();
                let suggestion = pending_suggestion_clone.borrow();

                let cur_pos = cursor_offset_clone.get();
                let default_pos = cmd.chars().count() + preedit.chars().count();
                let explicit_pos = if cur_pos != default_pos { Some(cur_pos) } else { None };

                set_active_command_buffer_at(
                    &command_buffer_clone,
                    &cmd,
                    &preedit,
                    cursor_visible_clone.get(),
                    &suggestion,
                    &cursor_color_for_timer,
                    &cursor_foreground_for_timer,
                    explicit_pos,
                );

                glib::ControlFlow::Continue
            });

            set_active_command_buffer_at(
                &command_buffer,
                "",
                "",
                true,
                "",
                &config.cursor,
                &config.cursor_foreground,
                None,
            );
        }

        ActiveBlock {
            widget,
            prompt_view,
            prompt_buffer,
            command_view,
            command_buffer,
            output_vte,
            raw_output: Rc::new(RefCell::new(Vec::new())),
            pending_cmd,
            pending_preedit,
            pending_suggestion,
            cursor_visible,
            cursor_offset,
            cursor_color: config.cursor,
            cursor_foreground: config.cursor_foreground,
            running_label,
            running_timer_handle: Rc::new(RefCell::new(None)),
        }
    }

    fn set_prompt(&self, text: &str) {
        set_active_prompt_buffer(&self.prompt_buffer, text);
    }

    fn set_cmd(&self, text: &str) {
        log::debug!("set_cmd: text={:?}", text);
        *self.pending_cmd.borrow_mut() = text.to_string();
        self.pending_preedit.borrow_mut().clear();
        *self.pending_suggestion.borrow_mut() = String::new();
        self.cursor_offset.set(text.chars().count());
        self.update_content_view();
    }

    fn set_preedit(&self, text: &str) {
        *self.pending_preedit.borrow_mut() = text.to_string();
        self.update_content_view();
    }

    fn set_cmd_parts(&self, user_part: &str, suggestion_part: &str) {
        let user_plain = plain_text_from_ansi(user_part);
        let suggestion_plain = plain_text_from_ansi(suggestion_part);

        log::debug!(
            "set_cmd_parts: user={:?}, suggestion={:?}",
            user_plain,
            suggestion_plain
        );

        *self.pending_cmd.borrow_mut() = user_plain;
        self.pending_preedit.borrow_mut().clear();
        *self.pending_suggestion.borrow_mut() = suggestion_plain;
        self.update_content_view();
    }

    fn update_content_view(&self) {
        if self.command_view.is_editable() {
            return;
        }
        let cmd = self.pending_cmd.borrow();
        let preedit = self.pending_preedit.borrow();
        let suggestion = self.pending_suggestion.borrow();
        log::debug!(
            "update_content_view: cmd={:?}, suggestion={:?}, cursor_visible={}",
            cmd,
            suggestion,
            self.cursor_visible.get()
        );

        let cursor_pos = self.cursor_offset.get();
        let default_pos = cmd.chars().count() + preedit.chars().count();
        let explicit_pos = if cursor_pos != default_pos {
            Some(cursor_pos)
        } else {
            None
        };

        set_active_command_buffer_at(
            &self.command_buffer,
            &cmd,
            &preedit,
            self.cursor_visible.get(),
            &suggestion,
            &self.cursor_color,
            &self.cursor_foreground,
            explicit_pos,
        );
    }

    fn feed_output(&self, raw_bytes: &[u8]) {
        if !self.output_vte.is_visible() {
            self.output_vte.set_visible(true);
        }
        self.raw_output.borrow_mut().extend_from_slice(raw_bytes);
        self.output_vte.feed(raw_bytes);
    }

    fn flush_output(&self) {
        // VTE renders immediately on feed(), no flush needed
    }

    fn output_text(&self) -> String {
        let raw = self.raw_output.borrow();
        if raw.is_empty() {
            return String::new();
        }
        String::from_utf8_lossy(&raw).into_owned()
    }

    fn append_output(&self, text: &str) {
        self.feed_output(text.as_bytes());
    }

    fn clear_output(&self) {
        self.raw_output.borrow_mut().clear();
        self.output_vte.reset(true, true);
        self.output_vte.set_visible(false);
    }

    fn start_command(&self, command: &str) {
        *self.pending_cmd.borrow_mut() = command.to_string();
        self.pending_preedit.borrow_mut().clear();
        self.pending_suggestion.borrow_mut().clear();
        self.clear_output();
        self.command_buffer.set_text("");
    }

    fn start_timer(&self) {
        self.stop_timer();
        self.running_label.set_text("0s");
        self.running_label.set_visible(true);
        let label = self.running_label.clone();
        let start = std::time::Instant::now();
        let handle = glib::timeout_add_local(
            std::time::Duration::from_secs(1),
            move || {
                let elapsed = start.elapsed().as_secs();
                let text = if elapsed < 60 {
                    format!("{}s", elapsed)
                } else {
                    format!("{}m{}s", elapsed / 60, elapsed % 60)
                };
                label.set_text(&text);
                glib::ControlFlow::Continue
            },
        );
        *self.running_timer_handle.borrow_mut() = Some(handle);
    }

    fn stop_timer(&self) {
        if let Some(handle) = self.running_timer_handle.borrow_mut().take() {
            handle.remove();
        }
        self.running_label.set_visible(false);
    }

    fn reset_for_next_prompt(&self) {
        self.stop_timer();
        self.set_prompt("");
        *self.pending_cmd.borrow_mut() = String::new();
        self.pending_preedit.borrow_mut().clear();
        self.pending_suggestion.borrow_mut().clear();
        self.clear_output();
        self.cursor_visible.set(true);
        self.cursor_offset.set(0);
        self.command_buffer.set_text("");
    }

    fn widget(&self) -> &gtk4::Box {
        &self.widget
    }

    fn grab_focus(&self) {
        self.command_view.grab_focus();
    }
}

// ─── TermView state machine ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum BlockState {
    /// Waiting for first PromptStart or any bytes
    Idle,
    /// Between PromptStart and PromptEnd — collecting prompt text
    CollectingPrompt,
    /// Between PromptEnd and CommandStart — user is typing
    AwaitingCommand,
    /// Between CommandStart and CommandEnd — collecting output
    CollectingOutput,
    /// Inside full-screen app (vim/less/etc.)
    AltScreen,
}

// ─── Virtual Scrolling ────────────────────────────────────────────────────────

struct ViewportState {
    first_visible: usize,
    last_visible: usize,
    total_height: i32,
}

impl Clone for ViewportState {
    fn clone(&self) -> Self {
        Self {
            first_visible: self.first_visible,
            last_visible: self.last_visible,
            total_height: self.total_height,
        }
    }
}

struct WidgetPool {
    available: Vec<gtk4::Box>,
    max_pool_size: usize,
}

impl WidgetPool {
    fn new() -> Self {
        Self {
            available: Vec::new(),
            max_pool_size: 20,
        }
    }

    fn acquire(&mut self) -> Option<gtk4::Box> {
        self.available.pop()
    }

    fn release(&mut self, widget: gtk4::Box) {
        if self.available.len() < self.max_pool_size {
            self.available.push(widget);
        }
    }
}

// ─── TermView ─────────────────────────────────────────────────────────────────

pub struct TermView {
    root: gtk4::Box,
    block_scroll: ScrolledWindow,
    block_list: gtk4::Box,
    vte_box: gtk4::Box,
    vte: Terminal,
    active: Rc<RefCell<ActiveBlock>>,
    bstate: Rc<Cell<BlockState>>,
    prompt_buf: Rc<RefCell<String>>,
    cmd_buf: Rc<RefCell<String>>,
    pty: Rc<OwnedPty>,
    cwd_callbacks: Rc<RefCell<Vec<Box<dyn Fn(&str)>>>>,
    exited_callbacks: Rc<RefCell<Vec<Box<dyn Fn(i32)>>>>,
    bell_callbacks: Rc<RefCell<Vec<Box<dyn Fn()>>>>,
    title_callbacks: Rc<RefCell<Vec<Box<dyn Fn(&str)>>>>,
    activity_callbacks: Rc<RefCell<Vec<Box<dyn Fn()>>>>,
    bracketed_paste_mode: Rc<Cell<bool>>,
    application_cursor_mode: Rc<Cell<bool>>,
    mouse_reporting_mode: Rc<Cell<MouseReportingMode>>,
    cursor_shape: Rc<Cell<TermCursorShape>>,
    config: Rc<RefCell<Config>>,
    block_data: Rc<RefCell<VecDeque<BlockData>>>,
    finished_blocks: Rc<RefCell<Vec<FinishedBlock>>>,
    ansi_cache: Rc<RefCell<LruCache<String, String>>>,
    viewport: Rc<RefCell<ViewportState>>,
    widget_pool: Rc<RefCell<WidgetPool>>,
    visible_indices: Rc<RefCell<std::collections::HashSet<usize>>>,
    search_cache: Rc<std::sync::Mutex<std::collections::HashMap<String, Vec<usize>>>>, // Cache search results
    selected_block_id: Rc<Cell<Option<u64>>>,
}

impl TermView {
    pub fn new(
        config: &Config,
        shell_argv: &[String],
        cwd: Option<&str>,
        session_id: Option<&str>,
        initial_commands: Option<&str>,
    ) -> Self {
        // ── Build widget tree ──────────────────────────────────────────────
        let root = gtk4::Box::new(Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.add_css_class("term-view-root");

        // Block list inside a scrolled window
        let block_list = gtk4::Box::new(Orientation::Vertical, 0);
        block_list.set_vexpand(false); // Don't expand - only take space needed
        block_list.set_valign(gtk4::Align::Start); // Align to top
        block_list.set_margin_bottom(8);
        block_list.add_css_class("block-list");

        let block_scroll = ScrolledWindow::new();
        block_scroll.set_hexpand(true);
        block_scroll.set_vexpand(true);
        block_scroll.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Automatic);
        block_scroll.set_child(Some(&block_list));
        block_scroll.add_css_class("block-scroll");

        // Active block always at bottom
        let active = Rc::new(RefCell::new(ActiveBlock::new(
            config.output_batch_min_ms,
            config.output_batch_max_ms,
            config,
        )));
        // Active block is pinned outside the scroll area (appended to root below)

        // VTE fallback for alt-screen mode
        let vte = build_vte(config);
        let vte_scrollbar = gtk4::Scrollbar::new(Orientation::Vertical, vte.vadjustment().as_ref());
        let vte_box = gtk4::Box::new(Orientation::Horizontal, 0);
        vte_box.set_hexpand(true);
        vte_box.set_vexpand(true);
        vte_box.add_css_class("terminal-box"); // Allow find_first_terminal to discover the VTE inside
        vte_box.append(&vte);
        vte_box.append(&vte_scrollbar);
        vte_box.set_visible(false); // hidden until alt-screen

        block_list.append(active.borrow().widget());
        root.append(&block_scroll);
        root.append(&vte_box);

        // ── PTY ───────────────────────────────────────────────────────────
        // Detect rsh shell for session_id passing
        let is_rsh = shell_argv.first()
            .and_then(|s| std::path::Path::new(s).file_name())
            .and_then(|f| f.to_str())
            .map(|name| name == "rsh")
            .unwrap_or(false);

        // Build argv with optional --session for rsh
        let mut argv_vec: Vec<String> = shell_argv.to_vec();
        if let Some(sid) = session_id {
            if is_rsh {
                argv_vec.push("--session".to_string());
                argv_vec.push(sid.to_string());
            }
        }
        let argv: Vec<&str> = argv_vec.iter().map(|s| s.as_str()).collect();

        let mut env_extra: Vec<(&str, &str)> = vec![];
        let session_id_owned = session_id.map(|s| s.to_string());
        if let Some(ref sid) = session_id_owned {
            if is_rsh {
                env_extra.push(("RSH_SESSION_ID", sid.as_str()));
            }
        }

        let pty = Rc::new(OwnedPty::spawn(&argv, cwd, &env_extra).expect("PTY spawn failed"));

        // Store child PID on VTE widget so kill_all_terminal_children can find it
        unsafe {
            vte.set_data::<i32>("child-pid", pty.pid_i32());
        }

        // ── Register CSS ──────────────────────────────────────────────────
        install_block_css(config);

        // ── Shared state ──────────────────────────────────────────────────
        let bstate = Rc::new(Cell::new(BlockState::Idle));
        let prompt_buf: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let cmd_buf: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let cmd_display_raw: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let cmd_display_markup: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let last_nonempty_cmd_raw: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let last_nonempty_cmd_markup: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let executing_cmd_raw: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let executing_cmd_markup: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let cwd_callbacks: Rc<RefCell<Vec<Box<dyn Fn(&str)>>>> = Rc::new(RefCell::new(vec![]));
        let exited_callbacks: Rc<RefCell<Vec<Box<dyn Fn(i32)>>>> = Rc::new(RefCell::new(vec![]));
        let bell_callbacks: Rc<RefCell<Vec<Box<dyn Fn()>>>> = Rc::new(RefCell::new(vec![]));
        let title_callbacks: Rc<RefCell<Vec<Box<dyn Fn(&str)>>>> = Rc::new(RefCell::new(vec![]));
        let activity_callbacks: Rc<RefCell<Vec<Box<dyn Fn()>>>> = Rc::new(RefCell::new(vec![]));
        let bracketed_paste_mode: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let application_cursor_mode: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let mouse_reporting_mode: Rc<Cell<MouseReportingMode>> = Rc::new(Cell::new(MouseReportingMode::None));
        let cursor_shape: Rc<Cell<TermCursorShape>> = Rc::new(Cell::new(TermCursorShape::Block));
        let block_data_rc: Rc<RefCell<VecDeque<BlockData>>> =
            Rc::new(RefCell::new(VecDeque::new()));
        let finished_blocks_rc: Rc<RefCell<Vec<FinishedBlock>>> = Rc::new(RefCell::new(Vec::new()));
        let pager_snapshots: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let pager_snapshot_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        let ansi_cache: Rc<RefCell<LruCache<String, String>>> = Rc::new(RefCell::new(
            LruCache::new(NonZeroUsize::new(config.ansi_cache_capacity as usize).unwrap()),
        ));

        let widget_pool: Rc<RefCell<WidgetPool>> = Rc::new(RefCell::new(WidgetPool::new()));
        let pty_synced: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let tab_pending: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let completion_active: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let user_scrolled_up: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let programmatic_scroll: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let selected_block_id: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));

        // ── Wire PTY → parser → block events ─────────────────────────────
        {
            let active_rc = active.clone();
            let bstate_rc = bstate.clone();
            let prompt_buf_rc = prompt_buf.clone();
            let cmd_buf_rc = cmd_buf.clone();
            let cmd_display_raw_rc = cmd_display_raw.clone();
            let cmd_display_markup_rc = cmd_display_markup.clone();
            let last_nonempty_cmd_raw_rc = last_nonempty_cmd_raw.clone();
            let last_nonempty_cmd_markup_rc = last_nonempty_cmd_markup.clone();
            let executing_cmd_raw_rc = executing_cmd_raw.clone();
            let executing_cmd_markup_rc = executing_cmd_markup.clone();
            let block_list_rc = block_list.clone();
            let block_scroll_rc = block_scroll.clone();
            let vte_for_alt = vte.clone();
            let vte_box_rc = vte_box.clone();
            let pty_for_resize = pty.clone();
            let cwd_cbs = cwd_callbacks.clone();
            let exited_cbs = exited_callbacks.clone();
            let bell_cbs = bell_callbacks.clone();
            let title_cbs = title_callbacks.clone();
            let activity_cbs = activity_callbacks.clone();
            let bracketed_paste_rc = bracketed_paste_mode.clone();
            let application_cursor_rc = application_cursor_mode.clone();
            let mouse_reporting_rc = mouse_reporting_mode.clone();
            let cursor_shape_rc = cursor_shape.clone();
            let config_for_cb = Rc::new(RefCell::new(config.clone()));
            let parser = Rc::new(RefCell::new(Parser::new()));
            let block_data_for_cb = block_data_rc.clone();
            let finished_blocks_for_cb = finished_blocks_rc.clone();
            let pager_snapshots_rc = pager_snapshots.clone();
            let pager_snapshot_generation_rc = pager_snapshot_generation.clone();
            let scroll_debouncer = ScrollDebouncer::with_scroll_lock(
                user_scrolled_up.clone(),
                programmatic_scroll.clone(),
            );
            let ansi_cache_for_cb = ansi_cache.clone();
            let widget_pool_for_cb = widget_pool.clone();
            let editor_input_for_cb = config.editor_input;
            let tab_pending_rc = tab_pending.clone();
            let pty_synced_rc = pty_synced.clone();
            let completion_active_rc = completion_active.clone();

            // Command queue for replaying initial_commands on PromptEnd events
            let init_cmds_queue: Rc<RefCell<std::collections::VecDeque<String>>> = Rc::new(RefCell::new(
                initial_commands
                    .map(|s| s.split(", ")
                        .map(|c| c.trim().to_string())
                        .filter(|c| !c.is_empty())
                        .collect())
                    .unwrap_or_default()
            ));
            let init_cmds_queue_for_cb = Rc::clone(&init_cmds_queue);
            let pty_for_init = Rc::clone(&pty);
            let block_start_time: Rc<Cell<Option<SystemTime>>> = Rc::new(Cell::new(None));
            let block_start_time_for_cb = block_start_time.clone();
            let current_cwd: Rc<RefCell<String>> = Rc::new(RefCell::new(
                cwd.unwrap_or("").to_string()
            ));
            let current_cwd_for_cb = current_cwd.clone();

            pty.start_reader(
                move |data: Vec<u8>| {
                    log::debug!("PTY data: {} bytes, state={:?}", data.len(), bstate_rc.get());
                    if data.len() < 512 {
                        log::debug!("PTY hex: {:02x?}", &data);
                    }
                    let events = parser.borrow_mut().feed(&data);

                    for event in &events {
                        let state = bstate_rc.get();
                        log::debug!("ParserEvent: {:?} (state={:?})", event, state);
                        match event {
                            ParserEvent::Bytes(bytes) => {
                                // Check for bell character (BEL = 0x07) and trigger callbacks
                                if bytes.contains(&7) {
                                    for cb in bell_cbs.borrow().iter() {
                                        cb();
                                    }
                                }

                                // Check for OSC title sequences (OSC 0/2 title)
                                let bytes_str = String::from_utf8_lossy(bytes);
                                if bytes_str.contains("\x1b]0;") || bytes_str.contains("\x1b]2;") {
                                    // Simple extraction: look for title between \x1b]<n>; and \x07 or \x1b\\
                                    if let Some(start_idx) = bytes_str.find(';') {
                                        if let Some(end_idx) = bytes_str[start_idx..].find('\x07')
                                            .or_else(|| bytes_str[start_idx..].find("\x1b\\"))
                                        {
                                            let title = &bytes_str[start_idx + 1..start_idx + end_idx];
                                            if !title.is_empty() {
                                                for cb in title_cbs.borrow().iter() {
                                                    cb(title);
                                                }
                                            }
                                        }
                                    }
                                }

                                // Check for bracketed paste mode (CSI ?2004h = enable, CSI ?2004l = disable)
                                if bytes_str.contains("\x1b[?2004h") {
                                    bracketed_paste_rc.set(true);
                                    log::info!("Bracketed paste mode ENABLED");
                                }
                                if bytes_str.contains("\x1b[?2004l") {
                                    bracketed_paste_rc.set(false);
                                    log::info!("Bracketed paste mode DISABLED");
                                }

                                // Git's default pager options often keep less on the main screen
                                // while still enabling application cursor keys for navigation.
                                if bytes_str.contains("\x1b[?1h") {
                                    application_cursor_rc.set(true);
                                } else if bytes_str.contains("\x1b[?1l") {
                                    application_cursor_rc.set(false);
                                }

                                // Check for mouse reporting mode changes
                                if bytes_str.contains("\x1b[?1000h") {
                                    mouse_reporting_rc.set(MouseReportingMode::Click);
                                } else if bytes_str.contains("\x1b[?1000l") {
                                    mouse_reporting_rc.set(MouseReportingMode::None);
                                } else if bytes_str.contains("\x1b[?1002h") {
                                    mouse_reporting_rc.set(MouseReportingMode::Button);
                                } else if bytes_str.contains("\x1b[?1002l") {
                                    mouse_reporting_rc.set(MouseReportingMode::None);
                                } else if bytes_str.contains("\x1b[?1003h") {
                                    mouse_reporting_rc.set(MouseReportingMode::Motion);
                                } else if bytes_str.contains("\x1b[?1003l") {
                                    mouse_reporting_rc.set(MouseReportingMode::None);
                                } else if bytes_str.contains("\x1b[?1006h") {
                                    mouse_reporting_rc.set(MouseReportingMode::SGR);
                                } else if bytes_str.contains("\x1b[?1006l") {
                                    mouse_reporting_rc.set(MouseReportingMode::None);
                                }

                                // Check for cursor shape changes (DECSCUSR: CSI Ps SP q)
                                if let Some(pos) = bytes_str.find("\x1b[") {
                                    if let Some(end_pos) = bytes_str[pos+2..].find('q') {
                                        let shape_str = bytes_str[pos+2..pos+2+end_pos].trim_end_matches(' ');
                                        match shape_str {
                                            "0" | "1" => cursor_shape_rc.set(TermCursorShape::Block),
                                            "3" | "4" => cursor_shape_rc.set(TermCursorShape::Underline),
                                            "5" | "6" => cursor_shape_rc.set(TermCursorShape::Bar),
                                            _ => {}
                                        }
                                    }
                                }

                                // Check for Sixel graphics (DCS: ESC P)
                                // These will be handled by VTE in alt-screen mode
                                if bytes_str.contains("\x1bP") {
                                    log::debug!("Sixel graphics detected (displayed in VTE/alt-screen mode)");
                                }

                                let text = String::from_utf8_lossy(bytes).to_string();
                                match state {
                                    BlockState::CollectingPrompt => {
                                        prompt_buf_rc.borrow_mut().push_str(&text);
                                        // strip trailing whitespace/newlines and ANSI codes from prompt display
                                        let clean = strip_ansi(&text).trim_end().to_string();
                                        if !clean.is_empty() {
                                            active_rc.borrow().set_prompt(&clean);
                                        }
                                        // Auto-scroll to bottom while collecting prompt
                                        scroll_debouncer.mark_dirty(&block_scroll_rc);
                                    }
                                    BlockState::AwaitingCommand => {
                                        if editor_input_for_cb {
                                            let raw_text = text.clone();
                                            let stripped = strip_ansi(&raw_text);
                                            let prompt_text = strip_ansi(&prompt_buf_rc.borrow());
                                            let prompt_clean = prompt_text.trim();

                                            // Detect multi-line content (completion menu)
                                            let after_prompt = if !prompt_clean.is_empty() {
                                                stripped.strip_prefix(prompt_clean).unwrap_or(&stripped)
                                            } else {
                                                &stripped
                                            };
                                            let has_menu_content = after_prompt.contains('\n')
                                                || raw_text.contains("\x1b[B")
                                                || raw_text.contains("\x1b[A");

                                            if completion_active_rc.get() {
                                                // Completion menu is active: render in output VTE
                                                active_rc.borrow().feed_output(bytes);

                                                // Check if this is a clean single-line redraw (menu closed)
                                                if !has_menu_content && !stripped.is_empty()
                                                    && (prompt_clean.is_empty() || stripped.contains(prompt_clean))
                                                {
                                                    // Menu closed — extract the completed command
                                                    let cmd_part = if !prompt_clean.is_empty() {
                                                        after_prompt.trim()
                                                    } else {
                                                        stripped.trim()
                                                    };
                                                    if !cmd_part.is_empty() {
                                                        *active_rc.borrow().pending_cmd.borrow_mut() = cmd_part.to_string();
                                                        active_rc.borrow().cursor_offset.set(cmd_part.chars().count());
                                                        pty_synced_rc.set(true);
                                                    }
                                                    // Hide completion output
                                                    active_rc.borrow().output_vte.set_visible(false);
                                                    active_rc.borrow().raw_output.borrow_mut().clear();
                                                    completion_active_rc.set(false);
                                                    tab_pending_rc.set(false);
                                                    *active_rc.borrow().pending_suggestion.borrow_mut() = String::new();
                                                    active_rc.borrow().update_content_view();
                                                }
                                                scroll_debouncer.mark_dirty(&block_scroll_rc);
                                                continue;
                                            }

                                            if tab_pending_rc.get() && has_menu_content {
                                                // Tab triggered a completion menu — show it in output VTE
                                                completion_active_rc.set(true);
                                                active_rc.borrow().raw_output.borrow_mut().clear();
                                                active_rc.borrow().feed_output(bytes);
                                                scroll_debouncer.mark_dirty(&block_scroll_rc);
                                                continue;
                                            }

                                            // Normal single-line: extract suggestion
                                            if !prompt_clean.is_empty() && stripped.starts_with(prompt_clean) {
                                                *cmd_buf_rc.borrow_mut() = raw_text.clone();
                                            } else {
                                                cmd_buf_rc.borrow_mut().push_str(&raw_text);
                                            }

                                            let current_raw_buf = cmd_buf_rc.borrow().clone();
                                            let current_stripped = strip_ansi(&current_raw_buf);
                                            let prompt_char_count = prompt_clean.chars().count();
                                            let (mut raw_cmd, mut command_column_offset) = if !prompt_clean.is_empty() {
                                                if current_stripped.strip_prefix(prompt_clean).is_some() {
                                                    (
                                                        skip_ansi_visible_chars(&current_raw_buf, prompt_char_count),
                                                        prompt_char_count,
                                                    )
                                                } else if let Some(pos) = current_stripped.find(prompt_clean) {
                                                    let pos_chars = current_stripped[..pos].chars().count();
                                                    (
                                                        skip_ansi_visible_chars(&current_raw_buf, pos_chars + prompt_char_count),
                                                        pos_chars + prompt_char_count,
                                                    )
                                                } else {
                                                    (current_raw_buf.clone(), 0)
                                                }
                                            } else {
                                                (current_raw_buf.clone(), 0)
                                            };

                                            command_column_offset += strip_ansi(&raw_cmd)
                                                .chars()
                                                .take_while(|ch| ch.is_whitespace() && *ch != '\n')
                                                .count();
                                            raw_cmd = raw_cmd.trim_start().to_string();
                                            let display = raw_cmd.trim_end_matches('\n').trim_end();

                                            let (user_raw, suggestion_raw) = separate_input_and_suggestion(display, command_column_offset);

                                            if tab_pending_rc.get() {
                                                // Single-line tab completion (direct insert, no menu)
                                                let user_plain = plain_text_from_ansi(&user_raw);
                                                if !user_plain.is_empty() {
                                                    *active_rc.borrow().pending_cmd.borrow_mut() = user_plain.clone();
                                                    active_rc.borrow().cursor_offset.set(user_plain.chars().count());
                                                }
                                                tab_pending_rc.set(false);
                                            }

                                            let suggestion_plain = plain_text_from_ansi(&suggestion_raw);
                                            *active_rc.borrow().pending_suggestion.borrow_mut() = suggestion_plain;
                                            active_rc.borrow().update_content_view();
                                            scroll_debouncer.mark_dirty(&block_scroll_rc);
                                            continue;
                                        }
                                        // Shell's line editor sends the full line (prompt + input) with each keystroke.
                                        // Store raw text with ANSI codes preserved
                                        let raw_text = text.clone();
                                        let stripped = strip_ansi(&raw_text);

                                        let prompt_text = strip_ansi(&prompt_buf_rc.borrow());
                                        let prompt_clean = prompt_text.trim();

                                        // If this chunk starts with the prompt, it's a fresh redraw - replace buffer
                                        if !prompt_clean.is_empty() && stripped.starts_with(prompt_clean) {
                                            *cmd_buf_rc.borrow_mut() = raw_text.clone();
                                        } else {
                                            // No prompt at start means this is continuation input
                                            cmd_buf_rc.borrow_mut().push_str(&raw_text);
                                        }

                                        // Now extract the command from the raw buffer
                                        let current_raw_buf = cmd_buf_rc.borrow().clone();
                                        let current_stripped = strip_ansi(&current_raw_buf);

                                        // Skip the prompt visible characters to get to the command
                                        // Use character count, not byte length (important for UTF-8 chars like ❯)
                                        let prompt_char_count = prompt_clean.chars().count();
                                        let (mut raw_cmd, mut command_column_offset) = if !prompt_clean.is_empty() {
                                            if let Some(_after_prompt) = current_stripped.strip_prefix(prompt_clean) {
                                                // Calculate visible chars to skip in raw text
                                                (
                                                    skip_ansi_visible_chars(&current_raw_buf, prompt_char_count),
                                                    prompt_char_count,
                                                )
                                            } else if let Some(pos) = current_stripped.find(prompt_clean) {
                                                let pos_chars = current_stripped[..pos].chars().count();
                                                (
                                                    skip_ansi_visible_chars(&current_raw_buf, pos_chars + prompt_char_count),
                                                    pos_chars + prompt_char_count,
                                                )
                                            } else {
                                                (current_raw_buf.clone(), 0)
                                            }
                                        } else {
                                            (current_raw_buf.clone(), 0)
                                        };

                                        command_column_offset += strip_ansi(&raw_cmd)
                                            .chars()
                                            .take_while(|ch| ch.is_whitespace() && *ch != '\n')
                                            .count();
                                        raw_cmd = raw_cmd.trim_start().to_string();
                                        let display = raw_cmd.trim_end_matches('\n').trim_end();

                                        let (user_raw, suggestion_raw) = separate_input_and_suggestion(display, command_column_offset);

                                        // Use LRU cache for ANSI → Pango conversion
                                        let _user_markup = {
                                            let mut cache = ansi_cache_for_cb.borrow_mut();
                                            if let Some(cached) = cache.get(&user_raw) {
                                                cached.clone()
                                            } else {
                                                let result = ansi_to_pango(&user_raw, &config_for_cb.borrow().palette);
                                                // LRU automatically evicts least-recently-used entry
                                                cache.put(user_raw.clone(), result.clone());
                                                result
                                            }
                                        };

                                        active_rc.borrow().set_cmd_parts(&user_raw, &suggestion_raw);

                                        // Save the full command (user input + suggestion) for CommandEnd
                                        // This ensures commands accepted via autocomplete are properly recorded
                                        let full_cmd_raw = display.to_string();
                                        let full_cmd_markup = ansi_to_pango(&full_cmd_raw, &config_for_cb.borrow().palette);

                                        if !strip_ansi(&full_cmd_raw).trim().is_empty() {
                                            *last_nonempty_cmd_raw_rc.borrow_mut() = full_cmd_raw.clone();
                                            *last_nonempty_cmd_markup_rc.borrow_mut() = full_cmd_markup.clone();
                                        }
                                        *cmd_display_raw_rc.borrow_mut() = full_cmd_raw;
                                        *cmd_display_markup_rc.borrow_mut() = full_cmd_markup;

                                        // Auto-scroll to bottom while typing command
                                        scroll_debouncer.mark_dirty(&block_scroll_rc);
                                    }
                                    BlockState::CollectingOutput => {
                                        for cb in activity_cbs.borrow().iter() {
                                            cb();
                                        }

                                        if contains_interactive_screen_enter(bytes) {
                                            bstate_rc.set(BlockState::AltScreen);
                                            pager_snapshots_rc.borrow_mut().clear();
                                            pager_snapshot_generation_rc.set(
                                                pager_snapshot_generation_rc.get().wrapping_add(1),
                                            );
                                            vte_for_alt.reset(true, true);
                                            let prior = active_rc.borrow().raw_output.borrow().clone();
                                            if !prior.is_empty() {
                                                vte_for_alt.feed(&prior);
                                            }
                                            show_alt_screen(
                                                &block_scroll_rc,
                                                &vte_box_rc,
                                                &vte_for_alt,
                                                pty_for_resize.clone(),
                                                Some(bytes),
                                            );
                                            record_pager_snapshot(&vte_for_alt, &pager_snapshots_rc);
                                            schedule_pager_snapshot(
                                                &vte_for_alt,
                                                &pager_snapshots_rc,
                                                &pager_snapshot_generation_rc,
                                            );
                                            continue;
                                        }

                                        // Feed raw bytes directly to VTE output widget
                                        active_rc.borrow().feed_output(bytes);
                                        scroll_debouncer.mark_dirty(&block_scroll_rc);
                                    }
                                    BlockState::AltScreen => {
                                        // If bytes contain clear screen, record current page BEFORE clearing
                                        if contains_clear_screen(bytes) {
                                            log::debug!("Detected clear screen in pager, recording current page first");
                                            record_pager_snapshot(&vte_for_alt, &pager_snapshots_rc);
                                        }

                                        // Feed raw bytes directly to VTE
                                        vte_for_alt.feed(bytes);

                                        // Schedule snapshot to capture the new page after rendering
                                        schedule_pager_snapshot(
                                            &vte_for_alt,
                                            &pager_snapshots_rc,
                                            &pager_snapshot_generation_rc,
                                        );
                                    }
                                    BlockState::Idle => {
                                        // Bytes before first prompt — ignore (pre-prompt noise)
                                    }
                                }
                            }

                            ParserEvent::PromptStart => {
                                bstate_rc.set(BlockState::CollectingPrompt);
                                prompt_buf_rc.borrow_mut().clear();
                                // Auto-scroll to bottom when new prompt starts
                                scroll_debouncer.mark_dirty(&block_scroll_rc);
                            }

                            ParserEvent::PromptEnd => {
                                bstate_rc.set(BlockState::AwaitingCommand);
                                cmd_buf_rc.borrow_mut().clear();
                                cmd_display_raw_rc.borrow_mut().clear();
                                cmd_display_markup_rc.borrow_mut().clear();
                                active_rc.borrow().set_cmd("");
                                pty_synced_rc.set(false);
                                tab_pending_rc.set(false);
                                if completion_active_rc.get() {
                                    active_rc.borrow().output_vte.set_visible(false);
                                    active_rc.borrow().raw_output.borrow_mut().clear();
                                    completion_active_rc.set(false);
                                }

                                if editor_input_for_cb {
                                    let active_for_prompt_focus = active_rc.clone();
                                    glib::idle_add_local_once(move || {
                                        active_for_prompt_focus.borrow().grab_focus();
                                    });
                                }

                                // Feed next initial command if any
                                if let Some(cmd) = init_cmds_queue_for_cb.borrow_mut().pop_front() {
                                    let text = format!("{}\r", cmd);
                                    pty_for_init.write_bytes(text.as_bytes());
                                }

                                // Reset scroll lock and auto-scroll when prompt ends
                                scroll_debouncer.reset_scroll_lock();
                                scroll_debouncer.mark_dirty(&block_scroll_rc);
                            }

                            ParserEvent::CommandStart => {
                                bstate_rc.set(BlockState::CollectingOutput);
                                block_start_time_for_cb.set(Some(SystemTime::now()));
                                let raw_cmd = cmd_display_raw_rc.borrow().clone();
                                if !raw_cmd.trim().is_empty() {
                                    *executing_cmd_raw_rc.borrow_mut() = raw_cmd;
                                    *executing_cmd_markup_rc.borrow_mut() = cmd_display_markup_rc.borrow().clone();
                                } else if !last_nonempty_cmd_raw_rc.borrow().trim().is_empty() {
                                    *executing_cmd_raw_rc.borrow_mut() = last_nonempty_cmd_raw_rc.borrow().clone();
                                    *executing_cmd_markup_rc.borrow_mut() = last_nonempty_cmd_markup_rc.borrow().clone();
                                } else {
                                    let active_cmd = active_rc.borrow().pending_cmd.borrow().clone();
                                    *executing_cmd_raw_rc.borrow_mut() = active_cmd.clone();
                                    *executing_cmd_markup_rc.borrow_mut() = ansi_to_pango(&active_cmd, &config_for_cb.borrow().palette);
                                }
                                let executing_cmd = plain_text_from_ansi(&executing_cmd_raw_rc.borrow())
                                    .trim()
                                    .to_string();
                                active_rc.borrow().start_command(&executing_cmd);
                                active_rc.borrow().start_timer();
                                // Auto-scroll to bottom when command starts executing
                                scroll_debouncer.mark_dirty(&block_scroll_rc);
                            }

                            ParserEvent::CommandEnd(code) => {
                                active_rc.borrow().stop_timer();
                                if bstate_rc.get() == BlockState::AltScreen || vte_box_rc.is_visible() {
                                    record_pager_snapshot(&vte_for_alt, &pager_snapshots_rc);
                                    hide_alt_screen(&block_scroll_rc, &vte_box_rc);
                                }

                                let pager_output = drain_pager_snapshots(&pager_snapshots_rc);
                                pager_snapshot_generation_rc.set(
                                    pager_snapshot_generation_rc.get().wrapping_add(1),
                                );
                                if !pager_output.is_empty() {
                                    let needs_separator = !active_rc.borrow().output_text().trim().is_empty();
                                    if needs_separator {
                                        active_rc.borrow().append_output("\n\n");
                                    }
                                    active_rc.borrow().append_output(&pager_output);
                                }

                                // Flush any pending output first
                                active_rc.borrow().flush_output();

                                // Freeze the active block into a finished block
                                let prompt = strip_ansi(&prompt_buf_rc.borrow()).trim().to_string();

                                // Use the last displayed command (saved in AwaitingCommand)
                                // This avoids issues when the shell redraws with just the prompt after Enter
                                let mut raw_cmd_with_ansi = executing_cmd_raw_rc.borrow().clone();
                                let mut cmd_markup = executing_cmd_markup_rc.borrow().clone();
                                if raw_cmd_with_ansi.trim().is_empty()
                                    && !last_nonempty_cmd_raw_rc.borrow().trim().is_empty()
                                {
                                    raw_cmd_with_ansi = last_nonempty_cmd_raw_rc.borrow().clone();
                                    cmd_markup = last_nonempty_cmd_markup_rc.borrow().clone();
                                }
                                let cmd = strip_ansi(&raw_cmd_with_ansi).trim().to_string();

                                let output = active_rc.borrow().output_text();
                                let output_plain = strip_ansi(&output);
                                let output_trimmed = output_plain.trim().to_string();
                                let output_display = output.trim().to_string();
                                let preview = output_plain.chars().take(20).collect::<String>();
                                let bytes_preview: Vec<u8> = output_plain.bytes().take(10).collect();
                                log::debug!("CommandEnd: cmd={:?}, output_len_before={}, output_len_after={}, starts_with_newline={}, first_20_chars={:?}, first_10_bytes={:?}",
                                    cmd, output.len(), output_trimmed.len(), output.starts_with('\n'), preview, bytes_preview);

                                // Create BlockData (logical representation)
                                let line_count = output_trimmed.lines().count();
                                let estimated_height = (line_count as i32 * 20).max(60);  // Rough estimate

                                // Calculate duration if we have a start time
                                let start_time = block_start_time_for_cb.get();
                                let end_time = Some(SystemTime::now());
                                let duration_ms = start_time.and_then(|st| {
                                    end_time.and_then(|et| {
                                        et.duration_since(st).ok().map(|d| d.as_millis() as u64)
                                    })
                                });

                                let block_cwd = {
                                    let cwd_str = current_cwd_for_cb.borrow().clone();
                                    if cwd_str.is_empty() { None } else { Some(cwd_str) }
                                };

                                let block_data = BlockData {
                                    id: next_block_id(),
                                    prompt: prompt.clone(),
                                    cmd: cmd.clone(),
                                    cmd_markup: if cmd_markup.is_empty() { None } else { Some(cmd_markup.clone()) },
                                    output: output_trimmed.clone(),
                                    exit_code: *code,
                                    estimated_height,
                                    line_count,
                                    start_time,
                                    end_time,
                                    duration_ms,
                                    cwd: block_cwd.clone(),
                                };

                                block_data_for_cb.borrow_mut().push_back(block_data);

                                // Create widget (physical representation)
                                let finished = FinishedBlock::new(
                                    &prompt, &cmd, if cmd_markup.is_empty() { None } else { Some(&cmd_markup) }, &output_display, *code, &config_for_cb.borrow(),
                                    duration_ms, end_time, block_cwd.as_deref(),
                                );

                                // Insert before the active block (last child in block_list)
                                finished.widget().insert_before(&block_list_rc, Some(active_rc.borrow().widget()));

                                // Track finished blocks and limit history
                                let max_blocks = config_for_cb.borrow().max_visible_blocks as usize;
                                let finished_clone = finished.clone();
                                let finished_widget = finished_clone.widget().clone();
                                finished_blocks_for_cb.borrow_mut().push(finished);

                                // Setup right-click context menu for this block
                                let finished_blocks_for_menu = finished_blocks_for_cb.clone();
                                let block_list_for_menu = block_list_rc.clone();
                                let vte_for_copy = vte_for_alt.clone();
                                let block_id = finished_clone.id;

                                let right_click = gtk4::GestureClick::new();
                                right_click.set_button(3); // right mouse button

                                let finished_menu_clone = finished_clone.clone();
                                let block_data_for_export = block_data_for_cb.clone();
                                right_click.connect_pressed(move |gesture, _n_press, x, y| {
                                    gesture.set_state(gtk4::EventSequenceState::Claimed);

                                    let menu = gtk4::gio::Menu::new();
                                    menu.append(Some("Copy Block"), Some("block-ctx.copy"));

                                    // Export submenu
                                    let export_menu = gtk4::gio::Menu::new();
                                    export_menu.append(Some("Export as JSON"), Some("block-ctx.export-json"));
                                    export_menu.append(Some("Export as Markdown"), Some("block-ctx.export-markdown"));
                                    menu.append_submenu(Some("Export Block"), &export_menu);

                                    menu.append(Some("Delete Block"), Some("block-ctx.delete"));

                                    let popover = gtk4::PopoverMenu::from_model(Some(&menu));
                                    let widget: &gtk4::Widget = &finished_menu_clone.widget().clone().upcast::<gtk4::Widget>();
                                    popover.set_parent(widget);
                                    popover.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(
                                        x as i32, y as i32, 1, 1,
                                    )));
                                    popover.set_has_arrow(false);

                                    let action_group = gtk4::gio::SimpleActionGroup::new();

                                    // Copy action
                                    let copy_action = gtk4::gio::SimpleAction::new("copy", None);
                                    let finished_for_copy = finished_menu_clone.clone();
                                    let vte_for_action = vte_for_copy.clone();
                                    copy_action.connect_activate(move |_, _| {
                                        let prompt_text = finished_for_copy.prompt_buffer.text(
                                            &finished_for_copy.prompt_buffer.start_iter(),
                                            &finished_for_copy.prompt_buffer.end_iter(),
                                            true,
                                        );
                                        let cmd_text = finished_for_copy.command_buffer.text(
                                            &finished_for_copy.command_buffer.start_iter(),
                                            &finished_for_copy.command_buffer.end_iter(),
                                            true,
                                        );
                                        let output_text = finished_for_copy.output_buffer.text(
                                            &finished_for_copy.output_buffer.start_iter(),
                                            &finished_for_copy.output_buffer.end_iter(),
                                            true,
                                        );
                                        let full_text = format!("{}\n{}\n{}", prompt_text, cmd_text, output_text);
                                        vte_for_action.clipboard().set_text(&full_text);
                                    });
                                    action_group.add_action(&copy_action);

                                    // Export as JSON action
                                    let export_json_action = gtk4::gio::SimpleAction::new("export-json", None);
                                    let block_data_for_json = block_data_for_export.clone();
                                    let vte_for_json = vte_for_copy.clone();
                                    let block_id_json = block_id;
                                    export_json_action.connect_activate(move |_, _| {
                                        let blocks = block_data_for_json.borrow();
                                        if let Some(block) = blocks.iter().find(|b| b.id == block_id_json) {
                                            let json = block.to_json();
                                            vte_for_json.clipboard().set_text(&json);
                                            log::info!("Block exported as JSON to clipboard");
                                        }
                                    });
                                    action_group.add_action(&export_json_action);

                                    // Export as Markdown action
                                    let export_md_action = gtk4::gio::SimpleAction::new("export-markdown", None);
                                    let block_data_for_md = block_data_for_export.clone();
                                    let vte_for_md = vte_for_copy.clone();
                                    let block_id_md = block_id;
                                    export_md_action.connect_activate(move |_, _| {
                                        let blocks = block_data_for_md.borrow();
                                        if let Some(block) = blocks.iter().find(|b| b.id == block_id_md) {
                                            let markdown = block.to_markdown();
                                            vte_for_md.clipboard().set_text(&markdown);
                                            log::info!("Block exported as Markdown to clipboard");
                                        }
                                    });
                                    action_group.add_action(&export_md_action);

                                    // Delete action
                                    let delete_action = gtk4::gio::SimpleAction::new("delete", None);
                                    let finished_blocks_for_delete = finished_blocks_for_menu.clone();
                                    let block_list_for_delete = block_list_for_menu.clone();
                                    let block_id_del = block_id;
                                    delete_action.connect_activate(move |_, _| {
                                        let mut blocks = finished_blocks_for_delete.borrow_mut();
                                        if let Some(pos) = blocks.iter().position(|b| b.id == block_id_del) {
                                            let block = blocks.remove(pos);
                                            block_list_for_delete.remove(block.widget());
                                        }
                                    });
                                    action_group.add_action(&delete_action);

                                    let finished_for_actions = finished_menu_clone.clone();
                                    finished_for_actions.widget().insert_action_group("block-ctx", Some(&action_group));

                                    let finished_for_cleanup = finished_menu_clone.clone();
                                    popover.connect_closed(move |p| {
                                        p.unparent();
                                        finished_for_cleanup
                                            .widget()
                                            .insert_action_group("block-ctx", None::<&gtk4::gio::SimpleActionGroup>);
                                    });

                                    popover.popup();
                                });
                                finished_widget.add_controller(right_click);

                                // Remove oldest block if we exceed the limit
                                if finished_blocks_for_cb.borrow().len() > max_blocks {
                                    let oldest = finished_blocks_for_cb.borrow_mut().remove(0);
                                    let widget_to_release = oldest.widget().clone();
                                    block_list_rc.remove(&widget_to_release);
                                    // Return widget to pool for potential reuse
                                    widget_pool_for_cb.borrow_mut().release(widget_to_release);
                                }

                                // Also evict from block_data if needed
                                if block_data_for_cb.borrow().len() > max_blocks {
                                    block_data_for_cb.borrow_mut().pop_front();
                                }

                                // Reset active block for next command
                                active_rc.borrow().reset_for_next_prompt();

                                // Grab focus to ensure cursor is visible at new prompt
                                // Use timeout to ensure UI is fully updated and scrolled
                                let active_for_focus = active_rc.clone();
                                let block_scroll_for_focus = block_scroll_rc.clone();
                                let programmatic_for_focus = scroll_debouncer.programmatic_scroll.clone();
                                scroll_debouncer.reset_scroll_lock();
                                glib::timeout_add_local_once(std::time::Duration::from_millis(50), move || {
                                    let adj = block_scroll_for_focus.vadjustment();
                                    let target = adj.upper() - adj.page_size();
                                    programmatic_for_focus.set(true);
                                    adj.set_value(target);
                                    programmatic_for_focus.set(false);
                                    active_for_focus.borrow().grab_focus();
                                });

                                executing_cmd_raw_rc.borrow_mut().clear();
                                executing_cmd_markup_rc.borrow_mut().clear();
                                last_nonempty_cmd_raw_rc.borrow_mut().clear();
                                last_nonempty_cmd_markup_rc.borrow_mut().clear();

                                // Scroll to bottom after layout updates
                                scroll_debouncer.mark_dirty(&block_scroll_rc);

                                bstate_rc.set(BlockState::Idle);
                            }

                            ParserEvent::CwdUpdate(path) => {
                                *current_cwd_for_cb.borrow_mut() = path.clone();
                                for cb in cwd_cbs.borrow().iter() {
                                    cb(&path);
                                }
                            }

                            ParserEvent::AltScreenEnter => {
                                bstate_rc.set(BlockState::AltScreen);
                                pager_snapshots_rc.borrow_mut().clear();
                                pager_snapshot_generation_rc.set(
                                    pager_snapshot_generation_rc.get().wrapping_add(1),
                                );
                                vte_for_alt.reset(true, true);
                                show_alt_screen(
                                    &block_scroll_rc,
                                    &vte_box_rc,
                                    &vte_for_alt,
                                    pty_for_resize.clone(),
                                    None,
                                );
                            }

                            ParserEvent::AltScreenLeave => {
                                record_pager_snapshot(&vte_for_alt, &pager_snapshots_rc);
                                hide_alt_screen(&block_scroll_rc, &vte_box_rc);
                                bstate_rc.set(BlockState::CollectingOutput);
                                // Give focus to active block's TextView
                                active_rc.borrow().command_view.grab_focus();
                            }
                        }
                    }
                },
                move |exit_code| {
                    log::debug!("Shell exited with code {}", exit_code);
                    for cb in exited_cbs.borrow().iter() {
                        cb(exit_code);
                    }
                },
            );
        }

        // ── Scroll lock: detect user scrolling up ─────────────────────────
        {
            let user_scrolled = user_scrolled_up.clone();
            let programmatic = programmatic_scroll.clone();
            block_scroll.vadjustment().connect_value_changed(move |adj| {
                if programmatic.get() {
                    return;
                }
                let at_bottom = adj.value() >= adj.upper() - adj.page_size() - 5.0;
                user_scrolled.set(!at_bottom);
            });
        }

        // ── VTE is used as a display-only widget (fed via feed() in alt-screen mode)
        //    so we do NOT attach it to the PTY. Our reader thread handles all I/O.

        // ── GTK input method support ─────────────────────────────────────
        let im_context = gtk4::IMMulticontext::new();
        let im_client_widget = active.borrow().command_view.clone();

        im_context.set_client_widget(Some(&im_client_widget));

        {
            let pty_for_commit = pty.clone();
            let active_for_commit = active.clone();
            let bstate_for_commit = bstate.clone();
            let pty_synced_for_commit = pty_synced.clone();
            let editor_input_for_commit = config.editor_input;
            im_context.connect_commit(move |_, text| {
                if editor_input_for_commit && bstate_for_commit.get() == BlockState::AwaitingCommand {
                    let active = active_for_commit.borrow();
                    let pos = active.cursor_offset.get();
                    let mut cmd = active.pending_cmd.borrow().clone();
                    let byte_pos = cmd.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(cmd.len());
                    cmd.insert_str(byte_pos, text);
                    let new_pos = pos + text.chars().count();
                    *active.pending_cmd.borrow_mut() = cmd.clone();
                    active.cursor_offset.set(new_pos);
                    active.pending_preedit.borrow_mut().clear();
                    if new_pos == cmd.chars().count() {
                        pty_for_commit.write_bytes(text.as_bytes());
                        pty_synced_for_commit.set(true);
                    } else if pty_synced_for_commit.get() {
                        pty_for_commit.write_bytes(b"\x15");
                        pty_for_commit.write_bytes(cmd.as_bytes());
                    }
                    *active.pending_suggestion.borrow_mut() = String::new();
                    active.cursor_visible.set(true);
                    active.update_content_view();
                } else {
                    pty_for_commit.write_bytes(text.as_bytes());
                }
            });
        }

        {
            let active_for_preedit = active.clone();
            im_context.connect_preedit_changed(move |context| {
                let (preedit, _, _) = context.preedit_string();
                active_for_preedit.borrow().set_preedit(preedit.as_str());
            });
        }

        if config.editor_input {
            // Editor mode: keep the external IM focused for non-AwaitingCommand states
            // but don't attach focus controller to command_view (would conflict with
            // the TextView's internal IM context).
            im_context.focus_in();
        } else {
            let focus_ctrl = gtk4::EventControllerFocus::new();
            let im_for_focus_in = im_context.clone();
            focus_ctrl.connect_enter(move |_| {
                im_for_focus_in.focus_in();
            });

            let im_for_focus_out = im_context.clone();
            let active_for_focus_out = active.clone();
            focus_ctrl.connect_leave(move |_| {
                im_for_focus_out.focus_out();
                im_for_focus_out.reset();
                active_for_focus_out.borrow().set_preedit("");
            });
            im_client_widget.add_controller(focus_ctrl);
            im_context.focus_in();
        }

        // ── Keyboard input → PTY ──────────────────────────────────────────
        {
            let pty_for_key = pty.clone();
            let vte_for_key = vte.clone();
            let root_for_key = root.clone();
            let im_context_for_key = im_context.clone();
            let application_cursor_for_key = application_cursor_mode.clone();
            let bracketed_paste_for_key = bracketed_paste_mode.clone();
            let bstate_for_key = bstate.clone();
            let active_for_key = active.clone();
            let editor_input_enabled = config.editor_input;
            let block_data_for_key = block_data_rc.clone();
            let history_index: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
            let pty_synced_for_key = pty_synced.clone();
            let tab_pending_for_key = tab_pending.clone();
            let completion_active_for_key = completion_active.clone();
            let finished_blocks_for_key = finished_blocks_rc.clone();
            let block_list_for_key = block_list.clone();
            let user_scrolled_up_for_key = user_scrolled_up.clone();
            let selected_block_id_for_key = selected_block_id.clone();
            let block_scroll_for_key = block_scroll.clone();
            let key_ctrl = gtk4::EventControllerKey::new();
            key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);

            key_ctrl.connect_key_pressed(move |controller, keyval, _keycode, modifiers| {
                let ctrl = modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
                let shift = modifiers.contains(gtk4::gdk::ModifierType::SHIFT_MASK);
                let alt = modifiers.contains(gtk4::gdk::ModifierType::ALT_MASK);

                log::debug!("KEY: keyval={:?}, ctrl={}, shift={}, alt={}", keyval, ctrl, shift, alt);

                // Editor mode: when awaiting command input, handle editing locally
                // During completion menu, forward keys directly to PTY (pass-through)
                if editor_input_enabled && bstate_for_key.get() == BlockState::AwaitingCommand && !completion_active_for_key.get() {
                    // Ctrl+Shift+V/C: always handle clipboard ourselves
                    if ctrl && shift && (keyval == gtk4::gdk::Key::v || keyval == gtk4::gdk::Key::V) {
                        let clipboard = root_for_key.clipboard();
                        let active_for_paste = active_for_key.clone();
                        let pty_for_paste = pty_for_key.clone();
                        let pty_synced_for_paste = pty_synced_for_key.clone();
                        clipboard.read_text_async(None::<&gtk4::gio::Cancellable>, move |result| {
                            if let Ok(Some(text)) = result {
                                let active = active_for_paste.borrow();
                                let pos = active.cursor_offset.get();
                                let mut cmd = active.pending_cmd.borrow().clone();
                                let byte_pos = cmd.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(cmd.len());
                                cmd.insert_str(byte_pos, &text);
                                let new_pos = pos + text.chars().count();
                                *active.pending_cmd.borrow_mut() = cmd.clone();
                                active.cursor_offset.set(new_pos);
                                // Resync PTY with new full content
                                pty_for_paste.write_bytes(b"\x15");
                                pty_for_paste.write_bytes(cmd.as_bytes());
                                pty_synced_for_paste.set(true);
                                *active.pending_suggestion.borrow_mut() = String::new();
                                active.cursor_visible.set(true);
                                active.update_content_view();
                            }
                        });
                        return glib::Propagation::Stop;
                    }

                    // Ctrl+Shift+C: copy (let it propagate for global handler)
                    if ctrl && shift && (keyval == gtk4::gdk::Key::c || keyval == gtk4::gdk::Key::C) {
                        let active = active_for_key.borrow();
                        let cmd = active.pending_cmd.borrow().clone();
                        if !cmd.is_empty() {
                            let clipboard = root_for_key.clipboard();
                            clipboard.set_text(&cmd);
                        }
                        return glib::Propagation::Stop;
                    }

                    // IME: let input method handle key events (switch, compose, commit)
                    if let Some(event) = controller.current_event() {
                        if im_context_for_key.filter_keypress(&event) {
                            return glib::Propagation::Stop;
                        }
                    }

                    // Enter: send command to PTY
                    if keyval == gtk4::gdk::Key::Return || keyval == gtk4::gdk::Key::KP_Enter {
                        if shift {
                            // Shift+Enter: insert newline
                            let active = active_for_key.borrow();
                            let pos = active.cursor_offset.get();
                            let mut cmd = active.pending_cmd.borrow().clone();
                            let byte_pos = cmd.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(cmd.len());
                            cmd.insert(byte_pos, '\n');
                            *active.pending_cmd.borrow_mut() = cmd;
                            active.cursor_offset.set(pos + 1);
                            active.cursor_visible.set(true);
                            active.update_content_view();
                            return glib::Propagation::Stop;
                        }
                        // If a block is selected, copy its command to input instead of submitting
                        if let Some(sel_id) = selected_block_id_for_key.get() {
                            let finished = finished_blocks_for_key.borrow();
                            if let Some(block) = finished.iter().find(|b| b.id == sel_id) {
                                block.widget().remove_css_class("block-selected");
                                let cmd_text = block.cmd_text.clone();
                                selected_block_id_for_key.set(None);
                                drop(finished);
                                let active = active_for_key.borrow();
                                *active.pending_cmd.borrow_mut() = cmd_text.clone();
                                active.cursor_offset.set(cmd_text.chars().count());
                                if pty_synced_for_key.get() {
                                    pty_for_key.write_bytes(b"\x15");
                                }
                                pty_for_key.write_bytes(cmd_text.as_bytes());
                                pty_synced_for_key.set(true);
                                *active.pending_suggestion.borrow_mut() = String::new();
                                active.cursor_visible.set(true);
                                active.update_content_view();
                            } else {
                                selected_block_id_for_key.set(None);
                            }
                            return glib::Propagation::Stop;
                        }
                        // Regular Enter: send the command
                        let active = active_for_key.borrow();
                        let cmd = active.pending_cmd.borrow().clone();
                        let trimmed = cmd.trim();
                        if pty_synced_for_key.get() {
                            pty_for_key.write_bytes(b"\r");
                        } else if !trimmed.is_empty() {
                            pty_for_key.write_bytes(format!("{}\r", trimmed).as_bytes());
                        } else {
                            pty_for_key.write_bytes(b"\r");
                        }
                        active.cursor_offset.set(0);
                        *active.pending_suggestion.borrow_mut() = String::new();
                        pty_synced_for_key.set(false);
                        history_index.set(None);
                        user_scrolled_up_for_key.set(false);
                        return glib::Propagation::Stop;
                    }

                    // Ctrl+C: send SIGINT
                    if ctrl && (keyval == gtk4::gdk::Key::c || keyval == gtk4::gdk::Key::C) {
                        pty_for_key.write_bytes(b"\x03");
                        let active = active_for_key.borrow();
                        *active.pending_cmd.borrow_mut() = String::new();
                        active.cursor_offset.set(0);
                        active.cursor_visible.set(true);
                        active.update_content_view();
                        history_index.set(None);
                        return glib::Propagation::Stop;
                    }

                    // Tab: trigger shell completion
                    if keyval == gtk4::gdk::Key::Tab {
                        if !pty_synced_for_key.get() {
                            let active = active_for_key.borrow();
                            let cmd = active.pending_cmd.borrow().clone();
                            pty_for_key.write_bytes(cmd.as_bytes());
                            pty_synced_for_key.set(true);
                        }
                        pty_for_key.write_bytes(b"\t");
                        tab_pending_for_key.set(true);
                        return glib::Propagation::Stop;
                    }

                    // Ctrl+Shift+Up/Down: block navigation
                    if ctrl && shift && (keyval == gtk4::gdk::Key::Up || keyval == gtk4::gdk::Key::Down) {
                        let finished = finished_blocks_for_key.borrow();
                        if finished.is_empty() {
                            return glib::Propagation::Stop;
                        }
                        let current = selected_block_id_for_key.get();
                        let current_idx = current.and_then(|id| finished.iter().position(|b| b.id == id));

                        let new_idx = if keyval == gtk4::gdk::Key::Up {
                            match current_idx {
                                None => Some(finished.len() - 1),
                                Some(0) => Some(0),
                                Some(i) => Some(i - 1),
                            }
                        } else {
                            match current_idx {
                                None => None,
                                Some(i) if i >= finished.len() - 1 => None,
                                Some(i) => Some(i + 1),
                            }
                        };

                        if let Some(old_idx) = current_idx {
                            if let Some(block) = finished.get(old_idx) {
                                block.widget().remove_css_class("block-selected");
                            }
                        }

                        if let Some(idx) = new_idx {
                            if let Some(block) = finished.get(idx) {
                                block.widget().add_css_class("block-selected");
                                selected_block_id_for_key.set(Some(block.id));
                                let widget = block.widget().clone();
                                let scroll = block_scroll_for_key.clone();
                                glib::idle_add_local_once(move || {
                                    if let Some(point) = widget.compute_point(
                                        &scroll,
                                        &gtk4::graphene::Point::new(0.0, 0.0),
                                    ) {
                                        let adj = scroll.vadjustment();
                                        let target = (point.y() as f64) - adj.page_size() / 3.0;
                                        adj.set_value(target.max(0.0));
                                    }
                                });
                            }
                        } else {
                            selected_block_id_for_key.set(None);
                        }
                        return glib::Propagation::Stop;
                    }

                    // Up/Down: history navigation
                    if keyval == gtk4::gdk::Key::Up || keyval == gtk4::gdk::Key::Down {
                        let block_data = block_data_for_key.borrow();
                        if block_data.is_empty() {
                            return glib::Propagation::Stop;
                        }
                        let current_idx = history_index.get();
                        let new_idx = if keyval == gtk4::gdk::Key::Up {
                            match current_idx {
                                None => Some(block_data.len().saturating_sub(1)),
                                Some(0) => Some(0),
                                Some(i) => Some(i - 1),
                            }
                        } else {
                            match current_idx {
                                None => None,
                                Some(i) if i >= block_data.len().saturating_sub(1) => None,
                                Some(i) => Some(i + 1),
                            }
                        };
                        history_index.set(new_idx);
                        let active = active_for_key.borrow();
                        if let Some(idx) = new_idx {
                            if let Some(block) = block_data.get(idx) {
                                *active.pending_cmd.borrow_mut() = block.cmd.clone();
                                active.cursor_offset.set(block.cmd.chars().count());
                                // Resync PTY with history selection
                                pty_for_key.write_bytes(b"\x15");
                                pty_for_key.write_bytes(block.cmd.as_bytes());
                                pty_synced_for_key.set(true);
                            }
                        } else {
                            *active.pending_cmd.borrow_mut() = String::new();
                            active.cursor_offset.set(0);
                            if pty_synced_for_key.get() {
                                pty_for_key.write_bytes(b"\x15");
                                pty_synced_for_key.set(false);
                            }
                        }
                        *active.pending_suggestion.borrow_mut() = String::new();
                        active.cursor_visible.set(true);
                        active.update_content_view();
                        return glib::Propagation::Stop;
                    }

                    // Escape: clear input and block selection
                    if keyval == gtk4::gdk::Key::Escape {
                        if let Some(sel_id) = selected_block_id_for_key.get() {
                            let finished = finished_blocks_for_key.borrow();
                            if let Some(block) = finished.iter().find(|b| b.id == sel_id) {
                                block.widget().remove_css_class("block-selected");
                            }
                            selected_block_id_for_key.set(None);
                        }
                        let active = active_for_key.borrow();
                        *active.pending_cmd.borrow_mut() = String::new();
                        active.cursor_offset.set(0);
                        if pty_synced_for_key.get() {
                            pty_for_key.write_bytes(b"\x15");
                            pty_synced_for_key.set(false);
                        }
                        *active.pending_suggestion.borrow_mut() = String::new();
                        active.cursor_visible.set(true);
                        active.update_content_view();
                        history_index.set(None);
                        return glib::Propagation::Stop;
                    }

                    // Backspace: delete character before cursor
                    if keyval == gtk4::gdk::Key::BackSpace {
                        let active = active_for_key.borrow();
                        let pos = active.cursor_offset.get();
                        if pos > 0 {
                            let mut cmd = active.pending_cmd.borrow().clone();
                            let byte_pos = cmd.char_indices().nth(pos - 1).map(|(i, _)| i).unwrap_or(0);
                            let next_byte = cmd.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(cmd.len());
                            cmd.drain(byte_pos..next_byte);
                            *active.pending_cmd.borrow_mut() = cmd.clone();
                            active.cursor_offset.set(pos - 1);
                            if pty_synced_for_key.get() {
                                let new_cursor = active.cursor_offset.get();
                                if new_cursor == cmd.chars().count() {
                                    pty_for_key.write_bytes(b"\x7f");
                                } else {
                                    pty_for_key.write_bytes(b"\x15");
                                    pty_for_key.write_bytes(cmd.as_bytes());
                                }
                            }
                            *active.pending_suggestion.borrow_mut() = String::new();
                            active.cursor_visible.set(true);
                            active.update_content_view();
                        }
                        return glib::Propagation::Stop;
                    }

                    // Delete: delete character after cursor
                    if keyval == gtk4::gdk::Key::Delete {
                        let active = active_for_key.borrow();
                        let pos = active.cursor_offset.get();
                        let mut cmd = active.pending_cmd.borrow().clone();
                        let char_count = cmd.chars().count();
                        if pos < char_count {
                            let byte_pos = cmd.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(cmd.len());
                            let next_byte = cmd.char_indices().nth(pos + 1).map(|(i, _)| i).unwrap_or(cmd.len());
                            cmd.drain(byte_pos..next_byte);
                            *active.pending_cmd.borrow_mut() = cmd.clone();
                            if pty_synced_for_key.get() {
                                pty_for_key.write_bytes(b"\x15");
                                pty_for_key.write_bytes(cmd.as_bytes());
                            }
                            *active.pending_suggestion.borrow_mut() = String::new();
                            active.cursor_visible.set(true);
                            active.update_content_view();
                        }
                        return glib::Propagation::Stop;
                    }

                    // Left: move cursor left
                    if keyval == gtk4::gdk::Key::Left {
                        let active = active_for_key.borrow();
                        let pos = active.cursor_offset.get();
                        if pos > 0 {
                            active.cursor_offset.set(pos - 1);
                            if pty_synced_for_key.get() {
                                pty_for_key.write_bytes(b"\x1b[D");
                            }
                            active.cursor_visible.set(true);
                            active.update_content_view();
                        }
                        return glib::Propagation::Stop;
                    }

                    // Right: move cursor right or accept suggestion at EOL
                    if keyval == gtk4::gdk::Key::Right {
                        let active = active_for_key.borrow();
                        let pos = active.cursor_offset.get();
                        let len = active.pending_cmd.borrow().chars().count();
                        if pos < len {
                            active.cursor_offset.set(pos + 1);
                            if pty_synced_for_key.get() {
                                pty_for_key.write_bytes(b"\x1b[C");
                            }
                        } else {
                            // At end of line: accept inline suggestion if present
                            let suggestion = active.pending_suggestion.borrow().clone();
                            if !suggestion.is_empty() {
                                let mut cmd = active.pending_cmd.borrow().clone();
                                cmd.push_str(&suggestion);
                                *active.pending_cmd.borrow_mut() = cmd.clone();
                                active.cursor_offset.set(cmd.chars().count());
                                *active.pending_suggestion.borrow_mut() = String::new();
                                pty_for_key.write_bytes(b"\x1b[C");
                                pty_synced_for_key.set(true);
                            }
                        }
                        active.cursor_visible.set(true);
                        active.update_content_view();
                        return glib::Propagation::Stop;
                    }

                    // Home: move cursor to start
                    if keyval == gtk4::gdk::Key::Home {
                        let active = active_for_key.borrow();
                        active.cursor_offset.set(0);
                        if pty_synced_for_key.get() {
                            pty_for_key.write_bytes(b"\x1b[H");
                        }
                        active.cursor_visible.set(true);
                        active.update_content_view();
                        return glib::Propagation::Stop;
                    }

                    // End: move cursor to end
                    if keyval == gtk4::gdk::Key::End {
                        let active = active_for_key.borrow();
                        let len = active.pending_cmd.borrow().chars().count();
                        active.cursor_offset.set(len);
                        if pty_synced_for_key.get() {
                            pty_for_key.write_bytes(b"\x1b[F");
                        }
                        active.cursor_visible.set(true);
                        active.update_content_view();
                        return glib::Propagation::Stop;
                    }

                    // Ctrl+A: move cursor to beginning of line
                    if ctrl && (keyval == gtk4::gdk::Key::a || keyval == gtk4::gdk::Key::A) {
                        let active = active_for_key.borrow();
                        active.cursor_offset.set(0);
                        if pty_synced_for_key.get() {
                            pty_for_key.write_bytes(b"\x01");
                        }
                        active.cursor_visible.set(true);
                        active.update_content_view();
                        return glib::Propagation::Stop;
                    }

                    // Ctrl+E: move cursor to end of line
                    if ctrl && (keyval == gtk4::gdk::Key::e || keyval == gtk4::gdk::Key::E) {
                        let active = active_for_key.borrow();
                        let len = active.pending_cmd.borrow().chars().count();
                        active.cursor_offset.set(len);
                        if pty_synced_for_key.get() {
                            pty_for_key.write_bytes(b"\x05");
                        }
                        active.cursor_visible.set(true);
                        active.update_content_view();
                        return glib::Propagation::Stop;
                    }

                    // Ctrl+K: kill text from cursor to end of line
                    if ctrl && (keyval == gtk4::gdk::Key::k || keyval == gtk4::gdk::Key::K) {
                        let active = active_for_key.borrow();
                        let pos = active.cursor_offset.get();
                        let mut cmd = active.pending_cmd.borrow().clone();
                        let byte_pos = cmd.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(cmd.len());
                        cmd.truncate(byte_pos);
                        *active.pending_cmd.borrow_mut() = cmd.clone();
                        if pty_synced_for_key.get() {
                            pty_for_key.write_bytes(b"\x15");
                            if !cmd.is_empty() {
                                pty_for_key.write_bytes(cmd.as_bytes());
                            }
                        }
                        *active.pending_suggestion.borrow_mut() = String::new();
                        active.cursor_visible.set(true);
                        active.update_content_view();
                        return glib::Propagation::Stop;
                    }

                    // Ctrl+B: move cursor back one character
                    if ctrl && (keyval == gtk4::gdk::Key::b || keyval == gtk4::gdk::Key::B) {
                        let active = active_for_key.borrow();
                        let pos = active.cursor_offset.get();
                        if pos > 0 {
                            active.cursor_offset.set(pos - 1);
                            if pty_synced_for_key.get() {
                                pty_for_key.write_bytes(b"\x02");
                            }
                            active.cursor_visible.set(true);
                            active.update_content_view();
                        }
                        return glib::Propagation::Stop;
                    }

                    // Ctrl+F: move cursor forward one character
                    if ctrl && (keyval == gtk4::gdk::Key::f || keyval == gtk4::gdk::Key::F) {
                        let active = active_for_key.borrow();
                        let pos = active.cursor_offset.get();
                        let len = active.pending_cmd.borrow().chars().count();
                        if pos < len {
                            active.cursor_offset.set(pos + 1);
                            if pty_synced_for_key.get() {
                                pty_for_key.write_bytes(b"\x06");
                            }
                            active.cursor_visible.set(true);
                            active.update_content_view();
                        }
                        return glib::Propagation::Stop;
                    }

                    // Ctrl+U: clear line before cursor
                    if ctrl && (keyval == gtk4::gdk::Key::u || keyval == gtk4::gdk::Key::U) {
                        let active = active_for_key.borrow();
                        let pos = active.cursor_offset.get();
                        let mut cmd = active.pending_cmd.borrow().clone();
                        let byte_pos = cmd.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(cmd.len());
                        cmd.drain(..byte_pos);
                        *active.pending_cmd.borrow_mut() = cmd.clone();
                        active.cursor_offset.set(0);
                        if pty_synced_for_key.get() {
                            pty_for_key.write_bytes(b"\x15");
                            if !cmd.is_empty() {
                                pty_for_key.write_bytes(cmd.as_bytes());
                            }
                        }
                        *active.pending_suggestion.borrow_mut() = String::new();
                        active.cursor_visible.set(true);
                        active.update_content_view();
                        return glib::Propagation::Stop;
                    }

                    // Ctrl+W: delete word before cursor
                    if ctrl && (keyval == gtk4::gdk::Key::w || keyval == gtk4::gdk::Key::W) {
                        let active = active_for_key.borrow();
                        let pos = active.cursor_offset.get();
                        if pos > 0 {
                            let mut cmd = active.pending_cmd.borrow().clone();
                            let chars: Vec<char> = cmd.chars().collect();
                            let mut new_pos = pos;
                            // Skip trailing spaces
                            while new_pos > 0 && chars[new_pos - 1] == ' ' {
                                new_pos -= 1;
                            }
                            // Skip word chars
                            while new_pos > 0 && chars[new_pos - 1] != ' ' {
                                new_pos -= 1;
                            }
                            let start_byte = cmd.char_indices().nth(new_pos).map(|(i, _)| i).unwrap_or(0);
                            let end_byte = cmd.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(cmd.len());
                            cmd.drain(start_byte..end_byte);
                            *active.pending_cmd.borrow_mut() = cmd.clone();
                            active.cursor_offset.set(new_pos);
                            if pty_synced_for_key.get() {
                                pty_for_key.write_bytes(b"\x15");
                                if !cmd.is_empty() {
                                    pty_for_key.write_bytes(cmd.as_bytes());
                                }
                            }
                            *active.pending_suggestion.borrow_mut() = String::new();
                            active.cursor_visible.set(true);
                            active.update_content_view();
                        }
                        return glib::Propagation::Stop;
                    }

                    // Ctrl+L: clear visible block history
                    if ctrl && (keyval == gtk4::gdk::Key::l || keyval == gtk4::gdk::Key::L) {
                        let mut blocks = finished_blocks_for_key.borrow_mut();
                        for block in blocks.drain(..) {
                            block_list_for_key.remove(block.widget());
                        }
                        pty_for_key.write_bytes(b"\x0c");
                        return glib::Propagation::Stop;
                    }

                    // Alt+B: move cursor back one word
                    if alt && !ctrl && (keyval == gtk4::gdk::Key::b || keyval == gtk4::gdk::Key::B) {
                        let active = active_for_key.borrow();
                        let pos = active.cursor_offset.get();
                        let cmd = active.pending_cmd.borrow().clone();
                        let chars: Vec<char> = cmd.chars().collect();
                        let mut new_pos = pos;
                        while new_pos > 0 && !chars[new_pos - 1].is_alphanumeric() {
                            new_pos -= 1;
                        }
                        while new_pos > 0 && chars[new_pos - 1].is_alphanumeric() {
                            new_pos -= 1;
                        }
                        active.cursor_offset.set(new_pos);
                        if pty_synced_for_key.get() {
                            pty_for_key.write_bytes(b"\x1bb");
                        }
                        active.cursor_visible.set(true);
                        active.update_content_view();
                        return glib::Propagation::Stop;
                    }

                    // Alt+F: move cursor forward one word
                    if alt && !ctrl && (keyval == gtk4::gdk::Key::f || keyval == gtk4::gdk::Key::F) {
                        let active = active_for_key.borrow();
                        let pos = active.cursor_offset.get();
                        let cmd = active.pending_cmd.borrow().clone();
                        let chars: Vec<char> = cmd.chars().collect();
                        let len = chars.len();
                        let mut new_pos = pos;
                        while new_pos < len && !chars[new_pos].is_alphanumeric() {
                            new_pos += 1;
                        }
                        while new_pos < len && chars[new_pos].is_alphanumeric() {
                            new_pos += 1;
                        }
                        active.cursor_offset.set(new_pos);
                        if pty_synced_for_key.get() {
                            pty_for_key.write_bytes(b"\x1bf");
                        }
                        active.cursor_visible.set(true);
                        active.update_content_view();
                        return glib::Propagation::Stop;
                    }

                    // Normal printable characters: insert at cursor position
                    if !ctrl && !alt {
                        if let Some(ch) = keyval.to_unicode() {
                            if !ch.is_control() {
                                let active = active_for_key.borrow();
                                let pos = active.cursor_offset.get();
                                let mut cmd = active.pending_cmd.borrow().clone();
                                let byte_pos = cmd.char_indices().nth(pos).map(|(i, _)| i).unwrap_or(cmd.len());
                                let mut buf = [0u8; 4];
                                let s = ch.encode_utf8(&mut buf);
                                cmd.insert_str(byte_pos, s);
                                *active.pending_cmd.borrow_mut() = cmd.clone();
                                active.cursor_offset.set(pos + 1);
                                // Mirror to PTY for suggestion generation
                                let new_cursor = active.cursor_offset.get();
                                if new_cursor == cmd.chars().count() {
                                    pty_for_key.write_bytes(s.as_bytes());
                                    pty_synced_for_key.set(true);
                                } else if pty_synced_for_key.get() {
                                    pty_for_key.write_bytes(b"\x15");
                                    pty_for_key.write_bytes(cmd.as_bytes());
                                    pty_synced_for_key.set(true);
                                }
                                *active.pending_suggestion.borrow_mut() = String::new();
                                active.cursor_visible.set(true);
                                active.update_content_view();
                                return glib::Propagation::Stop;
                            }
                        }
                    }

                    // Unhandled keys: consume to prevent interference
                    return glib::Propagation::Stop;
                }

                // Handle Ctrl+Shift+C (copy) and Ctrl+Shift+V (paste)
                if ctrl && shift {
                    match keyval {
                        v if v == gtk4::gdk::Key::c || v == gtk4::gdk::Key::C => {
                            log::warn!(">>> COPY: Ctrl+Shift+C pressed");
                            // Copy selected text to clipboard
                            // First try VTE (for alt-screen mode)
                            if let Some(text) = vte_for_key.text_selected(vte4::Format::Text) {
                                log::warn!(">>> Copy: got {} chars from VTE", text.len());
                                if !text.is_empty() {
                                    let clipboard = vte_for_key.clipboard();
                                    clipboard.set_text(&text);
                                    log::warn!(">>> Copy: set VTE text to clipboard");
                                } else {
                                    log::warn!(">>> Copy: VTE text empty, trying PRIMARY");
                                    // Fall back to PRIMARY clipboard (selected text in labels)
                                    let display = root_for_key.display();
                                    let primary = display.primary_clipboard();
                                    log::warn!(">>> Copy: got PRIMARY clipboard, calling read_text_async");
                                    let clipboard = display.clipboard();
                                    primary.read_text_async(None::<&gtk4::gio::Cancellable>, move |result: Result<Option<gtk4::glib::GString>, _>| {
                                        log::warn!(">>> Copy callback: result={:?}", result.as_ref().map(|opt| opt.as_ref().map(|s| s.len())));
                                        match result {
                                            Ok(text_opt) => {
                                                if let Some(text_str) = text_opt {
                                                    if !text_str.is_empty() {
                                                        log::warn!(">>> Copy: got {} chars from PRIMARY", text_str.len());
                                                        clipboard.set_text(&text_str);
                                                        log::warn!(">>> Copy: copied to regular clipboard");
                                                    } else {
                                                        log::warn!(">>> Copy: PRIMARY is empty");
                                                    }
                                                } else {
                                                    log::warn!(">>> Copy: PRIMARY is None");
                                                }
                                            }
                                            Err(e) => {
                                                log::warn!(">>> Copy: error reading PRIMARY: {}", e);
                                            }
                                        }
                                    });
                                }
                            } else {
                                log::warn!(">>> Copy: VTE returned None");
                            }
                            return glib::Propagation::Stop;
                        }
                        v if v == gtk4::gdk::Key::v || v == gtk4::gdk::Key::V => {
                            log::warn!(">>> PASTE: Ctrl+Shift+V pressed");
                            // Paste: read clipboard and write to PTY
                            let clipboard = vte_for_key.clipboard();
                            let pty_for_paste = pty_for_key.clone();
                            let bracketed_paste = bracketed_paste_for_key.get();
                            log::warn!(">>> Paste: got clipboard, bracketed_paste={}, calling read_text_async", bracketed_paste);
                            clipboard.read_text_async(None::<&gtk4::gio::Cancellable>, move |result| {
                                log::warn!(">>> Paste callback: result={:?}", result.as_ref().map(|opt| opt.as_ref().map(|s| s.len())));
                                match result {
                                    Ok(text_opt) => {
                                        if let Some(text_str) = text_opt {
                                            log::warn!(">>> Paste: got {} chars from clipboard", text_str.len());
                                            // Wrap paste with bracketed paste mode if enabled
                                            if bracketed_paste {
                                                pty_for_paste.write_bytes(b"\x1b[200~");
                                                pty_for_paste.write_bytes(text_str.as_bytes());
                                                pty_for_paste.write_bytes(b"\x1b[201~");
                                            } else {
                                                pty_for_paste.write_bytes(text_str.as_bytes());
                                            }
                                            log::warn!(">>> Paste: wrote {} bytes to PTY", text_str.len());
                                        } else {
                                            log::warn!(">>> Paste: clipboard is None");
                                        }
                                    }
                                    Err(e) => {
                                        log::error!(">>> Paste: error: {}", e);
                                    }
                                }
                            });
                            return glib::Propagation::Stop;
                        }
                        _ => {}
                    }
                }

                if let Some(event) = controller.current_event() {
                    if im_context_for_key.filter_keypress(&event) {
                        return glib::Propagation::Stop;
                    }
                }

                let bytes: Option<Vec<u8>> = match keyval {
                    v if v == gtk4::gdk::Key::Return || v == gtk4::gdk::Key::KP_Enter => {
                        Some(b"\r".to_vec())
                    }
                    v if v == gtk4::gdk::Key::BackSpace => Some(b"\x7f".to_vec()),
                    v if v == gtk4::gdk::Key::Tab => Some(b"\t".to_vec()),
                    v if v == gtk4::gdk::Key::Escape => Some(b"\x1b".to_vec()),
                    v if v == gtk4::gdk::Key::Up && application_cursor_for_key.get() => Some(b"\x1bOA".to_vec()),
                    v if v == gtk4::gdk::Key::Down && application_cursor_for_key.get() => Some(b"\x1bOB".to_vec()),
                    v if v == gtk4::gdk::Key::Right && application_cursor_for_key.get() => Some(b"\x1bOC".to_vec()),
                    v if v == gtk4::gdk::Key::Left && application_cursor_for_key.get() => Some(b"\x1bOD".to_vec()),
                    v if v == gtk4::gdk::Key::Up => Some(b"\x1b[A".to_vec()),
                    v if v == gtk4::gdk::Key::Down => Some(b"\x1b[B".to_vec()),
                    v if v == gtk4::gdk::Key::Right => Some(b"\x1b[C".to_vec()),
                    v if v == gtk4::gdk::Key::Left => Some(b"\x1b[D".to_vec()),
                    v if v == gtk4::gdk::Key::Home => Some(b"\x1b[H".to_vec()),
                    v if v == gtk4::gdk::Key::End => Some(b"\x1b[F".to_vec()),
                    v if v == gtk4::gdk::Key::Delete => Some(b"\x1b[3~".to_vec()),
                    v if v == gtk4::gdk::Key::Insert => Some(b"\x1b[2~".to_vec()),
                    v if v == gtk4::gdk::Key::Page_Up => Some(b"\x1b[5~".to_vec()),
                    v if v == gtk4::gdk::Key::Page_Down => Some(b"\x1b[6~".to_vec()),
                    v if v == gtk4::gdk::Key::F1 => Some(b"\x1bOP".to_vec()),
                    v if v == gtk4::gdk::Key::F2 => Some(b"\x1bOQ".to_vec()),
                    v if v == gtk4::gdk::Key::F3 => Some(b"\x1bOR".to_vec()),
                    v if v == gtk4::gdk::Key::F4 => Some(b"\x1bOS".to_vec()),
                    v if v == gtk4::gdk::Key::F5 => Some(b"\x1b[15~".to_vec()),
                    v if v == gtk4::gdk::Key::F6 => Some(b"\x1b[17~".to_vec()),
                    v if v == gtk4::gdk::Key::F7 => Some(b"\x1b[18~".to_vec()),
                    v if v == gtk4::gdk::Key::F8 => Some(b"\x1b[19~".to_vec()),
                    v if v == gtk4::gdk::Key::F9 => Some(b"\x1b[20~".to_vec()),
                    v if v == gtk4::gdk::Key::F10 => Some(b"\x1b[21~".to_vec()),
                    v if v == gtk4::gdk::Key::F11 => Some(b"\x1b[23~".to_vec()),
                    v if v == gtk4::gdk::Key::F12 => Some(b"\x1b[24~".to_vec()),
                    v if ctrl => {
                        if let Some(ch) = v.to_unicode() {
                            let ctrl_byte = (ch as u8).wrapping_sub(b'`');
                            if ctrl_byte < 32 {
                                Some(vec![ctrl_byte])
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    v => {
                        if let Some(ch) = v.to_unicode() {
                            let mut buf = [0u8; 4];
                            Some(ch.encode_utf8(&mut buf).as_bytes().to_vec())
                        } else {
                            None
                        }
                    }
                };
                if let Some(data) = bytes {
                    pty_for_key.write_bytes(&data);
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });

            let im_context_for_release = im_context.clone();
            key_ctrl.connect_key_released(move |controller, _keyval, _keycode, _modifiers| {
                if let Some(event) = controller.current_event() {
                    im_context_for_release.filter_keypress(&event);
                }
            });
            root.add_controller(key_ctrl);
            // Don't set root as focusable - it prevents child labels from being selectable
            // root.set_focusable(true);
        }

        let term_view = TermView {
            root,
            block_scroll,
            block_list,
            vte_box,
            vte,
            active,
            bstate,
            prompt_buf,
            cmd_buf,
            pty,
            cwd_callbacks,
            exited_callbacks,
            bell_callbacks,
            title_callbacks,
            activity_callbacks,
            bracketed_paste_mode,
            application_cursor_mode,
            mouse_reporting_mode,
            cursor_shape,
            config: Rc::new(RefCell::new(config.clone())),
            block_data: block_data_rc,
            finished_blocks: finished_blocks_rc,
            ansi_cache,
            viewport: Rc::new(RefCell::new(ViewportState {
                first_visible: 0,
                last_visible: 0,
                total_height: 0,
            })),
            widget_pool,
            visible_indices: Rc::new(RefCell::new(std::collections::HashSet::new())),
            search_cache: Rc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            selected_block_id,
        };

        // Load history if configured
        let _ = term_view.load_history();

        // Create widgets for loaded blocks
        {
            let block_data_ref = term_view.block_data.borrow();
            let config = term_view.config.borrow();
            for block in block_data_ref.iter() {
                let finished = FinishedBlock::new(
                    &block.prompt,
                    &block.cmd,
                    block.cmd_markup.as_deref(),
                    &block.output,
                    block.exit_code,
                    &config,
                    block.duration_ms,
                    block.end_time,
                    block.cwd.as_deref(),
                );
                finished.widget().insert_before(&term_view.block_list, Some(term_view.active.borrow().widget()));
                term_view.finished_blocks.borrow_mut().push(finished);
            }
        }

        // Initialize viewport and visibility
        term_view.update_viewport();
        term_view.update_block_visibility();

        // Wire virtual scrolling: connect scroll signals
        {
            let viewport = term_view.viewport.clone();
            let block_scroll = term_view.block_scroll.clone();
            let block_data = term_view.block_data.clone();
            let config = term_view.config.clone();
            let finished_blocks = term_view.finished_blocks.clone();
            let visible_indices = term_view.visible_indices.clone();

            let vadjust = block_scroll.vadjustment();
            vadjust.connect_changed(move |_| {
                // Update viewport on scroll change
                let adj = block_scroll.vadjustment();
                let scroll_top = adj.value() as i32;
                let viewport_height = adj.page_size() as i32;
                let margin = (config.borrow().virtual_scroll_margin as i32) * viewport_height;

                let visible_top = (scroll_top - margin).max(0);
                let visible_bottom = scroll_top + viewport_height + margin;

                let block_data_ref = block_data.borrow();
                let mut y = 0;
                let mut first = None;
                let mut last = 0;

                for (i, block) in block_data_ref.iter().enumerate() {
                    if first.is_none() && y + block.estimated_height > visible_top {
                        first = Some(i);
                    }
                    if y < visible_bottom {
                        last = i;
                    }
                    y += block.estimated_height;
                }

                let mut vp = viewport.borrow_mut();
                vp.first_visible = first.unwrap_or(0);
                vp.last_visible = last;
                vp.total_height = y;
                drop(vp);

                // Schedule visibility update on next idle
                let vp = viewport.clone();
                let finished = finished_blocks.clone();
                let visible = visible_indices.clone();
                glib::idle_add_local_once(move || {
                    let vp_ref = vp.borrow();
                    let mut new_visible = std::collections::HashSet::new();

                    for i in
                        vp_ref.first_visible..=vp_ref.last_visible.min(vp_ref.first_visible + 1000)
                    {
                        new_visible.insert(i);
                    }

                    let finished_ref = finished.borrow();
                    let mut visible_ref = visible.borrow_mut();

                    for (i, block) in finished_ref.iter().enumerate() {
                        if new_visible.contains(&i) && !visible_ref.contains(&i) {
                            block.widget().set_visible(true);
                        } else if !new_visible.contains(&i) && visible_ref.contains(&i) {
                            block.widget().set_visible(false);
                        }
                    }

                    *visible_ref = new_visible;
                });
            });
        }

        // Give initial focus to ActiveBlock's TextView for cursor blinking
        term_view.active.borrow().command_view.grab_focus();

        term_view
    }

    /// Root GTK widget to embed in the notebook page.
    pub fn widget(&self) -> gtk4::Widget {
        self.root.clone().upcast()
    }

    /// Send key bytes into the PTY (user input).
    pub fn write_input(&self, data: &[u8]) {
        self.pty.write_bytes(data);
    }

    /// Resize the PTY.
    pub fn resize(&self, cols: u16, rows: u16) {
        self.pty.resize(cols, rows);
        self.active.borrow().output_vte.set_size(cols as i64, rows as i64);
    }

    /// Kill the child process.
    pub fn kill(&self) {
        self.pty.kill();
    }

    pub fn pid_i32(&self) -> i32 {
        self.pty.pid_i32()
    }

    pub fn vte(&self) -> &Terminal {
        &self.vte
    }

    pub fn grab_focus(&self) {
        if self.vte_box.is_visible() {
            self.vte.grab_focus();
        } else {
            self.root.grab_focus();
        }
    }

    /// Copy selected text to clipboard.
    /// In block mode: tries to copy from GTK's selection (PRIMARY clipboard).
    /// In alt-screen mode: copies from VTE terminal.
    pub fn copy_to_clipboard(&self) {
        log::warn!(">>> TermView::copy_to_clipboard called");
        // First try VTE (for alt-screen mode)
        let vte_text = self.vte.text_selected(vte4::Format::Text);
        let has_vte_text = vte_text.as_ref().map(|t| !t.is_empty()).unwrap_or(false);

        if has_vte_text {
            let text = vte_text.unwrap();
            log::warn!(">>> TermView copy: got {} chars from VTE", text.len());
            let clipboard = self.vte.clipboard();
            clipboard.set_text(&text);
            log::warn!(">>> TermView copy: set VTE text to clipboard");
        } else {
            log::warn!(">>> TermView copy: VTE text empty or None, trying PRIMARY");
            // Fall back to PRIMARY clipboard (selected text in labels)
            let display = self.root.display();
            let root_clone = self.root.clone();
            let primary = display.primary_clipboard();
            log::warn!(">>> TermView copy: got PRIMARY clipboard, calling read_text_async");
            primary.read_text_async(
                None::<&gtk4::gio::Cancellable>,
                move |result: Result<Option<gtk4::glib::GString>, _>| {
                    log::warn!(
                        ">>> TermView copy callback: result={:?}",
                        result
                            .as_ref()
                            .map(|opt| opt.as_ref().map(|s| (s.len(), s.as_str())))
                    );
                    match result {
                        Ok(text_opt) => {
                            if let Some(text_str) = text_opt {
                                if !text_str.is_empty() {
                                    log::warn!(
                                        ">>> TermView copy: got {} chars from PRIMARY: {:?}",
                                        text_str.len(),
                                        &text_str[..text_str.len().min(50)]
                                    );
                                    // Copy to regular clipboard (CLIPBOARD)
                                    let display2 = root_clone.display();
                                    let cb = display2.clipboard();
                                    cb.set_text(&text_str);
                                    log::warn!(">>> TermView copy: copied to CLIPBOARD");
                                } else {
                                    log::warn!(">>> TermView copy: PRIMARY text is empty");
                                }
                            } else {
                                log::warn!(">>> TermView copy: PRIMARY is None - no text selected");
                            }
                        }
                        Err(e) => {
                            log::warn!(">>> TermView copy: error reading PRIMARY: {}", e);
                        }
                    }
                },
            );
        }
    }

    /// Paste from clipboard to PTY.
    pub fn paste_from_clipboard(&self) {
        log::warn!(">>> TermView::paste_from_clipboard called");
        let clipboard = self.vte.clipboard();
        let pty = self.pty.clone();
        let bracketed_paste = self.bracketed_paste_mode.get();
        log::warn!(">>> TermView paste: got clipboard, calling read_text_async");
        clipboard.read_text_async(None::<&gtk4::gio::Cancellable>, move |result| {
            log::warn!(
                ">>> TermView paste callback: result={:?}",
                result.as_ref().map(|opt| opt.as_ref().map(|s| s.len()))
            );
            match result {
                Ok(text_opt) => {
                    if let Some(text_str) = text_opt {
                        log::warn!(
                            ">>> TermView paste: got {} chars from clipboard",
                            text_str.len()
                        );
                        // Wrap paste with bracketed paste mode if enabled
                        if bracketed_paste {
                            pty.write_bytes(b"\x1b[200~");
                            pty.write_bytes(text_str.as_bytes());
                            pty.write_bytes(b"\x1b[201~");
                        } else {
                            pty.write_bytes(text_str.as_bytes());
                        }
                        log::warn!(">>> TermView paste: wrote {} bytes to PTY", text_str.len());
                    } else {
                        log::warn!(">>> TermView paste: clipboard is None");
                    }
                }
                Err(e) => {
                    log::error!(">>> TermView paste: error: {}", e);
                }
            }
        });
    }

    pub fn connect_cwd_changed<F: Fn(&str) + 'static>(&self, f: F) {
        self.cwd_callbacks.borrow_mut().push(Box::new(f));
    }

    pub fn connect_exited<F: Fn(i32) + 'static>(&self, f: F) {
        self.exited_callbacks.borrow_mut().push(Box::new(f));
    }

    pub fn connect_bell<F: Fn() + 'static>(&self, f: F) {
        self.bell_callbacks.borrow_mut().push(Box::new(f));
    }

    pub fn connect_title_changed<F: Fn(&str) + 'static>(&self, f: F) {
        self.title_callbacks.borrow_mut().push(Box::new(f));
    }

    pub fn connect_activity<F: Fn() + 'static>(&self, f: F) {
        self.activity_callbacks.borrow_mut().push(Box::new(f));
    }

    pub fn cursor_shape(&self) -> TermCursorShape {
        self.cursor_shape.get()
    }

    /// Apply updated theme colors to the block widgets.
    pub fn apply_theme(&self) {
        install_block_css(&self.config.borrow());
    }

    /// Update font for VTE terminal and block view CSS.
    pub fn set_font(&self, font_desc: &FontDescription) {
        self.vte.set_font(Some(font_desc));
        // Update config and regenerate CSS with new font
        self.config.borrow_mut().font_desc = font_desc.to_string();
        install_block_css(&self.config.borrow());
    }

    /// Update font scale for VTE terminal and block view CSS.
    pub fn set_font_scale(&self, scale: f64) {
        self.vte.set_font_scale(scale);
        self.config.borrow_mut().default_font_scale = scale;
        // Regenerate CSS with updated font scale
        install_block_css(&self.config.borrow());
    }

    /// Update virtual scrolling viewport state based on scroll position.
    pub fn update_viewport(&self) {
        let adj = self.block_scroll.vadjustment();
        let scroll_top = adj.value() as i32;
        let viewport_height = adj.page_size() as i32;
        let margin = (self.config.borrow().virtual_scroll_margin as i32) * viewport_height;

        let visible_top = (scroll_top - margin).max(0);
        let visible_bottom = scroll_top + viewport_height + margin;

        let block_data = self.block_data.borrow();
        let mut y = 0;
        let mut first = None;
        let mut last = 0;

        for (i, block) in block_data.iter().enumerate() {
            if first.is_none() && y + block.estimated_height > visible_top {
                first = Some(i);
            }
            if y < visible_bottom {
                last = i;
            }
            y += block.estimated_height;
        }

        let mut vp = self.viewport.borrow_mut();
        vp.first_visible = first.unwrap_or(0);
        vp.last_visible = last;
        vp.total_height = y;
    }

    /// Update block visibility based on viewport: show visible blocks, hide off-screen ones.
    pub fn update_block_visibility(&self) {
        let vp = self.viewport.borrow().clone();
        let mut new_visible = std::collections::HashSet::new();

        // Only show blocks in the visible range
        for i in vp.first_visible..=vp.last_visible.min(vp.first_visible + 1000) {
            new_visible.insert(i);
        }

        let finished = self.finished_blocks.borrow();
        let mut visible = self.visible_indices.borrow_mut();

        // Update visibility: hide blocks not in new_visible, show blocks in new_visible
        for (i, block) in finished.iter().enumerate() {
            if new_visible.contains(&i) && !visible.contains(&i) {
                block.widget().set_visible(true);
            } else if !new_visible.contains(&i) && visible.contains(&i) {
                block.widget().set_visible(false);
            }
        }

        *visible = new_visible;
    }

    /// Search blocks for a query string (case-insensitive).
    /// Returns indices of matching blocks.
    pub fn search_blocks(&self, query: &str) -> Vec<usize> {
        self.search_blocks_with_filters(query, &BlockFilters::default())
    }

    /// Search blocks with optional filters
    pub fn search_blocks_with_filters(&self, query: &str, filters: &BlockFilters) -> Vec<usize> {
        let q = query.to_lowercase();

        // Perform search with filters
        let results: Vec<usize> = self
            .block_data
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                // Text search
                let text_match = if q.is_empty() {
                    true
                } else {
                    b.prompt.to_lowercase().contains(&q)
                        || b.cmd.to_lowercase().contains(&q)
                        || b.output.to_lowercase().contains(&q)
                };

                if !text_match {
                    return false;
                }

                // Exit code filter
                if let Some(exit_code) = filters.exit_code {
                    if b.exit_code != exit_code {
                        return false;
                    }
                }

                // Failed only filter
                if filters.failed_only && b.exit_code == 0 {
                    return false;
                }

                // Duration filters
                if let Some(duration) = b.duration_ms {
                    if let Some(min_dur) = filters.min_duration_ms {
                        if duration < min_dur {
                            return false;
                        }
                    }
                    if let Some(max_dur) = filters.max_duration_ms {
                        if duration > max_dur {
                            return false;
                        }
                    }
                    if filters.slow_only && duration < filters.slow_threshold_ms {
                        return false;
                    }
                }

                true
            })
            .map(|(i, _)| i)
            .collect();

        results
    }

    /// Get only failed blocks (exit_code != 0)
    pub fn get_failed_blocks(&self) -> Vec<usize> {
        let filters = BlockFilters {
            failed_only: true,
            ..Default::default()
        };
        self.search_blocks_with_filters("", &filters)
    }

    /// Get only slow blocks (duration > threshold)
    pub fn get_slow_blocks(&self, threshold_ms: u64) -> Vec<usize> {
        let filters = BlockFilters {
            slow_only: true,
            slow_threshold_ms: threshold_ms,
            ..Default::default()
        };
        self.search_blocks_with_filters("", &filters)
    }

    /// Export a block by ID to JSON format
    pub fn export_block_json(&self, block_id: u64) -> Option<String> {
        let blocks = self.block_data.borrow();
        blocks.iter().find(|b| b.id == block_id).map(|b| b.to_json())
    }

    /// Export a block by ID to Markdown format
    pub fn export_block_markdown(&self, block_id: u64) -> Option<String> {
        let blocks = self.block_data.borrow();
        blocks.iter().find(|b| b.id == block_id).map(|b| b.to_markdown())
    }

    /// Export all blocks in the session as JSON
    pub fn export_session_json(&self) -> String {
        let blocks = self.block_data.borrow();
        let blocks_vec: Vec<&BlockData> = blocks.iter().collect();
        serde_json::to_string_pretty(&blocks_vec).unwrap_or_else(|_| "[]".to_string())
    }

    /// Export all blocks in the session as Markdown
    pub fn export_session_markdown(&self) -> String {
        let blocks = self.block_data.borrow();
        let mut md = String::new();

        md.push_str("# Terminal Session Export\n\n");
        md.push_str(&format!("Total blocks: {}\n\n", blocks.len()));
        md.push_str("---\n\n");

        for (index, block) in blocks.iter().enumerate() {
            md.push_str(&format!("## Block #{}\n\n", index + 1));
            md.push_str(&block.to_markdown());
            md.push_str("\n---\n\n");
        }

        md
    }

    pub fn scroll_to_block(&self, block_index: usize) {
        let finished = self.finished_blocks.borrow();
        if block_index >= finished.len() {
            return;
        }
        if let Some(block) = finished.get(block_index) {
            block.widget().grab_focus();
            let adj = self.block_scroll.vadjustment();
            if let Some(value) = block
                .widget()
                .compute_point(&self.block_scroll, &gtk4::graphene::Point::new(0.0, 0.0))
            {
                adj.set_value(value.y() as f64);
            }
        }
    }

    /// Delete a block by ID (for right-click menu).
    pub fn delete_block_by_id(&self, block_id: u64) {
        let mut finished = self.finished_blocks.borrow_mut();
        if let Some(pos) = finished.iter().position(|b| b.id == block_id) {
            let block_to_remove = finished.remove(pos);
            let widget_to_release = block_to_remove.widget().clone();
            self.block_list.remove(&widget_to_release);
            // Return widget to pool for potential reuse
            self.widget_pool.borrow_mut().release(widget_to_release);
        }
    }

    /// Copy a block's content to clipboard (prompt + cmd + output).
    pub fn copy_block_by_id(&self, block_id: u64) {
        let finished = self.finished_blocks.borrow();
        if let Some(block) = finished.iter().find(|b| b.id == block_id) {
            let prompt_text = block.prompt_buffer.text(
                &block.prompt_buffer.start_iter(),
                &block.prompt_buffer.end_iter(),
                true,
            );
            let cmd_text = block.command_buffer.text(
                &block.command_buffer.start_iter(),
                &block.command_buffer.end_iter(),
                true,
            );
            let output_text = block.output_buffer.text(
                &block.output_buffer.start_iter(),
                &block.output_buffer.end_iter(),
                true,
            );

            let full_text = format!("{}\n{}\n{}", prompt_text, cmd_text, output_text);
            let clipboard = self.vte.clipboard();
            clipboard.set_text(&full_text);
        }
    }

    /// Save block history to file (if configured).
    pub fn save_history(&self) -> std::io::Result<()> {
        let path_opt = self.config.borrow().block_history_path.as_ref().cloned();
        if path_opt.is_none() {
            return Ok(());
        }

        let path = path_opt.unwrap();
        let blocks = self.block_data.borrow();

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        for block in blocks.iter() {
            log::debug!("Saving block to history: prompt={:?}, cmd={:?}, output_len={}, exit_code={}",
                &block.prompt, &block.cmd, block.output.len(), block.exit_code);
            let serialized = bincode::serialize(block)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

            if self.config.borrow().block_history_compress {
                let compressed = zstd::encode_all(serialized.as_slice(), 3)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
                use std::io::Write;
                let mut f = file.try_clone()?;
                f.write_all(&(compressed.len() as u32).to_le_bytes())?;
                f.write_all(&compressed)?;
            } else {
                use std::io::Write;
                let mut f = file.try_clone()?;
                f.write_all(&(serialized.len() as u32).to_le_bytes())?;
                f.write_all(&serialized)?;
            }
        }

        Ok(())
    }

    /// Load block history from file (if configured).
    pub fn load_history(&self) -> std::io::Result<()> {
        let path_opt = self.config.borrow().block_history_path.as_ref().cloned();
        if path_opt.is_none() {
            return Ok(());
        }

        let path = path_opt.unwrap();
        if !std::path::Path::new(&path).exists() {
            return Ok(());
        }

        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let lazy_load_threshold = self.config.borrow().lazy_load_threshold as usize;
        let mut temp_blocks = Vec::new();

        // First pass: load all blocks into temporary storage
        loop {
            let mut len_bytes = [0u8; 4];
            if file.read_exact(&mut len_bytes).is_err() {
                break;
            }

            let len = u32::from_le_bytes(len_bytes) as usize;
            let mut data = vec![0u8; len];
            file.read_exact(&mut data)?;

            let decoded = if self.config.borrow().block_history_compress {
                zstd::decode_all(data.as_slice())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?
            } else {
                data
            };

            if let Ok(block) = bincode::deserialize::<BlockData>(&decoded) {
                temp_blocks.push(block);
            }
        }

        // Second pass: only load the most recent N blocks (lazy loading optimization)
        let total_loaded = temp_blocks.len();
        let start_idx = if total_loaded > lazy_load_threshold {
            log::info!("Lazy loading history: keeping {} recent blocks out of {} total (skipping {} old blocks)",
                lazy_load_threshold, total_loaded, total_loaded - lazy_load_threshold);
            total_loaded - lazy_load_threshold
        } else {
            0
        };

        let mut blocks = self.block_data.borrow_mut();
        for (idx, block) in temp_blocks.into_iter().enumerate() {
            if idx >= start_idx {
                log::debug!("Loaded historical block #{}: prompt={:?}, cmd={:?}, output_len={}, exit_code={}",
                    idx, &block.prompt, &block.cmd, block.output.len(), block.exit_code);
                blocks.push_back(block);
            }
        }

        Ok(())
    }
}

// ─── VTE builder ─────────────────────────────────────────────────────────────

fn build_vte(config: &Config) -> Terminal {
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(true)
        .can_focus(true)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(true)
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

fn build_output_vte(config: &Config) -> Terminal {
    let terminal = Terminal::builder()
        .hexpand(true)
        .vexpand(false)
        .can_focus(false)
        .allow_hyperlink(true)
        .bold_is_bright(true)
        .input_enabled(false)
        .scrollback_lines(0)
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

// ─── CSS ──────────────────────────────────────────────────────────────────────

fn install_block_css(config: &Config) {
    let fg = &config.foreground;
    let bg = &config.background;
    let bg_hex = rgba_to_hex(bg);
    let fg_hex = rgba_to_hex(fg);
    let dim_fg = format!(
        "rgba({},{},{},0.55)",
        (fg.red() * 255.0) as u8,
        (fg.green() * 255.0) as u8,
        (fg.blue() * 255.0) as u8,
    );
    let cursor_hex = rgba_to_hex(&config.cursor);
    // Accent color for active chevron (use palette color 2 = green-ish)
    let accent = rgba_to_hex(&config.palette[2]);

    let fg_r = (fg.red() * 255.0) as u8;
    let fg_g = (fg.green() * 255.0) as u8;
    let fg_b = (fg.blue() * 255.0) as u8;

    // Slightly different background for finished blocks (3% toward fg)
    let bg_r = (bg.red() * 255.0) as u8;
    let bg_g = (bg.green() * 255.0) as u8;
    let bg_b = (bg.blue() * 255.0) as u8;
    let block_bg_hex = format!(
        "#{:02x}{:02x}{:02x}",
        (bg_r as f32 + (fg_r as f32 - bg_r as f32) * 0.03) as u8,
        (bg_g as f32 + (fg_g as f32 - bg_g as f32) * 0.03) as u8,
        (bg_b as f32 + (fg_b as f32 - bg_b as f32) * 0.03) as u8,
    );

    // Parse font description to extract font family and size
    // Format: "FontName Style Size" e.g. "SauceCodePro Nerd Font Regular 14"
    let parts: Vec<&str> = config.font_desc.split_whitespace().collect();
    let (font_family, base_size) = if parts.len() >= 2 {
        // Last part is usually the size
        if let Ok(size) = parts[parts.len() - 1].parse::<i32>() {
            let family = parts[..parts.len() - 1].join(" ");
            (family, size)
        } else {
            (config.font_desc.clone(), 14)
        }
    } else {
        (config.font_desc.clone(), 14)
    };

    // Apply font scale to the base size
    let scaled_size = (base_size as f64 * config.default_font_scale).round() as i32;
    let font_size = format!("{}pt", scaled_size);

    let css = format!(
        r#"
        .block-scroll {{
            background-color: {bg_hex};
        }}
        .block-list {{
            background-color: {bg_hex};
        }}
        .block-finished {{
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.10);
            border-radius: 6px;
            background-color: {block_bg_hex};
            min-height: 40px;
        }}
        .block-hovered {{
            background-color: rgba({fg_r},{fg_g},{fg_b},0.04);
            border-color: rgba({fg_r},{fg_g},{fg_b},0.18);
        }}
        .block-selected {{
            background-color: rgba({fg_r},{fg_g},{fg_b},0.08);
            border-left: 3px solid {fg_hex};
            padding-left: 9px;
        }}
        .block-active {{
            border: 1px solid rgba({fg_r},{fg_g},{fg_b},0.10);
            border-radius: 6px;
            margin: 4px 8px;
            padding-top: 4px;
            background-color: {block_bg_hex};
            min-height: 40px;
        }}
        .block-header {{
            border-radius: 6px 6px 0 0;
        }}
        .block-header-label {{
            color: {dim_fg};
            font-size: 0.85em;
        }}
        .block-collapse-btn {{
            color: {dim_fg};
            font-size: 0.75em;
            min-width: 20px;
            min-height: 20px;
            padding: 0;
        }}
        .block-prompt {{
            color: {dim_fg};
            font-family: "{font_family}";
            font-size: {font_size};
            line-height: 1.0;
            margin: 0;
        }}
        .block-prompt-view {{
            color: {dim_fg};
            font-family: "{font_family}";
            font-size: {font_size};
            padding: 0;
            line-height: 1.2;
            margin: 0;
            background-color: {bg_hex};
            min-height: 48px;
        }}
        .block-prompt-view text {{
            color: {dim_fg};
            background-color: {bg_hex};
        }}
        .block-command-view {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            padding: 0;
            line-height: 1.2;
            margin: 0;
            background-color: {bg_hex};
            min-height: 24px;
            caret-color: {cursor_hex};
        }}
        .block-command-view text {{
            color: {fg_hex};
            background-color: {bg_hex};
            caret-color: {cursor_hex};
        }}
        .block-output-view {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            padding: 0;
            line-height: 1.2;
            margin: 0;
            background-color: {bg_hex};
            min-height: 0;
        }}
        .block-output-view text {{
            color: {fg_hex};
            background-color: {bg_hex};
        }}
        .block-finished .block-command-view {{
            background-color: {block_bg_hex};
        }}
        .block-finished .block-command-view text {{
            background-color: {block_bg_hex};
        }}
        .block-finished .block-output-view {{
            background-color: {block_bg_hex};
        }}
        .block-finished .block-output-view text {{
            background-color: {block_bg_hex};
        }}
        .block-cmd {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            padding: 0;
            line-height: 1.0;
            margin: 0;
            min-height: 0;
        }}
        .block-cmd-active {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            padding: 0;
            line-height: 1.0;
            margin: 0;
            min-height: 0;
            background-color: {bg_hex};
            caret-color: {fg_hex};
        }}
        .block-cmd-active text {{
            background-color: {bg_hex};
            caret-color: {fg_hex};
        }}
        @keyframes blink {{
            0%, 49% {{ opacity: 1; }}
            50%, 100% {{ opacity: 0; }}
        }}
        .block-cmd-active text selection {{
            background-color: transparent;
        }}
        .block-cmd-finished {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            padding: 0;
            line-height: 1.0;
            margin: 0;
            min-height: 0;
            background-color: {bg_hex};
        }}
        .block-cmd-finished text {{
            background-color: {bg_hex};
        }}
        .block-exit-bad {{
            color: #ff5555;
            background-color: rgba(255,85,85,0.18);
            border-radius: 3px;
            font-size: 0.8em;
        }}
        .block-meta-badge {{
            color: {dim_fg};
            background-color: rgba({fg_r},{fg_g},{fg_b},0.08);
            border-radius: 3px;
            font-size: 0.8em;
        }}
        .block-running-label {{
            color: {dim_fg};
            font-size: 0.85em;
            padding-right: 8px;
        }}
        .block-output {{
            background-color: {bg_hex};
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            min-height: 0;
            line-height: 1.0;
            padding: 0;
            margin: 0;
        }}
        .block-show-more {{
            color: {accent};
            margin-left: 12px;
            margin-top: 4px;
            margin-bottom: 4px;
            font-size: 0.85em;
            padding: 2px 8px;
        }}
        "#,
    );

    let provider = gtk4::CssProvider::new();
    provider.load_from_data(&css);
    gtk4::style_context_add_provider_for_display(
        &gtk4::gdk::Display::default().unwrap(),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

#[cfg(test)]
mod tests {
    use super::{
        ansi_text_runs, ansi_to_pango, command_line_plain_text, separate_input_and_suggestion,
        skip_ansi_visible_chars, strip_ansi, strip_ansi_with_clear_detect,
    };
    use gtk4::gdk::RGBA;

    fn palette() -> [RGBA; 16] {
        [RGBA::new(0.0, 0.0, 0.0, 1.0); 16]
    }

    #[test]
    fn strips_charset_designation_from_output() {
        assert_eq!(strip_ansi("\u{1b}(Btop"), "top");
    }

    #[test]
    fn skips_charset_designation_when_counting_visible_chars() {
        assert_eq!(skip_ansi_visible_chars("\u{1b}(Btop", 1), "op");
    }

    #[test]
    fn ignores_charset_designation_in_command_plain_text() {
        assert_eq!(command_line_plain_text("\u{1b}(Btop"), "top");
    }

    #[test]
    fn ignores_charset_designation_in_pango_conversion() {
        assert_eq!(ansi_to_pango("\u{1b}(Btop", &palette()), "top");
    }

    #[test]
    fn preserves_colored_output_runs() {
        let runs = ansi_text_runs("a\u{1b}[31mred\u{1b}[0mz", &palette());
        assert_eq!(
            runs.iter().map(|run| run.text.as_str()).collect::<String>(),
            "aredz"
        );
        assert!(runs
            .iter()
            .any(|run| run.text == "red" && run.style.foreground.is_some()));
    }

    #[test]
    fn cursor_home_and_partial_erase_do_not_clear_block_output() {
        assert_eq!(
            strip_ansi_with_clear_detect("\u{1b}[Hgit output"),
            ("git output".to_string(), false)
        );
        assert_eq!(
            strip_ansi_with_clear_detect("\u{1b}[Jgit output"),
            ("git output".to_string(), false)
        );
        assert_eq!(
            strip_ansi_with_clear_detect("\u{1b}[2Jfresh"),
            ("fresh".to_string(), true)
        );
    }

    #[test]
    fn separates_dim_suggestion_without_cursor_padding() {
        let (input, suggestion) = separate_input_and_suggestion("git p\u{1b}[2mull\u{1b}[0m", 0);
        assert_eq!(input, "git p");
        assert_eq!(suggestion, "ull");
    }

    #[test]
    fn output_cursor_col_end_of_line() {
        // Plain text: cursor at end of last line
        assert_eq!(super::output_cursor_col("hello"), (5, false));
    }

    #[test]
    fn output_cursor_col_after_newline() {
        // After \n: cursor at start of new (empty) line, after_newline=true
        assert_eq!(super::output_cursor_col("hello\n"), (0, true));
    }

    #[test]
    fn output_cursor_col_carriage_return() {
        // After \r: cursor at start of current line, after_newline=false
        assert_eq!(super::output_cursor_col("hello\r"), (0, false));
    }

    #[test]
    fn output_cursor_col_progress_update() {
        // \r then overwrite: cursor ends at col 8 (length of "50% done")
        assert_eq!(super::output_cursor_col("Loading...\r50% done"), (8, false));
    }

    #[test]
    fn output_cursor_col_multiline() {
        // Multi-line: cursor at end of last line
        assert_eq!(super::output_cursor_col("line1\nline2\nend"), (3, false));
    }

    // ── IME / Chinese input support tests ────────────────────────────────

    /// Simulate the logic from connect_commit: insert text at cursor position
    fn simulate_ime_commit(cmd: &str, cursor_pos: usize, committed: &str) -> (String, usize) {
        let mut buf = cmd.to_string();
        let byte_pos = buf
            .char_indices()
            .nth(cursor_pos)
            .map(|(i, _)| i)
            .unwrap_or(buf.len());
        buf.insert_str(byte_pos, committed);
        let new_pos = cursor_pos + committed.chars().count();
        (buf, new_pos)
    }

    #[test]
    fn ime_commit_chinese_at_end() {
        let (buf, pos) = simulate_ime_commit("ls ", 3, "你好");
        assert_eq!(buf, "ls 你好");
        assert_eq!(pos, 5);
    }

    #[test]
    fn ime_commit_chinese_at_beginning() {
        let (buf, pos) = simulate_ime_commit("hello", 0, "世界");
        assert_eq!(buf, "世界hello");
        assert_eq!(pos, 2);
    }

    #[test]
    fn ime_commit_chinese_in_middle() {
        let (buf, pos) = simulate_ime_commit("echo test", 5, "中文");
        assert_eq!(buf, "echo 中文test");
        assert_eq!(pos, 7);
    }

    #[test]
    fn ime_commit_after_existing_chinese() {
        let (buf, pos) = simulate_ime_commit("你好", 2, "世界");
        assert_eq!(buf, "你好世界");
        assert_eq!(pos, 4);
    }

    #[test]
    fn ime_commit_mixed_cjk_ascii() {
        let (buf, pos) = simulate_ime_commit("git commit -m \"", 15, "修复bug");
        assert_eq!(buf, "git commit -m \"修复bug");
        // 修复bug = 5 chars (修,复,b,u,g), so pos = 15 + 5 = 20
        assert_eq!(pos, 20);
    }

    #[test]
    fn ime_preedit_cursor_position() {
        // During composition, cursor should be after cmd + preedit
        let cmd = "echo ";
        let preedit = "niha"; // pinyin input not yet committed
        let cursor_pos = cmd.chars().count() + preedit.chars().count();
        assert_eq!(cursor_pos, 9);
    }

    #[test]
    fn ime_preedit_buffer_format() {
        // The display buffer format: "{cmd}{preedit} {suggestion}"
        let cmd = "echo ";
        let preedit = "你好";
        let suggestion = "";
        let text = format!("{}{} {}", cmd, preedit, suggestion);
        assert_eq!(text, "echo 你好 ");
        // Preedit tag range: cmd.chars().count() .. cmd.chars().count() + preedit.chars().count()
        let preedit_start = cmd.chars().count();
        let preedit_end = preedit_start + preedit.chars().count();
        assert_eq!(preedit_start, 5);
        assert_eq!(preedit_end, 7);
    }

    #[test]
    fn ime_commit_clears_preedit_state() {
        // After commit, preedit should be empty and cursor advances
        let cmd = "ls ";
        let _preedit = "zhong"; // composing
        // Simulate commit of "中"
        let (buf, pos) = simulate_ime_commit(cmd, cmd.chars().count(), "中");
        assert_eq!(buf, "ls 中");
        assert_eq!(pos, 4);
        // preedit should be cleared (tested by set_preedit("") after commit)
        let final_preedit = "";
        let display = format!("{} {}", buf, final_preedit);
        assert_eq!(display, "ls 中 ");
    }

    #[test]
    fn ime_backspace_chinese_char() {
        // Backspace should delete one full CJK character
        let cmd = "你好世界";
        let pos = 4; // cursor at end
        let mut buf = cmd.to_string();
        let byte_pos = buf
            .char_indices()
            .nth(pos - 1)
            .map(|(i, _)| i)
            .unwrap_or(0);
        let next_byte = buf
            .char_indices()
            .nth(pos)
            .map(|(i, _)| i)
            .unwrap_or(buf.len());
        buf.drain(byte_pos..next_byte);
        assert_eq!(buf, "你好世");
        assert_eq!(buf.chars().count(), 3);
    }

    #[test]
    fn ime_cursor_movement_with_chinese() {
        // Left/right should move by one char (not byte)
        let cmd = "你好world";
        let chars: Vec<char> = cmd.chars().collect();
        assert_eq!(chars.len(), 7); // 你好 = 2 chars, world = 5 chars
        // At pos 2, cursor is between '好' and 'w'
        let pos = 2;
        assert_eq!(chars[pos - 1], '好');
        assert_eq!(chars[pos], 'w');
    }
}
