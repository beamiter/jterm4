use gtk4::gdk::RGBA;
use gtk4::pango::FontDescription;
use gtk4::prelude::*;
use gtk4::{glib, Orientation, ScrolledWindow};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use vte4::Terminal;
use vte4::{TerminalExt, TerminalExtManual};

use crate::config::Config;
use crate::parser::{ColorKind, Parser, ParserConfig, ParserEvent};
use crate::pty::OwnedPty;

mod alt_screen;
mod ansi;
mod blocks;
mod cross_selection;
mod css;
mod export;
mod find;
mod history;
mod palette;
mod scroll;
pub(crate) use alt_screen::*;
pub(crate) use ansi::*;
pub(crate) use blocks::*;
pub(crate) use cross_selection::*;
pub(crate) use css::*;
pub(crate) use find::*;
pub(crate) use palette::*;
pub(crate) use scroll::*;

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
        let n = if unread > 99 {
            "99+".to_string()
        } else {
            unread.to_string()
        };
        fab.set_label(&format!("\u{f078}  {}", n));
    } else {
        fab.set_label("\u{f078}");
    }
}

/// Probe the cwd for git metadata and update the strip label. Hides the
/// label when cwd is empty, missing, or not inside a repo — the user
/// shouldn't see a stale branch from a previous pane state.
fn refresh_repo_strip(label: &gtk4::Label, cwd: &str) {
    if cwd.is_empty() {
        label.set_visible(false);
        return;
    }
    let path = std::path::Path::new(cwd);
    match crate::git_meta::read(path) {
        Some(meta) => {
            label.set_text(&crate::git_meta::format_strip(&meta));
            label.set_visible(true);
        }
        None => {
            label.set_visible(false);
        }
    }
}

fn sample_output_for_event(output: &str) -> String {
    const MAX_CHARS: usize = 32 * 1024;
    if output.len() <= MAX_CHARS {
        return output.to_string();
    }
    let half = MAX_CHARS / 2;
    let head_end = output
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= half)
        .last()
        .unwrap_or(0);
    let tail_start = output
        .char_indices()
        .map(|(i, _)| i)
        .find(|&i| i >= output.len().saturating_sub(half))
        .unwrap_or(output.len());
    format!(
        "{}\n... [{} bytes elided] ...\n{}",
        &output[..head_end],
        tail_start.saturating_sub(head_end),
        &output[tail_start..]
    )
}

fn truncate_plain_output_for_height(output_plain: &str, line_limit: usize) -> (String, usize) {
    let trimmed = output_plain.trim();
    let total_lines = trimmed.lines().count();
    if total_lines <= line_limit {
        return (trimmed.to_string(), total_lines);
    }

    let kept = trimmed
        .lines()
        .take(line_limit)
        .collect::<Vec<_>>()
        .join("\n");
    let truncated = format!(
        "{}\n\n[... truncated: {} lines total, showing first {}]",
        kept, total_lines, line_limit
    );
    let displayed_lines = truncated.lines().count();
    (truncated, displayed_lines)
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

fn build_color_query_reply(config: &Config, kind: ColorKind) -> String {
    let rgba = match kind {
        ColorKind::Foreground => config.foreground,
        ColorKind::Background => config.background,
        ColorKind::Cursor => config.cursor,
        ColorKind::Palette(idx) => {
            let (r, g, b) = ansi256_to_rgb(idx, &config.palette);
            RGBA::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
        }
    };
    let r = (rgba.red() * 65535.0) as u16;
    let g = (rgba.green() * 65535.0) as u16;
    let b = (rgba.blue() * 65535.0) as u16;
    match kind {
        ColorKind::Foreground => format!("\x1b]10;rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\"),
        ColorKind::Background => format!("\x1b]11;rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\"),
        ColorKind::Cursor => format!("\x1b]12;rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\"),
        ColorKind::Palette(idx) => {
            format!("\x1b]4;{idx};rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\")
        }
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

/// Install the shared click-to-select behavior for a finished block. New blocks
/// and restored history blocks must use the same handler; otherwise keyboard
/// block actions only work on commands produced after app startup.
fn install_finished_block_selection(
    block: &FinishedBlock,
    active: &Rc<RefCell<ActiveBlock>>,
    finished_blocks: &Rc<RefCell<Vec<FinishedBlock>>>,
    selected_block_id: &Rc<Cell<Option<u64>>>,
) {
    let active_for_click = active.clone();
    let header_for_click = block.header_row.clone();
    let finished_blocks_for_select = finished_blocks.clone();
    let selected_for_click = selected_block_id.clone();
    let this_id = block.id;
    let left_click = gtk4::GestureClick::new();
    left_click.set_button(1);
    left_click.set_propagation_phase(gtk4::PropagationPhase::Capture);
    left_click.connect_pressed(move |gesture, n_press, _, y| {
        // Only act on the first press of a sequence. Refiring grab_focus() on
        // the 2nd/3rd press would interrupt VTE's native double/triple-click
        // word/line selection in the output VTE child.
        if n_press != 1 {
            gesture.set_state(gtk4::EventSequenceState::Denied);
            return;
        }
        active_for_click.borrow().grab_focus();
        // Header strip occupies the top of the block; a press there toggles
        // this block's selection. Output clicks stay native VTE selection.
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
    block.widget().add_controller(left_click);
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

type BlockFinishedCallbacks = Rc<RefCell<Vec<Box<dyn Fn(String, i32, String)>>>>;

pub struct TermView {
    root: gtk4::Box,
    block_scroll: ScrolledWindow,
    block_list: gtk4::Box,
    /// The single persistent live VTE (jterm1 model): prompt + typing + output all
    /// render here natively; finished commands snapshot into styled blocks above.
    active_vte: Terminal,
    active: Rc<RefCell<ActiveBlock>>,
    bstate: Rc<Cell<BlockState>>,
    #[allow(dead_code)]
    prompt_buf: Rc<RefCell<String>>,
    /// Keystroke shadow used only to size the idle input cell (line count). The
    /// authoritative finished-command text is read off the live VTE at
    /// CommandStart, so this never has to round-trip to display.
    #[allow(dead_code)]
    typed_cmd: Rc<RefCell<String>>,
    /// True while an alt-screen app owns the viewport (finished blocks hidden).
    fullscreen: Rc<Cell<bool>>,
    /// True once the user has scrolled up off the live prompt; while false the
    /// view follows the bottom. Read by the per-frame tick to re-pin the prompt.
    #[allow(dead_code)]
    user_scrolled_up: Rc<Cell<bool>>,
    /// Guards programmatic scrolls so the scroll-lock detector doesn't mistake
    /// them for a user drag.
    #[allow(dead_code)]
    programmatic_scroll: Rc<Cell<bool>>,
    pty: Rc<OwnedPty>,
    cwd_callbacks: StrCallbacks,
    remote_session_callbacks: StrCallbacks,
    exited_callbacks: IntCallbacks,
    bell_callbacks: VoidCallbacks,
    title_callbacks: StrCallbacks,
    activity_callbacks: VoidCallbacks,
    mouse_reporting_mode: Rc<Cell<MouseReportingMode>>,
    config: Rc<RefCell<Config>>,
    block_data: Rc<RefCell<VecDeque<BlockData>>>,
    finished_blocks: Rc<RefCell<Vec<FinishedBlock>>>,
    viewport: Rc<RefCell<ViewportState>>,
    widget_pool: Rc<RefCell<WidgetPool>>,
    visible_indices: Rc<RefCell<std::collections::HashSet<usize>>>,
    selected_block_id: Rc<Cell<Option<u64>>>,
    bookmarks: Rc<RefCell<std::collections::HashSet<u64>>>,
    /// Find-within-blocks state: every match across the finished blocks plus a
    /// cursor into it, so Ctrl+F highlights all hits and Next/Prev step through
    /// them (Warp's FindWithinBlock). Tags are stripped on close via clear_find.
    find_state: Rc<RefCell<FindState>>,
    current_cwd: Rc<RefCell<String>>,
    /// Per-frame resize tick installed on `root`. Held so it can be removed on
    /// Drop — otherwise the callback runs forever and keeps its Rc captures
    /// (pty/active/vte/vte_box) alive past tab close.
    resize_tick_id: RefCell<Option<gtk4::TickCallbackId>>,
    /// Tracks per-VTE selections so a drag that crosses block boundaries can be
    /// copied as one contiguous string via Ctrl+Shift+C.
    cross_selection: Rc<CrossSelection>,
    block_finished_callbacks: BlockFinishedCallbacks,
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
    /// Keystroke-shadow input line, used only as a fallback if the VTE-text
    /// capture at CommandStart returns empty.
    typed_cmd_rc: Rc<RefCell<String>>,
    /// Command text read from the live VTE at CommandStart; primary source
    /// for the finished block.
    vte_typed_cmd_rc: Rc<RefCell<String>>,
    /// VTE cursor position (col, row) captured at PromptEnd; the start anchor
    /// for the text-range read that produces `vte_typed_cmd_rc`.
    prompt_end_pos_rc: Rc<Cell<(i64, i64)>>,
    /// Rendered prompt (last non-empty line) captured at PromptEnd, used by the
    /// finalize path since prompt_buf is cleared once the prompt ends.
    prompt_display_rc: Rc<RefCell<String>>,
    block_list_rc: gtk4::Box,
    block_scroll_rc: ScrolledWindow,
    remote_session_cbs: StrCallbacks,
    exited_cbs: IntCallbacks,
    activity_cbs: VoidCallbacks,
    mouse_reporting_rc: Rc<Cell<MouseReportingMode>>,
    config_for_cb: Rc<RefCell<Config>>,
    parser: Rc<RefCell<Parser>>,
    block_data_for_cb: Rc<RefCell<VecDeque<BlockData>>>,
    finished_blocks_for_cb: Rc<RefCell<Vec<FinishedBlock>>>,
    scroll_debouncer: ScrollDebouncer,
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
    /// Re-runs the input-cell sizing (`update_input_height`). Called at every
    /// FTCS state transition so the live VTE switches between compact (idle)
    /// and viewport (running / alt-screen) deterministically — without waiting
    /// for the next `contents_changed` to race with the child's first draw.
    resize_active: Rc<dyn Fn()>,
    /// Bottom-of-pane repo metadata label. Re-probed every time a block
    /// finishes (the user may have just run `git commit`, `git pull`,
    /// or anything else that changes branch/dirty/ahead-behind).
    repo_strip: gtk4::Label,
    block_finished_cbs: BlockFinishedCallbacks,
}

/// Fold every run of consecutive `ParserEvent::Bytes(_)` entries in `events`
/// into a single Bytes event whose payload is the concatenation. Preserves
/// the relative order of all other event kinds. The reader callback dispatches
/// per-event side effects (active_vte.feed, mark_dirty, accumulate_output,
/// activity_cbs), so coalescing replaces N feeds + N mark_dirty calls inside
/// one chunk with one of each per stretch — a win on `top` redraws, `cargo
/// build` spew, and any sustained byte-only output. Safe because boundary
/// events (PromptStart/End, AltScreen*, CommandStart/End) are NOT merged and
/// keep their own synchronous mark_dirty.
fn coalesce_bytes_events(events: &mut Vec<ParserEvent>) {
    if events.len() < 2 {
        return;
    }
    let mut write = 0usize;
    let mut i = 0usize;
    let n = events.len();
    while i < n {
        if matches!(events[i], ParserEvent::Bytes(_)) {
            // Move the first Bytes payload out so we can extend it in place.
            let placeholder = ParserEvent::Bytes(Vec::new());
            let first = std::mem::replace(&mut events[i], placeholder);
            let mut merged = match first {
                ParserEvent::Bytes(b) => b,
                _ => unreachable!(),
            };
            i += 1;
            while i < n {
                if let ParserEvent::Bytes(b) = &events[i] {
                    merged.extend_from_slice(b);
                    i += 1;
                } else {
                    break;
                }
            }
            events[write] = ParserEvent::Bytes(merged);
            write += 1;
        } else {
            if write != i {
                events.swap(write, i);
            }
            write += 1;
            i += 1;
        }
    }
    events.truncate(write);
}

fn is_post_command_metadata(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x1b]7;")
        || bytes.starts_with(b"\x1b]0;")
        || bytes.starts_with(b"\x1b]1;")
        || bytes.starts_with(b"\x1b]2;")
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
            typed_cmd_rc,
            vte_typed_cmd_rc,
            prompt_end_pos_rc,
            prompt_display_rc,
            block_list_rc,
            block_scroll_rc,
            remote_session_cbs,
            exited_cbs,
            activity_cbs,
            mouse_reporting_rc,
            config_for_cb,
            parser,
            block_data_for_cb,
            finished_blocks_for_cb,
            scroll_debouncer,
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
            resize_active,
            repo_strip,
            block_finished_cbs,
        } = self;
        pty.start_reader(
            move |data: Vec<u8>| {
                let mut events = event_buf.borrow_mut();
                events.clear();
                parser.borrow_mut().feed(&data, &mut events);
                // Fold runs of consecutive `Bytes` events into one so the live
                // VTE feed, autoscroll mark-dirty, and accumulate_output happen
                // once per stretch instead of once per parser chunk. Boundary
                // events (PromptStart/End, AltScreen*, CommandStart/End) still
                // run their synchronous mark_dirty between stretches, keeping
                // the scroll-invariant from [[scroll_synchronous_autoscroll]].
                coalesce_bytes_events(&mut events);

                for event in events.iter() {
                    let state = bstate_rc.get();
                    match event {
                        ParserEvent::DecsetMode { mode, set } => {
                            // VTE handles paste/cursor/etc. natively from its
                            // own bytes; block_view only needs mouse-reporting
                            // state for wheel suppression in alt-screen apps.
                            let new_mode = match (*mode, *set) {
                                (1000, true) => Some(MouseReportingMode::Click),
                                (1002, true) => Some(MouseReportingMode::Button),
                                (1003, true) => Some(MouseReportingMode::Motion),
                                (1006, true) => Some(MouseReportingMode::Sgr),
                                (1000 | 1002 | 1003 | 1006, false) => {
                                    Some(MouseReportingMode::None)
                                }
                                _ => None,
                            };
                            if let Some(m) = new_mode {
                                mouse_reporting_rc.set(m);
                            }
                        }
                        ParserEvent::Bytes(bytes) => {
                            // No shell integration seen yet: once real output flows,
                            // stream everything into the live VTE (raw fallback).
                            if state == BlockState::Idle {
                                bstate_rc.set(BlockState::RawFallback);
                            }

                            let feed_active_vte = match bstate_rc.get() {
                                BlockState::CollectingPrompt => {
                                    let text = String::from_utf8_lossy(bytes);
                                    prompt_buf_rc.borrow_mut().push_str(&text);
                                    scroll_debouncer.mark_dirty(&block_scroll_rc);
                                    true
                                }
                                BlockState::AwaitingCommand => {
                                    // The command text is read off the live VTE
                                    // at CommandStart (`text_range_format`), so
                                    // no shadow accumulation is needed here.
                                    scroll_debouncer.mark_dirty(&block_scroll_rc);
                                    true
                                }
                                BlockState::CollectingOutput | BlockState::PostCommand => {
                                    if bstate_rc.get() != BlockState::PostCommand
                                        || !is_post_command_metadata(bytes)
                                    {
                                        active_rc.borrow().accumulate_output(bytes);
                                    }
                                    for cb in activity_cbs.borrow().iter() {
                                        cb();
                                    }
                                    true
                                }
                                BlockState::AltScreen => {
                                    // Alt-screen bytes go to the live VTE only — they
                                    // are not captured into block output (ephemeral).
                                    true
                                }
                                _ => true,
                            };

                            if feed_active_vte {
                                active_vte.feed(bytes);
                            }
                        }

                        ParserEvent::PromptStart => {
                            ftcs_seen_rc.set(true);
                            let state = bstate_rc.get();
                            if state == BlockState::CollectingOutput
                                || state == BlockState::AltScreen
                            {
                                continue;
                            }
                            // Finalize the previous command (deferred from CommandEnd).
                            if state == BlockState::PostCommand {
                                // The VTE-text capture taken at CommandStart is
                                // authoritative — it reflects what was on screen
                                // when the user pressed Enter. Fall back to the
                                // keystroke shadow only if the VTE read came back
                                // empty (which would indicate the prompt-end
                                // anchor never captured a valid cursor position).
                                let vte_cmd = vte_typed_cmd_rc.borrow().trim().to_string();
                                let cmd = if !vte_cmd.is_empty() {
                                    vte_cmd
                                } else {
                                    typed_cmd_rc.borrow().trim().to_string()
                                };

                                if cmd.is_empty() {
                                    // Nothing meaningful to record; just reset.
                                    let preserve = config_for_cb.borrow().preserve_live_scrollback;
                                    active_rc.borrow().reset_active(preserve);
                                    bstate_rc.set(BlockState::CollectingPrompt);
                                    prompt_buf_rc.borrow_mut().clear();
                                    scroll_debouncer.mark_dirty(&block_scroll_rc);
                                    continue;
                                }

                                let prompt = prompt_display_rc.borrow().clone();

                                // The raw bytes already carry CRLF — the PTY's
                                // ONLCR turns `\n` into `\r\n` on the master side
                                // before we ever see them — and the finished VTE
                                // handles in-line CR overwrites natively, just
                                // like the live VTE did while the command ran. So
                                // we feed the captured bytes verbatim, with no
                                // reconstruction pass.
                                let output_with_ansi = active_rc.borrow().output_text();

                                let output_plain = strip_ansi(&output_with_ansi);

                                let truncation_limit =
                                    config_for_cb.borrow().truncation_threshold_lines as usize;
                                let (_output_trimmed, line_count) =
                                    truncate_plain_output_for_height(
                                        &output_plain,
                                        truncation_limit,
                                    );
                                let estimated_height = estimated_finished_block_height(
                                    &config_for_cb.borrow(),
                                    line_count,
                                );

                                let start_time = block_start_time_for_cb.get();
                                let now = SystemTime::now();
                                let end_time_ms = now
                                    .duration_since(SystemTime::UNIX_EPOCH)
                                    .ok()
                                    .map(|d| d.as_millis() as u64);
                                let start_time_ms = start_time.and_then(|st| {
                                    st.duration_since(SystemTime::UNIX_EPOCH)
                                        .ok()
                                        .map(|d| d.as_millis() as u64)
                                });
                                let duration_ms = start_time.and_then(|st| {
                                    now.duration_since(st).ok().map(|d| d.as_millis() as u64)
                                });

                                let block_cwd = {
                                    let cwd_str = current_cwd_for_cb.borrow().clone();
                                    if cwd_str.is_empty() {
                                        None
                                    } else {
                                        Some(cwd_str)
                                    }
                                };

                                let exit_code = pending_exit_code_rc.get();

                                // Single id shared by the serializable BlockData and
                                // the GTK FinishedBlock so id-keyed lookups (export,
                                // delete) resolve in both lists.
                                let block_id = next_block_id();
                                // Capture cols now (live VTE is allocated by the time
                                // a command finishes) and store it on BlockData so
                                // session restore can recreate the finished VTE at
                                // the same width — preserving column-formatted output
                                // (ls, git log, etc.) instead of reflowing it.
                                let cols = active_rc.borrow().grid_cols() as i64;
                                let block_data = BlockData {
                                    id: block_id,
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
                                    cols: cols.clamp(1, u16::MAX as i64) as u16,
                                };

                                block_data_for_cb.borrow_mut().push_back(block_data);

                                let recycled = widget_pool_for_cb.borrow_mut().acquire();
                                let finished = FinishedBlock::new_with_pool(
                                    block_id,
                                    &prompt,
                                    &cmd,
                                    None,
                                    &output_with_ansi,
                                    exit_code,
                                    &config_for_cb.borrow(),
                                    duration_ms,
                                    end_time_ms,
                                    block_cwd.as_deref(),
                                    cols,
                                    recycled,
                                );
                                finished.widget().insert_before(
                                    &block_list_rc,
                                    Some(active_rc.borrow().widget()),
                                );

                                let was_user_scrolled = scroll_debouncer.user_scrolled_up.get();

                                // If the user is reading history (scrolled up), this
                                // freshly-finished block is "unread": bump the FAB badge
                                // so they can see work completed below and jump to it.
                                if was_user_scrolled {
                                    unread_count_rc.set(unread_count_rc.get().saturating_add(1));
                                    set_jump_fab_label(&jump_fab, unread_count_rc.get());
                                    jump_fab.set_visible(true);
                                }

                                let max_blocks = config_for_cb.borrow().max_visible_blocks as usize;
                                let finished_clone = finished.clone();
                                let finished_widget = finished_clone.widget().clone();

                                finished_clone.connect_actions(
                                    &active_vte,
                                    &pty_for_init,
                                    &pty_synced_rc,
                                    &active_rc,
                                );
                                finished_clone.connect_scroll_forwarding(&block_scroll_rc);

                                finished_blocks_for_cb.borrow_mut().push(finished);

                                let output_sample = sample_output_for_event(&output_plain);
                                for cb in block_finished_cbs.borrow().iter() {
                                    cb(cmd.clone(), exit_code, output_sample.clone());
                                }

                                {
                                    let cfg = config_for_cb.borrow();
                                    if cfg.notify_long_blocks {
                                        if let Some(ms) = duration_ms {
                                            if ms >= cfg.notify_long_block_threshold_ms {
                                                crate::notify::long_block_finished(
                                                    &cmd, exit_code, ms,
                                                );
                                            }
                                        }
                                    }
                                    // Re-probe git state — the command that just
                                    // finished may have changed branch/dirty/upstream.
                                    if cfg.show_repo_strip {
                                        let cwd = current_cwd_for_cb.borrow().clone();
                                        refresh_repo_strip(&repo_strip, &cwd);
                                    }
                                }

                                // Right-click context menu.
                                let finished_blocks_for_menu = finished_blocks_for_cb.clone();
                                let block_list_for_menu = block_list_rc.clone();
                                let vte_for_copy = active_vte.clone();
                                let selected_for_menu = selected_block_id_rc.clone();
                                let block_id = finished_clone.id;

                                let right_click = gtk4::GestureClick::new();
                                right_click.set_button(3);

                                let finished_menu_clone = finished_clone.clone();
                                let block_data_for_export = block_data_for_cb.clone();
                                right_click.connect_pressed(move |gesture, _n_press, x, y| {
                                    gesture.set_state(gtk4::EventSequenceState::Claimed);
                                    {
                                        let finished = finished_blocks_for_menu.borrow();
                                        select_finished_block(
                                            &finished,
                                            &selected_for_menu,
                                            Some(block_id),
                                        );
                                    }

                                    let popover = gtk4::Popover::new();
                                    let widget: &gtk4::Widget = &finished_menu_clone
                                        .widget()
                                        .clone()
                                        .upcast::<gtk4::Widget>();
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
                                            let prompt_text = finished_for_copy.prompt_text.clone();
                                            let cmd_text = finished_for_copy.cmd_text.clone();
                                            let full_text =
                                                finished_for_copy.with_stripped_output(|output| {
                                                    format!("{prompt_text}\n{cmd_text}\n{output}")
                                                });
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
                                            if let Some(block) =
                                                blocks.iter().find(|b| b.id == block_id_json)
                                            {
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
                                            if let Some(block) =
                                                blocks.iter().find(|b| b.id == block_id_md)
                                            {
                                                let markdown = block.to_markdown();
                                                vte_for_md.clipboard().set_text(&markdown);
                                            }
                                        });
                                        vbox.append(&item);
                                    }

                                    {
                                        let item = make_item("Delete Block");
                                        let popover_c = popover.clone();
                                        let finished_blocks_for_delete =
                                            finished_blocks_for_menu.clone();
                                        let block_list_for_delete = block_list_for_menu.clone();
                                        let block_data_for_delete = block_data_for_export.clone();
                                        let selected_for_delete = selected_for_menu.clone();
                                        let block_id_del = block_id;
                                        item.connect_clicked(move |_| {
                                            popover_c.popdown();
                                            let mut blocks =
                                                finished_blocks_for_delete.borrow_mut();
                                            if let Some(pos) =
                                                blocks.iter().position(|b| b.id == block_id_del)
                                            {
                                                let block = blocks.remove(pos);
                                                block_list_for_delete.remove(block.widget());
                                            }
                                            if selected_for_delete.get() == Some(block_id_del) {
                                                selected_for_delete.set(None);
                                            }
                                            // Keep block_data in lockstep with the widget list.
                                            block_data_for_delete
                                                .borrow_mut()
                                                .retain(|b| b.id != block_id_del);
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

                                install_finished_block_selection(
                                    &finished_clone,
                                    &active_rc,
                                    &finished_blocks_for_cb,
                                    &selected_block_id_rc,
                                );

                                if finished_blocks_for_cb.borrow().len() > max_blocks {
                                    let oldest = finished_blocks_for_cb.borrow_mut().remove(0);
                                    if selected_block_id_rc.get() == Some(oldest.id) {
                                        selected_block_id_rc.set(None);
                                    }
                                    let widget_to_release = oldest.widget().clone();
                                    block_list_rc.remove(&widget_to_release);
                                    widget_pool_for_cb.borrow_mut().release(widget_to_release);
                                }

                                if block_data_for_cb.borrow().len() > max_blocks {
                                    block_data_for_cb.borrow_mut().pop_front();
                                }

                                let preserve = config_for_cb.borrow().preserve_live_scrollback;
                                active_rc.borrow().reset_active(preserve);
                                if !was_user_scrolled {
                                    scroll_debouncer.reset_scroll_lock();
                                    scroll_debouncer.pin_to_bottom_deferred(&block_scroll_rc);
                                }
                            }
                            bstate_rc.set(BlockState::CollectingPrompt);
                            prompt_buf_rc.borrow_mut().clear();
                            // Live VTE collapses back to the compact input cell
                            // now that no command is running. Sync the PTY size
                            // so the shell sees the new winsize before it reads
                            // anything past the prompt.
                            sync_active_to_pty(&resize_active, &active_vte, &pty_for_init);
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
                            typed_cmd_rc.borrow_mut().clear();
                            vte_typed_cmd_rc.borrow_mut().clear();
                            // Snapshot the live VTE cursor at the moment the
                            // prompt finishes drawing — this is where the user's
                            // command starts. CommandStart will read text from
                            // here to the cursor's then-position to recover the
                            // command as it really appeared on screen.
                            let (col, row) = active_vte.cursor_position();
                            prompt_end_pos_rc.set((col, row));
                            pty_synced_rc.set(false);
                            bstate_rc.set(BlockState::AwaitingCommand);
                            resize_active();
                            let active_for_focus = active_rc.clone();
                            glib::idle_add_local_once(move || {
                                active_for_focus.borrow().grab_focus();
                            });

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
                            if state == BlockState::CollectingOutput
                                || state == BlockState::AltScreen
                            {
                                osc133_depth_rc.set(osc133_depth_rc.get() + 1);
                                continue;
                            }
                            if state != BlockState::AwaitingCommand {
                                continue;
                            }
                            osc133_depth_rc.set(0);
                            active_rc.borrow().reset_output_buffer();
                            block_start_time_for_cb.set(Some(SystemTime::now()));
                            // Read the typed command directly off the live VTE,
                            // not from a shadow keystroke buffer. The VTE shows
                            // what the user actually saw — including history
                            // recalls and rsh autosuggestion accepts — so what we
                            // capture here is faithful to the run. Range goes
                            // from the cursor position captured at PromptEnd to
                            // the current cursor position (right before the
                            // shell echoes a newline and starts the command).
                            let (cmd_end_col, cmd_end_row) = active_vte.cursor_position();
                            let (start_col, start_row) = prompt_end_pos_rc.get();
                            let cmd_from_vte = active_vte
                                .text_range_format(
                                    vte4::Format::Text,
                                    start_row,
                                    start_col,
                                    cmd_end_row,
                                    cmd_end_col,
                                )
                                .0
                                .map(|gs| gs.to_string())
                                .unwrap_or_default()
                                .trim()
                                .to_string();
                            *vte_typed_cmd_rc.borrow_mut() = cmd_from_vte.clone();
                            *running_cmd_rc.borrow_mut() = cmd_from_vte;
                            cmd_running_rc.set(true);
                            bstate_rc.set(BlockState::CollectingOutput);
                            typed_cmd_rc.borrow_mut().clear();
                            // Match jterm1's block-mode runtime model: keep the
                            // active VTE as the live surface while the command
                            // runs, then snapshot it into a finished block on the
                            // next prompt. Interactive CLIs such as Codex rely on
                            // VTE applying cursor positioning/redraws directly.
                            sync_active_to_pty(&resize_active, &active_vte, &pty_for_init);
                            scroll_debouncer.mark_dirty(&block_scroll_rc);
                        }

                        ParserEvent::CommandEnd(code) => {
                            let state = bstate_rc.get();
                            if state != BlockState::CollectingOutput
                                && state != BlockState::AltScreen
                            {
                                continue;
                            }
                            if osc133_depth_rc.get() > 0 {
                                osc133_depth_rc.set(osc133_depth_rc.get() - 1);
                                continue;
                            }
                            // Safety net (Warp parity): if the alt-screen app
                            // crashed or exited without rmcup, force the UI back
                            // to the block list so the next prompt is usable.
                            if state == BlockState::AltScreen {
                                active_vte.feed(b"\x1b[?1049l");
                                exit_fullscreen(
                                    &finished_blocks_for_cb,
                                    &visible_indices_rc,
                                    &fullscreen_rc,
                                );
                                resize_active();
                            }
                            pending_exit_code_rc.set(*code);
                            cmd_running_rc.set(false);
                            bstate_rc.set(BlockState::PostCommand);
                            scroll_debouncer.mark_dirty(&block_scroll_rc);
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
                            // Hand the viewport to the alt-screen app: hide finished
                            // blocks so the live VTE fills the scroll area.
                            enter_fullscreen(
                                &finished_blocks_for_cb,
                                &visible_indices_rc,
                                &fullscreen_rc,
                            );
                            // Grow the live VTE to the full viewport before the
                            // app draws (see sync_active_to_pty doc).
                            sync_active_to_pty(&resize_active, &active_vte, &pty_for_init);
                            active_vte.feed(b"\x1b[?1049h");
                        }

                        ParserEvent::AltScreenLeave => {
                            if bstate_rc.get() != BlockState::AltScreen {
                                continue;
                            }
                            // Warp parity: alt-screen content is ephemeral and is
                            // NOT merged into the block. The active block keeps
                            // just the command name + exit code.
                            active_vte.feed(b"\x1b[?1049l");
                            exit_fullscreen(
                                &finished_blocks_for_cb,
                                &visible_indices_rc,
                                &fullscreen_rc,
                            );
                            osc133_depth_rc.set(0);
                            bstate_rc.set(prev_state_rc.get());
                            // Collapse the live VTE back to the compact input cell
                            // now that the alt app has released the viewport.
                            sync_active_to_pty(&resize_active, &active_vte, &pty_for_init);
                            let active_for_idle = active_rc.clone();
                            glib::idle_add_local_once(move || {
                                active_for_idle.borrow().grab_focus();
                            });
                        }

                        ParserEvent::ClipboardSet(text) => {
                            if config_for_cb.borrow().allow_remote_clipboard_write {
                                if let Some(display) = gtk4::gdk::Display::default() {
                                    let clipboard = display.clipboard();
                                    clipboard.set_text(text);
                                }
                            }
                        }

                        ParserEvent::ClipboardQuery => {
                            pty_for_init.write_bytes(b"\x1b]52;c;\x1b\\");
                        }

                        ParserEvent::ColorQuery(kind) => {
                            let reply = build_color_query_reply(&config_for_cb.borrow(), *kind);
                            pty_for_init.write_bytes(reply.as_bytes());
                        }

                        ParserEvent::KeyboardProtocolQuery(_query) => {}

                        ParserEvent::RemoteSessionId(id) => {
                            for cb in remote_session_cbs.borrow().iter() {
                                cb(id);
                            }
                        }

                        ParserEvent::ApcSequence(payload) => {
                            // Forward Kitty graphics (APC G ...) to the live VTE
                            // regardless of block state — tools like `kitten icat`
                            // emit them at the shell prompt (main screen), not
                            // only inside alt-screen apps. Limiting to AltScreen
                            // dropped images that appeared as part of a finished
                            // block's output.
                            let is_kitty = payload.first() == Some(&b'G');
                            if is_kitty {
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

/// Run `resize_active` (recompute target rows from layout + state) and then push
/// the resulting (cols, rows) to the PTY synchronously. Used at state
/// transitions where the child needs to see a correct winsize on its very first
/// read — `top` queries TIOCGWINSZ before painting, less/vim do the same.
/// Without the synchronous push the per-frame resize tick would catch up only
/// on the next frame, racing with the child.
fn sync_active_to_pty(resize_active: &Rc<dyn Fn()>, vte: &Terminal, pty: &OwnedPty) {
    resize_active();
    let cols = vte.column_count().max(1) as u16;
    let rows = vte.row_count().max(1) as u16;
    pty.resize(cols, rows);
}

fn viewport_rows_for(vte: &Terminal, scroll: &ScrolledWindow) -> Option<i64> {
    let cell_h = (vte.char_height() as i32).max(1);
    let page = scroll.vadjustment().page_size() as i32;
    if page <= 1 {
        return None;
    }
    // .block-active wraps the VTE with margin+border+padding; subtract it from
    // page_size so the holder total fits. Running commands use this same row
    // count for their live active VTE, matching jterm1's block-mode behavior.
    let usable = (page - css::BLOCK_ACTIVE_VCHROME_PX).max(cell_h);
    Some(((usable / cell_h).max(1)) as i64)
}

fn compute_viewport_state(
    block_data: &VecDeque<BlockData>,
    visible_top: i32,
    visible_bottom: i32,
) -> ViewportState {
    let mut y = 0;
    let mut first = None;
    let mut last = 0;
    let mut iter = block_data.iter().enumerate();

    while let Some((i, block)) = iter.next() {
        let block_top = y;
        let block_bottom = y + block.estimated_height;
        if first.is_none() && block_bottom > visible_top {
            first = Some(i);
        }
        if block_top < visible_bottom {
            last = i;
        }
        y = block_bottom;

        if first.is_some() && y >= visible_bottom {
            for (_, block) in iter {
                y += block.estimated_height;
            }
            break;
        }
    }

    ViewportState {
        first_visible: first.unwrap_or(0),
        last_visible: last,
        total_height: y,
    }
}

fn visible_indices_for_viewport(vp: &ViewportState) -> std::collections::HashSet<usize> {
    let mut new_visible = std::collections::HashSet::new();
    for i in vp.first_visible..=vp.last_visible.min(vp.first_visible + 1000) {
        new_visible.insert(i);
    }
    new_visible
}

fn apply_visible_indices(
    finished: &[FinishedBlock],
    visible: &mut std::collections::HashSet<usize>,
    new_visible: std::collections::HashSet<usize>,
) {
    for &i in visible.difference(&new_visible) {
        if let Some(block) = finished.get(i) {
            block.widget().set_visible(false);
        }
    }
    for &i in new_visible.difference(visible) {
        if let Some(block) = finished.get(i) {
            block.widget().set_visible(true);
        }
    }
    *visible = new_visible;
}

/// Hand the viewport to an alt-screen app: hide every finished block so the live
/// VTE fills the scroll area like a normal full-screen terminal.
fn enter_fullscreen(
    finished: &Rc<RefCell<Vec<FinishedBlock>>>,
    visible_indices: &Rc<RefCell<std::collections::HashSet<usize>>>,
    fullscreen: &Rc<Cell<bool>>,
) {
    if fullscreen.replace(true) {
        return;
    }
    let finished = finished.borrow();
    let mut visible = visible_indices.borrow_mut();
    visible.clear();
    for (i, block) in finished.iter().enumerate() {
        if block.widget().is_visible() {
            visible.insert(i);
        }
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

fn running_root_control_bytes(
    keyval: gtk4::gdk::Key,
    modifiers: gtk4::gdk::ModifierType,
) -> Option<&'static [u8]> {
    use gtk4::gdk::Key;

    let ctrl = modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
    let alt = modifiers.contains(gtk4::gdk::ModifierType::ALT_MASK);
    if !ctrl || alt {
        return None;
    }

    if matches!(keyval, Key::c | Key::C) {
        Some(b"\x03")
    } else if matches!(keyval, Key::d | Key::D) {
        Some(b"\x04")
    } else {
        None
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
    block_data_for_key: Rc<RefCell<VecDeque<BlockData>>>,
    block_list_for_key: gtk4::Box,
    selected_block_id_for_key: Rc<Cell<Option<u64>>>,
    block_scroll_for_key: ScrolledWindow,
    bookmarks_for_key: Rc<RefCell<std::collections::HashSet<u64>>>,
    bstate_for_key: Rc<Cell<BlockState>>,
}

impl KeyCtx {
    fn connect(self, key_ctrl: &gtk4::EventControllerKey) {
        let KeyCtx {
            pty_for_key,
            active_vte_for_key,
            typed_cmd_for_key,
            finished_blocks_for_key,
            block_data_for_key,
            block_list_for_key,
            selected_block_id_for_key,
            block_scroll_for_key,
            bookmarks_for_key,
            bstate_for_key,
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

            // Ctrl+Shift+Up/Down: move the finished-block selection (Warp uses
            // Ctrl+Up/Down, but VTE swallows plain Ctrl+arrow before the capture
            // handler, so Shift is required in practice; the condition stays
            // permissive so plain Ctrl+arrow also works where it is delivered).
            if ctrl && !alt && matches!(keyval, Key::Up | Key::Down) {
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
                            if let Some(point) =
                                widget.compute_point(&scroll, &gtk4::graphene::Point::new(0.0, 0.0))
                            {
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

            // Ctrl+B: toggle a bookmark on the selected block (Warp's
            // ToggleBookmarkBlock). Shows the gutter star + accent stripe.
            // Only consume the key when bookmark logic actually fires — in
            // alt-screen (vim/less) or with no selection, let VTE deliver
            // Ctrl+B to the running app (e.g. vim's page-up).
            if ctrl
                && !shift
                && !alt
                && matches!(keyval, Key::b | Key::B)
                && bstate_for_key.get() != BlockState::AltScreen
            {
                if let Some(sel_id) = selected_block_id_for_key.get() {
                    let finished = finished_blocks_for_key.borrow();
                    if let Some(block) = finished.iter().find(|b| b.id == sel_id) {
                        let mut marks = bookmarks_for_key.borrow_mut();
                        let now_marked = if marks.remove(&sel_id) {
                            false
                        } else {
                            marks.insert(sel_id);
                            true
                        };
                        block.bookmark_star.set_visible(now_marked);
                        if now_marked {
                            block.widget().add_css_class("block-bookmarked");
                        } else {
                            block.widget().remove_css_class("block-bookmarked");
                        }
                        return glib::Propagation::Stop;
                    }
                }
            }

            // Ctrl+,/Ctrl+. : jump to the previous/next bookmarked block (Warp's
            // SelectBookmarkUp/Down). VTE swallows Alt+arrow and plain Ctrl+arrow
            // before the capture handler sees them, so comma/period are used here.
            if ctrl && !alt && !shift && matches!(keyval, Key::comma | Key::period) {
                let finished = finished_blocks_for_key.borrow();
                let marks = bookmarks_for_key.borrow();
                if marks.is_empty() {
                    return glib::Propagation::Stop;
                }
                let marked_idx: Vec<usize> = finished
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| marks.contains(&b.id))
                    .map(|(i, _)| i)
                    .collect();
                if marked_idx.is_empty() {
                    return glib::Propagation::Stop;
                }
                let cur = selected_block_id_for_key
                    .get()
                    .and_then(|id| finished.iter().position(|b| b.id == id));
                let target = if keyval == Key::comma {
                    marked_idx
                        .iter()
                        .rev()
                        .find(|&&i| cur.map(|c| i < c).unwrap_or(true))
                        .copied()
                        .or_else(|| marked_idx.last().copied())
                } else {
                    marked_idx
                        .iter()
                        .find(|&&i| cur.map(|c| i > c).unwrap_or(true))
                        .copied()
                        .or_else(|| marked_idx.first().copied())
                };
                if let Some(idx) = target {
                    let new_id = finished.get(idx).map(|b| b.id);
                    select_finished_block(&finished, &selected_block_id_for_key, new_id);
                    if let Some(block) = finished.get(idx) {
                        let widget = block.widget().clone();
                        let scroll = block_scroll_for_key.clone();
                        glib::idle_add_local_once(move || {
                            if let Some(point) =
                                widget.compute_point(&scroll, &gtk4::graphene::Point::new(0.0, 0.0))
                            {
                                let adj = scroll.vadjustment();
                                let target = (point.y() as f64) - adj.page_size() / 3.0;
                                adj.set_value(target.max(0.0));
                            }
                        });
                    }
                }
                return glib::Propagation::Stop;
            }

            // Ctrl+Shift+L: clear visible finished blocks + send form feed to
            // the shell. (Plain Ctrl+L is left to the shell so readline's
            // clear-screen still works as users expect inside the live cell.)
            if ctrl && shift && !alt && matches!(keyval, Key::l | Key::L) {
                let mut blocks = finished_blocks_for_key.borrow_mut();
                for block in blocks.drain(..) {
                    block_list_for_key.remove(block.widget());
                }
                pty_for_key.write_bytes(b"\x0c");
                return glib::Propagation::Stop;
            }

            // Ctrl+Shift+C / Ctrl+Shift+V are handled at the window-level
            // capture handler in main.rs (via TermView::copy_to_clipboard /
            // paste_from_clipboard) so they work regardless of which child
            // widget currently has focus — in particular after the user
            // mouse-selects text inside a finished block's TextView, focus
            // sits there and this per-VTE controller never fires.

            // Ctrl+P: fuzzy command-history palette. Build a deduped, most-recent
            // -first entry list from block_data (which carries exit code + duration
            // for the failed/slow filters) and pop it up.
            if ctrl && !shift && !alt && matches!(keyval, Key::p | Key::P) {
                let mut seen = std::collections::HashSet::new();
                let mut entries = Vec::new();
                {
                    let block_data = block_data_for_key.borrow();
                    for b in block_data.iter().rev() {
                        let c = b.cmd.lines().next().unwrap_or("").trim().to_string();
                        if c.is_empty() {
                            continue;
                        }
                        if seen.insert(c.clone()) {
                            entries.push(PaletteEntry {
                                cmd: c,
                                failed: b.exit_code != 0,
                                slow: b.duration_ms.map(|d| d >= PALETTE_SLOW_MS).unwrap_or(false),
                            });
                        }
                    }
                }
                show_command_palette(
                    &block_scroll_for_key,
                    entries,
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
        root.set_focusable(true);
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

        // ── Repo-status strip ────────────────────────────────────────────
        // A thin always-visible label at the bottom showing the current
        // pane's git branch + dirty marker + ahead/behind. Refreshed on
        // cwd change and on every finished block (the user may have just
        // run `git commit` or `git pull`). Hidden when cwd isn't a repo.
        let repo_strip = gtk4::Label::new(None);
        repo_strip.set_halign(gtk4::Align::Start);
        repo_strip.set_xalign(0.0);
        repo_strip.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        repo_strip.add_css_class("repo-strip");
        repo_strip.set_visible(false);
        if config.show_repo_strip {
            root.append(&repo_strip);
        }

        let unread_count: Rc<Cell<u32>> = Rc::new(Cell::new(0));

        // ── PTY ───────────────────────────────────────────────────────────
        // Detect rsh shell for session_id passing
        let is_rsh = shell_argv
            .first()
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

        // Git defaults LESS to "FRX" when the user has not set it. "F" quits
        // the pager when output fits on one screen, and "X" disables less'
        // alternate-screen setup. Default to raw-control-char rendering only:
        // keep colored git output, keep the interactive pager even for a short
        // `git log`, and let less use alt-screen so transient pager content
        // stays ephemeral. Respect an explicit user-provided LESS.
        let mut env_extra: Vec<(&str, &str)> = if std::env::var_os("LESS").is_none() {
            vec![("LESS", "R")]
        } else {
            Vec::new()
        };
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

        // Keystroke-shadow command line; kept only to drive the idle input-cell
        // height (newline count). The authoritative command text is read off the
        // VTE at CommandStart, so this shadow is no longer load-bearing — it
        // does not need to match the rendered line in edge cases.
        let typed_cmd: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        // Command text snapshot taken at CommandStart from the VTE itself,
        // between `prompt_end_pos` and the current cursor. This is what
        // finalize uses to record the run.
        let vte_typed_cmd: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        // VTE cursor position (col, row) right after the prompt finished
        // drawing — anchor for the text-range read at CommandStart.
        let prompt_end_pos: Rc<Cell<(i64, i64)>> = Rc::new(Cell::new((0, 0)));

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
            let last_size_target: Rc<Cell<(i64, i64)>> = Rc::new(Cell::new((0, 0)));
            Rc::new(move || {
                let cell_h = (vte.char_height() as i32).max(1);
                let Some(viewport_rows) = viewport_rows_for(&vte, &scroll) else {
                    return;
                };
                let cols = vte.column_count().max(1);
                let state = bstate.get();
                if matches!(
                    state,
                    BlockState::CollectingOutput | BlockState::PostCommand
                ) {
                    let target = (cols, viewport_rows);
                    if last_size_target.get() != target {
                        vte.set_size(cols, viewport_rows);
                        last_size_target.set(target);
                    }
                    holder.set_visible(true);
                    holder.set_height_request((viewport_rows as i32) * cell_h);
                    return;
                }
                holder.set_visible(true);
                let compact_rows = || {
                    let input_lines =
                        1 + typed_cmd.borrow().bytes().filter(|&b| b == b'\n').count() as i64;
                    let floor = (MIN_INPUT_ROWS as i64).min(viewport_rows);
                    let cap = viewport_rows.max(floor);
                    input_lines.clamp(floor, cap)
                };
                let target_rows = match state {
                    // A real terminal's grid is the viewport, always. While a
                    // alt-screen app owns the screen, or while we fall back to
                    // raw VTE (no OSC-133), keep the live VTE pinned to the full
                    // viewport. Normal command output is rendered into the
                    // running block above, so the live input cell can stay
                    // compact instead of stealing the page.
                    BlockState::AltScreen | BlockState::RawFallback => viewport_rows,
                    // Between prompts the live cell collapses to fit the
                    // typed command (warp-style compact input). Must NOT use
                    // cursor_position().1 here: it's the absolute scrollback
                    // row and climbs without bound across the session.
                    BlockState::Idle
                    | BlockState::CollectingPrompt
                    | BlockState::AwaitingCommand => compact_rows(),
                    BlockState::CollectingOutput | BlockState::PostCommand => viewport_rows,
                };
                // Drive the VTE grid directly. `set_height_request` only sets a
                // *minimum*, so it cannot shrink a VTE whose natural height
                // (row_count * char_height) is larger — the cell would stay
                // full-height. `set_size` sets the preferred grid, shrinking the
                // VTE's natural height so the (non-expanding) holder collapses to
                // it. The PTY-resize tick then follows row_count up/down.
                let target = (cols, target_rows);
                if last_size_target.get() != target {
                    vte.set_size(cols, target_rows);
                    last_size_target.set(target);
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
        // Rendered prompt captured at PromptEnd (prompt_buf is cleared once the
        // prompt ends, so the finalize path reads this instead).
        let prompt_display: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        // True while an alt-screen app owns the viewport (finished blocks hidden).
        let fullscreen: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let cwd_callbacks: StrCallbacks = Rc::new(RefCell::new(vec![]));
        let remote_session_callbacks: StrCallbacks = Rc::new(RefCell::new(vec![]));
        let exited_callbacks: IntCallbacks = Rc::new(RefCell::new(vec![]));
        let bell_callbacks: VoidCallbacks = Rc::new(RefCell::new(vec![]));
        // Bell signal is delivered natively by VTE — no need to scan the byte
        // stream for BEL ourselves (and disambiguate it from OSC string
        // terminators). VTE already does that disambiguation inside its parser.
        {
            let bell_cbs = bell_callbacks.clone();
            active_vte.connect_bell(move |_| {
                for cb in bell_cbs.borrow().iter() {
                    cb();
                }
            });
        }
        let title_callbacks: StrCallbacks = Rc::new(RefCell::new(vec![]));
        let activity_callbacks: VoidCallbacks = Rc::new(RefCell::new(vec![]));
        let block_finished_callbacks: BlockFinishedCallbacks = Rc::new(RefCell::new(vec![]));
        let mouse_reporting_mode: Rc<Cell<MouseReportingMode>> =
            Rc::new(Cell::new(MouseReportingMode::None));
        let block_data_rc: Rc<RefCell<VecDeque<BlockData>>> =
            Rc::new(RefCell::new(VecDeque::new()));
        let finished_blocks_rc: Rc<RefCell<Vec<FinishedBlock>>> = Rc::new(RefCell::new(Vec::new()));

        let pending_exit_code: Rc<Cell<i32>> = Rc::new(Cell::new(0));

        let widget_pool: Rc<RefCell<WidgetPool>> = Rc::new(RefCell::new(WidgetPool::new()));
        let pty_synced: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let selected_block_id: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));
        // Bookmarked block ids (in-memory for the session). Toggled with Ctrl+B;
        // navigated with Alt+Up/Down. Not persisted (avoids an rkyv schema bump).
        let block_bookmarks: Rc<RefCell<std::collections::HashSet<u64>>> =
            Rc::new(RefCell::new(std::collections::HashSet::new()));
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
        let current_cwd: Rc<RefCell<String>> = Rc::new(RefCell::new(cwd.unwrap_or("").to_string()));

        // CWD updates come from VTE's native OSC 7 signal (the parser passes
        // OSC 7 through unchanged, see parser.rs). Title updates likewise come
        // from VTE's window-title-changed (OSC 0/2).
        {
            let cwd_cbs = cwd_callbacks.clone();
            let current_cwd_for_signal = current_cwd.clone();
            let vte_for_cwd = active_vte.clone();
            let repo_strip_for_cwd = repo_strip.clone();
            active_vte.connect_current_directory_uri_notify(move |_| {
                if let Some(uri) = vte_for_cwd.current_directory_uri() {
                    let file = gtk4::gio::File::for_uri(uri.as_str());
                    if let Some(path) = file
                        .path()
                        .map(|p| p.to_string_lossy().to_string())
                        .filter(|s| !s.is_empty())
                    {
                        *current_cwd_for_signal.borrow_mut() = path.clone();
                        refresh_repo_strip(&repo_strip_for_cwd, &path);
                        for cb in cwd_cbs.borrow().iter() {
                            cb(&path);
                        }
                    }
                }
            });
        }

        // Initial probe so the strip is populated for the starting cwd
        // before the user has cd'd anywhere (the OSC 7 above only fires
        // on a change).
        {
            let initial_cwd = current_cwd.borrow().clone();
            refresh_repo_strip(&repo_strip, &initial_cwd);
        }
        {
            let title_cbs = title_callbacks.clone();
            let vte_for_title = active_vte.clone();
            active_vte.connect_window_title_changed(move |_| {
                if let Some(title) = vte_for_title.window_title() {
                    let title_str = title.to_string();
                    if !title_str.is_empty() {
                        for cb in title_cbs.borrow().iter() {
                            cb(&title_str);
                        }
                    }
                }
            });
        }

        // ── Wire PTY → parser → block events ─────────────────────────────
        {
            let active_rc = active.clone();
            let active_vte_rc = active_vte.clone();
            let bstate_rc = bstate.clone();
            let prev_state_rc = prev_state.clone();
            let osc133_depth_rc = osc133_depth.clone();
            let prompt_buf_rc = prompt_buf.clone();
            let typed_cmd_rc = typed_cmd.clone();
            let vte_typed_cmd_rc = vte_typed_cmd.clone();
            let prompt_end_pos_rc = prompt_end_pos.clone();
            let prompt_display_rc = prompt_display.clone();
            let block_list_rc = block_list.clone();
            let block_scroll_rc = block_scroll.clone();
            let exited_cbs = exited_callbacks.clone();
            let activity_cbs = activity_callbacks.clone();
            let mouse_reporting_rc = mouse_reporting_mode.clone();
            let config_for_cb = Rc::new(RefCell::new(config.clone()));
            let parser = Rc::new(RefCell::new(Parser::with_config(ParserConfig {
                mouse_reporting: config.mouse_reporting_enabled,
                focus_reporting: config.focus_reporting_enabled,
            })));
            let block_data_for_cb = block_data_rc.clone();
            let finished_blocks_for_cb = finished_blocks_rc.clone();
            let scroll_debouncer = ScrollDebouncer::with_scroll_lock(
                user_scrolled_up.clone(),
                programmatic_scroll.clone(),
            );
            let widget_pool_for_cb = widget_pool.clone();
            let pty_synced_rc = pty_synced.clone();
            let visible_indices_rc = visible_indices.clone();
            let fullscreen_rc = fullscreen.clone();
            let ftcs_seen_rc = ftcs_seen.clone();

            // Command queue for replaying initial_commands on PromptEnd events
            let init_cmds_queue: Rc<RefCell<std::collections::VecDeque<String>>> =
                Rc::new(RefCell::new(
                    initial_commands
                        .map(|s| {
                            s.split(", ")
                                .map(|c| c.trim().to_string())
                                .filter(|c| !c.is_empty())
                                .collect()
                        })
                        .unwrap_or_default(),
                ));
            let init_cmds_queue_for_cb = Rc::clone(&init_cmds_queue);
            let pty_for_init = Rc::clone(&pty);
            let block_start_time_for_cb = block_start_time.clone();
            let pending_exit_code_rc = pending_exit_code.clone();
            let current_cwd_for_cb = current_cwd.clone();

            let event_buf: Rc<RefCell<Vec<ParserEvent>>> =
                Rc::new(RefCell::new(Vec::with_capacity(32)));
            ReaderCtx {
                active_rc,
                active_vte: active_vte_rc,
                bstate_rc,
                prev_state_rc,
                osc133_depth_rc,
                prompt_buf_rc,
                typed_cmd_rc,
                vte_typed_cmd_rc,
                prompt_end_pos_rc,
                prompt_display_rc,
                block_list_rc,
                block_scroll_rc,
                remote_session_cbs: remote_session_callbacks.clone(),
                exited_cbs,
                activity_cbs,
                mouse_reporting_rc,
                config_for_cb,
                parser,
                block_data_for_cb,
                finished_blocks_for_cb,
                scroll_debouncer,
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
                resize_active: update_input_height.clone(),
                repo_strip: repo_strip.clone(),
                block_finished_cbs: block_finished_callbacks.clone(),
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
            block_scroll
                .vadjustment()
                .connect_value_changed(move |_adj| {
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
                // The finished-block command text comes from a live-VTE
                // text_range read at CommandStart (see PromptEnd / CommandStart
                // handlers), so this shadow buffer is only used to size the
                // input cell while idle (line count). We do not need to track
                // escape sequences or replay deletes — newline count is what
                // drives `update_input_height`.
                if bstate_for_commit.get() == BlockState::AwaitingCommand {
                    let mut cmd = typed_cmd_for_commit.borrow_mut();
                    for ch in text.chars() {
                        if ch == '\r' || ch == '\n' {
                            // Submitted — leave whatever is in the buffer; it
                            // is cleared at PromptEnd for the next prompt.
                        } else if ch == '\x7f' || ch == '\x08' {
                            cmd.pop();
                        } else if (ch as u32) < 0x20 {
                            // Control bytes: ignore.
                        } else {
                            cmd.push(ch);
                        }
                    }
                }
            });
        }

        // While a normal command is running, the active VTE is still the live
        // terminal surface. Let it own printable keys, Enter, Backspace, control
        // sequences, and IME preedit/commit. This root capture handler is only a
        // focus fallback for interrupt/EOF; forwarding text here would bypass
        // GTK's input method context and break CJK composition.
        {
            let pty_for_root_key = pty.clone();
            let bstate_for_root_key = bstate.clone();
            let root_key = gtk4::EventControllerKey::new();
            root_key.set_propagation_phase(gtk4::PropagationPhase::Capture);
            root_key.connect_key_pressed(move |_controller, keyval, _keycode, modifiers| {
                if !matches!(
                    bstate_for_root_key.get(),
                    BlockState::CollectingOutput | BlockState::PostCommand
                ) {
                    return glib::Propagation::Proceed;
                }

                if let Some(bytes) = running_root_control_bytes(keyval, modifiers) {
                    pty_for_root_key.write_bytes(bytes);
                    return glib::Propagation::Stop;
                }

                glib::Propagation::Proceed
            });
            root.add_controller(root_key);
        }

        // ── Keyboard navigation / copy-paste (Capture phase) ──────────────
        {
            let pty_for_key = pty.clone();
            let active_vte_for_key = active_vte.clone();
            let typed_cmd_for_key = typed_cmd.clone();
            let finished_blocks_for_key = finished_blocks_rc.clone();
            let block_data_for_key = block_data_rc.clone();
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
                block_data_for_key,
                block_list_for_key,
                selected_block_id_for_key,
                block_scroll_for_key,
                bookmarks_for_key: block_bookmarks.clone(),
                bstate_for_key: bstate.clone(),
            }
            .connect(&key_ctrl);

            active_vte.add_controller(key_ctrl);
        }

        // Wheel handling inside an alt-screen + mouse-reporting app (less / vim /
        // htop). VTE only synthesizes mouse-wheel CSI sequences when it owns the
        // PTY; ours is fed by our reader, so we synthesize and write the bytes
        // ourselves. The pointer cell under the cursor is tracked via a motion
        // controller so the column/row in the report matches what the user sees.
        //
        // - alt-screen + mouse mode + scroll_reporting_enabled → encode wheel,
        //   write to PTY, stop propagation (so block_scroll doesn't also scroll).
        // - alt-screen + mouse mode + !scroll_reporting_enabled → swallow wheel
        //   (user has opted out of mouse-driven paging).
        // - otherwise → let the event bubble to block_scroll for normal scroll.
        {
            // Track pointer position over the live VTE in cell coordinates so
            // wheel events emitted below can include accurate col/row.
            let pointer_cell: Rc<Cell<(i64, i64)>> = Rc::new(Cell::new((1, 1)));
            {
                let pointer_for_motion = pointer_cell.clone();
                let vte_for_motion = active_vte.clone();
                let motion = gtk4::EventControllerMotion::new();
                motion.set_propagation_phase(gtk4::PropagationPhase::Capture);
                motion.connect_motion(move |_, x, y| {
                    let cw = (vte_for_motion.char_width() as f64).max(1.0);
                    let ch = (vte_for_motion.char_height() as f64).max(1.0);
                    let col = (x / cw).floor() as i64 + 1;
                    let row = (y / ch).floor() as i64 + 1;
                    pointer_for_motion.set((col.max(1), row.max(1)));
                });
                active_vte.add_controller(motion);
            }

            let fullscreen_for_scroll = fullscreen.clone();
            let mouse_mode_for_scroll = mouse_reporting_mode.clone();
            let scroll_enabled = config.scroll_reporting_enabled;
            let pty_for_scroll = pty.clone();
            let pointer_for_scroll = pointer_cell.clone();
            let scroll_ctrl = gtk4::EventControllerScroll::new(
                gtk4::EventControllerScrollFlags::VERTICAL
                    | gtk4::EventControllerScrollFlags::HORIZONTAL,
            );
            scroll_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);
            scroll_ctrl.connect_scroll(move |_, _dx, dy| {
                let in_mouse_app = fullscreen_for_scroll.get()
                    && mouse_mode_for_scroll.get() != MouseReportingMode::None;
                if !in_mouse_app {
                    return glib::Propagation::Proceed;
                }
                if !scroll_enabled {
                    return glib::Propagation::Stop;
                }
                let (col, row) = pointer_for_scroll.get();
                if let Some(bytes) = encode_mouse_wheel(mouse_mode_for_scroll.get(), dy, col, row) {
                    pty_for_scroll.write_bytes(&bytes);
                }
                glib::Propagation::Stop
            });
            active_vte.add_controller(scroll_ctrl);
        }

        let cross_selection = CrossSelection::install(
            &block_scroll,
            finished_blocks_rc.clone(),
            active_vte.clone(),
        );

        let term_view = TermView {
            root,
            block_scroll,
            block_list,
            active_vte,
            active,
            bstate,
            prompt_buf,
            typed_cmd,
            fullscreen,
            user_scrolled_up: user_scrolled_up.clone(),
            programmatic_scroll: programmatic_scroll.clone(),
            pty,
            cwd_callbacks,
            remote_session_callbacks,
            exited_callbacks,
            bell_callbacks,
            title_callbacks,
            activity_callbacks,
            mouse_reporting_mode,
            config: Rc::new(RefCell::new(config.clone())),
            block_data: block_data_rc,
            finished_blocks: finished_blocks_rc,
            viewport: Rc::new(RefCell::new(ViewportState {
                first_visible: 0,
                last_visible: 0,
                total_height: 0,
            })),
            widget_pool,
            visible_indices,
            selected_block_id,
            bookmarks: block_bookmarks,
            find_state: Rc::new(RefCell::new(FindState::default())),
            current_cwd: current_cwd.clone(),
            resize_tick_id: RefCell::new(None),
            cross_selection,
            block_finished_callbacks,
        };

        // Load history if configured
        let _ = term_view.load_history();

        // Create widgets for loaded blocks. Each block's `cols` is what the live
        // VTE was wrapping at when the command ran; restoring at the same cols
        // reproduces the exact line breaks (so `ls` columns don't get split
        // mid-word). For old saves without a cols field (cols == 0), fall back
        // to the live VTE's current column count.
        {
            let block_data_ref = term_view.block_data.borrow();
            let config = term_view.config.borrow();
            let fallback_cols = term_view.active.borrow().grid_cols() as i64;
            for block in block_data_ref.iter() {
                let cols = if block.cols > 0 {
                    block.cols as i64
                } else {
                    fallback_cols
                };
                let finished = FinishedBlock::new(
                    block.id,
                    &block.prompt,
                    &block.cmd,
                    block.cmd_markup.as_deref(),
                    &block.output,
                    block.exit_code,
                    &config,
                    block.duration_ms,
                    block.end_time_ms,
                    block.cwd.as_deref(),
                    cols,
                );
                finished.widget().insert_before(
                    &term_view.block_list,
                    Some(term_view.active.borrow().widget()),
                );
                finished.connect_actions(
                    &term_view.active_vte,
                    &term_view.pty,
                    &pty_synced,
                    &term_view.active,
                );
                finished.connect_scroll_forwarding(&term_view.block_scroll);
                install_finished_block_selection(
                    &finished,
                    &term_view.active,
                    &term_view.finished_blocks,
                    &term_view.selected_block_id,
                );
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
            let fullscreen = term_view.fullscreen.clone();
            let visibility_update_pending = Rc::new(Cell::new(false));

            let vadjust = block_scroll.vadjustment();
            vadjust.connect_changed(move |_| {
                if fullscreen.get() {
                    return;
                }
                // Update viewport on scroll change
                let adj = block_scroll.vadjustment();
                let scroll_top = adj.value() as i32;
                let viewport_height = adj.page_size() as i32;
                let margin = (config.borrow().virtual_scroll_margin as i32) * viewport_height;

                let visible_top = (scroll_top - margin).max(0);
                let visible_bottom = scroll_top + viewport_height + margin;

                let block_data_ref = block_data.borrow();
                let next_viewport =
                    compute_viewport_state(&block_data_ref, visible_top, visible_bottom);

                let mut vp = viewport.borrow_mut();
                *vp = next_viewport;
                drop(vp);

                if visibility_update_pending.get() {
                    return;
                }
                visibility_update_pending.set(true);

                // Schedule visibility update on next idle
                let vp = viewport.clone();
                let finished = finished_blocks.clone();
                let visible = visible_indices.clone();
                let fullscreen = fullscreen.clone();
                let pending = visibility_update_pending.clone();
                glib::idle_add_local_once(move || {
                    pending.set(false);
                    if fullscreen.get() {
                        return;
                    }
                    let vp_ref = vp.borrow();
                    let new_visible = visible_indices_for_viewport(&vp_ref);

                    let finished_ref = finished.borrow();
                    let mut visible_ref = visible.borrow_mut();
                    apply_visible_indices(&finished_ref, &mut visible_ref, new_visible);
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
    /// no pixel math, no chrome calibration. State-driven sizing (compact
    /// when idle, viewport when running) is handled in `update_input_height`,
    /// and FTCS transitions push TIOCSWINSZ synchronously from the reader so
    /// the child never sees a stale winsize on its first read.
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
    /// Priority: (1) live VTE selection (alt-screen apps + idle input cell),
    /// (2) any finished-block TextBuffer with an active selection, (3) PRIMARY
    /// clipboard as a last-resort fallback. Step 2 is what makes Ctrl+Shift+C
    /// work for mouse-selected text inside finished command/output views —
    /// PRIMARY alone is unreliable across compositors (notably Wayland).
    pub fn copy_to_clipboard(&self) {
        self.copy_to_clipboard_with_modifier(false);
    }

    /// Same as `copy_to_clipboard` but also honors the Warp "copy block output
    /// only" modifier (Alt+Ctrl+Shift+C) when a whole block is selected.
    pub fn copy_to_clipboard_with_modifier(&self, alt_held: bool) {
        log::debug!(">>> TermView::copy_to_clipboard called (alt={})", alt_held);

        // (0) Whole-block selection (Warp's CopyBlock; +Alt → output only).
        if let Some(sel_id) = self.selected_block_id.get() {
            let bd = self.block_data.borrow();
            if let Some(b) = bd.iter().find(|b| b.id == sel_id) {
                let text = if alt_held {
                    b.output.clone()
                } else if b.output.trim().is_empty() {
                    b.cmd.clone()
                } else {
                    format!("{}\n{}", b.cmd, b.output)
                };
                log::debug!(
                    ">>> TermView copy: copied whole block {} ({} chars)",
                    sel_id,
                    text.len()
                );
                self.active_vte.clipboard().set_text(&text);
                return;
            }
        }

        // (0.5) Cross-block drag: if more than one VTE has a selection (the
        // user dragged across block boundaries, see cross_selection.rs), copy
        // the concatenated text in widget order instead of just one widget's.
        if self.cross_selection.has_cross_selection() {
            if let Some(text) = self.cross_selection.copy_text() {
                log::debug!(
                    ">>> TermView copy: got {} chars from cross-block selection",
                    text.len()
                );
                self.active_vte.clipboard().set_text(&text);
                return;
            }
        }

        // (1) Live VTE selection
        if let Some(text) = self.active_vte.text_selected(vte4::Format::Text) {
            if !text.is_empty() {
                log::debug!(">>> TermView copy: got {} chars from VTE", text.len());
                self.active_vte.clipboard().set_text(&text);
                return;
            }
        }

        // (2) Finished-block VTEs (output_vte / command_vte). GTK4 selection is
        // per-widget so only one block can have a live selection at a time —
        // that's the one we copy.
        for blk in self.finished_blocks.borrow().iter() {
            for vte in [&blk.output_vte, &blk.command_vte] {
                if let Some(text) = vte.text_selected(vte4::Format::Text) {
                    let s = text.to_string();
                    if !s.is_empty() {
                        log::debug!(
                            ">>> TermView copy: got {} chars from finished block VTE",
                            s.len()
                        );
                        self.active_vte.clipboard().set_text(&s);
                        return;
                    }
                }
            }
        }

        // No live VTE / finished-block selection. We deliberately do NOT
        // fall back to PRIMARY — on Wayland it is empty for our own widgets
        // anyway, and on X11 GTK already mirrors widget selections into both
        // clipboards so the path was never actually load-bearing. Bailing out
        // here keeps Ctrl+Shift+C deterministic: it copies what the user can
        // see is selected, and only that.
        log::debug!(">>> TermView copy: no selection found, nothing to copy");
    }

    /// Paste from clipboard to PTY via VTE's native `paste_clipboard()`.
    /// VTE tracks its own bracketed-paste state from the same byte stream the
    /// shell sent it, so this honors mode 2004 automatically — no need to
    /// duplicate the state machine in block_view.
    pub fn paste_from_clipboard(&self) {
        self.active_vte.paste_clipboard();
    }

    pub fn connect_cwd_changed<F: Fn(&str) + 'static>(&self, f: F) {
        self.cwd_callbacks.borrow_mut().push(Box::new(f));
    }

    pub fn connect_remote_session_id<F: Fn(&str) + 'static>(&self, f: F) {
        self.remote_session_callbacks.borrow_mut().push(Box::new(f));
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

    pub fn connect_block_finished<F>(&self, f: F)
    where
        F: Fn(String, i32, String) + 'static,
    {
        self.block_finished_callbacks.borrow_mut().push(Box::new(f));
    }

    pub fn scroll_lines(&self, lines: i32) {
        let adj = self.block_scroll.vadjustment();
        let cell_h = self.active_vte.char_height() as f64;
        let step = if cell_h > 0.0 {
            cell_h
        } else {
            adj.step_increment()
        };
        let max_val = (adj.upper() - adj.page_size()).max(adj.lower());
        let value = (adj.value() + step * lines as f64).clamp(adj.lower(), max_val);
        adj.set_value(value);
    }

    pub fn apply_failed_filter(&self) {
        if let Some(idx) = self.get_failed_blocks().first().copied() {
            self.scroll_to_block(idx);
        }
    }

    pub fn apply_slow_filter(&self) {
        if let Some(idx) = self.get_slow_blocks(1000).first().copied() {
            self.scroll_to_block(idx);
        }
    }

    pub fn apply_pinned_filter(&self) {
        let finished = self.finished_blocks.borrow();
        let bookmarks = self.bookmarks.borrow();
        if let Some((idx, _)) = finished
            .iter()
            .enumerate()
            .find(|(_, block)| bookmarks.contains(&block.id))
        {
            drop(bookmarks);
            drop(finished);
            self.scroll_to_block(idx);
        }
    }

    pub fn clear_block_filter(&self) {
        self.scroll_to_block(0);
    }

    pub fn jump_to_pinned(&self, direction: i32) {
        let finished = self.finished_blocks.borrow();
        let bookmarks = self.bookmarks.borrow();
        if bookmarks.is_empty() {
            return;
        }
        let marked: Vec<usize> = finished
            .iter()
            .enumerate()
            .filter(|(_, block)| bookmarks.contains(&block.id))
            .map(|(idx, _)| idx)
            .collect();
        if marked.is_empty() {
            return;
        }
        let cur = self
            .selected_block_id
            .get()
            .and_then(|id| finished.iter().position(|block| block.id == id));
        let target = if direction < 0 {
            marked
                .iter()
                .rev()
                .find(|&&idx| cur.map(|c| idx < c).unwrap_or(true))
                .copied()
                .or_else(|| marked.last().copied())
        } else {
            marked
                .iter()
                .find(|&&idx| cur.map(|c| idx > c).unwrap_or(true))
                .copied()
                .or_else(|| marked.first().copied())
        };
        drop(bookmarks);
        drop(finished);
        if let Some(idx) = target {
            self.scroll_to_block(idx);
        }
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
        self.active_vte
            .set_color_cursor_foreground(Some(&config.cursor_foreground));
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
        let next_viewport = compute_viewport_state(&block_data, visible_top, visible_bottom);

        let mut vp = self.viewport.borrow_mut();
        *vp = next_viewport;
    }

    /// Update block visibility based on viewport: show visible blocks, hide off-screen ones.
    pub fn update_block_visibility(&self) {
        let vp = self.viewport.borrow().clone();
        let new_visible = visible_indices_for_viewport(&vp);

        let finished = self.finished_blocks.borrow();
        let mut visible = self.visible_indices.borrow_mut();
        apply_visible_indices(&finished, &mut visible, new_visible);
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
        let selected = self
            .selected_block_id
            .get()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".to_string());

        vec![
            (
                "State",
                vec![
                    (
                        "Block state".to_string(),
                        format!("{:?}", self.bstate.get()),
                    ),
                    (
                        "Mouse reporting".to_string(),
                        format!("{:?}", self.mouse_reporting_mode.get()),
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
                    (
                        "Output grid".to_string(),
                        format!("{out_cols} × {out_rows}"),
                    ),
                ],
            ),
            (
                "Blocks",
                vec![
                    ("Finished blocks".to_string(), finished_len.to_string()),
                    ("Block data entries".to_string(), block_data_len.to_string()),
                    ("Failed blocks".to_string(), failed.to_string()),
                    ("Slow blocks (>1s)".to_string(), slow.to_string()),
                    (
                        "Total output bytes".to_string(),
                        total_output_bytes.to_string(),
                    ),
                    ("Selected block id".to_string(), selected),
                ],
            ),
            (
                "Viewport",
                vec![
                    (
                        "First visible".to_string(),
                        viewport.first_visible.to_string(),
                    ),
                    (
                        "Last visible".to_string(),
                        viewport.last_visible.to_string(),
                    ),
                    (
                        "Total height".to_string(),
                        format!("{}px", viewport.total_height),
                    ),
                    ("Realized widgets".to_string(), visible.to_string()),
                    ("Profiling".to_string(), prof_enabled().to_string()),
                ],
            ),
        ]
    }

    pub fn scroll_to_block(&self, block_index: usize) {
        let finished = self.finished_blocks.borrow();
        if block_index >= finished.len() {
            return;
        }
        if let Some(block) = finished.get(block_index) {
            select_finished_block(&finished, &self.selected_block_id, Some(block.id));
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
            // Keep the serializable record list in lockstep with the widget list;
            // otherwise the two desync and count-based eviction / id lookups drift.
            self.block_data.borrow_mut().retain(|b| b.id != block_id);
        }
    }

    /// Most-recent-first deduplicated list of finished-block command lines.
    /// Used to populate the Ctrl+Shift+H history palette. The first entry is
    /// the most recent unique command; whitespace-only commands are dropped.
    pub fn command_history(&self) -> Vec<String> {
        let finished = self.finished_blocks.borrow();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out: Vec<String> = Vec::new();
        for block in finished.iter().rev() {
            let cmd = block.cmd_text.trim();
            if cmd.is_empty() {
                continue;
            }
            if seen.insert(cmd.to_string()) {
                out.push(cmd.to_string());
            }
        }
        out
    }

    /// Snapshot the currently selected finished block as an `ai::BlockContext`,
    /// truncating the output to `head + tail = 2*lines_per_side + 1` lines so
    /// a `cargo build` block doesn't blow the request budget. Returns `None`
    /// when no block is selected (Ctrl+Shift+Q from the live cell etc.).
    pub fn selected_block_context(&self, lines_per_side: usize) -> Option<crate::ai::BlockContext> {
        let id = self.selected_block_id.get()?;
        let finished = self.finished_blocks.borrow();
        let block = finished.iter().find(|b| b.id == id)?;
        let data = self.block_data.borrow();
        let bd = data.iter().find(|b| b.id == id);

        let output =
            block.with_stripped_output(|s| crate::ai::truncate_for_context(s, lines_per_side));
        Some(crate::ai::BlockContext {
            cmd: block.cmd_text.clone(),
            output,
            cwd: bd.and_then(|b| b.cwd.clone()),
            exit_code: bd.map(|b| b.exit_code).unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        coalesce_bytes_events, compute_viewport_state, strip_ansi, strip_ansi_with_clear_detect,
        truncate_plain_output_for_height, visible_indices_for_viewport, BlockData, ViewportState,
    };
    use crate::parser::ParserEvent;
    use std::collections::VecDeque;

    fn ev_summary(events: &[ParserEvent]) -> Vec<String> {
        events
            .iter()
            .map(|e| match e {
                ParserEvent::Bytes(b) => format!("B({})", String::from_utf8_lossy(b)),
                ParserEvent::PromptStart => "PS".to_string(),
                ParserEvent::PromptEnd => "PE".to_string(),
                ParserEvent::CommandStart => "CS".to_string(),
                ParserEvent::CommandEnd(c) => format!("CE({})", c),
                ParserEvent::AltScreenEnter => "ALT+".to_string(),
                ParserEvent::AltScreenLeave => "ALT-".to_string(),
                _ => "?".to_string(),
            })
            .collect()
    }

    fn block_with_height(estimated_height: i32) -> BlockData {
        BlockData {
            id: 0,
            prompt: String::new(),
            cmd: String::new(),
            cmd_markup: None,
            output: String::new(),
            exit_code: 0,
            estimated_height,
            line_count: 0,
            start_time_ms: None,
            end_time_ms: None,
            duration_ms: None,
            cwd: None,
            cols: 0,
        }
    }

    #[test]
    fn viewport_state_keeps_total_height_after_visible_range() {
        let blocks: VecDeque<BlockData> = [10, 20, 30, 40]
            .into_iter()
            .map(block_with_height)
            .collect();

        let vp = compute_viewport_state(&blocks, 15, 55);

        assert_eq!(vp.first_visible, 1);
        assert_eq!(vp.last_visible, 2);
        assert_eq!(vp.total_height, 100);
    }

    #[test]
    fn visible_indices_are_capped_to_reasonable_window() {
        let vp = ViewportState {
            first_visible: 10,
            last_visible: 2_000,
            total_height: 0,
        };

        let visible = visible_indices_for_viewport(&vp);

        assert!(visible.contains(&10));
        assert!(visible.contains(&1010));
        assert!(!visible.contains(&1011));
        assert_eq!(visible.len(), 1001);
    }

    #[test]
    fn truncate_plain_output_passthrough_counts_trimmed_lines() {
        let (text, lines) = truncate_plain_output_for_height("\nalpha\nbeta\n", 10);

        assert_eq!(text, "alpha\nbeta");
        assert_eq!(lines, 2);
    }

    #[test]
    fn truncate_plain_output_collects_only_visible_prefix() {
        let (text, lines) = truncate_plain_output_for_height("a\nb\nc\nd", 2);

        assert_eq!(
            text,
            "a\nb\n\n[... truncated: 4 lines total, showing first 2]"
        );
        assert_eq!(lines, 4);
    }

    #[test]
    fn coalesce_merges_consecutive_bytes() {
        let mut events = vec![
            ParserEvent::Bytes(b"hello ".to_vec()),
            ParserEvent::Bytes(b"world".to_vec()),
            ParserEvent::Bytes(b"!".to_vec()),
        ];
        coalesce_bytes_events(&mut events);
        assert_eq!(ev_summary(&events), vec!["B(hello world!)"]);
    }

    #[test]
    fn coalesce_preserves_boundary_events_in_order() {
        let mut events = vec![
            ParserEvent::Bytes(b"$ ".to_vec()),
            ParserEvent::PromptEnd,
            ParserEvent::Bytes(b"ls".to_vec()),
            ParserEvent::Bytes(b" -la".to_vec()),
            ParserEvent::CommandStart,
            ParserEvent::Bytes(b"file1\n".to_vec()),
            ParserEvent::Bytes(b"file2\n".to_vec()),
            ParserEvent::CommandEnd(0),
            ParserEvent::PromptStart,
        ];
        coalesce_bytes_events(&mut events);
        assert_eq!(
            ev_summary(&events),
            vec![
                "B($ )",
                "PE",
                "B(ls -la)",
                "CS",
                "B(file1\nfile2\n)",
                "CE(0)",
                "PS",
            ]
        );
    }

    #[test]
    fn coalesce_noop_on_empty_or_single() {
        let mut empty: Vec<ParserEvent> = Vec::new();
        coalesce_bytes_events(&mut empty);
        assert!(empty.is_empty());

        let mut one = vec![ParserEvent::Bytes(b"x".to_vec())];
        coalesce_bytes_events(&mut one);
        assert_eq!(ev_summary(&one), vec!["B(x)"]);

        let mut one_boundary = vec![ParserEvent::PromptStart];
        coalesce_bytes_events(&mut one_boundary);
        assert_eq!(ev_summary(&one_boundary), vec!["PS"]);
    }

    #[test]
    fn coalesce_handles_only_boundary_events() {
        let mut events = vec![
            ParserEvent::PromptStart,
            ParserEvent::PromptEnd,
            ParserEvent::CommandStart,
            ParserEvent::CommandEnd(1),
        ];
        coalesce_bytes_events(&mut events);
        assert_eq!(ev_summary(&events), vec!["PS", "PE", "CS", "CE(1)"]);
    }

    #[test]
    fn strips_charset_designation_from_output() {
        assert_eq!(strip_ansi("\u{1b}(Btop"), "top");
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
    fn running_root_handler_only_falls_back_for_interrupt_and_eof() {
        use gtk4::gdk::{Key, ModifierType};

        assert_eq!(
            super::running_root_control_bytes(Key::c, ModifierType::CONTROL_MASK),
            Some(b"\x03".as_slice())
        );
        assert_eq!(
            super::running_root_control_bytes(Key::D, ModifierType::CONTROL_MASK),
            Some(b"\x04".as_slice())
        );
        assert_eq!(
            super::running_root_control_bytes(Key::a, ModifierType::empty()),
            None
        );
        assert_eq!(
            super::running_root_control_bytes(Key::Return, ModifierType::empty()),
            None
        );
        assert_eq!(
            super::running_root_control_bytes(Key::BackSpace, ModifierType::empty()),
            None
        );
        assert_eq!(
            super::running_root_control_bytes(
                Key::c,
                ModifierType::CONTROL_MASK | ModifierType::ALT_MASK
            ),
            None
        );
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
        let byte_pos = buf.char_indices().nth(pos - 1).map(|(i, _)| i).unwrap_or(0);
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
