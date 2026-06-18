use gtk4::pango::FontDescription;
use gtk4::prelude::*;
use gtk4::{glib, Orientation, ScrolledWindow};
use lru::LruCache;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use vte4::Terminal;
use vte4::{TerminalExt, TerminalExtManual};
use gtk4::gdk::RGBA;

use crate::config::Config;
use crate::parser::{Parser, ParserEvent};
use crate::pty::OwnedPty;

mod ansi;
mod alt_screen;
mod blocks;
mod css;
mod scroll;
mod select;
mod url;
pub(crate) use ansi::*;
pub(crate) use alt_screen::*;
pub(crate) use blocks::*;
pub(crate) use css::*;
pub(crate) use scroll::*;
pub(crate) use select::*;
pub(crate) use url::*;


// ── perf profiling (env JTERM_PROF=1) ───────────────────────────────────────
pub(crate) fn prof_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("JTERM_PROF").is_ok())
}

// Global block ID counter
static BLOCK_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_block_id() -> u64 {
    BLOCK_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Update the jump-to-bottom FAB's label to show an unread-block badge: just the
/// chevron when nothing is pending, chevron + count (clamped to "99+") otherwise.
fn set_jump_fab_label(fab: &gtk4::Button, unread: u32) {
    if unread > 0 {
        let n = if unread > 99 { "99+".to_string() } else { unread.to_string() };
        fab.set_label(&format!("\u{f078}  {}", n));
    } else {
        fab.set_label("\u{f078}");
    }
}

/// Move the finished-block selection to `new_id` (or clear it with `None`),
/// updating the selected CSS class and persistent quick-action visibility on both
/// the previously-selected and newly-selected blocks. Shared by click selection
/// and keyboard navigation so they stay in sync.
fn select_finished_block(
    finished: &[FinishedBlock],
    selected_block_id: &Rc<Cell<Option<u64>>>,
    new_id: Option<u64>,
) {
    let prev = selected_block_id.get();
    if let Some(pid) = prev {
        if let Some(b) = finished.iter().find(|b| b.id == pid) {
            b.widget().remove_css_class("block-selected");
            b.action_box.set_visible(false);
        }
    }
    if let Some(nid) = new_id {
        if let Some(b) = finished.iter().find(|b| b.id == nid) {
            b.widget().add_css_class("block-selected");
            b.action_box.set_visible(true);
        }
    }
    selected_block_id.set(new_id);
}

/// Subsequence fuzzy match: returns `Some(score)` if every char of `query`
/// appears in `text` in order (case-insensitive), else `None`. Lower score is a
/// better match (penalizes a late first match and gaps between matched chars).
fn fuzzy_score(query: &str, text: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut score: i64 = 0;
    let mut last: i64 = -1;
    for (ti, ch) in text.to_lowercase().chars().enumerate() {
        if qi < q.len() && ch == q[qi] {
            score += if last < 0 { ti as i64 } else { ti as i64 - last - 1 };
            last = ti as i64;
            qi += 1;
        }
    }
    if qi == q.len() { Some(score) } else { None }
}

/// Pop up a fuzzy-searchable command-history palette (Ctrl+P). `commands` should
/// be most-recent-first and deduped. Selecting an entry clears the live shell
/// line and types the command into it (without executing), mirroring the
/// single-block recall path so the user can edit before pressing Enter.
fn show_command_palette(
    parent: &ScrolledWindow,
    commands: Vec<String>,
    pty: Rc<OwnedPty>,
    typed_cmd: Rc<RefCell<String>>,
    live_vte: Terminal,
) {
    let popover = gtk4::Popover::new();
    popover.set_parent(parent);
    popover.set_has_arrow(false);
    popover.set_autohide(true);
    popover.add_css_class("command-palette");
    popover.set_position(gtk4::PositionType::Bottom);
    let pw = parent.width().max(1);
    popover.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(pw / 2, 0, 1, 1)));

    let vbox = gtk4::Box::new(Orientation::Vertical, 6);
    vbox.set_size_request(540, -1);

    let entry = gtk4::SearchEntry::new();
    entry.set_placeholder_text(Some("Search command history…"));
    vbox.append(&entry);

    let list = gtk4::ListBox::new();
    list.set_selection_mode(gtk4::SelectionMode::Single);
    list.add_css_class("command-palette-list");

    let scroller = ScrolledWindow::new();
    scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scroller.set_min_content_height(300);
    scroller.set_max_content_height(300);
    scroller.set_child(Some(&list));
    vbox.append(&scroller);
    popover.set_child(Some(&vbox));

    let commands = Rc::new(commands);
    let filtered: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let populate = {
        let list = list.clone();
        let commands = commands.clone();
        let filtered = filtered.clone();
        move |query: &str| {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let mut scored: Vec<(i64, &String)> = commands
                .iter()
                .filter_map(|c| fuzzy_score(query, c).map(|s| (s, c)))
                .collect();
            // Stable sort keeps recency (input order) as the tiebreak.
            scored.sort_by_key(|(s, _)| *s);
            let mut keep = Vec::with_capacity(scored.len());
            for (_, c) in scored {
                let row_label = gtk4::Label::new(Some(c));
                row_label.set_halign(gtk4::Align::Start);
                row_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                row_label.add_css_class("command-palette-row");
                let row = gtk4::ListBoxRow::new();
                row.set_child(Some(&row_label));
                list.append(&row);
                keep.push(c.clone());
            }
            *filtered.borrow_mut() = keep;
            if let Some(first) = list.row_at_index(0) {
                list.select_row(Some(&first));
            }
        }
    };
    populate("");

    let choose: Rc<dyn Fn()> = {
        let list = list.clone();
        let filtered = filtered.clone();
        let popover = popover.clone();
        let scroll = parent.clone();
        Rc::new(move || {
            let idx = list.selected_row().map(|r| r.index()).unwrap_or(-1);
            if idx >= 0 {
                if let Some(cmd) = filtered.borrow().get(idx as usize) {
                    pty.write_bytes(b"\x15");
                    pty.write_bytes(cmd.as_bytes());
                    typed_cmd.borrow_mut().clear();
                }
            }
            popover.popdown();
            // Dismissing the popover returns focus to the live VTE, which makes the
            // ScrolledWindow scroll to reveal the holder's *top* (jumping up into
            // history). Re-pin to the bottom so the user lands back on the prompt
            // with the recalled command.
            let scroll = scroll.clone();
            let live_vte = live_vte.clone();
            glib::idle_add_local_once(move || {
                live_vte.grab_focus();
                let adj = scroll.vadjustment();
                adj.set_value((adj.upper() - adj.page_size()).max(adj.lower()));
            });
        })
    };

    {
        let populate = populate.clone();
        entry.connect_search_changed(move |e| populate(e.text().as_str()));
    }

    {
        let choose = choose.clone();
        list.connect_row_activated(move |list, row| {
            list.select_row(Some(row));
            choose();
        });
    }

    {
        let list = list.clone();
        let popover = popover.clone();
        let choose = choose.clone();
        let key = gtk4::EventControllerKey::new();
        key.set_propagation_phase(gtk4::PropagationPhase::Capture);
        key.connect_key_pressed(move |_, keyval, _, _| {
            use gtk4::gdk::Key;
            let n_rows = {
                let mut n = 0;
                while list.row_at_index(n).is_some() {
                    n += 1;
                }
                n
            };
            let cur = list.selected_row().map(|r| r.index()).unwrap_or(-1);
            match keyval {
                Key::Up => {
                    if n_rows > 0 {
                        let next = if cur <= 0 { n_rows - 1 } else { cur - 1 };
                        if let Some(r) = list.row_at_index(next) {
                            list.select_row(Some(&r));
                        }
                    }
                    glib::Propagation::Stop
                }
                Key::Down => {
                    if n_rows > 0 {
                        let next = if cur < 0 || cur >= n_rows - 1 { 0 } else { cur + 1 };
                        if let Some(r) = list.row_at_index(next) {
                            list.select_row(Some(&r));
                        }
                    }
                    glib::Propagation::Stop
                }
                Key::Return | Key::KP_Enter => {
                    choose();
                    glib::Propagation::Stop
                }
                Key::Escape => {
                    popover.popdown();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        entry.add_controller(key);
    }

    // A Popover with an explicit parent must be unparented when dismissed or it
    // leaks (and warns at teardown).
    popover.connect_closed(|p| p.unparent());

    popover.popup();
    entry.grab_focus();
}

/// Cap on the retained raw output buffer for a single running command. The raw
/// byte buffer used to re-render the finished block grew without bound — a runaway
/// command (`cat /dev/urandom`) could exhaust memory before CommandEnd. When the
/// buffer exceeds this, the oldest bytes are dropped, keeping the most recent tail
/// (the part a finished block actually shows). 8 MiB comfortably covers any normal
/// command's output.
const MAX_RAW_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Minimum rows the live input cell is guaranteed when idle (warp-style compact
/// input): it shrinks to fit the prompt + typed command but never below this, so
/// there is always usable room to type. It grows with multiline input up to the
/// viewport, and is forced to the full viewport only for alt-screen apps.
const MIN_INPUT_ROWS: i32 = 6;

#[allow(dead_code)]
pub struct TermView {
    root: gtk4::Box,
    block_scroll: ScrolledWindow,
    block_list: gtk4::Box,
    /// The single persistent live VTE (jterm1 model): prompt + typing + output all
    /// render here natively; finished commands snapshot into styled blocks above.
    active_vte: Terminal,
    active: Rc<RefCell<ActiveBlock>>,
    bstate: Rc<Cell<BlockState>>,
    prompt_buf: Rc<RefCell<String>>,
    cmd_buf: Rc<RefCell<String>>,
    /// Command line reconstructed from VTE `commit` keystrokes while awaiting a
    /// command — the source the finalize path styles into the finished block.
    typed_cmd: Rc<RefCell<String>>,
    /// True while an alt-screen app owns the viewport (finished blocks hidden).
    fullscreen: Rc<Cell<bool>>,
    /// True once the user has scrolled up off the live prompt; while false the
    /// view follows the bottom. Read by the per-frame tick to re-pin the prompt.
    user_scrolled_up: Rc<Cell<bool>>,
    /// Guards programmatic scrolls so the scroll-lock detector doesn't mistake
    /// them for a user drag.
    programmatic_scroll: Rc<Cell<bool>>,
    pty: Rc<OwnedPty>,
    cwd_callbacks: StrCallbacks,
    exited_callbacks: IntCallbacks,
    bell_callbacks: VoidCallbacks,
    title_callbacks: StrCallbacks,
    activity_callbacks: VoidCallbacks,
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
    selected_block_id: Rc<Cell<Option<u64>>>,
    current_cwd: Rc<RefCell<String>>,
    /// Per-frame resize tick installed on `root`. Held so it can be removed on
    /// Drop — otherwise the callback runs forever and keeps its Rc captures
    /// (pty/active/vte/vte_box) alive past tab close.
    resize_tick_id: RefCell<Option<gtk4::TickCallbackId>>,
}

impl Drop for TermView {
    fn drop(&mut self) {
        if let Some(id) = self.resize_tick_id.borrow_mut().take() {
            id.remove();
        }
    }
}

/// Captures the shared handles the PTY reader/exit callbacks need, so
/// `TermView::new` does not carry the reader closure inline.
struct ReaderCtx {
    active_rc: Rc<RefCell<ActiveBlock>>,
    /// The live VTE — every byte is fed here; alt-screen toggles feed it 1049h/l.
    active_vte: Terminal,
    bstate_rc: Rc<Cell<BlockState>>,
    /// State to restore when an alt-screen app exits (jterm1 model).
    prev_state_rc: Rc<Cell<BlockState>>,
    osc133_depth_rc: Rc<Cell<u32>>,
    prompt_buf_rc: Rc<RefCell<String>>,
    cmd_buf_rc: Rc<RefCell<String>>,
    /// Keystroke-reconstructed command line (built in connect_commit).
    typed_cmd_rc: Rc<RefCell<String>>,
    /// Rendered prompt (last non-empty line) captured at PromptEnd, used by the
    /// finalize path since prompt_buf is cleared once the prompt ends.
    prompt_display_rc: Rc<RefCell<String>>,
    block_list_rc: gtk4::Box,
    block_scroll_rc: ScrolledWindow,
    cwd_cbs: StrCallbacks,
    exited_cbs: IntCallbacks,
    bell_cbs: VoidCallbacks,
    title_cbs: StrCallbacks,
    activity_cbs: VoidCallbacks,
    bracketed_paste_rc: Rc<Cell<bool>>,
    application_cursor_rc: Rc<Cell<bool>>,
    mouse_reporting_rc: Rc<Cell<MouseReportingMode>>,
    cursor_shape_rc: Rc<Cell<TermCursorShape>>,
    config_for_cb: Rc<RefCell<Config>>,
    parser: Rc<RefCell<Parser>>,
    block_data_for_cb: Rc<RefCell<VecDeque<BlockData>>>,
    finished_blocks_for_cb: Rc<RefCell<Vec<FinishedBlock>>>,
    pager_snapshots_rc: Rc<RefCell<Vec<String>>>,
    pager_snapshot_generation_rc: Rc<Cell<u64>>,
    pager_pre_clear_rc: Rc<RefCell<String>>,
    scroll_debouncer: ScrollDebouncer,
    ansi_cache_for_cb: Rc<RefCell<LruCache<String, String>>>,
    widget_pool_for_cb: Rc<RefCell<WidgetPool>>,
    pty_synced_rc: Rc<Cell<bool>>,
    visible_indices_rc: Rc<RefCell<std::collections::HashSet<usize>>>,
    fullscreen_rc: Rc<Cell<bool>>,
    ftcs_seen_rc: Rc<Cell<bool>>,
    init_cmds_queue_for_cb: Rc<RefCell<std::collections::VecDeque<String>>>,
    pty_for_init: Rc<OwnedPty>,
    block_start_time_for_cb: Rc<Cell<Option<SystemTime>>>,
    pending_exit_code_rc: Rc<Cell<i32>>,
    current_cwd_for_cb: Rc<RefCell<String>>,
    event_buf: Rc<RefCell<Vec<ParserEvent>>>,
    unread_count_rc: Rc<Cell<u32>>,
    jump_fab: gtk4::Button,
    selected_block_id_rc: Rc<Cell<Option<u64>>>,
    cmd_running_rc: Rc<Cell<bool>>,
    running_cmd_rc: Rc<RefCell<String>>,
}

impl ReaderCtx {
    fn install(self, pty: &Rc<OwnedPty>) {
        let ReaderCtx {
            active_rc,
            active_vte,
            bstate_rc,
            prev_state_rc,
            osc133_depth_rc,
            prompt_buf_rc,
            cmd_buf_rc,
            typed_cmd_rc,
            prompt_display_rc,
            block_list_rc,
            block_scroll_rc,
            cwd_cbs,
            exited_cbs,
            bell_cbs,
            title_cbs,
            activity_cbs,
            bracketed_paste_rc,
            application_cursor_rc,
            mouse_reporting_rc,
            cursor_shape_rc,
            config_for_cb,
            parser,
            block_data_for_cb,
            finished_blocks_for_cb,
            pager_snapshots_rc,
            pager_snapshot_generation_rc,
            pager_pre_clear_rc,
            scroll_debouncer,
            ansi_cache_for_cb,
            widget_pool_for_cb,
            pty_synced_rc,
            visible_indices_rc,
            fullscreen_rc,
            ftcs_seen_rc,
            init_cmds_queue_for_cb,
            pty_for_init,
            block_start_time_for_cb,
            pending_exit_code_rc,
            current_cwd_for_cb,
            event_buf,
            unread_count_rc,
            jump_fab,
            selected_block_id_rc,
            cmd_running_rc,
            running_cmd_rc,
        } = self;
        let _ = &ansi_cache_for_cb;
        pty.start_reader(
            move |data: Vec<u8>| {
                let mut events = event_buf.borrow_mut();
                events.clear();
                parser.borrow_mut().feed(&data, &mut events);

                for event in events.iter() {
                    let state = bstate_rc.get();
                    match event {
                        ParserEvent::Bytes(bytes) => {
                            // Bell (real BEL, not an OSC terminator).
                            if contains_bell(bytes) {
                                for cb in bell_cbs.borrow().iter() {
                                    cb();
                                }
                            }

                            // Single-pass byte scan for control sequences
                            {
                                let mut i = 0;
                                while i < bytes.len() {
                                    if bytes[i] == 0x1b && i + 1 < bytes.len() {
                                        match bytes[i + 1] {
                                            b']' => {
                                                if i + 3 < bytes.len()
                                                    && (bytes[i + 2] == b'0' || bytes[i + 2] == b'2')
                                                    && bytes[i + 3] == b';'
                                                {
                                                    let title_start = i + 4;
                                                    let mut title_end = title_start;
                                                    while title_end < bytes.len() {
                                                        if bytes[title_end] == 0x07 {
                                                            break;
                                                        }
                                                        if bytes[title_end] == 0x1b
                                                            && title_end + 1 < bytes.len()
                                                            && bytes[title_end + 1] == b'\\'
                                                        {
                                                            break;
                                                        }
                                                        title_end += 1;
                                                    }
                                                    if title_end > title_start {
                                                        if let Ok(title) = std::str::from_utf8(&bytes[title_start..title_end]) {
                                                            for cb in title_cbs.borrow().iter() {
                                                                cb(title);
                                                            }
                                                        }
                                                    }
                                                    i = title_end;
                                                }
                                            }
                                            b'[' => {
                                                if i + 2 < bytes.len() && bytes[i + 2] == b'?' {
                                                    let seq_start = i + 3;
                                                    let mut seq_end = seq_start;
                                                    while seq_end < bytes.len() && (bytes[seq_end].is_ascii_digit() || bytes[seq_end] == b';') {
                                                        seq_end += 1;
                                                    }
                                                    if seq_end < bytes.len() {
                                                        let final_byte = bytes[seq_end];
                                                        let param_slice = &bytes[seq_start..seq_end];
                                                        match (param_slice, final_byte) {
                                                            (b"2004", b'h') => bracketed_paste_rc.set(true),
                                                            (b"2004", b'l') => bracketed_paste_rc.set(false),
                                                            (b"1", b'h') => application_cursor_rc.set(true),
                                                            (b"1", b'l') => application_cursor_rc.set(false),
                                                            (b"1000", b'h') => mouse_reporting_rc.set(MouseReportingMode::Click),
                                                            (b"1002", b'h') => mouse_reporting_rc.set(MouseReportingMode::Button),
                                                            (b"1003", b'h') => mouse_reporting_rc.set(MouseReportingMode::Motion),
                                                            (b"1006", b'h') => mouse_reporting_rc.set(MouseReportingMode::Sgr),
                                                            (b"1000", b'l') => {
                                                                if mouse_reporting_rc.get() == MouseReportingMode::Click {
                                                                    mouse_reporting_rc.set(MouseReportingMode::None);
                                                                }
                                                            }
                                                            (b"1002", b'l') => {
                                                                if mouse_reporting_rc.get() == MouseReportingMode::Button {
                                                                    mouse_reporting_rc.set(MouseReportingMode::None);
                                                                }
                                                            }
                                                            (b"1003", b'l') => {
                                                                if mouse_reporting_rc.get() == MouseReportingMode::Motion {
                                                                    mouse_reporting_rc.set(MouseReportingMode::None);
                                                                }
                                                            }
                                                            (b"1006", b'l') => {
                                                                if mouse_reporting_rc.get() == MouseReportingMode::Sgr {
                                                                    mouse_reporting_rc.set(MouseReportingMode::None);
                                                                }
                                                            }
                                                            _ => {}
                                                        }
                                                        i = seq_end;
                                                    }
                                                } else {
                                                    let seq_start = i + 2;
                                                    let mut seq_end = seq_start;
                                                    while seq_end < bytes.len() && (bytes[seq_end].is_ascii_digit() || bytes[seq_end] == b' ') {
                                                        seq_end += 1;
                                                    }
                                                    if seq_end < bytes.len() && bytes[seq_end] == b'q' {
                                                        let param_slice = &bytes[seq_start..seq_end];
                                                        let param_str = param_slice.iter()
                                                            .filter(|b| b.is_ascii_digit())
                                                            .copied()
                                                            .collect::<Vec<u8>>();
                                                        match param_str.as_slice() {
                                                            b"0" | b"1" | b"2" => cursor_shape_rc.set(TermCursorShape::Block),
                                                            b"3" | b"4" => cursor_shape_rc.set(TermCursorShape::Underline),
                                                            b"5" | b"6" => cursor_shape_rc.set(TermCursorShape::Bar),
                                                            _ => {}
                                                        }
                                                        i = seq_end;
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                    i += 1;
                                }
                            }

                            // No shell integration seen yet: once real output flows,
                            // stream everything into the live VTE (raw fallback).
                            if state == BlockState::Idle {
                                bstate_rc.set(BlockState::RawFallback);
                            }

                            match bstate_rc.get() {
                                BlockState::CollectingPrompt => {
                                    let text = String::from_utf8_lossy(bytes);
                                    prompt_buf_rc.borrow_mut().push_str(&text);
                                    scroll_debouncer.mark_dirty(&block_scroll_rc);
                                }
                                BlockState::AwaitingCommand => {
                                    let text = String::from_utf8_lossy(bytes);
                                    cmd_buf_rc.borrow_mut().push_str(&text);
                                    scroll_debouncer.mark_dirty(&block_scroll_rc);
                                }
                                BlockState::CollectingOutput | BlockState::PostCommand => {
                                    active_rc.borrow().accumulate_output(bytes);
                                    for cb in activity_cbs.borrow().iter() {
                                        cb();
                                    }
                                    scroll_debouncer.mark_dirty(&block_scroll_rc);
                                }
                                BlockState::AltScreen => {
                                    // A pager repainting a fresh page clears the screen
                                    // first; snapshot the current grid before the clear
                                    // lands so paged content is preserved.
                                    if contains_clear_screen(bytes) {
                                        record_pager_snapshot(&active_vte, &pager_snapshots_rc, &pager_pre_clear_rc);
                                    }
                                    schedule_pager_snapshot(
                                        &active_vte,
                                        &pager_snapshots_rc,
                                        &pager_snapshot_generation_rc,
                                        &pager_pre_clear_rc,
                                    );
                                }
                                _ => {}
                            }

                            // Everything renders in the one live VTE.
                            active_vte.feed(bytes);
                        }

                        ParserEvent::PromptStart => {
                            ftcs_seen_rc.set(true);
                            let state = bstate_rc.get();
                            if state == BlockState::CollectingOutput || state == BlockState::AltScreen {
                                continue;
                            }
                            // Finalize the previous command (deferred from CommandEnd).
                            if state == BlockState::PostCommand {
                                // Command: prefer the keystroke-reconstructed line,
                                // fall back to scraping the last echoed line.
                                let typed = typed_cmd_rc.borrow().trim().to_string();
                                let cmd = if !typed.is_empty() {
                                    typed
                                } else {
                                    strip_ansi(&cmd_buf_rc.borrow())
                                        .lines()
                                        .next_back()
                                        .unwrap_or("")
                                        .trim()
                                        .to_string()
                                };

                                if cmd.is_empty() {
                                    // Nothing meaningful to record; just reset.
                                    active_rc.borrow().reset_active();
                                    bstate_rc.set(BlockState::CollectingPrompt);
                                    prompt_buf_rc.borrow_mut().clear();
                                    scroll_debouncer.mark_dirty(&block_scroll_rc);
                                    continue;
                                }

                                let prompt = prompt_display_rc.borrow().clone();

                                let raw_output_text = active_rc.borrow().output_text();

                                // Preserve ANSI codes for colored display, only handle
                                // \r overwrites within a line.
                                let output_with_ansi = {
                                    let mut result = String::new();
                                    for line in raw_output_text.lines() {
                                        if let Some(pos) = line.rfind('\r') {
                                            result.push_str(&line[pos + 1..]);
                                        } else {
                                            result.push_str(line);
                                        }
                                        result.push('\n');
                                    }
                                    result.trim().to_string()
                                };

                                let output_plain = strip_ansi(&output_with_ansi).to_string();

                                let truncation_limit = config_for_cb.borrow().truncation_threshold_lines as usize;
                                let output_trimmed = {
                                    let trimmed = output_plain.trim();
                                    let lines: Vec<&str> = trimmed.lines().collect();
                                    if lines.len() > truncation_limit {
                                        let kept: String = lines[..truncation_limit].join("\n");
                                        format!("{}\n\n[... truncated: {} lines total, showing first {}]", kept, lines.len(), truncation_limit)
                                    } else {
                                        trimmed.to_string()
                                    }
                                };

                                let line_count = output_trimmed.lines().count();
                                let estimated_height = {
                                    let cfg = config_for_cb.borrow();
                                    let parts: Vec<&str> = cfg.font_desc.split_whitespace().collect();
                                    let base_size = parts
                                        .last()
                                        .and_then(|s| s.parse::<i32>().ok())
                                        .unwrap_or(14);
                                    let scaled_pt = (base_size as f64 * cfg.default_font_scale).max(1.0);
                                    let per_line = (scaled_pt * (96.0 / 72.0) * 1.2).ceil() as i32;
                                    (line_count as i32 * per_line.max(1)).max(60)
                                };

                                let start_time = block_start_time_for_cb.get();
                                let now = SystemTime::now();
                                let end_time_ms = now.duration_since(SystemTime::UNIX_EPOCH).ok().map(|d| d.as_millis() as u64);
                                let start_time_ms = start_time.and_then(|st| st.duration_since(SystemTime::UNIX_EPOCH).ok().map(|d| d.as_millis() as u64));
                                let duration_ms = start_time.and_then(|st| {
                                    now.duration_since(st).ok().map(|d| d.as_millis() as u64)
                                });

                                let block_cwd = {
                                    let cwd_str = current_cwd_for_cb.borrow().clone();
                                    if cwd_str.is_empty() { None } else { Some(cwd_str) }
                                };

                                let exit_code = pending_exit_code_rc.get();

                                let block_data = BlockData {
                                    id: next_block_id(),
                                    prompt: prompt.clone(),
                                    cmd: cmd.clone(),
                                    cmd_markup: None,
                                    output: output_plain.trim().to_string(),
                                    exit_code,
                                    estimated_height,
                                    line_count,
                                    start_time_ms,
                                    end_time_ms,
                                    duration_ms,
                                    cwd: block_cwd.clone(),
                                };

                                block_data_for_cb.borrow_mut().push_back(block_data);

                                // Pre-wrap the finished output at the SAME column the
                                // live VTE wrapped at, so the completed block keeps the
                                // identical line breaks (no reflow jump on completion).
                                let wrap_cols = active_rc.borrow().grid_cols();
                                let display_output_ansi = blocks::wrap_ansi_at(&output_with_ansi, wrap_cols);

                                let recycled = widget_pool_for_cb.borrow_mut().acquire();
                                let finished = FinishedBlock::new_with_pool(
                                    &prompt, &cmd, None, &display_output_ansi, exit_code, &config_for_cb.borrow(),
                                    duration_ms, end_time_ms, block_cwd.as_deref(), recycled,
                                );

                                finished.widget().insert_before(&block_list_rc, Some(active_rc.borrow().widget()));

                                // If the user is reading history (scrolled up), this
                                // freshly-finished block is "unread": bump the FAB badge
                                // so they can see work completed below and jump to it.
                                if scroll_debouncer.user_scrolled_up.get() {
                                    unread_count_rc.set(unread_count_rc.get().saturating_add(1));
                                    set_jump_fab_label(&jump_fab, unread_count_rc.get());
                                    jump_fab.set_visible(true);
                                }

                                let max_blocks = config_for_cb.borrow().max_visible_blocks as usize;
                                let finished_clone = finished.clone();
                                let finished_widget = finished_clone.widget().clone();

                                finished_clone.connect_actions(&active_vte, &pty_for_init, &pty_synced_rc, &active_rc);

                                finished_blocks_for_cb.borrow_mut().push(finished);

                                // Right-click context menu.
                                let finished_blocks_for_menu = finished_blocks_for_cb.clone();
                                let block_list_for_menu = block_list_rc.clone();
                                let vte_for_copy = active_vte.clone();
                                let block_id = finished_clone.id;

                                let right_click = gtk4::GestureClick::new();
                                right_click.set_button(3);

                                let finished_menu_clone = finished_clone.clone();
                                let block_data_for_export = block_data_for_cb.clone();
                                right_click.connect_pressed(move |gesture, _n_press, x, y| {
                                    gesture.set_state(gtk4::EventSequenceState::Claimed);

                                    let popover = gtk4::Popover::new();
                                    let widget: &gtk4::Widget = &finished_menu_clone.widget().clone().upcast::<gtk4::Widget>();
                                    popover.set_parent(widget);
                                    popover.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(
                                        x as i32, y as i32, 1, 1,
                                    )));
                                    popover.set_has_arrow(false);

                                    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
                                    vbox.add_css_class("menu");

                                    let make_item = |label: &str| -> gtk4::Button {
                                        let btn = gtk4::Button::with_label(label);
                                        btn.set_has_frame(false);
                                        btn.set_halign(gtk4::Align::Fill);
                                        if let Some(child) = btn.child() {
                                            child.set_halign(gtk4::Align::Start);
                                        }
                                        btn.add_css_class("flat");
                                        btn
                                    };

                                    {
                                        let item = make_item("Copy Block");
                                        let popover_c = popover.clone();
                                        let finished_for_copy = finished_menu_clone.clone();
                                        let vte_for_action = vte_for_copy.clone();
                                        item.connect_clicked(move |_| {
                                            popover_c.popdown();
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
                                        vbox.append(&item);
                                    }

                                    {
                                        let item = make_item("Export as JSON");
                                        let popover_c = popover.clone();
                                        let block_data_for_json = block_data_for_export.clone();
                                        let vte_for_json = vte_for_copy.clone();
                                        let block_id_json = block_id;
                                        item.connect_clicked(move |_| {
                                            popover_c.popdown();
                                            let blocks = block_data_for_json.borrow();
                                            if let Some(block) = blocks.iter().find(|b| b.id == block_id_json) {
                                                let json = block.to_json();
                                                vte_for_json.clipboard().set_text(&json);
                                            }
                                        });
                                        vbox.append(&item);
                                    }

                                    {
                                        let item = make_item("Export as Markdown");
                                        let popover_c = popover.clone();
                                        let block_data_for_md = block_data_for_export.clone();
                                        let vte_for_md = vte_for_copy.clone();
                                        let block_id_md = block_id;
                                        item.connect_clicked(move |_| {
                                            popover_c.popdown();
                                            let blocks = block_data_for_md.borrow();
                                            if let Some(block) = blocks.iter().find(|b| b.id == block_id_md) {
                                                let markdown = block.to_markdown();
                                                vte_for_md.clipboard().set_text(&markdown);
                                            }
                                        });
                                        vbox.append(&item);
                                    }

                                    {
                                        let item = make_item("Delete Block");
                                        let popover_c = popover.clone();
                                        let finished_blocks_for_delete = finished_blocks_for_menu.clone();
                                        let block_list_for_delete = block_list_for_menu.clone();
                                        let block_id_del = block_id;
                                        item.connect_clicked(move |_| {
                                            popover_c.popdown();
                                            let mut blocks = finished_blocks_for_delete.borrow_mut();
                                            if let Some(pos) = blocks.iter().position(|b| b.id == block_id_del) {
                                                let block = blocks.remove(pos);
                                                block_list_for_delete.remove(block.widget());
                                            }
                                        });
                                        vbox.append(&item);
                                    }

                                    popover.set_child(Some(&vbox));
                                    popover.connect_closed(move |p| {
                                        p.unparent();
                                    });
                                    popover.popup();
                                });
                                finished_widget.add_controller(right_click);

                                // Single capture-phase click handler for the whole
                                // block: it both moves focus to the live VTE and (when
                                // the press lands on the header strip) toggles selection.
                                //
                                // This is deliberately ONE gesture rather than a separate
                                // GestureClick on header_row. A capture-phase gesture here
                                // grabs focus on press, which interrupts delivery of the
                                // bubble-phase event to any controller mounted on a child
                                // widget — so a header_row gesture would silently never
                                // fire. Instead we gate selection on the press y-coordinate
                                // falling within the header strip's height. Selection
                                // enables Enter-to-rerun and keeps the quick actions
                                // visible after the pointer leaves.
                                {
                                    let active_for_click = active_rc.clone();
                                    let header_for_click = finished_clone.header_row.clone();
                                    let finished_blocks_for_select = finished_blocks_for_cb.clone();
                                    let selected_for_click = selected_block_id_rc.clone();
                                    let this_id = finished_clone.id;
                                    let left_click = gtk4::GestureClick::new();
                                    left_click.set_button(1);
                                    left_click.set_propagation_phase(gtk4::PropagationPhase::Capture);
                                    left_click.connect_pressed(move |gesture, _, _, y| {
                                        active_for_click.borrow().grab_focus();
                                        // Header strip occupies the top of the block; a
                                        // press there toggles this block's selection.
                                        if y <= header_for_click.height() as f64 {
                                            let finished = finished_blocks_for_select.borrow();
                                            let target = if selected_for_click.get() == Some(this_id) {
                                                None
                                            } else {
                                                Some(this_id)
                                            };
                                            select_finished_block(&finished, &selected_for_click, target);
                                        }
                                        gesture.set_state(gtk4::EventSequenceState::Denied);
                                    });
                                    finished_widget.add_controller(left_click);
                                }

                                if finished_blocks_for_cb.borrow().len() > max_blocks {
                                    let oldest = finished_blocks_for_cb.borrow_mut().remove(0);
                                    let widget_to_release = oldest.widget().clone();
                                    block_list_rc.remove(&widget_to_release);
                                    widget_pool_for_cb.borrow_mut().release(widget_to_release);
                                }

                                if block_data_for_cb.borrow().len() > max_blocks {
                                    block_data_for_cb.borrow_mut().pop_front();
                                }

                                active_rc.borrow().reset_active();
                                scroll_debouncer.reset_scroll_lock();
                            }
                            bstate_rc.set(BlockState::CollectingPrompt);
                            prompt_buf_rc.borrow_mut().clear();
                            scroll_debouncer.mark_dirty(&block_scroll_rc);
                        }

                        ParserEvent::PromptEnd => {
                            if bstate_rc.get() != BlockState::CollectingPrompt {
                                continue;
                            }
                            // Capture the rendered prompt (last non-empty line) for the
                            // finished block / export.
                            let prompt_line = {
                                let pb = prompt_buf_rc.borrow();
                                strip_ansi(&pb)
                                    .lines()
                                    .rev()
                                    .find(|l| !l.trim().is_empty())
                                    .unwrap_or("")
                                    .trim()
                                    .to_string()
                            };
                            *prompt_display_rc.borrow_mut() = prompt_line;
                            prompt_buf_rc.borrow_mut().clear();
                            cmd_buf_rc.borrow_mut().clear();
                            typed_cmd_rc.borrow_mut().clear();
                            pty_synced_rc.set(false);
                            bstate_rc.set(BlockState::AwaitingCommand);

                            // Feed next initial command if any.
                            if let Some(cmd) = init_cmds_queue_for_cb.borrow_mut().pop_front() {
                                let text = format!("{}\r", cmd);
                                pty_for_init.write_bytes(text.as_bytes());
                            }

                            scroll_debouncer.reset_scroll_lock();
                            scroll_debouncer.mark_dirty(&block_scroll_rc);
                        }

                        ParserEvent::CommandStart => {
                            ftcs_seen_rc.set(true);
                            let state = bstate_rc.get();
                            if state == BlockState::CollectingOutput || state == BlockState::AltScreen {
                                osc133_depth_rc.set(osc133_depth_rc.get() + 1);
                                continue;
                            }
                            if state != BlockState::AwaitingCommand {
                                continue;
                            }
                            osc133_depth_rc.set(0);
                            active_rc.borrow().reset_output_buffer();
                            block_start_time_for_cb.set(Some(SystemTime::now()));
                            // Capture the command text for the sticky running-header.
                            *running_cmd_rc.borrow_mut() = typed_cmd_rc.borrow().trim().to_string();
                            cmd_running_rc.set(true);
                            bstate_rc.set(BlockState::CollectingOutput);
                            scroll_debouncer.mark_dirty(&block_scroll_rc);
                        }

                        ParserEvent::CommandEnd(code) => {
                            let state = bstate_rc.get();
                            if state != BlockState::CollectingOutput && state != BlockState::AltScreen {
                                continue;
                            }
                            if osc133_depth_rc.get() > 0 {
                                osc133_depth_rc.set(osc133_depth_rc.get() - 1);
                                continue;
                            }
                            pending_exit_code_rc.set(*code);
                            cmd_running_rc.set(false);
                            bstate_rc.set(BlockState::PostCommand);
                            scroll_debouncer.mark_dirty(&block_scroll_rc);
                        }

                        ParserEvent::CwdUpdate(path) => {
                            *current_cwd_for_cb.borrow_mut() = path.clone();
                            for cb in cwd_cbs.borrow().iter() {
                                cb(path);
                            }
                        }

                        ParserEvent::AltScreenEnter => {
                            let from_state = bstate_rc.get();
                            if from_state != BlockState::CollectingOutput
                                && from_state != BlockState::AwaitingCommand
                            {
                                continue;
                            }
                            prev_state_rc.set(from_state);
                            bstate_rc.set(BlockState::AltScreen);
                            pager_snapshot_generation_rc.set(
                                pager_snapshot_generation_rc.get().wrapping_add(1),
                            );
                            pager_snapshots_rc.borrow_mut().clear();
                            *pager_pre_clear_rc.borrow_mut() =
                                normalize_pager_snapshot(&visible_vte_text(&active_vte));
                            // Hand the viewport to the alt-screen app: hide finished
                            // blocks so the live VTE fills the scroll area.
                            enter_fullscreen(&finished_blocks_for_cb, &fullscreen_rc);
                            active_vte.feed(b"\x1b[?1049h");
                        }

                        ParserEvent::AltScreenLeave => {
                            if bstate_rc.get() != BlockState::AltScreen {
                                continue;
                            }
                            // Capture the final visible frame synchronously before
                            // switching back to the normal buffer.
                            record_pager_snapshot(&active_vte, &pager_snapshots_rc, &pager_pre_clear_rc);
                            active_vte.feed(b"\x1b[?1049l");
                            pager_snapshot_generation_rc.set(
                                pager_snapshot_generation_rc.get().wrapping_add(1),
                            );
                            let merged = drain_pager_snapshots(&pager_snapshots_rc);
                            if !merged.is_empty() {
                                let needs_separator = !active_rc.borrow().output_text().trim().is_empty();
                                if needs_separator {
                                    active_rc.borrow().accumulate_output(b"\n");
                                }
                                active_rc.borrow().accumulate_output(merged.as_bytes());
                            }
                            exit_fullscreen(&finished_blocks_for_cb, &visible_indices_rc, &fullscreen_rc);
                            osc133_depth_rc.set(0);
                            bstate_rc.set(prev_state_rc.get());
                            let active_for_idle = active_rc.clone();
                            glib::idle_add_local_once(move || {
                                active_for_idle.borrow().grab_focus();
                            });
                        }

                        ParserEvent::ClipboardSet(text) => {
                            if let Some(display) = gtk4::gdk::Display::default() {
                                let clipboard = display.clipboard();
                                clipboard.set_text(text);
                            }
                        }

                        ParserEvent::ApcSequence(payload) => {
                            let is_kitty = payload.first() == Some(&b'G');
                            if is_kitty && bstate_rc.get() == BlockState::AltScreen {
                                let mut seq = Vec::with_capacity(payload.len() + 4);
                                seq.push(0x1b);
                                seq.push(b'_');
                                seq.extend_from_slice(payload);
                                seq.push(0x1b);
                                seq.push(b'\\');
                                active_vte.feed(&seq);
                            }
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
}

/// Hand the viewport to an alt-screen app: hide every finished block so the live
/// VTE fills the scroll area like a normal full-screen terminal.
fn enter_fullscreen(
    finished: &Rc<RefCell<Vec<FinishedBlock>>>,
    fullscreen: &Rc<Cell<bool>>,
) {
    if fullscreen.replace(true) {
        return;
    }
    for block in finished.borrow().iter() {
        block.widget().set_visible(false);
    }
}

/// Restore the block list when the alt-screen app exits, re-applying virtual-scroll
/// visibility so only the previously-visible blocks reappear.
fn exit_fullscreen(
    finished: &Rc<RefCell<Vec<FinishedBlock>>>,
    visible_indices: &Rc<RefCell<std::collections::HashSet<usize>>>,
    fullscreen: &Rc<Cell<bool>>,
) {
    if !fullscreen.replace(false) {
        return;
    }
    let visible = visible_indices.borrow();
    for (i, block) in finished.borrow().iter().enumerate() {
        block.widget().set_visible(visible.contains(&i));
    }
}

/// Captures the handles the live-VTE key handler needs. With the VTE owning line
/// editing + IME natively (jterm1 model), this is reduced to a Capture-phase
/// navigation / copy-paste / block-selection handler; printable keys and editing
/// fall through to the VTE.
struct KeyCtx {
    pty_for_key: Rc<OwnedPty>,
    active_vte_for_key: Terminal,
    typed_cmd_for_key: Rc<RefCell<String>>,
    finished_blocks_for_key: Rc<RefCell<Vec<FinishedBlock>>>,
    block_list_for_key: gtk4::Box,
    selected_block_id_for_key: Rc<Cell<Option<u64>>>,
    block_scroll_for_key: ScrolledWindow,
}

impl KeyCtx {
    fn connect(self, key_ctrl: &gtk4::EventControllerKey) {
        let KeyCtx {
            pty_for_key,
            active_vte_for_key,
            typed_cmd_for_key,
            finished_blocks_for_key,
            block_list_for_key,
            selected_block_id_for_key,
            block_scroll_for_key,
        } = self;
        key_ctrl.connect_key_pressed(move |_controller, keyval, _keycode, modifiers| {
            use gtk4::gdk::Key;
            let ctrl = modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
            let shift = modifiers.contains(gtk4::gdk::ModifierType::SHIFT_MASK);
            let alt = modifiers.contains(gtk4::gdk::ModifierType::ALT_MASK);

            // Shift+PageUp/PageDown: page the block history locally. The
            // vadjustment value_changed handler keeps scroll-lock in sync.
            if shift && !ctrl && !alt && matches!(keyval, Key::Page_Up | Key::Page_Down) {
                let adj = block_scroll_for_key.vadjustment();
                let step = (adj.page_size() * 0.9).max(1.0);
                let delta = if keyval == Key::Page_Up { -step } else { step };
                let max_val = (adj.upper() - adj.page_size()).max(adj.lower());
                adj.set_value((adj.value() + delta).clamp(adj.lower(), max_val));
                return glib::Propagation::Stop;
            }

            // Ctrl+Shift+Up/Down: move the finished-block selection.
            if ctrl && shift && matches!(keyval, Key::Up | Key::Down) {
                let finished = finished_blocks_for_key.borrow();
                if finished.is_empty() {
                    return glib::Propagation::Stop;
                }
                let current = selected_block_id_for_key.get();
                let current_idx = current.and_then(|id| finished.iter().position(|b| b.id == id));
                let new_idx = if keyval == Key::Up {
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
                let new_id = new_idx.and_then(|idx| finished.get(idx).map(|b| b.id));
                select_finished_block(&finished, &selected_block_id_for_key, new_id);
                if let Some(idx) = new_idx {
                    if let Some(block) = finished.get(idx) {
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
                }
                return glib::Propagation::Stop;
            }

            // Enter while a block is selected: recall its command into the live
            // input line (clear the shell line with Ctrl+U, then type it).
            if matches!(keyval, Key::Return | Key::KP_Enter) {
                if let Some(sel_id) = selected_block_id_for_key.get() {
                    let finished = finished_blocks_for_key.borrow();
                    if let Some(block) = finished.iter().find(|b| b.id == sel_id) {
                        let cmd: String = block.cmd_text.lines().next().unwrap_or("").to_string();
                        pty_for_key.write_bytes(b"\x15");
                        pty_for_key.write_bytes(cmd.as_bytes());
                        typed_cmd_for_key.borrow_mut().clear();
                    }
                    select_finished_block(&finished, &selected_block_id_for_key, None);
                    return glib::Propagation::Stop;
                }
                return glib::Propagation::Proceed;
            }

            // Escape clears the block selection (when one is active).
            if keyval == Key::Escape {
                if selected_block_id_for_key.get().is_some() {
                    let finished = finished_blocks_for_key.borrow();
                    select_finished_block(&finished, &selected_block_id_for_key, None);
                    return glib::Propagation::Stop;
                }
                return glib::Propagation::Proceed;
            }

            // Ctrl+L: clear visible finished blocks + send form feed to the shell.
            if ctrl && !shift && !alt && matches!(keyval, Key::l | Key::L) {
                let mut blocks = finished_blocks_for_key.borrow_mut();
                for block in blocks.drain(..) {
                    block_list_for_key.remove(block.widget());
                }
                pty_for_key.write_bytes(b"\x0c");
                return glib::Propagation::Stop;
            }

            // Ctrl+Shift+C / Ctrl+Shift+V: copy / paste against the live VTE.
            if ctrl && shift && matches!(keyval, Key::c | Key::C) {
                active_vte_for_key.copy_clipboard_format(vte4::Format::Text);
                return glib::Propagation::Stop;
            }
            if ctrl && shift && matches!(keyval, Key::v | Key::V) {
                active_vte_for_key.paste_clipboard();
                return glib::Propagation::Stop;
            }

            // Ctrl+P: fuzzy command-history palette. Build a deduped, most-recent
            // -first command list from the finished blocks and pop it up.
            if ctrl && !shift && !alt && matches!(keyval, Key::p | Key::P) {
                let mut seen = std::collections::HashSet::new();
                let mut cmds = Vec::new();
                {
                    let finished = finished_blocks_for_key.borrow();
                    for b in finished.iter().rev() {
                        let c = b.cmd_text.lines().next().unwrap_or("").trim().to_string();
                        if c.is_empty() {
                            continue;
                        }
                        if seen.insert(c.clone()) {
                            cmds.push(c);
                        }
                    }
                }
                show_command_palette(
                    &block_scroll_for_key,
                    cmds,
                    pty_for_key.clone(),
                    typed_cmd_for_key.clone(),
                    active_vte_for_key.clone(),
                );
                return glib::Propagation::Stop;
            }

            // Everything else: let the VTE translate it (printable keys, editing,
            // control sequences, IME) and emit `commit`.
            glib::Propagation::Proceed
        });
    }
}

#[allow(dead_code)]
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
        block_list.set_vexpand(true); // jterm1: expand so the active card fills
                                      // the space left below finished blocks.
        block_list.add_css_class("block-list");

        let block_scroll = ScrolledWindow::new();
        block_scroll.set_hexpand(true);
        block_scroll.set_vexpand(true);
        block_scroll.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Automatic);
        block_scroll.set_child(Some(&block_list));
        block_scroll.add_css_class("block-scroll");
        // A focusable ScrolledWindow steals keyboard focus from the live VTE
        // child (cursor goes hollow, keystrokes never reach the terminal). Make
        // it not a focus target so focus delegates to the VTE. NOTE: use
        // `focusable(false)`, NOT `can_focus(false)` — in GTK4 `can-focus=false`
        // blocks the whole subtree (including the VTE) from ever taking focus.
        block_scroll.set_focusable(false);

        // Active block: a single persistent live VTE pinned at the bottom of the
        // block list. Prompt + typing + output all render here natively (jterm1
        // model); finished commands snapshot into styled blocks above it.
        let active = Rc::new(RefCell::new(ActiveBlock::new(config)));
        let active_vte = active.borrow().active_vte.clone();

        block_list.append(active.borrow().widget());

        // The live VTE holder is NOT pinned to the full viewport. Its height is
        // driven by `update_input_height` (installed after `bstate` exists below):
        // compact (content-sized, min MIN_INPUT_ROWS) while idle so history shows
        // above it (warp model), and full-viewport only for alt-screen apps.

        // ── Jump-to-bottom floating action button ─────────────────────────
        // Shown when the user scrolls up into history; an optional unread badge
        // counts finished blocks that completed while scrolled away. Clicking it
        // returns the view to the live prompt. Overlaid on the scroll area so it
        // floats over the block list without taking layout space.
        let jump_fab = gtk4::Button::new();
        jump_fab.add_css_class("jump-bottom-fab");
        jump_fab.add_css_class("flat");
        jump_fab.set_label("\u{f078}"); // nf-fa-chevron_down
        jump_fab.set_tooltip_text(Some("Jump to latest"));
        jump_fab.set_halign(gtk4::Align::End);
        jump_fab.set_valign(gtk4::Align::End);
        jump_fab.set_margin_end(18);
        jump_fab.set_margin_bottom(18);
        jump_fab.set_visible(false);
        jump_fab.set_can_focus(false);

        // ── Sticky running-command header ─────────────────────────────────
        // When a command is running and the user has scrolled up into history,
        // a thin bar pins to the top of the scroll area showing the live command
        // and its elapsed time, so they don't lose track of what's executing.
        let sticky_label = gtk4::Label::new(None);
        sticky_label.set_halign(gtk4::Align::Start);
        sticky_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        sticky_label.set_hexpand(true);
        sticky_label.add_css_class("sticky-running-label");
        let sticky_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        sticky_bar.add_css_class("sticky-running-header");
        sticky_bar.append(&sticky_label);
        sticky_bar.set_halign(gtk4::Align::Fill);
        sticky_bar.set_valign(gtk4::Align::Start);
        sticky_bar.set_visible(false);
        sticky_bar.set_can_focus(false);

        let scroll_overlay = gtk4::Overlay::new();
        scroll_overlay.set_child(Some(&block_scroll));
        scroll_overlay.add_overlay(&sticky_bar);
        scroll_overlay.add_overlay(&jump_fab);
        root.append(&scroll_overlay);

        let unread_count: Rc<Cell<u32>> = Rc::new(Cell::new(0));

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

        // Store child PID on the live VTE so kill_all_terminal_children can find it
        unsafe {
            active_vte.set_data::<i32>("child-pid", pty.pid_i32());
        }

        // ── Register CSS ──────────────────────────────────────────────────
        install_block_css(config);

        // ── Shared state ──────────────────────────────────────────────────
        let bstate = Rc::new(Cell::new(BlockState::Idle));

        // Command line reconstructed from VTE commit keystrokes; also drives the
        // idle input-cell height (line count), so it is declared before the
        // sizing closure below.
        let typed_cmd: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

        // Scroll-lock flags shared across the contents_changed pin, value_changed
        // detector, FAB, and ScrollDebouncer. `user_scrolled_up` suppresses the
        // follow-bottom pin while the user is reading history; `programmatic_scroll`
        // marks our own adjustment writes so the value_changed detector doesn't
        // mistake them for a user drag.
        let user_scrolled_up: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let programmatic_scroll: Rc<Cell<bool>> = Rc::new(Cell::new(false));

        // ── Warp-style input-cell sizing ──────────────────────────────────
        // The live VTE holder hugs its content (prompt + typed command) with a
        // guaranteed minimum height while idle, so finished blocks remain visible
        // above it. It is forced to the full viewport only for alt-screen apps
        // (vim/less/TUIs) which need real terminal rows. During a running command
        // the height is frozen at the idle value (no per-chunk resize / SIGWINCH
        // thrash); the full output is snapshotted into a finished block when done.
        let update_input_height: Rc<dyn Fn()> = {
            let holder = active.borrow().widget().clone();
            let vte = active_vte.clone();
            let scroll = block_scroll.clone();
            let bstate = bstate.clone();
            let typed_cmd = typed_cmd.clone();
            Rc::new(move || {
                let cell_h = (vte.char_height() as i32).max(1);
                let page = scroll.vadjustment().page_size() as i32;
                if page <= 1 {
                    return;
                }
                let viewport_rows = ((page / cell_h).max(1)) as i64;
                let cols = vte.column_count().max(1);
                let target_rows = match bstate.get() {
                    // Full-screen apps & non-OSC133 shells behave like a normal
                    // terminal: give them the whole viewport.
                    BlockState::AltScreen | BlockState::RawFallback => viewport_rows,
                    // Running / draining a command: leave the height frozen so the
                    // reported PTY rows stay stable for the command's lifetime.
                    BlockState::CollectingOutput | BlockState::PostCommand => {
                        return;
                    }
                    // Idle: size to the typed command's line count (1 + newlines),
                    // clamped to a usable minimum. We must NOT use the VTE cursor
                    // row: cursor_position().1 is the ABSOLUTE scrollback row, which
                    // climbs without bound as content accumulates and triggers a
                    // grow→redraw→grow runaway that fills the viewport.
                    BlockState::Idle
                    | BlockState::CollectingPrompt
                    | BlockState::AwaitingCommand => {
                        let input_lines =
                            1 + typed_cmd.borrow().bytes().filter(|&b| b == b'\n').count() as i64;
                        let floor = (MIN_INPUT_ROWS as i64).min(viewport_rows);
                        let cap = viewport_rows.max(floor);
                        input_lines.clamp(floor, cap)
                    }
                };
                // Drive the VTE grid directly. `set_height_request` only sets a
                // *minimum*, so it cannot shrink a VTE whose natural height
                // (row_count * char_height) is larger — the cell would stay
                // full-height. `set_size` sets the preferred grid, shrinking the
                // VTE's natural height so the (non-expanding) holder collapses to
                // it. The PTY-resize tick then follows row_count up/down.
                if vte.row_count() != target_rows {
                    vte.set_size(cols, target_rows);
                }
                holder.set_height_request((target_rows as i32) * cell_h);
            })
        };
        // Coalesces follow-bottom pins so a burst of contents-changed signals
        // schedules at most one deferred scroll.
        let pin_pending: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        {
            // Drive sizing from the data path (contents changed: prompt printed,
            // user typing, output streaming, alt-screen toggle), and follow the
            // bottom from here too — NOT from the vadjustment `changed` signal.
            //
            // Why a deferred idle and not `changed`: pinning inside `changed`
            // reacts to virtualization's own `upper` changes (off-screen blocks
            // collapse to 0 height when hidden), so pin → hide top block → upper
            // shrinks → `changed` → pin → block reappears → upper grows → `changed`
            // → … an infinite two-state oscillation. A low-priority idle runs once
            // per content burst, AFTER layout settles (so `upper` is final), and is
            // never re-triggered by the visibility side-effects of its own scroll.
            let f = update_input_height.clone();
            let scroll = block_scroll.clone();
            let user_scrolled = user_scrolled_up.clone();
            let programmatic = programmatic_scroll.clone();
            let pin_pending = pin_pending.clone();
            active_vte.connect_contents_changed(move |_| {
                f();
                if user_scrolled.get() || pin_pending.get() {
                    return;
                }
                pin_pending.set(true);
                let scroll = scroll.clone();
                let user_scrolled = user_scrolled.clone();
                let programmatic = programmatic.clone();
                let pin_pending = pin_pending.clone();
                glib::idle_add_local_once(move || {
                    pin_pending.set(false);
                    if user_scrolled.get() {
                        return;
                    }
                    let adj = scroll.vadjustment();
                    let target = (adj.upper() - adj.page_size()).max(adj.lower());
                    if (adj.value() - target).abs() > 1.0 {
                        programmatic.set(true);
                        adj.set_value(target);
                        programmatic.set(false);
                    }
                });
            });
        }

        // State to restore when an alt-screen app exits (jterm1 model).
        let prev_state: Rc<Cell<BlockState>> = Rc::new(Cell::new(BlockState::Idle));
        let osc133_depth: Rc<Cell<u32>> = Rc::new(Cell::new(0));
        let prompt_buf: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let cmd_buf: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        // Rendered prompt captured at PromptEnd (prompt_buf is cleared once the
        // prompt ends, so the finalize path reads this instead).
        let prompt_display: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        // True while an alt-screen app owns the viewport (finished blocks hidden).
        let fullscreen: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let cwd_callbacks: StrCallbacks = Rc::new(RefCell::new(vec![]));
        let exited_callbacks: IntCallbacks = Rc::new(RefCell::new(vec![]));
        let bell_callbacks: VoidCallbacks = Rc::new(RefCell::new(vec![]));
        let title_callbacks: StrCallbacks = Rc::new(RefCell::new(vec![]));
        let activity_callbacks: VoidCallbacks = Rc::new(RefCell::new(vec![]));
        let bracketed_paste_mode: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let application_cursor_mode: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let mouse_reporting_mode: Rc<Cell<MouseReportingMode>> = Rc::new(Cell::new(MouseReportingMode::None));
        let cursor_shape: Rc<Cell<TermCursorShape>> = Rc::new(Cell::new(TermCursorShape::Block));
        let block_data_rc: Rc<RefCell<VecDeque<BlockData>>> =
            Rc::new(RefCell::new(VecDeque::new()));
        let finished_blocks_rc: Rc<RefCell<Vec<FinishedBlock>>> = Rc::new(RefCell::new(Vec::new()));
        let pager_snapshots: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let pager_snapshot_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        // Normalized text of the shared alt VTE captured *before* we reset/clear
        // it on a new alt-screen entry. VTE feeds (reset + clear + new content)
        // render asynchronously, so idle snapshots scheduled right after entry can
        // read the *previous* command's still-rendered frame. Any snapshot equal
        // to this baseline is a stale read and must be dropped.
        let pager_pre_clear: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let ansi_cache: Rc<RefCell<LruCache<String, String>>> = Rc::new(RefCell::new(
            LruCache::new(NonZeroUsize::new((config.ansi_cache_capacity as usize).max(1)).unwrap()),
        ));

        let pending_exit_code: Rc<Cell<i32>> = Rc::new(Cell::new(0));

        let widget_pool: Rc<RefCell<WidgetPool>> = Rc::new(RefCell::new(WidgetPool::new()));
        let pty_synced: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let selected_block_id: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));
        // Sticky running-command header state: true while a command is executing,
        // plus the command text captured at CommandStart.
        let cmd_running: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let running_cmd: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let block_start_time: Rc<Cell<Option<SystemTime>>> = Rc::new(Cell::new(None));
        let visible_indices: Rc<RefCell<std::collections::HashSet<usize>>> =
            Rc::new(RefCell::new(std::collections::HashSet::new()));
        // Set once any OSC-133 (FTCS) event is seen, so the view knows shell
        // integration is live.
        let ftcs_seen: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let current_cwd: Rc<RefCell<String>> = Rc::new(RefCell::new(
            cwd.unwrap_or("").to_string()
        ));

        // ── Wire PTY → parser → block events ─────────────────────────────
        {
            let active_rc = active.clone();
            let active_vte_rc = active_vte.clone();
            let bstate_rc = bstate.clone();
            let prev_state_rc = prev_state.clone();
            let osc133_depth_rc = osc133_depth.clone();
            let prompt_buf_rc = prompt_buf.clone();
            let cmd_buf_rc = cmd_buf.clone();
            let typed_cmd_rc = typed_cmd.clone();
            let prompt_display_rc = prompt_display.clone();
            let block_list_rc = block_list.clone();
            let block_scroll_rc = block_scroll.clone();
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
            let pager_pre_clear_rc = pager_pre_clear.clone();
            let scroll_debouncer = ScrollDebouncer::with_scroll_lock(
                user_scrolled_up.clone(),
                programmatic_scroll.clone(),
            );
            let ansi_cache_for_cb = ansi_cache.clone();
            let widget_pool_for_cb = widget_pool.clone();
            let pty_synced_rc = pty_synced.clone();
            let visible_indices_rc = visible_indices.clone();
            let fullscreen_rc = fullscreen.clone();
            let ftcs_seen_rc = ftcs_seen.clone();

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
            let block_start_time_for_cb = block_start_time.clone();
            let pending_exit_code_rc = pending_exit_code.clone();
            let current_cwd_for_cb = current_cwd.clone();

            let event_buf: Rc<RefCell<Vec<ParserEvent>>> = Rc::new(RefCell::new(Vec::with_capacity(32)));
            ReaderCtx {
                active_rc,
                active_vte: active_vte_rc,
                bstate_rc,
                prev_state_rc,
                osc133_depth_rc,
                prompt_buf_rc,
                cmd_buf_rc,
                typed_cmd_rc,
                prompt_display_rc,
                block_list_rc,
                block_scroll_rc,
                cwd_cbs,
                exited_cbs,
                bell_cbs,
                title_cbs,
                activity_cbs,
                bracketed_paste_rc,
                application_cursor_rc,
                mouse_reporting_rc,
                cursor_shape_rc,
                config_for_cb,
                parser,
                block_data_for_cb,
                finished_blocks_for_cb,
                pager_snapshots_rc,
                pager_snapshot_generation_rc,
                pager_pre_clear_rc,
                scroll_debouncer,
                ansi_cache_for_cb,
                widget_pool_for_cb,
                pty_synced_rc,
                visible_indices_rc,
                fullscreen_rc,
                ftcs_seen_rc,
                init_cmds_queue_for_cb,
                pty_for_init,
                block_start_time_for_cb,
                pending_exit_code_rc,
                current_cwd_for_cb,
                event_buf,
                unread_count_rc: unread_count.clone(),
                jump_fab: jump_fab.clone(),
                selected_block_id_rc: selected_block_id.clone(),
                cmd_running_rc: cmd_running.clone(),
                running_cmd_rc: running_cmd.clone(),
            }
            .install(&pty);
        }

        // ── Scroll lock + jump-to-bottom FAB ──────────────────────────────
        // The block list virtualizes (off-screen finished blocks are hidden →
        // 0 height), so `adjustment.upper()` shrinks as you scroll and the usual
        // value-vs-max "at bottom" math can never be trusted. Instead detect the
        // live bottom geometrically off the never-virtualized live VTE holder.
        //
        // Key subtlety (see scroll.rs): in the normal follow state the holder is
        // one full page tall and parked at its *top*, so its top edge sits a little
        // below y=0 (≈ the just-finished block's height) and its bottom edge falls
        // *below* the viewport. So neither "top≈0" nor "bottom inside viewport"
        // alone is right. What actually distinguishes "following" from "scrolled
        // up into history" is whether the live prompt is still on screen: while
        // following, the holder's top is somewhere inside the viewport; scroll up
        // far enough and the holder (prompt) slides off the bottom. So: at-bottom
        // ⟺ holder top is above the viewport's bottom edge. Sampled on idle so it
        // reflects the settled post-scroll layout.
        {
            let user_scrolled = user_scrolled_up.clone();
            let fab = jump_fab.clone();
            let unread = unread_count.clone();
            let scroll = block_scroll.clone();
            let holder = active.borrow().widget().clone();
            let check_pending = Rc::new(Cell::new(false));
            block_scroll.vadjustment().connect_value_changed(move |_adj| {
                if check_pending.get() {
                    return;
                }
                check_pending.set(true);
                let user_scrolled = user_scrolled.clone();
                let fab = fab.clone();
                let unread = unread.clone();
                let scroll = scroll.clone();
                let holder = holder.clone();
                let check_pending = check_pending.clone();
                glib::idle_add_local_once(move || {
                    check_pending.set(false);
                    let vp_h = scroll.height() as f64;
                    let at_bottom = holder
                        .compute_bounds(&scroll)
                        .map(|b| (b.y() as f64) < vp_h - 4.0)
                        .unwrap_or(true);
                    user_scrolled.set(!at_bottom);
                    if at_bottom {
                        unread.set(0);
                        fab.set_visible(false);
                    } else {
                        set_jump_fab_label(&fab, unread.get());
                        fab.set_visible(true);
                    }
                });
            });
        }

        // ── Re-clamp input height on viewport resize ──────────────────────
        // `changed` fires during the viewport's size-allocate, after layout. We
        // re-clamp the input height here ONLY when the viewport itself resized
        // (page_size moved) — content-driven sizing comes from the data path
        // (contents_changed) above. We deliberately do NOT pin the scroll here:
        // pinning from `changed` reacts to virtualization's own `upper` changes
        // (hidden off-screen blocks collapse to 0 height) and oscillates forever.
        // The follow-bottom pin is the deferred idle scheduled on contents_changed.
        {
            let f = update_input_height.clone();
            let last_page = Rc::new(Cell::new(0.0f64));
            block_scroll.vadjustment().connect_changed(move |adj| {
                let page = adj.page_size();
                if (page - last_page.get()).abs() > 0.5 {
                    last_page.set(page);
                    f();
                }
            });
        }

        // ── Jump-to-bottom FAB click: return to the live prompt ───────────
        {
            let scroll = block_scroll.clone();
            let programmatic = programmatic_scroll.clone();
            let user_scrolled = user_scrolled_up.clone();
            let unread = unread_count.clone();
            let fab = jump_fab.clone();
            jump_fab.connect_clicked(move |_| {
                // Returning to the live prompt is not a single set_value: blocks
                // below the viewport are virtualized to 0 height, so `upper` only
                // grows as they scroll into view. One jump lands partway; we have
                // to re-apply `upper - page` across idle passes until `upper` stops
                // growing (true bottom reached) or we hit a small iteration cap.
                user_scrolled.set(false);
                unread.set(0);
                fab.set_visible(false);
                let adj = scroll.vadjustment();
                programmatic.set(true);
                adj.set_value((adj.upper() - adj.page_size()).max(adj.lower()));
                programmatic.set(false);

                let scroll = scroll.clone();
                let programmatic = programmatic.clone();
                let tries = Rc::new(Cell::new(0u8));
                glib::idle_add_local(move || {
                    // Runs for a handful of frames (cap below), too fast for the
                    // user to interrupt — so we don't watch user_scrolled here; the
                    // value_changed geometry check settles the FAB state afterward.
                    if tries.get() >= 12 {
                        return glib::ControlFlow::Break;
                    }
                    tries.set(tries.get() + 1);
                    let adj = scroll.vadjustment();
                    let before = adj.value();
                    let target = (adj.upper() - adj.page_size()).max(adj.lower());
                    programmatic.set(true);
                    adj.set_value(target);
                    programmatic.set(false);
                    // Stable once another pass no longer advances the position.
                    if (adj.value() - before).abs() < 1.0 {
                        glib::ControlFlow::Break
                    } else {
                        glib::ControlFlow::Continue
                    }
                });
            });
        }

        // ── Sticky running-command header: poll-driven refresh ────────────
        // Shown only while a command is executing AND the user has scrolled up
        // (so the live prompt is off-screen). Polling lets one place own both the
        // visibility decision and the elapsed-time tick without threading updates
        // through the reader's CommandStart/End and the scroll handler.
        {
            let sticky = sticky_bar.clone();
            let sticky_label = sticky_label.clone();
            let cmd_running = cmd_running.clone();
            let running_cmd = running_cmd.clone();
            let block_start_time = block_start_time.clone();
            let user_scrolled = user_scrolled_up.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
                // Stop the timer once the view is torn down (tab closed), so it
                // doesn't leak by keeping the widgets/state alive forever. The bar
                // is parented to the overlay at construction, so a None parent means
                // the overlay was disposed.
                if sticky.parent().is_none() {
                    return glib::ControlFlow::Break;
                }
                if cmd_running.get() && user_scrolled.get() {
                    let cmd = running_cmd.borrow();
                    let cmd_disp = cmd.trim();
                    let elapsed = block_start_time
                        .get()
                        .and_then(|st| SystemTime::now().duration_since(st).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let elapsed_str = if elapsed >= 60 {
                        format!("{}m{:02}s", elapsed / 60, elapsed % 60)
                    } else {
                        format!("{}s", elapsed)
                    };
                    let label = if cmd_disp.is_empty() {
                        format!("\u{25b6}  (running)    {}", elapsed_str)
                    } else {
                        format!("\u{25b6}  {}    {}", cmd_disp, elapsed_str)
                    };
                    sticky_label.set_text(&label);
                    if !sticky.get_visible() {
                        sticky.set_visible(true);
                    }
                } else if sticky.get_visible() {
                    sticky.set_visible(false);
                }
                glib::ControlFlow::Continue
            });
        }

        // ── VTE is used as a display-only widget (fed via feed() in alt-screen mode)
        //    so we do NOT attach it to the PTY. Our reader thread handles all I/O.

        // ── Live VTE input → PTY (jterm1 model) ───────────────────────────
        // The active VTE has input_enabled(true), so it translates keystrokes and
        // owns IME natively; its `commit` signal carries the bytes to send. We
        // forward them to the PTY and, while awaiting a command, reconstruct the
        // typed command line so the finalize path can style it into the block.
        {
            let pty_for_commit = pty.clone();
            let bstate_for_commit = bstate.clone();
            let typed_cmd_for_commit = typed_cmd.clone();
            active_vte.connect_commit(move |_, text, _size| {
                pty_for_commit.write_bytes(text.as_bytes());
                if bstate_for_commit.get() == BlockState::AwaitingCommand {
                    let mut cmd = typed_cmd_for_commit.borrow_mut();
                    for ch in text.chars() {
                        if ch == '\r' || ch == '\n' {
                            // Submit — leave the reconstructed line intact for finalize.
                        } else if ch == '\x7f' || ch == '\x08' {
                            cmd.pop();
                        } else if (ch as u32) < 0x20 {
                            // Control bytes (Tab, Ctrl-*, escape sequences): ignore.
                        } else {
                            cmd.push(ch);
                        }
                    }
                }
            });
        }

        // ── Keyboard navigation / copy-paste (Capture phase) ──────────────
        {
            let pty_for_key = pty.clone();
            let active_vte_for_key = active_vte.clone();
            let typed_cmd_for_key = typed_cmd.clone();
            let finished_blocks_for_key = finished_blocks_rc.clone();
            let block_list_for_key = block_list.clone();
            let selected_block_id_for_key = selected_block_id.clone();
            let block_scroll_for_key = block_scroll.clone();
            let key_ctrl = gtk4::EventControllerKey::new();
            key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);

            KeyCtx {
                pty_for_key,
                active_vte_for_key,
                typed_cmd_for_key,
                finished_blocks_for_key,
                block_list_for_key,
                selected_block_id_for_key,
                block_scroll_for_key,
            }
            .connect(&key_ctrl);

            active_vte.add_controller(key_ctrl);
        }

        let term_view = TermView {
            root,
            block_scroll,
            block_list,
            active_vte,
            active,
            bstate,
            prompt_buf,
            cmd_buf,
            typed_cmd,
            fullscreen,
            user_scrolled_up: user_scrolled_up.clone(),
            programmatic_scroll: programmatic_scroll.clone(),
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
            visible_indices,
            selected_block_id,
            current_cwd: current_cwd.clone(),
            resize_tick_id: RefCell::new(None),
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
                    block.end_time_ms,
                    block.cwd.as_deref(),
                );
                finished.widget().insert_before(&term_view.block_list, Some(term_view.active.borrow().widget()));
                finished.connect_actions(&term_view.active_vte, &term_view.pty, &pty_synced, &term_view.active);
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

        // ── Resize handler: sync PTY cols/rows when widget allocation changes ──
        term_view.install_resize_tick();

        // Give initial focus to the live VTE.
        term_view.active_vte.grab_focus();

        term_view
    }

    /// Keep the PTY grid in sync with the live VTE (jterm1 model). The VTE
    /// re-derives its own column/row count from its allocation on every
    /// size_allocate, so we just mirror that onto the PTY whenever it changes —
    /// no pixel math, no chrome calibration.
    fn install_resize_tick(&self) {
        let pty_for_resize = self.pty.clone();
        let last: Rc<Cell<(u16, u16)>> = Rc::new(Cell::new((0, 0)));
        let tick_id = self.active_vte.add_tick_callback(move |vte, _clock| {
            let cols = vte.column_count() as u16;
            let rows = vte.row_count() as u16;
            if cols > 0 && rows > 0 && (cols, rows) != last.get() {
                last.set((cols, rows));
                pty_for_resize.resize(cols, rows);
            }
            glib::ControlFlow::Continue
        });
        *self.resize_tick_id.borrow_mut() = Some(tick_id);
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
    }

    /// Kill the child process.
    pub fn kill(&self) {
        self.pty.kill();
    }

    pub fn pid_i32(&self) -> i32 {
        self.pty.pid_i32()
    }

    pub fn vte(&self) -> &Terminal {
        &self.active_vte
    }

    pub fn cwd(&self) -> String {
        self.current_cwd.borrow().clone()
    }

    pub fn grab_focus(&self) {
        self.active_vte.grab_focus();
    }

    /// Copy selected text to clipboard.
    /// In block mode: tries to copy from GTK's selection (PRIMARY clipboard).
    /// In alt-screen mode: copies from VTE terminal.
    pub fn copy_to_clipboard(&self) {
        log::debug!(">>> TermView::copy_to_clipboard called");
        // First try VTE (for alt-screen mode)
        let vte_text = self.active_vte.text_selected(vte4::Format::Text);
        let has_vte_text = vte_text.as_ref().map(|t| !t.is_empty()).unwrap_or(false);

        if has_vte_text {
            let text = vte_text.unwrap();
            log::debug!(">>> TermView copy: got {} chars from VTE", text.len());
            let clipboard = self.active_vte.clipboard();
            clipboard.set_text(&text);
            log::debug!(">>> TermView copy: set VTE text to clipboard");
        } else {
            log::debug!(">>> TermView copy: VTE text empty or None, trying PRIMARY");
            // Fall back to PRIMARY clipboard (selected text in labels)
            let display = self.root.display();
            let root_clone = self.root.clone();
            let primary = display.primary_clipboard();
            log::debug!(">>> TermView copy: got PRIMARY clipboard, calling read_text_async");
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
                                    log::debug!(">>> TermView copy: copied to CLIPBOARD");
                                } else {
                                    log::debug!(">>> TermView copy: PRIMARY text is empty");
                                }
                            } else {
                                log::debug!(">>> TermView copy: PRIMARY is None - no text selected");
                            }
                        }
                        Err(e) => {
                            log::debug!(">>> TermView copy: error reading PRIMARY: {}", e);
                        }
                    }
                },
            );
        }
    }

    /// Paste from clipboard to PTY.
    pub fn paste_from_clipboard(&self) {
        log::debug!(">>> TermView::paste_from_clipboard called");
        let clipboard = self.active_vte.clipboard();
        let pty = self.pty.clone();
        let bracketed_paste = self.bracketed_paste_mode.get();
        log::debug!(">>> TermView paste: got clipboard, calling read_text_async");
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
                        log::debug!(">>> TermView paste: wrote {} bytes to PTY", text_str.len());
                    } else {
                        log::debug!(">>> TermView paste: clipboard is None");
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

    /// Apply updated theme colors to the block widgets and the live VTE.
    pub fn apply_theme(&self) {
        let config = self.config.borrow();
        let palette_refs: Vec<&RGBA> = config.palette.iter().collect();
        self.active_vte.set_colors(
            Some(&config.foreground),
            Some(&config.background),
            &palette_refs,
        );
        self.active_vte.set_color_cursor(Some(&config.cursor));
        self.active_vte.set_color_cursor_foreground(Some(&config.cursor_foreground));
        install_block_css(&config);
    }

    /// Update font for VTE terminal and block view CSS.
    pub fn set_font(&self, font_desc: &FontDescription) {
        self.active_vte.set_font(Some(font_desc));
        // Update config and regenerate CSS with new font
        self.config.borrow_mut().font_desc = font_desc.to_string();
        install_block_css(&self.config.borrow());
    }

    /// Update font scale for VTE terminal and block view CSS.
    pub fn set_font_scale(&self, scale: f64) {
        self.active_vte.set_font_scale(scale);
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
        let q_bytes = q.as_bytes();

        let re = if filters.use_regex && !query.is_empty() {
            regex::RegexBuilder::new(query)
                .case_insensitive(true)
                .build()
                .ok()
        } else {
            None
        };

        let results: Vec<usize> = self
            .block_data
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                let text_match = if q.is_empty() {
                    true
                } else if let Some(ref re) = re {
                    re.is_match(&b.prompt)
                        || re.is_match(&b.cmd)
                        || re.is_match(&b.output)
                } else {
                    contains_case_insensitive(b.prompt.as_bytes(), q_bytes)
                        || contains_case_insensitive(b.cmd.as_bytes(), q_bytes)
                        || contains_case_insensitive(b.output.as_bytes(), q_bytes)
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

    /// Collect a snapshot of internal runtime state for the debug dashboard.
    /// Returns labelled sections, each a list of (key, value) rows.
    pub fn debug_info(&self) -> Vec<(&'static str, Vec<(String, String)>)> {
        let out_cols = self.active_vte.column_count();
        let out_rows = self.active_vte.row_count();

        let finished_len = self.finished_blocks.borrow().len();
        let block_data_len = self.block_data.borrow().len();
        let failed = self.get_failed_blocks().len();
        let slow = self.get_slow_blocks(1000).len();
        let total_output_bytes: usize = self
            .block_data
            .borrow()
            .iter()
            .map(|b| b.output.len())
            .sum();
        let viewport = self.viewport.borrow().clone();
        let visible = self.visible_indices.borrow().len();
        let ansi_cache_len = self.ansi_cache.borrow().len();
        let selected = self
            .selected_block_id
            .get()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".to_string());

        vec![
            (
                "State",
                vec![
                    ("Block state".to_string(), format!("{:?}", self.bstate.get())),
                    (
                        "Bracketed paste".to_string(),
                        self.bracketed_paste_mode.get().to_string(),
                    ),
                    (
                        "Application cursor".to_string(),
                        self.application_cursor_mode.get().to_string(),
                    ),
                    (
                        "Mouse reporting".to_string(),
                        format!("{:?}", self.mouse_reporting_mode.get()),
                    ),
                    (
                        "Cursor shape".to_string(),
                        format!("{:?}", self.cursor_shape.get()),
                    ),
                    (
                        "Alt screen visible".to_string(),
                        self.fullscreen.get().to_string(),
                    ),
                ],
            ),
            (
                "PTY",
                vec![
                    ("PID".to_string(), self.pty.pid_i32().to_string()),
                    ("CWD".to_string(), self.current_cwd.borrow().clone()),
                    ("Output grid".to_string(), format!("{out_cols} × {out_rows}")),
                ],
            ),
            (
                "Blocks",
                vec![
                    ("Finished blocks".to_string(), finished_len.to_string()),
                    ("Block data entries".to_string(), block_data_len.to_string()),
                    ("Failed blocks".to_string(), failed.to_string()),
                    ("Slow blocks (>1s)".to_string(), slow.to_string()),
                    ("Total output bytes".to_string(), total_output_bytes.to_string()),
                    ("Selected block id".to_string(), selected),
                ],
            ),
            (
                "Viewport",
                vec![
                    ("First visible".to_string(), viewport.first_visible.to_string()),
                    ("Last visible".to_string(), viewport.last_visible.to_string()),
                    ("Total height".to_string(), format!("{}px", viewport.total_height)),
                    ("Realized widgets".to_string(), visible.to_string()),
                    ("ANSI cache entries".to_string(), ansi_cache_len.to_string()),
                    ("Profiling".to_string(), prof_enabled().to_string()),
                ],
            ),
        ]
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
            // Use the full output (ANSI stripped), not the collapsed buffer.
            let output_text = strip_ansi(&block.full_output.borrow());

            let full_text = format!("{}\n{}\n{}", prompt_text, cmd_text, output_text);
            let clipboard = self.active_vte.clipboard();
            clipboard.set_text(&full_text);
        }
    }

    /// Save block history to file (if configured).
    pub fn save_history(&self) -> std::io::Result<()> {
        use std::io::Write;

        let path_opt = self.config.borrow().block_history_path.as_ref().cloned();
        if path_opt.is_none() {
            return Ok(());
        }

        let path = path_opt.unwrap();
        let blocks = self.block_data.borrow();

        // Overwrite (truncate), do NOT append. The in-memory deque was itself
        // seeded from this file at startup, so appending it re-wrote every loaded
        // block on each session — O(N²) file growth and duplicate blocks on the
        // next load. Persisting the current capped deque keeps the file bounded.
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;

        let compress = self.config.borrow().block_history_compress;

        for block in blocks.iter() {
            let serialized = rkyv::to_bytes::<_, 256>(block)
                .map_err(|e| std::io::Error::other(e.to_string()))?;

            let record: &[u8] = if compress {
                &zstd::encode_all(serialized.as_slice(), 3)
                    .map_err(|e| std::io::Error::other(e.to_string()))?
            } else {
                &serialized
            };

            // The length prefix is a u32; silently truncating it would corrupt all
            // following frame boundaries. Skip any (pathologically large) record
            // that would not fit rather than write a bad prefix.
            if record.len() > u32::MAX as usize {
                log::warn!("save_history: skipping block of {} bytes (exceeds u32 frame limit)", record.len());
                continue;
            }
            file.write_all(&(record.len() as u32).to_le_bytes())?;
            file.write_all(record)?;
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
            // Guard against a corrupt/misaligned length causing a giant allocation.
            const MAX_RECORD_BYTES: usize = 256 * 1024 * 1024;
            if len > MAX_RECORD_BYTES {
                log::warn!("load_history: record length {} exceeds {} — treating file as corrupt, stopping", len, MAX_RECORD_BYTES);
                break;
            }
            let mut data = vec![0u8; len];
            file.read_exact(&mut data)?;

            let decoded = if self.config.borrow().block_history_compress {
                zstd::decode_all(data.as_slice())
                    .map_err(|e| std::io::Error::other(e.to_string()))?
            } else {
                data
            };

            if let Ok(block) = rkyv::from_bytes::<BlockData>(&decoded) {
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

#[cfg(test)]
mod tests {
    use super::{ansi_text_runs, strip_ansi, strip_ansi_with_clear_detect};
    use gtk4::gdk::RGBA;

    fn palette() -> [RGBA; 16] {
        [RGBA::new(0.0, 0.0, 0.0, 1.0); 16]
    }

    #[test]
    fn strips_charset_designation_from_output() {
        assert_eq!(strip_ansi("\u{1b}(Btop"), "top");
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

    fn render(input: &str) -> String {
        ansi_text_runs(input, &palette())
            .iter()
            .map(|r| r.text.as_str())
            .collect()
    }

    #[test]
    fn carriage_return_overwrites_from_column_zero() {
        assert_eq!(render("Loading...\r50%"), "50%ding...");
    }

    #[test]
    fn cursor_up_repaints_previous_row() {
        // aaa\n bbb, back 2, up 1, write Z → row0 col1 = Z.
        assert_eq!(render("aaa\nbbb\u{1b}[2D\u{1b}[AZ"), "aZa\nbbb");
    }

    #[test]
    fn cursor_up_count_and_column_address() {
        // Three rows; CUU 2 then CHA col1 then write X overwrites row0 col0.
        assert_eq!(render("r0\nr1\nr2\u{1b}[2A\u{1b}[1GX"), "X0\nr1\nr2");
    }

    #[test]
    fn double_width_chars_round_trip() {
        assert_eq!(render("日本"), "日本");
    }

    #[test]
    fn double_width_advances_two_columns() {
        // After a wide char (cols 0-1), CHA to col3 (0-based 2) writes adjacent.
        assert_eq!(render("日\u{1b}[3GX"), "日X");
    }

    #[test]
    fn tab_pads_to_next_stop() {
        assert_eq!(render("a\tb"), format!("a{}b", " ".repeat(7)));
    }

    #[test]
    fn erase_chars_blanks_in_place() {
        assert_eq!(render("abcdef\u{1b}[3D\u{1b}[2X"), "abc  f");
    }

    #[test]
    fn delete_chars_shifts_left() {
        assert_eq!(render("abcdef\u{1b}[3D\u{1b}[2P"), "abcf");
    }

    #[test]
    fn insert_chars_shifts_right() {
        assert_eq!(render("abc\u{1b}[1G\u{1b}[2@"), "  abc");
    }

    #[test]
    fn combining_mark_attaches_to_base() {
        assert_eq!(render("e\u{0301}"), "e\u{0301}");
    }

    #[test]
    fn repeat_last_char() {
        assert_eq!(render("a\u{1b}[3b"), "aaaa");
    }

    #[test]
    fn erase_line_to_end() {
        assert_eq!(render("abcdef\u{1b}[3D\u{1b}[0K"), "abc");
    }

    #[test]
    fn newline_starts_fresh_row() {
        assert_eq!(render("ab\ncd"), "ab\ncd");
    }

    #[test]
    fn dec_line_drawing_maps_box_chars() {
        // ESC(0 selects line-drawing G0; lqk → ┌─┐ ; ESC(B restores ASCII.
        assert_eq!(render("\u{1b}(0lqk\u{1b}(B"), "┌─┐");
    }

    #[test]
    fn dec_line_drawing_restored_by_ascii_charset() {
        // After ESC(B, lqk are plain letters again.
        assert_eq!(render("\u{1b}(0l\u{1b}(Blqk"), "┌lqk");
    }

    #[test]
    fn shift_in_out_toggle_line_drawing() {
        // SO (0x0e) selects G1; designate G1 as line-drawing; SI (0x0f) back to G0.
        assert_eq!(render("\u{1b})0\u{0e}x\u{0f}x"), "│x");
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

    // ── strip_ansi_with_clear_detect: cursor model tests ────────────────

    #[test]
    fn carriage_return_overwrites_line() {
        // \r moves cursor to col 0, shorter text overwrites prefix but leaves tail
        assert_eq!(
            strip_ansi_with_clear_detect("Loading...\rDone!"),
            ("Done!ng...".to_string(), false)
        );
    }

    #[test]
    fn carriage_return_full_overwrite() {
        // Full overwrite of same-length text
        assert_eq!(
            strip_ansi_with_clear_detect("AAAA\rBBBB"),
            ("BBBB".to_string(), false)
        );
    }

    #[test]
    fn spinner_animation_shows_final_frame() {
        // Simulates spinner: multiple frames separated by \r
        assert_eq!(
            strip_ansi_with_clear_detect("| working\r/ working\r- working\r\\ working"),
            ("\\ working".to_string(), false)
        );
    }

    #[test]
    fn csi_erase_line_to_end() {
        // CSI 0K: erase from cursor to end of line
        assert_eq!(
            strip_ansi_with_clear_detect("hello world\r\u{1b}[0Kdone"),
            ("done".to_string(), false)
        );
    }

    #[test]
    fn csi_erase_line_implicit_zero() {
        // CSI K (no param) is same as CSI 0K
        assert_eq!(
            strip_ansi_with_clear_detect("old text\r\u{1b}[Knew"),
            ("new".to_string(), false)
        );
    }

    #[test]
    fn csi_erase_line_from_start() {
        // CSI 1K: erase from start to cursor (fills with spaces)
        assert_eq!(
            strip_ansi_with_clear_detect("abcdef\r\u{1b}[3C\u{1b}[1K"),
            ("   def".to_string(), false)
        );
    }

    #[test]
    fn csi_erase_entire_line() {
        // CSI 2K: erase entire line
        assert_eq!(
            strip_ansi_with_clear_detect("something\r\u{1b}[2Kresult"),
            ("result".to_string(), false)
        );
    }

    #[test]
    fn csi_cursor_forward() {
        // CSI C: move cursor forward
        assert_eq!(
            strip_ansi_with_clear_detect("abcdef\r\u{1b}[3CX"),
            ("abcXef".to_string(), false)
        );
    }

    #[test]
    fn csi_cursor_backward() {
        // CSI D: move cursor backward
        assert_eq!(
            strip_ansi_with_clear_detect("abcdef\u{1b}[2DXY"),
            ("abcdXY".to_string(), false)
        );
    }

    #[test]
    fn csi_cursor_absolute_column() {
        // CSI G: absolute column positioning (1-based)
        assert_eq!(
            strip_ansi_with_clear_detect("abcdef\u{1b}[2GX"),
            ("aXcdef".to_string(), false)
        );
    }

    #[test]
    fn backspace_moves_cursor_back() {
        assert_eq!(
            strip_ansi_with_clear_detect("abc\x08X"),
            ("abX".to_string(), false)
        );
    }

    #[test]
    fn backspace_at_start_does_not_underflow() {
        assert_eq!(
            strip_ansi_with_clear_detect("\x08\x08hello"),
            ("hello".to_string(), false)
        );
    }

    #[test]
    fn claude_code_progress_pattern() {
        // Claude Code CLI pattern: write progress, \r, erase line, write new status
        let input = "⠋ Thinking...\r\u{1b}[K⠙ Analyzing...\r\u{1b}[K✓ Done";
        assert_eq!(
            strip_ansi_with_clear_detect(input),
            ("✓ Done".to_string(), false)
        );
    }

    #[test]
    fn unicode_overwrite_preserves_chars() {
        // CJK characters with cursor moves
        assert_eq!(
            strip_ansi_with_clear_detect("你好世界\r\u{1b}[2C再"),
            ("你好再界".to_string(), false)
        );
    }

    #[test]
    fn mixed_ansi_colors_stripped_correctly() {
        // Colored text with cursor movement should strip colors and handle cursor
        assert_eq!(
            strip_ansi_with_clear_detect("\u{1b}[32mhello\u{1b}[0m\rbye"),
            ("byelo".to_string(), false)
        );
    }

    #[test]
    fn clear_screen_still_detected() {
        // CSI 2J and 3J still trigger clear
        assert_eq!(
            strip_ansi_with_clear_detect("\u{1b}[2J"),
            ("".to_string(), true)
        );
        assert_eq!(
            strip_ansi_with_clear_detect("\u{1b}[3J"),
            ("".to_string(), true)
        );
        // CSI 0J / CSI 1J do not trigger clear
        assert_eq!(
            strip_ansi_with_clear_detect("\u{1b}[0J"),
            ("".to_string(), false)
        );
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
