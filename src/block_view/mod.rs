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
use vte4::TerminalExt;

use crate::config::Config;
use crate::parser::{Parser, ParserEvent};
use crate::pty::OwnedPty;

mod ansi;
mod alt_screen;
mod blocks;
mod css;
mod scroll;
mod url;
pub(crate) use ansi::*;
pub(crate) use alt_screen::*;
pub(crate) use blocks::*;
pub(crate) use css::*;
pub(crate) use scroll::*;
pub(crate) use url::*;


// Global block ID counter
static BLOCK_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_block_id() -> u64 {
    BLOCK_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Upper bound on rows for the inline per-command output VTE. Matches the output
/// VTE scrollback (build_output_vte) so we never size the widget taller than the
/// content it can actually retain. Bounds resource use for runaway output.
const MAX_INLINE_OUTPUT_ROWS: i64 = 10_000;

#[allow(dead_code)]
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
        if let Some(initial_cwd) = cwd {
            active.borrow().update_cwd(initial_cwd);
        }
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
        let osc133_depth: Rc<Cell<u32>> = Rc::new(Cell::new(0));
        let prompt_buf: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let cmd_buf: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let cmd_display_raw: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let cmd_display_markup: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let last_nonempty_cmd_raw: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let last_nonempty_cmd_markup: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let executing_cmd_raw: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let executing_cmd_markup: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
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
        let ansi_cache: Rc<RefCell<LruCache<String, String>>> = Rc::new(RefCell::new(
            LruCache::new(NonZeroUsize::new(config.ansi_cache_capacity as usize).unwrap()),
        ));

        let pending_exit_code: Rc<Cell<i32>> = Rc::new(Cell::new(0));

        let widget_pool: Rc<RefCell<WidgetPool>> = Rc::new(RefCell::new(WidgetPool::new()));
        let pty_synced: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let tab_pending: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let completion_active: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let isearch_active: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let user_scrolled_up: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let programmatic_scroll: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let selected_block_id: Rc<Cell<Option<u64>>> = Rc::new(Cell::new(None));
        let current_cwd: Rc<RefCell<String>> = Rc::new(RefCell::new(
            cwd.unwrap_or("").to_string()
        ));

        // ── Wire PTY → parser → block events ─────────────────────────────
        {
            let active_rc = active.clone();
            let bstate_rc = bstate.clone();
            let osc133_depth_rc = osc133_depth.clone();
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
            let isearch_active_rc = isearch_active.clone();

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
            let pending_exit_code_rc = pending_exit_code.clone();
            let current_cwd_for_cb = current_cwd.clone();

            let last_pty_cols: Rc<Cell<u16>> = Rc::new(Cell::new(80));
            let event_buf: Rc<RefCell<Vec<ParserEvent>>> = Rc::new(RefCell::new(Vec::with_capacity(32)));
            pty.start_reader(
                move |data: Vec<u8>| {
                    log::debug!("PTY data: {} bytes, state={:?}", data.len(), bstate_rc.get());
                    if data.len() < 512 {
                        log::debug!("PTY hex: {:02x?}", &data);
                    }
                    // Sync PTY columns with the visible content width. Derive it
                    // from the always-visible command line, NOT the per-block output
                    // VTE: that VTE is hidden/auto-sized while idle, so its width
                    // collapses the PTY to the 40-col floor and makes `ls` print one
                    // entry per line. The command line is a full-width sibling of the
                    // output area; subtract its text margins (left 12 + right 8).
                    // Mirrors the tick-callback resize so the two never disagree.
                    {
                        let active = active_rc.borrow();
                        let char_w = active.output_vte.char_width();
                        let widget_w = active.command_view.allocated_width() as i64 - 20;
                        if char_w > 0 && widget_w > 0 {
                            let new_cols = (widget_w / char_w).max(40) as u16;
                            if new_cols != last_pty_cols.get() {
                                last_pty_cols.set(new_cols);
                                pty_for_resize.resize(new_cols, 24);
                            }
                        }
                    }
                    let mut events = event_buf.borrow_mut();
                    events.clear();
                    parser.borrow_mut().feed(&data, &mut events);

                    for event in events.iter() {
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

                                // Single-pass byte scan for control sequences
                                {
                                    let mut i = 0;
                                    while i < bytes.len() {
                                        if bytes[i] == 0x1b && i + 1 < bytes.len() {
                                            match bytes[i + 1] {
                                                b']' => {
                                                    // OSC: check for title (OSC 0; or OSC 2;)
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
                                                    // CSI: check for mode sequences
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
                                                                (b"2004", b'h') => {
                                                                    bracketed_paste_rc.set(true);
                                                                }
                                                                (b"2004", b'l') => {
                                                                    bracketed_paste_rc.set(false);
                                                                }
                                                                (b"1", b'h') => {
                                                                    application_cursor_rc.set(true);
                                                                }
                                                                (b"1", b'l') => {
                                                                    application_cursor_rc.set(false);
                                                                }
                                                                (b"1000", b'h') => {
                                                                    mouse_reporting_rc.set(MouseReportingMode::Click);
                                                                }
                                                                (b"1000", b'l') => {
                                                                    mouse_reporting_rc.set(MouseReportingMode::None);
                                                                }
                                                                (b"1002", b'h') => {
                                                                    mouse_reporting_rc.set(MouseReportingMode::Button);
                                                                }
                                                                (b"1002", b'l') => {
                                                                    mouse_reporting_rc.set(MouseReportingMode::None);
                                                                }
                                                                (b"1003", b'h') => {
                                                                    mouse_reporting_rc.set(MouseReportingMode::Motion);
                                                                }
                                                                (b"1003", b'l') => {
                                                                    mouse_reporting_rc.set(MouseReportingMode::None);
                                                                }
                                                                (b"1006", b'h') => {
                                                                    mouse_reporting_rc.set(MouseReportingMode::Sgr);
                                                                }
                                                                (b"1006", b'l') => {
                                                                    mouse_reporting_rc.set(MouseReportingMode::None);
                                                                }
                                                                _ => {}
                                                            }
                                                            i = seq_end;
                                                        }
                                                    } else {
                                                        // Non-? CSI: check for DECSCUSR (cursor shape: CSI Ps SP q)
                                                        let seq_start = i + 2;
                                                        let mut seq_end = seq_start;
                                                        while seq_end < bytes.len() && (bytes[seq_end].is_ascii_digit() || bytes[seq_end] == b' ') {
                                                            seq_end += 1;
                                                        }
                                                        if seq_end < bytes.len() && bytes[seq_end] == b'q' {
                                                            // DECSCUSR: extract parameter digit
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

                                let text = String::from_utf8_lossy(bytes).into_owned();
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
                                                    active_rc.borrow().reset_output_buffer();
                                                    completion_active_rc.set(false);
                                                    tab_pending_rc.set(false);
                                                    *active_rc.borrow().pending_suggestion.borrow_mut() = String::new();
                                                    active_rc.borrow().update_content_view();
                                                }
                                                scroll_debouncer.mark_dirty(&block_scroll_rc);
                                                continue;
                                            }

                                            if isearch_active_rc.get() {
                                                // Reverse/forward incremental search active: render in output VTE
                                                active_rc.borrow().feed_output(bytes);

                                                // Search ended when readline redraws a normal line w/o the marker
                                                if !detect_isearch_marker(&stripped) && !stripped.is_empty()
                                                    && (prompt_clean.is_empty() || stripped.contains(prompt_clean))
                                                {
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
                                                    active_rc.borrow().output_vte.set_visible(false);
                                                    active_rc.borrow().reset_output_buffer();
                                                    isearch_active_rc.set(false);
                                                    *active_rc.borrow().pending_suggestion.borrow_mut() = String::new();
                                                    active_rc.borrow().update_content_view();
                                                }
                                                scroll_debouncer.mark_dirty(&block_scroll_rc);
                                                continue;
                                            }

                                            // Entering incremental search (Ctrl-R / Ctrl-S)
                                            if detect_isearch_marker(&stripped) {
                                                isearch_active_rc.set(true);
                                                active_rc.borrow().reset_output_buffer();
                                                active_rc.borrow().feed_output(bytes);
                                                scroll_debouncer.mark_dirty(&block_scroll_rc);
                                                continue;
                                            }

                                            if tab_pending_rc.get() && has_menu_content {
                                                // Tab triggered a completion menu — show it in output VTE
                                                completion_active_rc.set(true);
                                                active_rc.borrow().reset_output_buffer();
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
                                    BlockState::PostCommand => {
                                        // Late-arriving output after CommandEnd — still feed to VTE
                                        active_rc.borrow().feed_output(bytes);
                                        scroll_debouncer.mark_dirty(&block_scroll_rc);
                                    }
                                    BlockState::Idle => {
                                        // Bytes before first prompt — ignore (pre-prompt noise)
                                    }
                                }
                            }

                            ParserEvent::PromptStart => {
                                let state = bstate_rc.get();
                                if state == BlockState::CollectingOutput || state == BlockState::AltScreen {
                                    continue;
                                }
                                // Finalize the previous block if we're in PostCommand state
                                if state == BlockState::PostCommand {
                                    active_rc.borrow().flush_output();

                                    let prompt = strip_ansi(&prompt_buf_rc.borrow()).trim().to_string();

                                    let mut raw_cmd_with_ansi = executing_cmd_raw_rc.borrow().clone();
                                    let mut cmd_markup = executing_cmd_markup_rc.borrow().clone();
                                    if raw_cmd_with_ansi.trim().is_empty()
                                        && !last_nonempty_cmd_raw_rc.borrow().trim().is_empty()
                                    {
                                        raw_cmd_with_ansi = last_nonempty_cmd_raw_rc.borrow().clone();
                                        cmd_markup = last_nonempty_cmd_markup_rc.borrow().clone();
                                    }
                                    let cmd = strip_ansi(&raw_cmd_with_ansi).trim().to_string();

                                    // Use raw_output for finalization — it's always correct
                                    // (properly cleared between commands). VTE text_range can
                                    // include stale scrollback content causing duplication.
                                    let raw_output_text = active_rc.borrow().output_text();

                                    // Preserve ANSI codes for colored display, only handle \r overwrites
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
                                    log::debug!("Finalize block: cmd={:?}, output_len={}, first_20_chars={:?}",
                                        cmd, output_trimmed.len(), output_plain.chars().take(20).collect::<String>());

                                    let line_count = output_trimmed.lines().count();
                                    let estimated_height = (line_count as i32 * 20).max(60);

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
                                        cmd_markup: if cmd_markup.is_empty() { None } else { Some(cmd_markup.clone()) },
                                        output: output_trimmed.clone(),
                                        exit_code,
                                        estimated_height,
                                        line_count,
                                        start_time_ms,
                                        end_time_ms,
                                        duration_ms,
                                        cwd: block_cwd.clone(),
                                    };

                                    block_data_for_cb.borrow_mut().push_back(block_data);

                                    let recycled = widget_pool_for_cb.borrow_mut().acquire();
                                    let finished = FinishedBlock::new_with_pool(
                                        &prompt, &cmd, if cmd_markup.is_empty() { None } else { Some(&cmd_markup) }, &output_with_ansi, exit_code, &config_for_cb.borrow(),
                                        duration_ms, end_time_ms, block_cwd.as_deref(), recycled,
                                    );

                                    finished.widget().insert_before(&block_list_rc, Some(active_rc.borrow().widget()));

                                    let max_blocks = config_for_cb.borrow().max_visible_blocks as usize;
                                    let finished_clone = finished.clone();
                                    let finished_widget = finished_clone.widget().clone();
                                    finished_blocks_for_cb.borrow_mut().push(finished);

                                    // Setup right-click context menu
                                    let finished_blocks_for_menu = finished_blocks_for_cb.clone();
                                    let block_list_for_menu = block_list_rc.clone();
                                    let vte_for_copy = vte_for_alt.clone();
                                    let block_id = finished_clone.id;

                                    let right_click = gtk4::GestureClick::new();
                                    right_click.set_button(3);

                                    let finished_menu_clone = finished_clone.clone();
                                    let block_data_for_export = block_data_for_cb.clone();
                                    right_click.connect_pressed(move |gesture, _n_press, x, y| {
                                        gesture.set_state(gtk4::EventSequenceState::Claimed);

                                        let menu = gtk4::gio::Menu::new();
                                        menu.append(Some("Copy Block"), Some("block-ctx.copy"));

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

                                    {
                                        let active_for_click = active_rc.clone();
                                        let left_click = gtk4::GestureClick::new();
                                        left_click.set_button(1);
                                        left_click.set_propagation_phase(gtk4::PropagationPhase::Capture);
                                        left_click.connect_pressed(move |gesture, _, _, _| {
                                            active_for_click.borrow().grab_focus();
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

                                    active_rc.borrow().reset_for_next_prompt();

                                    executing_cmd_raw_rc.borrow_mut().clear();
                                    executing_cmd_markup_rc.borrow_mut().clear();
                                    last_nonempty_cmd_raw_rc.borrow_mut().clear();
                                    last_nonempty_cmd_markup_rc.borrow_mut().clear();

                                    scroll_debouncer.reset_scroll_lock();
                                }
                                bstate_rc.set(BlockState::CollectingPrompt);
                                prompt_buf_rc.borrow_mut().clear();
                                // Auto-scroll to bottom when new prompt starts
                                scroll_debouncer.mark_dirty(&block_scroll_rc);
                            }

                            ParserEvent::PromptEnd => {
                                if bstate_rc.get() != BlockState::CollectingPrompt {
                                    continue;
                                }
                                bstate_rc.set(BlockState::AwaitingCommand);
                                cmd_buf_rc.borrow_mut().clear();
                                cmd_display_raw_rc.borrow_mut().clear();
                                cmd_display_markup_rc.borrow_mut().clear();
                                active_rc.borrow().set_cmd("");
                                pty_synced_rc.set(false);
                                tab_pending_rc.set(false);
                                if completion_active_rc.get() || isearch_active_rc.get() {
                                    active_rc.borrow().output_vte.set_visible(false);
                                    active_rc.borrow().reset_output_buffer();
                                    completion_active_rc.set(false);
                                    isearch_active_rc.set(false);
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
                                let state = bstate_rc.get();
                                if state == BlockState::CollectingOutput || state == BlockState::AltScreen {
                                    osc133_depth_rc.set(osc133_depth_rc.get() + 1);
                                    continue;
                                }
                                if state != BlockState::AwaitingCommand {
                                    continue;
                                }
                                osc133_depth_rc.set(0);
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
                                let state = bstate_rc.get();
                                if state != BlockState::CollectingOutput && state != BlockState::AltScreen {
                                    continue;
                                }
                                if osc133_depth_rc.get() > 0 {
                                    osc133_depth_rc.set(osc133_depth_rc.get() - 1);
                                    continue;
                                }
                                active_rc.borrow().stop_timer();
                                if state == BlockState::AltScreen || vte_box_rc.is_visible() {
                                    record_pager_snapshot(&vte_for_alt, &pager_snapshots_rc);
                                    hide_alt_screen(&block_scroll_rc, &vte_box_rc);
                                    let active_for_idle = active_rc.clone();
                                    glib::idle_add_local_once(move || {
                                        active_for_idle.borrow().grab_focus();
                                    });
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

                                // Save exit code and transition to PostCommand state.
                                // Block finalization is deferred until PromptStart arrives,
                                // allowing any late-arriving output bytes to be captured.
                                pending_exit_code_rc.set(*code);
                                bstate_rc.set(BlockState::PostCommand);
                                scroll_debouncer.mark_dirty(&block_scroll_rc);
                            }

                            ParserEvent::CwdUpdate(path) => {
                                *current_cwd_for_cb.borrow_mut() = path.clone();
                                active_rc.borrow().update_cwd(path);
                                for cb in cwd_cbs.borrow().iter() {
                                    cb(path);
                                }
                            }

                            ParserEvent::AltScreenEnter => {
                                if bstate_rc.get() != BlockState::CollectingOutput {
                                    continue;
                                }
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
                                    None,
                                );
                            }

                            ParserEvent::AltScreenLeave => {
                                if bstate_rc.get() != BlockState::AltScreen {
                                    continue;
                                }
                                record_pager_snapshot(&vte_for_alt, &pager_snapshots_rc);
                                hide_alt_screen(&block_scroll_rc, &vte_box_rc);
                                bstate_rc.set(BlockState::CollectingOutput);
                                let active_for_idle = active_rc.clone();
                                glib::idle_add_local_once(move || {
                                    active_for_idle.borrow().grab_focus();
                                });
                            }

                            ParserEvent::ClipboardSet(text) => {
                                if let Some(display) = gtk4::gdk::Display::default() {
                                    let clipboard = display.clipboard();
                                    clipboard.set_text(text);
                                    log::info!("OSC 52: clipboard set ({} chars)", text.len());
                                }
                            }

                            ParserEvent::ApcSequence(payload) => {
                                let is_kitty = payload.first() == Some(&b'G');
                                if is_kitty {
                                    log::info!("Kitty graphics protocol detected ({} bytes)", payload.len());
                                    if bstate_rc.get() == BlockState::AltScreen {
                                        let mut seq = Vec::with_capacity(payload.len() + 4);
                                        seq.push(0x1b);
                                        seq.push(b'_');
                                        seq.extend_from_slice(payload);
                                        seq.push(0x1b);
                                        seq.push(b'\\');
                                        vte_for_alt.feed(&seq);
                                    }
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
            // Editor mode: attach a focus controller on root so the IM context
            // is re-activated whenever focus returns to the terminal area (e.g.
            // after exiting an alt-screen pager when the sidebar is open).
            im_context.focus_in();
            let focus_ctrl = gtk4::EventControllerFocus::new();
            let im_for_root_focus_in = im_context.clone();
            focus_ctrl.connect_enter(move |_| {
                im_for_root_focus_in.focus_in();
            });
            root.add_controller(focus_ctrl);
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
            let isearch_active_for_key = isearch_active.clone();
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
                if editor_input_enabled && bstate_for_key.get() == BlockState::AwaitingCommand && !completion_active_for_key.get() && !isearch_active_for_key.get() {
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

                    // Ctrl+Shift+C: copy selected text (PRIMARY) or pending_cmd
                    if ctrl && shift && (keyval == gtk4::gdk::Key::c || keyval == gtk4::gdk::Key::C) {
                        let display = root_for_key.display();
                        let primary = display.primary_clipboard();
                        let clipboard = display.clipboard();
                        let active_for_copy = active_for_key.clone();
                        primary.read_text_async(None::<&gtk4::gio::Cancellable>, move |result| {
                            let copied_from_primary = match result {
                                Ok(Some(ref text)) if !text.is_empty() => {
                                    clipboard.set_text(text);
                                    true
                                }
                                _ => false,
                            };
                            if !copied_from_primary {
                                let active = active_for_copy.borrow();
                                let cmd = active.pending_cmd.borrow().clone();
                                if !cmd.is_empty() {
                                    clipboard.set_text(&cmd);
                                }
                            }
                        });
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

                    // Ctrl+D: send EOF when command is empty (closes shell)
                    if ctrl && (keyval == gtk4::gdk::Key::d || keyval == gtk4::gdk::Key::D) {
                        let active = active_for_key.borrow();
                        if active.pending_cmd.borrow().is_empty() {
                            pty_for_key.write_bytes(b"\x04");
                        }
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

                    // Ctrl+R: reverse incremental history search. Forward to the
                    // shell; its echoed search prompt is detected by the PTY
                    // reader, which routes the search UI to the output VTE.
                    if ctrl && !shift && !alt && (keyval == gtk4::gdk::Key::r || keyval == gtk4::gdk::Key::R) {
                        pty_for_key.write_bytes(b"\x12");
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

                    // Let bare modifier keys propagate for input method switching (e.g. Shift toggles Chinese/English in fcitx5/ibus)
                    match keyval {
                        v if v == gtk4::gdk::Key::Shift_L
                            || v == gtk4::gdk::Key::Shift_R
                            || v == gtk4::gdk::Key::Control_L
                            || v == gtk4::gdk::Key::Control_R
                            || v == gtk4::gdk::Key::Alt_L
                            || v == gtk4::gdk::Key::Alt_R
                            || v == gtk4::gdk::Key::Super_L
                            || v == gtk4::gdk::Key::Super_R
                            || v == gtk4::gdk::Key::Meta_L
                            || v == gtk4::gdk::Key::Meta_R => {
                            return glib::Propagation::Proceed;
                        }
                        _ => {}
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
            selected_block_id,
            current_cwd: current_cwd.clone(),
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

        // Give initial focus to ActiveBlock's TextView for cursor blinking
        term_view.active.borrow().command_view.grab_focus();

        term_view
    }

    fn install_resize_tick(&self) {
            let pty_for_resize = self.pty.clone();
            let active_for_resize = self.active.clone();
            let vte_for_resize = self.vte.clone();
            let vte_box_for_resize = self.vte_box.clone();
            let last_cols: Rc<Cell<u16>> = Rc::new(Cell::new(0));
            let last_rows: Rc<Cell<u16>> = Rc::new(Cell::new(0));
            let last_alloc_w: Rc<Cell<i32>> = Rc::new(Cell::new(0));
            let last_alloc_h: Rc<Cell<i32>> = Rc::new(Cell::new(0));
            let last_vte_visible: Rc<Cell<bool>> = Rc::new(Cell::new(false));

            self.root.add_tick_callback(move |widget, _clock| {
                let width = widget.allocated_width();
                let height = widget.allocated_height();
                if width <= 0 || height <= 0 {
                    return glib::ControlFlow::Continue;
                }
                let vte_visible = vte_box_for_resize.is_visible();
                let visibility_changed = vte_visible != last_vte_visible.get();
                let alloc_changed =
                    width != last_alloc_w.get() || height != last_alloc_h.get();
                // While the alt screen is shown we must re-check VTE's grid every
                // frame. Entering the alt screen does not change the root
                // allocation, and the VTE's row/column counts only settle a frame
                // or two after it becomes visible (tick callbacks run in the frame
                // clock's UPDATE phase, before the LAYOUT phase that allocates the
                // just-shown VTE). Without this poll the child keeps the stale
                // pre-alt-screen row count and leaves blank space at the bottom of
                // the window. In block mode we still only react to allocation
                // changes.
                if !vte_visible && !alloc_changed && !visibility_changed {
                    return glib::ControlFlow::Continue;
                }
                last_vte_visible.set(vte_visible);
                last_alloc_w.set(width);
                last_alloc_h.set(height);
                let (cols, rows) = if vte_visible {
                    // Use VTE's OWN grid as the source of truth, not pixel math.
                    // See alt_screen_pty_size for why pixel-derived counts corrupt
                    // box-drawing characters on sidebar toggle.
                    match alt_screen_pty_size(
                        vte_for_resize.column_count(),
                        vte_for_resize.row_count(),
                    ) {
                        Some(size) => size,
                        None => return glib::ControlFlow::Continue,
                    }
                } else {
                    let Ok(active) = active_for_resize.try_borrow() else {
                        return glib::ControlFlow::Continue;
                    };
                    let char_w = active.output_vte.char_width();
                    if char_w <= 0 {
                        return glib::ControlFlow::Continue;
                    }
                    // The live output VTE is hidden while idle, so its
                    // allocated_width() is 0 — using it here clamped the PTY to
                    // the 40-column floor, which made programs like `ls` collapse
                    // their multi-column layout to one entry per line. Derive the
                    // width from the always-visible command line instead: it is a
                    // sibling of the output area and spans the same content width.
                    // Subtract the TextView text margins (left 12 + right 8) so
                    // the column count matches what actually fits without wrapping.
                    let view_w = active.command_view.allocated_width() as i64;
                    let widget_w = view_w - 20;
                    if widget_w <= 0 {
                        return glib::ControlFlow::Continue;
                    }
                    let c = (widget_w / char_w).max(40) as u16;
                    let char_h = active.output_vte.char_height();
                    let r = if char_h > 0 {
                        (height as i64 / char_h).max(1) as u16
                    } else {
                        24
                    };
                    (c, r)
                };
                if cols != last_cols.get() || rows != last_rows.get() {
                    last_cols.set(cols);
                    last_rows.set(rows);
                    pty_for_resize.resize(cols, rows);
                    if vte_box_for_resize.is_visible() {
                        // Do NOT call vte.set_size() here. The VTE widget already
                        // re-derives its grid (column_count/row_count) from its own
                        // allocation on every size_allocate. Forcing a pixel-computed
                        // size on top of that fights VTE's auto-sizing: the values
                        // disagree by the integer-division remainder / scrollbar /
                        // padding, so the grid VTE draws into no longer matches the
                        // size the child (e.g. Claude Code) redraws for after the
                        // SIGWINCH from pty.resize — producing the overlapping
                        // "historical frame" artifact when the sidebar toggles.
                        // Resizing only the PTY mirrors the known-good initial path
                        // in show_alt_screen and keeps everything consistent.
                    } else if let Ok(active) = active_for_resize.try_borrow() {
                        let current_rows = active.output_vte.row_count();
                        active.output_vte.set_size(cols as i64, current_rows.max(rows as i64));
                    }
                }
                glib::ControlFlow::Continue
            });
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
        let active = self.active.borrow();
        let current_rows = active.output_vte.row_count();
        let new_rows = current_rows.max(rows as i64);
        active.output_vte.set_size(cols as i64, new_rows);
    }

    /// Kill the child process.
    pub fn kill(&self) {
        self.active.borrow().cancel_blink_timer();
        self.pty.kill();
    }

    pub fn pid_i32(&self) -> i32 {
        self.pty.pid_i32()
    }

    pub fn vte(&self) -> &Terminal {
        &self.vte
    }

    pub fn cwd(&self) -> String {
        self.current_cwd.borrow().clone()
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
        use std::io::Write;

        let path_opt = self.config.borrow().block_history_path.as_ref().cloned();
        if path_opt.is_none() {
            return Ok(());
        }

        let path = path_opt.unwrap();
        let blocks = self.block_data.borrow();

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        let compress = self.config.borrow().block_history_compress;

        for block in blocks.iter() {
            let serialized = rkyv::to_bytes::<_, 256>(block)
                .map_err(|e| std::io::Error::other(e.to_string()))?;

            if compress {
                let compressed = zstd::encode_all(serialized.as_slice(), 3)
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                file.write_all(&(compressed.len() as u32).to_le_bytes())?;
                file.write_all(&compressed)?;
            } else {
                file.write_all(&(serialized.len() as u32).to_le_bytes())?;
                file.write_all(&serialized)?;
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
