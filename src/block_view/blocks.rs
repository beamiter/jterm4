//! blocks — finished-block widgets (VTE-backed) and the live ActiveBlock.
use super::*;
use crate::config::Config;
use crate::terminal::open_uri;
use gtk4::Orientation;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::Terminal;
use vte4::TerminalExt;

// ─── FinishedBlock ────────────────────────────────────────────────────────────

/// Data for a finished command block (decoupled from widget representation)
#[derive(Clone, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub(crate) struct BlockData {
    pub(crate) id: u64,
    pub(crate) prompt: String,
    pub(crate) cmd: String,
    pub(crate) cmd_markup: Option<String>,
    pub(crate) output: String,
    pub(crate) exit_code: i32,
    pub(crate) estimated_height: i32,
    pub(crate) line_count: usize,
    #[serde(default)]
    pub(crate) start_time_ms: Option<u64>,
    #[serde(default)]
    pub(crate) end_time_ms: Option<u64>,
    #[serde(default)]
    pub(crate) duration_ms: Option<u64>,
    #[serde(default)]
    pub(crate) cwd: Option<String>,
    /// Live-VTE column count at the time this block was finalized. Restored
    /// blocks render at the same cols so their byte stream (which was formatted
    /// for this width, e.g. by `ls`) reproduces the original line breaks
    /// instead of being reflowed at the current window's width. 0 = unknown
    /// (old saves before this field existed) — caller should fall back.
    #[serde(default)]
    pub(crate) cols: u16,
}

impl BlockData {
    pub(crate) fn is_background(&self) -> bool {
        self.cmd.trim().is_empty()
    }

    /// Export block to JSON format
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Export block to Markdown format
    pub fn to_markdown(&self) -> String {
        let mut md = String::new();

        if self.is_background() {
            md.push_str("## Background Output\n\n");
        } else {
            md.push_str("## Command Block\n\n");

            if !self.prompt.is_empty() {
                md.push_str(&format!("**Prompt:** `{}`\n\n", self.prompt));
            }

            md.push_str("**Command:**\n```bash\n");
            md.push_str(&self.cmd);
            md.push_str("\n```\n\n");
        }

        if !self.output.is_empty() {
            md.push_str("**Output:**\n```\n");
            md.push_str(&self.output);
            md.push_str("\n```\n\n");
        }

        if !self.is_background() {
            md.push_str(&format!("**Exit Code:** {}\n\n", self.exit_code));
        }

        if let Some(dur) = self.duration_ms {
            let dur_sec = dur as f64 / 1000.0;
            md.push_str(&format!("**Duration:** {:.3}s\n\n", dur_sec));
        }

        md
    }
}

pub(crate) fn block_clipboard_text(cmd: &str, output: &str, output_only: bool) -> String {
    if output_only || cmd.trim().is_empty() {
        output.to_string()
    } else if output.trim().is_empty() {
        cmd.to_string()
    } else {
        format!("{}\n{}", cmd, output)
    }
}

/// Filters for searching/filtering blocks
#[derive(Clone, Default)]
pub struct BlockFilters {
    pub exit_code: Option<i32>,
    pub min_duration_ms: Option<u64>,
    pub max_duration_ms: Option<u64>,
    pub failed_only: bool,
    pub slow_only: bool,
    pub slow_threshold_ms: u64,
    pub use_regex: bool,
}

pub(crate) struct FinishedBlock {
    pub(crate) id: u64,
    /// Commandless output emitted while the shell prompt was idle.
    pub(crate) is_background: bool,
    pub(crate) widget: gtk4::Box,
    /// Inner card content. Virtualization hides this child while the outer box
    /// retains a measured placeholder height, keeping one stable history canvas.
    content: gtk4::Box,
    virtualized_height: Rc<Cell<i32>>,
    virtualized: Rc<Cell<bool>>,
    pub(crate) prompt_text: String,
    /// Read-only VTE displaying the executed command line (single-row typically).
    pub(crate) command_vte: vte4::Terminal,
    /// Read-only VTE displaying captured output. Normal long blocks expand into
    /// the outer history; only exceptionally large snapshots retain private VTE
    /// scrollback until explicitly expanded.
    pub(crate) output_vte: vte4::Terminal,
    /// Raw ANSI-bearing output bytes — the source for filter re-feed and the
    /// copy-output action. Mutable so filter can swap the displayed slice
    /// without losing the original.
    pub(crate) full_output: Rc<RefCell<String>>,
    /// The currently displayed output. Usually identical to `full_output`, but
    /// filters can narrow it. Running blocks append to both so remap re-feeds
    /// the bytes already shown instead of waiting for a final snapshot.
    pub(crate) displayed_output: Rc<RefCell<String>>,
    /// Lazy-populated ANSI-stripped view of `full_output`, used as the haystack
    /// for find-within-blocks. Avoids re-stripping on every keystroke. Cleared
    /// when `full_output` is rewritten by a filter action; otherwise kept for
    /// the lifetime of the block (finished blocks are append-once in practice).
    pub(crate) stripped_output: Rc<RefCell<Option<String>>>,
    pub(crate) cmd_text: String,
    pub(crate) copy_cmd_btn: gtk4::Button,
    pub(crate) copy_output_btn: gtk4::Button,
    pub(crate) rerun_btn: gtk4::Button,
    pub(crate) header_row: gtk4::Box,
    pub(crate) action_box: gtk4::Box,
    /// Keyboard affordances shown only while this block is selected.
    pub(crate) selection_hint: gtk4::Label,
    /// Toggle the output filter while preserving the current query.
    pub(crate) toggle_filter: Rc<dyn Fn()>,
    /// Warp-style jump affordance for oversized output.
    pub(crate) jump_bottom_btn: gtk4::Button,
    pub(crate) bookmark_star: gtk4::Label,
    pub(crate) status_icon: gtk4::Label,
    /// Column count the output VTE is sized to — needed for re-feed (filter).
    pub(crate) cols: i64,
    /// Visible rows allocated to this full-height finished block.
    pub(crate) viewport_cap: i64,
    /// Whether this block exceeds the configured long-output threshold.
    pub(crate) long_output: bool,
}

impl Clone for FinishedBlock {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            is_background: self.is_background,
            widget: self.widget.clone(),
            content: self.content.clone(),
            virtualized_height: self.virtualized_height.clone(),
            virtualized: self.virtualized.clone(),
            prompt_text: self.prompt_text.clone(),
            command_vte: self.command_vte.clone(),
            output_vte: self.output_vte.clone(),
            cmd_text: self.cmd_text.clone(),
            full_output: self.full_output.clone(),
            displayed_output: self.displayed_output.clone(),
            stripped_output: self.stripped_output.clone(),
            copy_cmd_btn: self.copy_cmd_btn.clone(),
            copy_output_btn: self.copy_output_btn.clone(),
            rerun_btn: self.rerun_btn.clone(),
            header_row: self.header_row.clone(),
            action_box: self.action_box.clone(),
            selection_hint: self.selection_hint.clone(),
            toggle_filter: self.toggle_filter.clone(),
            jump_bottom_btn: self.jump_bottom_btn.clone(),
            bookmark_star: self.bookmark_star.clone(),
            status_icon: self.status_icon.clone(),
            cols: self.cols,
            viewport_cap: self.viewport_cap,
            long_output: self.long_output,
        }
    }
}

/// Lightweight shell-command syntax highlighter (Warp-style). Emits an ANSI
/// (SGR) string so it can flow through the same `set_active_output_buffer`
/// rendering path as real shell output. Best-effort, dependency-free:
///   - command name (first word, and first word after a pipe/operator): bold cyan
///   - flags (`-x`, `--long`): dim/gray
///   - quoted strings: green
///   - operators (`| & ; > <`): magenta
///   - `$VAR` references: cyan
///
/// Whitespace and all other text are emitted verbatim in the default color, so
/// the reconstructed buffer text matches the command exactly.
pub(crate) fn highlight_command_to_ansi(cmd: &str) -> String {
    const RESET: &str = "\x1b[0m";
    let chars: Vec<char> = cmd.chars().collect();
    let mut out = String::with_capacity(cmd.len() + 32);
    let mut i = 0;
    let mut expect_command = true;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            out.push(c);
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            let quote = c;
            let start = i;
            i += 1;
            while i < chars.len() {
                if quote == '"' && chars[i] == '\\' && i + 1 < chars.len() {
                    i += 2;
                    continue;
                }
                let done = chars[i] == quote;
                i += 1;
                if done {
                    break;
                }
            }
            out.push_str("\x1b[32m");
            out.extend(chars[start..i].iter());
            out.push_str(RESET);
            expect_command = false;
            continue;
        }
        if matches!(c, '|' | '&' | ';' | '>' | '<') {
            let start = i;
            while i < chars.len() && matches!(chars[i], '|' | '&' | ';' | '>' | '<') {
                i += 1;
            }
            out.push_str("\x1b[35m");
            out.extend(chars[start..i].iter());
            out.push_str(RESET);
            expect_command = true;
            continue;
        }
        let start = i;
        while i < chars.len() {
            let cc = chars[i];
            if cc.is_whitespace() || matches!(cc, '|' | '&' | ';' | '>' | '<' | '"' | '\'') {
                break;
            }
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        if word.starts_with('-') {
            out.push_str("\x1b[90m");
            out.push_str(&word);
            out.push_str(RESET);
        } else if word.starts_with('$') {
            out.push_str("\x1b[36m");
            out.push_str(&word);
            out.push_str(RESET);
        } else if expect_command {
            out.push_str("\x1b[1;36m");
            out.push_str(&word);
            out.push_str(RESET);
            expect_command = false;
        } else {
            out.push_str(&word);
        }
    }
    out
}

/// Filter raw output (ANSI preserved) to the lines matching `query`, honoring
/// regex / case / invert and `context` lines of surroundings (Warp's
/// BlockFilterQuery). Empty query, or an invalid regex, returns `full` verbatim.
fn filter_output_lines(
    full: &str,
    query: &str,
    use_regex: bool,
    case_sensitive: bool,
    invert: bool,
    context: usize,
) -> Result<String, regex::Error> {
    if query.is_empty() {
        return Ok(full.to_string());
    }
    let re = if use_regex {
        Some(
            regex::RegexBuilder::new(query)
                .case_insensitive(!case_sensitive)
                .build()?,
        )
    } else {
        None
    };
    let ascii_query = (!case_sensitive && query.is_ascii()).then(|| {
        query
            .as_bytes()
            .iter()
            .map(|b| b.to_ascii_lowercase())
            .collect::<Vec<_>>()
    });
    let lc_query = if case_sensitive || ascii_query.is_some() {
        String::new()
    } else {
        query.to_lowercase()
    };
    let lines: Vec<&str> = full.lines().collect();
    let matches_line = |line: &str| -> bool {
        let hit = if let Some(ref re) = re {
            re.is_match(line)
        } else if case_sensitive {
            line.contains(query)
        } else if let Some(ref q) = ascii_query {
            contains_case_insensitive(line.as_bytes(), q)
        } else {
            line.to_lowercase().contains(&lc_query)
        };
        hit ^ invert
    };
    let mut keep = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        if matches_line(line) {
            let lo = i.saturating_sub(context);
            let hi = (i + context + 1).min(lines.len());
            for slot in keep.iter_mut().take(hi).skip(lo) {
                *slot = true;
            }
        }
    }
    let mut out = String::new();
    for (line, keep) in lines.iter().zip(keep.iter()) {
        if !*keep {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    Ok(out)
}

fn output_row_count(text: &str) -> i64 {
    let text = output_display_text(text);
    if text.is_empty() {
        1
    } else {
        let trailing_blank_row =
            text.ends_with('\n') || (text.ends_with('\r') && !text.ends_with("\r\n"));
        let rows = text.lines().count().max(1) as i64;
        if trailing_blank_row {
            rows + 1
        } else {
            rows
        }
    }
}

/// Rows occupied after VTE wraps the snapshot at `cols`. Finished cards need
/// this rather than the logical line count, otherwise long stack-trace lines
/// are still pushed into the VTE's private scrollback.
fn output_visual_row_count(text: &str, cols: i64) -> i64 {
    use unicode_width::UnicodeWidthChar;

    let cols = cols.max(1) as usize;
    // Count what the terminal leaves on screen, not the byte stream used to
    // produce it. Programs such as apt repeatedly repaint a progress row with
    // CR + EL and wrap ordinary text in SGR/OSC sequences. Counting those
    // control bytes (and every overwritten progress update) can turn a short
    // result into a false "long output" block. Long blocks are fitted to the
    // pane height, so that misclassification shows up as a large blank tail.
    // `strip_ansi` applies the horizontal cursor/erase semantics as well as
    // removing escape sequences, which makes this estimate match the VTE
    // snapshot closely enough for the short/long decision.
    let rendered = strip_ansi(text);
    let text = output_display_text(&rendered);
    if text.is_empty() {
        return 1;
    }

    text.split('\n')
        .map(|line| {
            let mut width = 0usize;
            for ch in line.trim_end_matches('\r').chars() {
                width += match ch {
                    '\t' => 8 - (width % 8),
                    _ => UnicodeWidthChar::width(ch).unwrap_or(0),
                };
            }
            width.max(1).div_ceil(cols) as i64
        })
        .sum::<i64>()
        .max(1)
}

fn output_display_text(text: &str) -> &str {
    let text = if let Some(stripped) = text.strip_prefix("\r\n") {
        stripped
    } else if let Some(stripped) = text.strip_prefix('\n') {
        stripped
    } else if let Some(stripped) = text.strip_prefix('\r') {
        stripped
    } else {
        text
    };

    if let Some(stripped) = text.strip_suffix("\r\n") {
        stripped
    } else if let Some(stripped) = text.strip_suffix('\n') {
        stripped
    } else if let Some(stripped) = text.strip_suffix('\r') {
        stripped
    } else {
        text
    }
}

fn line_count_text(rows: i64) -> String {
    if rows == 1 {
        "1 line".to_string()
    } else {
        format!("{rows} lines")
    }
}

/// Copy for the compact placeholder shown when a block's output is folded.
/// Keeping this as a small pure helper makes the collapsed state useful even
/// after a per-block filter changes the number of displayed rows.
fn collapsed_output_summary(rows: i64) -> String {
    format!("▸ {} hidden — click to show", line_count_text(rows))
}

/// Rows consumed by a finished block outside its output VTE: metadata header,
/// command row, and card chrome. Together with the compact live input rows this
/// leaves a long block filling the rest of the pane without growing the outer
/// document by hundreds of rows.
const FINISHED_BLOCK_NON_OUTPUT_ROWS: i64 = 3;

/// A single outer history is the normal Warp-style interaction. Keeping a hard
/// ceiling avoids constructing a multi-megapixel GTK/VTE widget for pathological
/// output; those blocks retain the existing inner viewport until expanded.
const MAX_AUTO_DOCUMENT_OUTPUT_ROWS: i64 = 4096;

fn uses_outer_document_scroll(output_rows: i64) -> bool {
    output_rows.max(1) <= MAX_AUTO_DOCUMENT_OUTPUT_ROWS
}

fn finished_output_cap(output_rows: i64, fitted_cap: i64, manually_expanded: bool) -> i64 {
    let output_rows = output_rows.max(1);
    if manually_expanded || uses_outer_document_scroll(output_rows) {
        output_rows
    } else {
        fitted_cap.max(1).min(output_rows)
    }
}

fn fitted_output_rows_for_viewport(
    viewport_rows: Option<i64>,
    fallback_rows: i64,
    output_rows: i64,
) -> i64 {
    let output_rows = output_rows.max(1);
    let reserve = super::MIN_INPUT_ROWS as i64 + FINISHED_BLOCK_NON_OUTPUT_ROWS;
    viewport_rows
        .map(|rows| rows.saturating_sub(reserve))
        .unwrap_or(fallback_rows)
        .max(3)
        .min(output_rows)
}

fn fitted_output_rows_for_widget(
    vte: &vte4::Terminal,
    fallback_rows: i64,
    output_rows: i64,
) -> i64 {
    let viewport_rows = vte
        .ancestor(gtk4::ScrolledWindow::static_type())
        .and_then(|widget| widget.downcast::<gtk4::ScrolledWindow>().ok())
        .and_then(|scroll| super::viewport_rows_for(vte, &scroll));
    fitted_output_rows_for_viewport(viewport_rows, fallback_rows, output_rows)
}

fn block_edge_scroll_target(
    current: f64,
    relative_top: f64,
    block_height: f64,
    page_size: f64,
    lower: f64,
    upper: f64,
    bottom: bool,
) -> f64 {
    let max_value = (upper - page_size).max(lower);
    let absolute_top = current + relative_top;
    let target = if bottom {
        absolute_top + block_height - page_size
    } else {
        absolute_top
    };
    target.clamp(lower, max_value)
}

fn forward_outer_scroll(outer: &gtk4::ScrolledWindow, dy: f64) {
    let outer_adj = outer.vadjustment();
    let step = outer_adj.step_increment().max(outer_adj.page_size() * 0.1);
    let max_value = (outer_adj.upper() - outer_adj.page_size()).max(outer_adj.lower());
    let target = (outer_adj.value() + dy * step).clamp(outer_adj.lower(), max_value);
    outer_adj.set_value(target);
}

pub(crate) fn estimated_cell_height_px(config: &Config) -> i32 {
    let parts: Vec<&str> = config.font_desc.split_whitespace().collect();
    let base_size = parts
        .last()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(14.0);
    (base_size
        * config.default_font_scale
        * (96.0 / 72.0)
        * 1.2
        * super::alt_screen::BLOCK_CELL_HEIGHT_SCALE)
        .ceil()
        .max(1.0) as i32
}

pub(crate) fn estimated_finished_block_height(config: &Config, output_rows: i64) -> i32 {
    let cell = estimated_cell_height_px(config);
    let rows = output_rows.clamp(1, i32::MAX as i64) as i32;
    rows.saturating_add(2)
        .saturating_mul(cell)
        .saturating_add(34)
}

/// Virtualization metadata must follow terminal visual rows rather than logical
/// newlines. Wide glyphs and long stack-trace lines can wrap many times.
pub(crate) fn estimated_finished_block_height_for_text(
    config: &Config,
    output: &str,
    cols: i64,
) -> i32 {
    let rows = output_visual_row_count(output, cols).max(1);
    let fallback_cap = (config.finished_block_viewport_rows as i64).max(3);
    let document_rows = finished_output_cap(rows, fallback_cap, false);
    estimated_finished_block_height(config, document_rows)
}

fn flash_button_label(btn: &gtk4::Button, label: &'static str, tooltip: &'static str) {
    let old_label = btn.label().map(|s| s.to_string()).unwrap_or_default();
    let old_tooltip = btn.tooltip_text().map(|s| s.to_string());
    btn.set_label(label);
    btn.set_tooltip_text(Some(tooltip));
    let btn_for_restore = btn.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(900), move || {
        btn_for_restore.set_label(&old_label);
        btn_for_restore.set_tooltip_text(old_tooltip.as_deref());
    });
}

/// Render a finished snapshot with enough temporary capture capacity for VTE's
/// real terminal semantics. The post-feed settle pass expands short/full-height
/// blocks to the actual retained buffer span, covering ANSI cursor movement,
/// carriage-return redraws, combining/wide glyphs, tabs, and soft wrapping.
pub(crate) fn render_bytes_into_finished_vte(
    vte: &vte4::Terminal,
    text: &str,
    cols: i64,
    output_rows: i64,
    viewport_cap: i64,
    capture_rows: i64,
    expand_to_buffer: bool,
) {
    let display_text = output_display_text(text);
    // The pixel height request below is based on this same row count. Capping
    // the VTE grid at 32 while requesting a taller widget created the large
    // blank tail visible in long cards.
    let visible_rows = output_rows.min(viewport_cap).max(1);
    let overflow_rows = output_rows.saturating_sub(visible_rows).saturating_add(64);
    let scrollback = capture_rows.max(overflow_rows).max(64);
    vte.set_scroll_on_output(false);
    vte.set_size(cols.max(1), visible_rows);
    vte.set_scrollback_lines(scrollback);
    vte.reset(true, true);
    vte.set_size(cols.max(1), visible_rows);
    vte.set_scrollback_lines(scrollback);
    vte.feed(display_text.as_bytes());
    if expand_to_buffer {
        settle_finished_terminal_after_feed(vte);
    } else {
        // feed() settles asynchronously. Keep capped snapshots anchored at the
        // first retained row without invoking the full-height settle path.
        settle_finished_terminal_at_top(vte);
    }
    if let Some(adj) = vte.vadjustment() {
        adj.set_value(adj.lower());
    }
}

/// VTE treats a bare LF as “move down, retain column”. Captured command text
/// uses ordinary logical newlines, so convert only bare LF bytes to CRLF before
/// feeding the read-only command snapshot.
fn terminalize_line_breaks(bytes: &[u8]) -> Vec<u8> {
    let extra_crs = bytes
        .iter()
        .enumerate()
        .filter(|&(i, &b)| b == b'\n' && (i == 0 || bytes[i - 1] != b'\r'))
        .count();
    if extra_crs == 0 {
        return bytes.to_vec();
    }
    let mut terminal_bytes = Vec::with_capacity(bytes.len() + extra_crs);
    for (i, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
            terminal_bytes.push(b'\r');
        }
        terminal_bytes.push(byte);
    }
    terminal_bytes
}

impl FinishedBlock {
    fn with_cached_stripped_output<R>(
        full_output: &Rc<RefCell<String>>,
        stripped_output: &Rc<RefCell<Option<String>>>,
        f: impl FnOnce(&str) -> R,
    ) -> R {
        if stripped_output.borrow().is_none() {
            let s = strip_ansi(&full_output.borrow());
            *stripped_output.borrow_mut() = Some(s);
        }
        let guard = stripped_output.borrow();
        f(guard.as_deref().unwrap_or(""))
    }

    /// Returns the ANSI-stripped view of `full_output`, populating the cache on
    /// first call. Caller passes a closure to handle the cached string by ref to
    /// avoid an extra clone — `stripped_output` lives in a `RefCell` so we can't
    /// hand out a `Ref` that outlives the borrow.
    pub(crate) fn with_stripped_output<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        Self::with_cached_stripped_output(&self.full_output, &self.stripped_output, f)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: u64,
        prompt: &str,
        cmd: &str,
        cmd_ansi: Option<&str>,
        output: &str,
        exit_code: i32,
        config: &Config,
        duration_ms: Option<u64>,
        end_time_ms: Option<u64>,
        cwd: Option<&str>,
        cols: i64,
    ) -> Self {
        Self::new_with_pool(
            id,
            prompt,
            cmd,
            cmd_ansi,
            output,
            exit_code,
            config,
            duration_ms,
            end_time_ms,
            cwd,
            cols,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_pool(
        id: u64,
        prompt: &str,
        cmd: &str,
        cmd_ansi: Option<&str>,
        output: &str,
        exit_code: i32,
        config: &Config,
        duration_ms: Option<u64>,
        end_time_ms: Option<u64>,
        cwd: Option<&str>,
        cols: i64,
        recycled: Option<gtk4::Box>,
    ) -> Self {
        let is_background = cmd.trim().is_empty();
        let output_rows = output_visual_row_count(output, cols);
        let fallback_viewport_cap = (config.finished_block_viewport_rows as i64).max(3);
        let viewport_cap =
            fitted_output_rows_for_viewport(None, fallback_viewport_cap, output_rows);
        let current_viewport_cap = Rc::new(Cell::new(viewport_cap));
        let long_output = output_rows > viewport_cap;
        let virtualized_height = Rc::new(Cell::new(estimated_finished_block_height_for_text(
            config, output, cols,
        )));
        let virtualized = Rc::new(Cell::new(false));
        let capture_rows = output_rows
            .max(config.truncation_threshold_lines as i64)
            .max(4096);

        let outer = if let Some(reused) = recycled {
            while let Some(child) = reused.first_child() {
                reused.remove(&child);
            }
            reused.remove_css_class("block-hovered");
            reused.remove_css_class("block-selected");
            reused.remove_css_class("block-selection-active");
            reused.remove_css_class("block-bookmarked");
            reused.remove_css_class("block-success");
            reused.remove_css_class("block-failed");
            reused.remove_css_class("block-background");
            reused.remove_css_class("block-compact");
            reused
        } else {
            let b = gtk4::Box::new(Orientation::Vertical, 0);
            b.add_css_class("block-finished");
            b
        };
        // Pooled cards must not retain expansion flags from an earlier use.
        // The output VTE owns the explicit height; the card itself never absorbs
        // spare vertical space from the document box.
        outer.set_hexpand(true);
        outer.set_vexpand(false);
        if config.block_compact {
            outer.add_css_class("block-compact");
            outer.set_margin_top(1);
            outer.set_margin_bottom(1);
            outer.set_margin_start(4);
            outer.set_margin_end(4);
        } else {
            outer.remove_css_class("block-compact");
            outer.set_margin_top(4);
            outer.set_margin_bottom(4);
            outer.set_margin_start(8);
            outer.set_margin_end(8);
        }

        let content = gtk4::Box::new(Orientation::Vertical, 0);
        content.set_hexpand(true);
        content.set_vexpand(false);
        outer.append(&content);

        // Status stripe: green on success, red on failure, cyan for idle output.
        outer.add_css_class(if is_background {
            "block-background"
        } else if exit_code == 0 {
            "block-success"
        } else {
            "block-failed"
        });

        // Add hover highlighting to show block is interactive (and reveal the
        // quick-action buttons). The action box is created below; it's wired into
        // these handlers after construction.
        let hover_ctrl = gtk4::EventControllerMotion::new();

        // ── Header row ──────────────────────────────────────────────────────
        let header_row = gtk4::Box::new(Orientation::Horizontal, 8);
        header_row.add_css_class("block-header");
        header_row.set_tooltip_text(Some(if is_background {
            "Click to select · Shift-click range · Ctrl+Shift-click toggle"
        } else {
            "Click to select · Shift-click range · Ctrl+Shift-click toggle · Enter recalls"
        }));
        if config.block_compact {
            header_row.set_margin_start(8);
            header_row.set_margin_end(6);
            header_row.set_margin_top(3);
            header_row.set_margin_bottom(1);
        } else {
            header_row.set_margin_start(12);
            header_row.set_margin_end(8);
            header_row.set_margin_top(6);
            header_row.set_margin_bottom(2);
        }

        // Bookmark star (gutter marker), hidden until the block is bookmarked.
        let bookmark_star = gtk4::Label::new(Some("\u{f02e}")); // nf-fa-bookmark
        bookmark_star.add_css_class("block-bookmark-star");
        bookmark_star.set_halign(gtk4::Align::Start);
        bookmark_star.set_visible(false);
        header_row.append(&bookmark_star);

        // Status icon: success, failure, or asynchronous/background output.
        let status_icon = gtk4::Label::new(Some(if is_background {
            "\u{f110}"
        } else if exit_code == 0 {
            "\u{f00c}"
        } else {
            "\u{f00d}"
        }));
        status_icon.add_css_class(if is_background {
            "block-status-background"
        } else if exit_code == 0 {
            "block-status-ok"
        } else {
            "block-status-bad"
        });
        status_icon.set_halign(gtk4::Align::Start);
        header_row.append(&status_icon);
        if is_background {
            let chip = gtk4::Label::new(Some("Background output"));
            chip.add_css_class("block-background-chip");
            chip.set_halign(gtk4::Align::Start);
            header_row.append(&chip);
        }

        // Context chips (Warp-style): cwd pill + git-branch pill.
        if let Some(cwd_path) = cwd {
            let shortened = shorten_path(cwd_path);
            // nf-fa-folder () prefix
            let cwd_chip = gtk4::Label::new(Some(&format!("\u{f07b} {}", shortened)));
            cwd_chip.add_css_class("block-chip");
            cwd_chip.set_halign(gtk4::Align::Start);
            cwd_chip.set_ellipsize(gtk4::pango::EllipsizeMode::Start);
            cwd_chip.set_max_width_chars(40);
            header_row.append(&cwd_chip);

            // git-branch chip (nf-dev-git-branch )
            if let Some(branch) = git_branch_for(cwd_path) {
                let git_chip = gtk4::Label::new(Some(&format!("\u{e725} {}", branch)));
                git_chip.add_css_class("block-chip-git");
                git_chip.set_halign(gtk4::Align::Start);
                git_chip.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                git_chip.set_max_width_chars(28);
                header_row.append(&git_chip);
            }
        }

        // Spacer
        let spacer = gtk4::Box::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        header_row.append(&spacer);

        // Timestamp label
        if let Some(et_ms) = end_time_ms {
            let secs = et_ms / 1000;
            let local_offset = chrono_local_offset_secs();
            let local_secs = (secs as i64 + local_offset).rem_euclid(86400) as u64;
            let h = local_secs / 3600;
            let m = (local_secs % 3600) / 60;
            let sec = local_secs % 60;
            let ts_label = gtk4::Label::new(Some(&format!("{:02}:{:02}:{:02}", h, m, sec)));
            ts_label.add_css_class("block-header-label");
            header_row.append(&ts_label);
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
        if !is_background && exit_code != 0 {
            let badge = gtk4::Label::new(Some(&format!("exit:{}", exit_code)));
            badge.add_css_class("block-exit-bad");
            header_row.append(&badge);
        }

        // Selected blocks behave like a lightweight navigation mode. Keep the
        // available keyboard actions visible instead of making users memorize them.
        let selection_hint = gtk4::Label::new(Some(
            "↵ recall  ·  Ctrl+↵ run  ·  Del remove  ·  Esc cancel",
        ));
        selection_hint.add_css_class("block-selection-hint");
        selection_hint.set_visible(false);
        selection_hint.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        selection_hint.set_max_width_chars(38);
        header_row.append(&selection_hint);

        // Quick-action buttons (hidden until the block is hovered). Handlers are
        // wired by the caller, which has access to the clipboard + active block.
        let action_box = gtk4::Box::new(Orientation::Horizontal, 2);
        action_box.set_visible(false);
        // Small gap between the meta badges (timestamp/duration/exit) on the
        // right and the action button group, so they read as separate units
        // rather than one undifferentiated cluster.
        action_box.set_margin_start(6);
        let copy_cmd_btn = gtk4::Button::with_label("\u{f0c5}"); // nf-fa-copy  copy command
        copy_cmd_btn.set_tooltip_text(Some("Copy command"));
        let copy_output_btn = gtk4::Button::with_label("\u{f0ea}"); // nf-fa-clipboard  copy output
        copy_output_btn.set_tooltip_text(Some("Copy output"));
        let rerun_btn = gtk4::Button::with_label("\u{f021}"); // nf-fa-refresh  re-run
        rerun_btn.set_tooltip_text(Some("Insert command at prompt"));
        copy_cmd_btn.set_visible(!is_background);
        rerun_btn.set_visible(!is_background);
        let filter_btn = gtk4::Button::with_label("\u{f0b0}"); // nf-fa-filter  filter output
        filter_btn.set_tooltip_text(Some("Filter output"));
        let jump_bottom_btn = gtk4::Button::with_label("\u{f103}");
        jump_bottom_btn.set_tooltip_text(Some("Jump to bottom of this block"));
        jump_bottom_btn.set_visible(long_output);
        // Expand button: kept for the capped-height path. Full-height finished
        // blocks hide it because their viewport already contains every row.
        let expand_btn = gtk4::Button::with_label("\u{f065}"); // nf-fa-expand
        expand_btn.set_tooltip_text(Some("Expand block"));
        for btn in [
            &copy_cmd_btn,
            &copy_output_btn,
            &rerun_btn,
            &filter_btn,
            &jump_bottom_btn,
            &expand_btn,
        ] {
            btn.add_css_class("block-action-btn");
            btn.add_css_class("flat");
            action_box.append(btn);
        }
        header_row.append(&action_box);

        let outer_for_enter = outer.clone();
        let action_box_for_enter = action_box.clone();
        hover_ctrl.connect_enter(move |_, _, _| {
            outer_for_enter.add_css_class("block-hovered");
            action_box_for_enter.set_visible(true);
        });
        let outer_for_leave = outer.clone();
        let action_box_for_leave = action_box.clone();
        hover_ctrl.connect_leave(move |_| {
            outer_for_leave.remove_css_class("block-hovered");
            // Only the active edge of a multi-selection owns persistent actions.
            if !outer_for_leave.has_css_class("block-selection-active") {
                action_box_for_leave.set_visible(false);
            }
        });
        outer.add_controller(hover_ctrl);

        // Collapse toggle button
        let collapse_btn = gtk4::Button::with_label("\u{f078}"); // nf-fa-chevron_down
        collapse_btn.add_css_class("block-collapse-btn");
        collapse_btn.add_css_class("flat");
        header_row.append(&collapse_btn);

        content.append(&header_row);

        // ── VTE-rendered command + output ─────────────────────────────────
        // Command VTE: full-height read-only renderer for the executed command.
        let cmd_bytes: Vec<u8> = match cmd_ansi {
            Some(ansi) if !ansi.is_empty() && !cmd.is_empty() => ansi.as_bytes().to_vec(),
            _ if cmd.is_empty() => b"(empty)".to_vec(),
            _ => highlight_command_to_ansi(cmd).into_bytes(),
        };
        let cmd_bytes = terminalize_line_breaks(&cmd_bytes);
        let cmd_rows = cmd_bytes.iter().filter(|&&b| b == b'\n').count() as i64 + 1;
        let command_vte =
            create_finished_terminal(config, cols, cmd_rows.max(1), cmd_rows.max(1), false);
        // Defer feeds until the widget is actually mapped — VTE's internal
        // grid resize from set_size() doesn't take effect until the widget is
        // realized, so feeding immediately wraps content at a smaller default
        // width (the ls-output misalignment bug). connect_map fires once the
        // widget has been allocated, when the grid actually matches set_size.
        // One-shot: re-mapping during scroll must not re-feed.
        {
            let cmd_bytes_for_map = cmd_bytes.clone();
            let cols_for_map = cols.max(1);
            let cmd_rows_for_map = cmd_rows.max(1);
            let fed = Cell::new(false);
            command_vte.connect_map(move |w| {
                if fed.get() {
                    return;
                }
                fed.set(true);
                w.set_size(cols_for_map, cmd_rows_for_map);
                w.feed(&cmd_bytes_for_map);
                settle_finished_terminal_after_feed(w);
                let ch = w.char_height() as i32;
                if ch > 0 {
                    w.set_height_request(cmd_rows_for_map as i32 * ch);
                }
            });
        }

        // Normal long output expands into the outer block document, matching
        // Warp's single history. Only exceptionally large snapshots receive a
        // bounded inner viewport; wheel events then forward at its edges.
        let full_output: Rc<RefCell<String>> = Rc::new(RefCell::new(output.to_string()));
        let displayed_output: Rc<RefCell<String>> = Rc::new(RefCell::new(output.to_string()));
        let output_vte = create_finished_terminal(config, cols, output_rows, viewport_cap, false);
        let initial_visible_rows = output_rows.min(viewport_cap).max(1);
        output_vte
            .set_height_request(initial_visible_rows as i32 * estimated_cell_height_px(config));
        // Tracks whether the user has toggled this block to its complete height.
        // The default cap is recomputed whenever virtualization remaps the card.
        let expanded: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        {
            let cols_for_map = cols.max(1);
            let fallback_cap_for_map = viewport_cap;
            let current_cap_for_map = current_viewport_cap.clone();
            let displayed_for_map = displayed_output.clone();
            let expanded_for_map = expanded.clone();
            let expand_btn_for_map = expand_btn.clone();
            let jump_btn_for_map = jump_bottom_btn.clone();
            output_vte.connect_map(move |w| {
                let text = displayed_for_map.borrow();
                let rows = output_visual_row_count(&text, cols_for_map);
                let fitted_cap = fitted_output_rows_for_widget(w, fallback_cap_for_map, rows);
                current_cap_for_map.set(fitted_cap);
                let document_scroll = uses_outer_document_scroll(rows);
                let manually_expanded = expanded_for_map.get();
                let cap = finished_output_cap(rows, fitted_cap, manually_expanded);
                let visible_rows = rows.min(cap).max(1);
                let can_expand = rows > fitted_cap && !document_scroll;
                expand_btn_for_map.set_visible(can_expand);
                jump_btn_for_map.set_visible(rows > fitted_cap);
                render_bytes_into_finished_vte(
                    w,
                    &text,
                    cols_for_map,
                    rows,
                    cap,
                    capture_rows,
                    document_scroll || manually_expanded,
                );
                // The VTE grid and pixel request use the identical row count.
                // This prevents GTK from allocating a tall empty card around a
                // smaller terminal surface.
                let ch = w.char_height() as i32;
                if ch > 0 {
                    w.set_height_request((visible_rows as i32) * ch);
                }
            });
        }

        // Geometry is finalized on map, so install the handler for every
        // block and let the map callback decide whether expansion is useful.
        expand_btn.set_visible(long_output && !uses_outer_document_scroll(output_rows));
        {
            let expand_for_btn = expanded.clone();
            let output_vte_for_btn = output_vte.clone();
            let displayed_for_btn = displayed_output.clone();
            let current_cap_for_btn = current_viewport_cap.clone();
            let cols_for_btn = cols.max(1);
            expand_btn.connect_clicked(move |btn| {
                let now_expanded = !expand_for_btn.get();
                expand_for_btn.set(now_expanded);
                let rows = output_visual_row_count(&displayed_for_btn.borrow(), cols_for_btn);
                let fitted_cap = fitted_output_rows_for_widget(
                    &output_vte_for_btn,
                    current_cap_for_btn.get(),
                    rows,
                );
                current_cap_for_btn.set(fitted_cap);
                let document_scroll = uses_outer_document_scroll(rows);
                let cap = finished_output_cap(rows, fitted_cap, now_expanded);
                let visible_rows = rows.min(cap).max(1);
                render_bytes_into_finished_vte(
                    &output_vte_for_btn,
                    &displayed_for_btn.borrow(),
                    cols_for_btn,
                    rows,
                    cap,
                    capture_rows,
                    document_scroll || now_expanded,
                );
                let ch = output_vte_for_btn.char_height() as i32;
                if ch > 0 {
                    output_vte_for_btn.set_height_request((visible_rows as i32) * ch);
                }
                btn.set_label(if now_expanded { "\u{f066}" } else { "\u{f065}" });
                btn.set_tooltip_text(Some(if now_expanded {
                    "Collapse to viewport height"
                } else {
                    "Expand block"
                }));
            });
        }

        // Command row: Warp-style accent prompt chevron + the command VTE.
        let cmd_row = gtk4::Box::new(Orientation::Horizontal, 0);
        let chevron = gtk4::Label::new(Some("\u{276f}")); // ❯
        chevron.add_css_class("block-prompt-chevron");
        chevron.set_valign(gtk4::Align::Start);
        cmd_row.append(&chevron);
        cmd_row.append(&command_vte);

        content.append(&cmd_row);
        cmd_row.set_visible(!is_background);
        // Always use a read-only VTE, including short output. The previous Label
        // fast path stripped ANSI SGR bytes, so `ls` and `git status` lost the
        // colors users see in regular VTE mode.
        let output_widget: gtk4::Widget = output_vte.clone().upcast::<gtk4::Widget>();
        content.append(&output_vte);

        // Folding used to leave only a tiny chevron in the header. That made a
        // collapsed block look like it had no output at all, especially once it
        // had scrolled away from the pointer. Keep a compact, keyboard-focusable
        // summary in the document instead; it both preserves the output's scale
        // and is a large, obvious target to restore it.
        let collapsed_summary = gtk4::Button::with_label(&collapsed_output_summary(output_rows));
        collapsed_summary.add_css_class("block-output-summary");
        collapsed_summary.add_css_class("flat");
        collapsed_summary.set_halign(gtk4::Align::Start);
        collapsed_summary.set_margin_start(18);
        collapsed_summary.set_margin_end(8);
        collapsed_summary.set_margin_bottom(4);
        collapsed_summary.set_tooltip_text(Some("Show block output"));
        collapsed_summary.set_visible(false);
        content.append(&collapsed_summary);

        // Ctrl+click on a URL inside the output VTE → open in browser.
        // VTE's `match_add_regex` (registered in create_finished_terminal) makes
        // `check_match_at` return the matching URL at the pointer position;
        // VTE handles word/line double/triple-click selection natively.
        {
            let click = gtk4::GestureClick::new();
            click.set_button(1);
            let vte_for_click = output_vte.clone();
            click.connect_pressed(move |controller, n_press, x, y| {
                if n_press != 1 {
                    return;
                }
                let state = controller.current_event_state();
                if !state.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
                    return;
                }
                let (uri, _tag) = vte_for_click.check_match_at(x, y);
                if let Some(uri) = uri {
                    let s = uri.to_string();
                    if !s.is_empty() {
                        open_uri(&s);
                        controller.set_state(gtk4::EventSequenceState::Claimed);
                    }
                }
            });
            output_vte.add_controller(click);
        }

        let has_output = !output.trim().is_empty();
        // Output-only controls are noise for commands such as `cd`,
        // `mkdir`, and successful redirects.
        copy_output_btn.set_visible(has_output);
        filter_btn.set_visible(has_output);
        collapse_btn.set_visible(has_output);
        if !has_output {
            output_widget.set_visible(false);
        } else {
            collapse_btn.set_tooltip_text(Some(&format!(
                "Toggle output ({})",
                line_count_text(output_rows)
            )));
        }
        // Header chevron and the inline summary share one folded-state update,
        // so either target consistently restores the same output surface.
        let set_collapsed: Rc<dyn Fn(bool)> = {
            let output_widget = output_widget.clone();
            let collapsed_summary = collapsed_summary.clone();
            let collapse_btn = collapse_btn.clone();
            Rc::new(move |collapsed| {
                output_widget.set_visible(!collapsed);
                collapsed_summary.set_visible(collapsed);
                collapse_btn.set_label(if collapsed { "\u{f054}" } else { "\u{f078}" });
                collapse_btn.set_tooltip_text(Some(if collapsed {
                    "Show output"
                } else {
                    "Hide output"
                }));
            })
        };
        {
            let set_collapsed = set_collapsed.clone();
            let output_widget = output_widget.clone();
            collapse_btn.connect_clicked(move |_| set_collapsed(output_widget.is_visible()));
        }
        {
            let set_collapsed = set_collapsed.clone();
            collapsed_summary.connect_clicked(move |_| set_collapsed(false));
        }

        // Per-block output filter (Warp's BlockFilterQuery): the funnel button in
        // the action box toggles a compact row that narrows the output to lines
        // matching the query, honoring regex / case / invert / context-lines.
        let toggle_filter = {
            let filter_row = gtk4::Box::new(Orientation::Horizontal, 4);
            filter_row.add_css_class("block-filter-row");
            filter_row.set_visible(false);
            filter_row.set_margin_start(12);
            filter_row.set_margin_end(8);
            filter_row.set_margin_top(2);
            filter_row.set_margin_bottom(2);

            let filter_enabled = Rc::new(Cell::new(false));
            let filter_entry = gtk4::SearchEntry::new();
            filter_entry.set_placeholder_text(Some("Filter output…"));
            filter_entry.set_hexpand(true);
            let regex_tg = gtk4::ToggleButton::with_label(".*");
            regex_tg.set_tooltip_text(Some("Regular expression"));
            let case_tg = gtk4::ToggleButton::with_label("Aa");
            case_tg.set_tooltip_text(Some("Case sensitive"));
            let invert_tg = gtk4::ToggleButton::with_label("!");
            invert_tg.set_tooltip_text(Some("Invert match (hide matching lines)"));
            let ctx_spin = gtk4::SpinButton::with_range(0.0, 9.0, 1.0);
            ctx_spin.set_tooltip_text(Some("Lines of context around each match"));
            ctx_spin.set_value(0.0);
            let filter_status = gtk4::Label::new(None);
            filter_status.add_css_class("block-filter-status");
            filter_status.set_halign(gtk4::Align::Start);
            for w in [&regex_tg, &case_tg, &invert_tg] {
                w.add_css_class("flat");
                w.add_css_class("block-filter-toggle");
            }
            filter_row.append(&filter_entry);
            filter_row.append(&regex_tg);
            filter_row.append(&case_tg);
            filter_row.append(&invert_tg);
            filter_row.append(&ctx_spin);
            filter_row.append(&filter_status);

            content.append(&filter_row);
            content.reorder_child_after(&filter_row, Some(&cmd_row));

            let apply = {
                let output_vte = output_vte.clone();
                let full_output = full_output.clone();
                let displayed_output = displayed_output.clone();
                let filter_enabled = filter_enabled.clone();
                let filter_entry = filter_entry.clone();
                let regex_tg = regex_tg.clone();
                let case_tg = case_tg.clone();
                let invert_tg = invert_tg.clone();
                let ctx_spin = ctx_spin.clone();
                let filter_status = filter_status.clone();
                let expand_btn = expand_btn.clone();
                let expanded = expanded.clone();
                let current_viewport_cap = current_viewport_cap.clone();
                let filter_btn = filter_btn.clone();
                let jump_bottom_btn = jump_bottom_btn.clone();
                let collapsed_summary = collapsed_summary.clone();
                move || {
                    let q = filter_entry.text().to_string();
                    let full = full_output.borrow();
                    let full_rows = output_row_count(&full);
                    let filtered = if !filter_enabled.get() || q.is_empty() {
                        Ok(full.to_string())
                    } else {
                        filter_output_lines(
                            full.as_str(),
                            &q,
                            regex_tg.is_active(),
                            case_tg.is_active(),
                            invert_tg.is_active(),
                            ctx_spin.value() as usize,
                        )
                    };
                    let (shown, invalid_regex) = match filtered {
                        Ok(shown) => (shown, false),
                        Err(_) => (full.to_string(), true),
                    };
                    let shown_rows = output_row_count(&shown);
                    let shown_visual_rows = output_visual_row_count(&shown, cols);
                    let fitted_cap = fitted_output_rows_for_widget(
                        &output_vte,
                        current_viewport_cap.get(),
                        shown_visual_rows,
                    );
                    current_viewport_cap.set(fitted_cap);
                    let document_scroll = uses_outer_document_scroll(shown_visual_rows);
                    let can_expand = shown_visual_rows > fitted_cap && !document_scroll;
                    // A narrow filter result must not leave the block logically
                    // expanded; clearing the query should return to its default mode.
                    if !can_expand && expanded.replace(false) {
                        expand_btn.set_label("\u{f065}");
                        expand_btn.set_tooltip_text(Some("Expand block"));
                    }
                    let manually_expanded = expanded.get();
                    let active_cap =
                        finished_output_cap(shown_visual_rows, fitted_cap, manually_expanded);
                    render_bytes_into_finished_vte(
                        &output_vte,
                        &shown,
                        cols,
                        shown_visual_rows,
                        active_cap,
                        capture_rows,
                        document_scroll || manually_expanded,
                    );
                    let ch = output_vte.char_height() as i32;
                    if ch > 0 {
                        output_vte.set_height_request(
                            (shown_visual_rows.min(active_cap).max(1) as i32) * ch,
                        );
                    }
                    let has_query = filter_enabled.get() && !q.trim().is_empty();
                    if invalid_regex {
                        filter_btn.add_css_class("block-action-active");
                        filter_status.set_visible(true);
                        filter_status.set_text("Invalid regular expression");
                        filter_status.add_css_class("block-filter-empty");
                    } else if has_query {
                        filter_btn.add_css_class("block-action-active");
                        filter_status.set_visible(true);
                        let hidden = full_rows.saturating_sub(shown_rows);
                        if shown.trim().is_empty() {
                            filter_status.set_text("No matches");
                            filter_status.add_css_class("block-filter-empty");
                        } else {
                            filter_status.remove_css_class("block-filter-empty");
                            filter_status.set_text(&format!(
                                "{} shown, {} hidden",
                                line_count_text(shown_rows),
                                hidden
                            ));
                        }
                    } else {
                        filter_btn.remove_css_class("block-action-active");
                        filter_status.remove_css_class("block-filter-empty");
                        filter_status.set_visible(false);
                    }
                    collapsed_summary.set_label(&collapsed_output_summary(shown_rows));
                    expand_btn.set_visible(can_expand);
                    jump_bottom_btn.set_visible(shown_visual_rows > fitted_cap);
                    // Keep `displayed_output` in sync so a later unmap → remap
                    // (block scrolls out of view, then back) re-feeds the
                    // filtered text, not the full output.
                    *displayed_output.borrow_mut() = shown;
                }
            };
            let apply = Rc::new(apply);
            {
                let a = apply.clone();
                filter_entry.connect_search_changed(move |_| a());
            }
            for tg in [&regex_tg, &case_tg, &invert_tg] {
                let a = apply.clone();
                tg.connect_toggled(move |_| a());
            }
            {
                let a = apply.clone();
                ctx_spin.connect_value_changed(move |_| a());
            }

            let filter_row_for_toggle = filter_row.downgrade();
            let entry_for_toggle = filter_entry.downgrade();
            let filter_enabled_for_toggle = filter_enabled.clone();
            let apply_for_toggle = apply.clone();
            let filter_btn_for_toggle = filter_btn.downgrade();
            let set_collapsed_for_filter = set_collapsed.clone();
            let toggle: Rc<dyn Fn()> = Rc::new(move || {
                let (Some(filter_row), Some(entry), Some(button)) = (
                    filter_row_for_toggle.upgrade(),
                    entry_for_toggle.upgrade(),
                    filter_btn_for_toggle.upgrade(),
                ) else {
                    return;
                };
                let show = !filter_row.is_visible();
                filter_enabled_for_toggle.set(show);
                filter_row.set_visible(show);
                if show {
                    set_collapsed_for_filter(false);
                    button.add_css_class("block-action-active");
                    entry.grab_focus();
                } else {
                    button.remove_css_class("block-action-active");
                }
                apply_for_toggle();
            });
            let toggle_for_button = toggle.clone();
            filter_btn.connect_clicked(move |_| toggle_for_button());
            toggle
        };

        FinishedBlock {
            id,
            is_background,
            widget: outer,
            content,
            virtualized_height,
            virtualized,
            prompt_text: prompt.to_string(),
            command_vte,
            output_vte,
            full_output,
            displayed_output,
            stripped_output: Rc::new(RefCell::new(None)),
            cmd_text: cmd.to_string(),
            copy_cmd_btn,
            copy_output_btn,
            rerun_btn,
            header_row,
            action_box,
            selection_hint,
            toggle_filter,
            jump_bottom_btn,
            bookmark_star,
            status_icon,
            cols,
            viewport_cap,
            long_output,
        }
    }

    pub(crate) fn widget(&self) -> &gtk4::Box {
        &self.widget
    }

    /// Unmap expensive VTE content while preserving the card's measured height.
    /// Returning the placeholder height lets the caller keep virtualization
    /// metadata synchronized with the actual GTK allocation.
    pub(crate) fn set_virtualized(&self, virtualized: bool) -> i32 {
        if self.virtualized.replace(virtualized) == virtualized {
            return self.virtualized_height.get().max(1);
        }

        if virtualized {
            let allocated = self.widget.height();
            if allocated > 1 {
                self.virtualized_height.set(allocated);
            }
            let height = self.virtualized_height.get().max(1);
            self.widget.set_height_request(height);
            self.content.set_visible(false);
            height
        } else {
            self.content.set_visible(true);
            self.widget.set_height_request(-1);
            self.virtualized_height.get().max(1)
        }
    }

    /// Scroll this block's top or bottom edge into the outer history canvas.
    pub(crate) fn scroll_to_edge(&self, outer: &gtk4::ScrolledWindow, bottom: bool) {
        let widget = self.widget.clone();
        let outer = outer.clone();
        glib::idle_add_local_once(move || {
            let Some(bounds) = widget.compute_bounds(&outer) else {
                return;
            };
            let adj = outer.vadjustment();
            let target = block_edge_scroll_target(
                adj.value(),
                bounds.y() as f64,
                bounds.height() as f64,
                adj.page_size(),
                adj.lower(),
                adj.upper(),
                bottom,
            );
            adj.set_value(target);
        });
    }

    /// Forward wheel events on the output VTE to the outer ScrolledWindow once
    /// the VTE's internal scrollback can't move further in the wheel direction.
    /// Without this the user's scroll "sticks" at a long block's edge: VTE
    /// silently swallows wheels that no longer scroll its own buffer, and the
    /// page never resumes. Closes the perceptual gap with a single-scrollback
    /// VTE pane (terminator/xterm).
    pub(crate) fn connect_scroll_forwarding(&self, outer: &gtk4::ScrolledWindow) {
        let block_for_jump = self.clone();
        let outer_for_jump = outer.clone();
        self.jump_bottom_btn.connect_clicked(move |_| {
            if let Some(adj) = block_for_jump.output_vte.vadjustment() {
                let target = (adj.upper() - adj.page_size()).max(adj.lower());
                if target > adj.lower() + f64::EPSILON {
                    adj.set_value(target);
                    return;
                }
            }
            block_for_jump.scroll_to_edge(&outer_for_jump, true);
        });

        let command_scroll =
            gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        command_scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let outer_for_command = outer.clone();
        command_scroll.connect_scroll(move |_, _dx, dy| {
            forward_outer_scroll(&outer_for_command, dy);
            glib::Propagation::Stop
        });
        self.command_vte.add_controller(command_scroll);

        let scroll_ctrl =
            gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        let vte = self.output_vte.clone();
        let outer_for_vte = outer.clone();
        scroll_ctrl.connect_scroll(move |_, _dx, dy| {
            // The cap is determined only after map/resize. Inspect the actual
            // VTE adjustment on every wheel event rather than trusting a stale
            // construction-time flag.
            let Some(inner_adj) = vte.vadjustment() else {
                return glib::Propagation::Proceed;
            };
            let at_top = inner_adj.value() <= inner_adj.lower() + f64::EPSILON;
            let at_bottom =
                inner_adj.value() + inner_adj.page_size() >= inner_adj.upper() - f64::EPSILON;
            let going_up = dy < 0.0;
            let going_down = dy > 0.0;
            if (going_up && !at_top) || (going_down && !at_bottom) {
                // VTE still has room to scroll itself; let it.
                return glib::Propagation::Proceed;
            }
            // Drive the outer ScrolledWindow by one step in the wheel direction.
            forward_outer_scroll(&outer_for_vte, dy);
            glib::Propagation::Stop
        });
        self.output_vte.add_controller(scroll_ctrl);
    }

    /// Wire the hover quick-action buttons (copy command, copy output, re-run).
    /// Kept separate from construction because handlers need the clipboard, PTY,
    /// and active block, which only the owning `TermView` has.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn connect_actions(
        &self,
        vte: &Terminal,
        pty: &Rc<crate::pty::OwnedPty>,
        pty_synced: &Rc<Cell<bool>>,
        active: &Rc<RefCell<ActiveBlock>>,
        typed_cmd: &Rc<RefCell<String>>,
        bstate: &Rc<Cell<BlockState>>,
        bracketed_paste: &Rc<Cell<bool>>,
    ) {
        let vte_for_cmd = vte.clone();
        let cmd_for_copy = self.cmd_text.clone();
        self.copy_cmd_btn.connect_clicked(move |btn| {
            vte_for_cmd.clipboard().set_text(&cmd_for_copy);
            flash_button_label(btn, "\u{f00c}", "Command copied");
        });

        let vte_for_out = vte.clone();
        // Copy the FULL output (ANSI stripped), not just the collapsed first-N
        // lines shown in output_buffer before "Show more" is clicked.
        let full_output_for_copy = self.full_output.clone();
        let stripped_output_for_copy = self.stripped_output.clone();
        self.copy_output_btn.connect_clicked(move |btn| {
            let text = Self::with_cached_stripped_output(
                &full_output_for_copy,
                &stripped_output_for_copy,
                |s| s.to_string(),
            );
            vte_for_out.clipboard().set_text(&text);
            flash_button_label(btn, "\u{f00c}", "Output copied");
        });

        let pty_for_rerun = Rc::clone(pty);
        let pty_synced_for_rerun = pty_synced.clone();
        let active_for_rerun = active.clone();
        let typed_cmd_for_rerun = typed_cmd.clone();
        let bstate_for_rerun = bstate.clone();
        let bracketed_for_rerun = bracketed_paste.clone();
        let cmd_for_rerun = self.cmd_text.clone();
        self.rerun_btn.connect_clicked(move |btn| {
            if recall_command_at_prompt(
                &pty_for_rerun,
                &pty_synced_for_rerun,
                &typed_cmd_for_rerun,
                bstate_for_rerun.get(),
                &cmd_for_rerun,
                bracketed_for_rerun.get(),
            ) {
                active_for_rerun.borrow().grab_focus();
                flash_button_label(btn, "\u{f00c}", "Command inserted");
            } else {
                flash_button_label(btn, "\u{f071}", "Wait for an editable prompt");
            }
        });
    }
}

// ─── ActiveBlock ──────────────────────────────────────────────────────────────

/// The live area: a single persistent input-enabled VTE pinned to the viewport
/// height. The shell's prompt, the user's typing, and command
/// output all render natively in this one VTE. When a command finishes, its
/// accumulated output (`raw_output`) is snapshotted into a styled FinishedBlock
/// stacked above this card.
pub(crate) struct ActiveBlock {
    pub(crate) widget: gtk4::Box,
    pub(crate) active_vte: Terminal,
    /// Raw output bytes accumulated during CollectingOutput, consumed by the
    /// finalize path to build the styled finished block (jterm1's `out_buf`).
    pub(crate) raw_output: Rc<RefCell<Vec<u8>>>,
}

impl ActiveBlock {
    pub(crate) fn new(config: &Config) -> Self {
        let widget = gtk4::Box::new(Orientation::Vertical, 0);
        widget.add_css_class("block-active");
        if config.block_compact {
            widget.add_css_class("block-compact");
        }
        // focusable(false) keeps the holder Box from being a focus target, but we
        // must NOT set can_focus(false): in GTK4 that blocks all descendants
        // (including active_vte) from ever receiving focus.
        widget.set_focusable(false);
        widget.set_hexpand(true);
        // The outer block document owns vertical expansion. The live surface is
        // explicitly sized compact/full by block_view; keeping it non-expanding
        // prevents GTK from adding document slack to its grid.
        widget.set_vexpand(false);

        let active_vte = create_active_terminal(config);
        active_vte.set_hexpand(true);
        active_vte.set_vexpand(false);
        widget.append(&active_vte);

        // Focus the live VTE as soon as it is realized (jterm1 block.rs:324-328).
        {
            let av = active_vte.clone();
            active_vte.connect_realize(move |_| {
                av.grab_focus();
            });
        }
        // realize fires before the toplevel is presented, so grab_focus there
        // can be lost. connect_map fires when the VTE actually becomes visible
        // (window shown / tab switched), which is the reliable point to take
        // keyboard focus.
        {
            let av = active_vte.clone();
            active_vte.connect_map(move |_| {
                av.grab_focus();
            });
        }

        ActiveBlock {
            widget,
            active_vte,
            raw_output: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Append raw command-output bytes to the snapshot buffer (bounded). The bytes
    /// are also fed to the live VTE separately by the reader; this buffer is only
    /// the source the finalize path styles into a finished block.
    pub(crate) fn accumulate_output(&self, raw_bytes: &[u8]) {
        let mut buf = self.raw_output.borrow_mut();
        buf.extend_from_slice(raw_bytes);
        if buf.len() > super::MAX_RAW_OUTPUT_BYTES {
            let drop = buf.len() - super::MAX_RAW_OUTPUT_BYTES;
            buf.drain(..drop);
        }
    }

    pub(crate) fn output_text(&self) -> String {
        let raw = self.raw_output.borrow();
        if raw.is_empty() {
            return String::new();
        }
        String::from_utf8_lossy(&raw).into_owned()
    }

    /// Clear the accumulated output buffer (without touching the VTE).
    pub(crate) fn reset_output_buffer(&self) {
        self.raw_output.borrow_mut().clear();
    }

    /// The column count the live VTE is wrapping at — the single source of truth
    /// for pre-wrapping finished blocks so they align with what the user watched.
    pub(crate) fn grid_cols(&self) -> usize {
        (self.active_vte.column_count().max(20)) as usize
    }

    /// Reset the live VTE for the next prompt (jterm1 block.rs:1028-1044). `reset`
    /// acts immediately, but already-queued feed() bytes are processed async, so the
    /// in-stream clear (fed after them) wipes stale output in the correct order.
    ///
    /// `preserve_scrollback`: when true, keep the VTE's buffer + scrollback intact
    /// (only the accumulated raw_output snapshot for the *next* block is cleared,
    /// and SGR state is soft-reset). When false (the default), finished blocks
    /// remain the sole historical surface and the compact live cell shows only
    /// the current prompt.
    pub(crate) fn reset_active(&self, preserve_scrollback: bool) {
        if preserve_scrollback {
            self.active_vte.feed(b"\x1b[0m");
        } else {
            self.active_vte.reset(true, true);
            self.active_vte.feed(b"\x1b[H\x1b[2J\x1b[3J");
        }
        self.raw_output.borrow_mut().clear();
    }

    pub(crate) fn widget(&self) -> &gtk4::Box {
        &self.widget
    }

    pub(crate) fn grab_focus(&self) {
        self.active_vte.grab_focus();
    }
}

// ─── TermView state machine ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum BlockState {
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
    /// Between CommandEnd and next PromptStart — still collecting late output
    PostCommand,
    /// Shell has no OSC-133 integration: route all bytes to the raw VTE so output
    /// is never dropped. Entered from Idle when output arrives but no FTCS event
    /// has been seen within the startup grace window. Recovered to block mode if a
    /// PromptStart ever arrives (late-loading integration).
    RawFallback,
}

/// Recalling a finished command is safe only while the shell is sitting at a
/// prompt. In every other state, writing command bytes would feed the currently
/// running process (or vim/less) instead of the shell line editor.
pub(crate) fn command_recall_available(state: BlockState) -> bool {
    state == BlockState::AwaitingCommand
}

/// Select text that can be inserted without accidentally submitting multiple
/// commands. With bracketed paste enabled, the whole multiline edit buffer is
/// safe; otherwise fall back to the first line.
fn recalled_command_text(cmd: &str, bracketed_paste: bool) -> (&str, bool) {
    let cmd = cmd.trim_end_matches(['\r', '\n']);
    if bracketed_paste {
        return (cmd, false);
    }
    let first_break = cmd
        .char_indices()
        .find_map(|(idx, ch)| matches!(ch, '\r' | '\n').then_some(idx));
    match first_break {
        Some(idx) => (&cmd[..idx], true),
        None => (cmd, false),
    }
}

/// Replace the current shell edit buffer with a recalled command. Multiline
/// commands use bracketed-paste markers when the shell advertised DECSET 2004,
/// so embedded newlines remain editable instead of executing early.
pub(crate) fn write_recalled_command(
    pty: &crate::pty::OwnedPty,
    cmd: &str,
    bracketed_paste: bool,
    execute: bool,
) -> bool {
    let (text, first_line_only) = recalled_command_text(cmd, bracketed_paste);
    let multiline = text.contains('\n') || text.contains('\r');

    // Always clear the line. User-typed text is not represented by pty_synced,
    // so conditioning Ctrl+U on that flag appends history to partial input.
    pty.write_bytes(b"\x15");
    if bracketed_paste && multiline {
        pty.write_bytes(b"\x1b[200~");
        pty.write_bytes(text.as_bytes());
        pty.write_bytes(b"\x1b[201~");
    } else {
        pty.write_bytes(text.as_bytes());
    }
    if execute {
        pty.write_bytes(b"\r");
    }
    first_line_only
}

#[cfg(test)]
mod tests {
    use super::{
        block_clipboard_text, collapsed_output_summary, command_recall_available,
        filter_output_lines, terminalize_line_breaks, BlockState,
    };

    #[test]
    fn whole_block_copy_preserves_terminal_grouping() {
        assert_eq!(block_clipboard_text("echo ok", "ok", false), "echo ok\nok");
        assert_eq!(block_clipboard_text("echo ok", "ok", true), "ok");
        assert_eq!(block_clipboard_text("pwd", "", false), "pwd");
    }

    #[test]
    fn command_recall_is_only_available_at_the_prompt() {
        assert!(command_recall_available(BlockState::AwaitingCommand));
        for state in [
            BlockState::Idle,
            BlockState::CollectingPrompt,
            BlockState::CollectingOutput,
            BlockState::AltScreen,
            BlockState::PostCommand,
            BlockState::RawFallback,
        ] {
            assert!(!command_recall_available(state), "{state:?}");
        }
    }

    #[test]
    fn multiline_recall_uses_full_text_with_bracketed_paste() {
        assert_eq!(
            super::recalled_command_text("printf one\nprintf two\n", true),
            ("printf one\nprintf two", false)
        );
    }

    #[test]
    fn multiline_recall_falls_back_without_bracketed_paste() {
        assert_eq!(
            super::recalled_command_text("printf one\nprintf two", false),
            ("printf one", true)
        );
    }

    #[test]
    fn terminalize_command_line_breaks_return_to_the_command_column() {
        assert_eq!(
            terminalize_line_breaks(b"cd /tmp\npython3 demo.py"),
            b"cd /tmp\r\npython3 demo.py"
        );
        assert_eq!(
            terminalize_line_breaks(b"\x1b[36mrun\x1b[0m\r\nnext"),
            b"\x1b[36mrun\x1b[0m\r\nnext"
        );
    }

    #[test]
    fn filter_output_lines_matches_ascii_case_insensitive() {
        assert_eq!(
            filter_output_lines("alpha\nERROR: nope\nomega", "error", false, false, false, 0)
                .unwrap(),
            "ERROR: nope"
        );
    }

    #[test]
    fn filter_output_lines_preserves_unicode_case_insensitive_search() {
        assert_eq!(
            filter_output_lines("alpha\n你好世界\nomega", "你好", false, false, false, 0).unwrap(),
            "你好世界"
        );
    }

    #[test]
    fn filter_output_lines_reports_invalid_regex() {
        assert!(filter_output_lines("alpha", "[", true, false, false, 0).is_err());
    }

    #[test]
    fn collapsed_summary_uses_singular_and_plural_line_counts() {
        assert_eq!(
            collapsed_output_summary(1),
            "▸ 1 line hidden — click to show"
        );
        assert_eq!(
            collapsed_output_summary(42),
            "▸ 42 lines hidden — click to show"
        );
    }

    #[test]
    fn visual_row_count_includes_terminal_wrapping() {
        assert_eq!(super::output_visual_row_count("123456789\nabc", 4), 4);
        assert_eq!(super::output_visual_row_count("界界界", 4), 2);
    }

    #[test]
    fn long_output_cap_fills_space_above_compact_input() {
        assert_eq!(
            super::fitted_output_rows_for_viewport(Some(60), 30, 200),
            51
        );
        assert_eq!(super::fitted_output_rows_for_viewport(Some(60), 30, 40), 40);
        assert_eq!(super::fitted_output_rows_for_viewport(None, 30, 200), 30);
        assert_eq!(super::fitted_output_rows_for_viewport(Some(8), 30, 200), 3);
    }

    #[test]
    fn ordinary_long_output_uses_the_outer_document() {
        assert!(super::uses_outer_document_scroll(200));
        assert_eq!(super::finished_output_cap(200, 30, false), 200);
    }

    #[test]
    fn pathological_output_stays_bounded_until_expanded() {
        let rows = super::MAX_AUTO_DOCUMENT_OUTPUT_ROWS + 1;
        assert!(!super::uses_outer_document_scroll(rows));
        assert_eq!(super::finished_output_cap(rows, 42, false), 42);
        assert_eq!(super::finished_output_cap(rows, 42, true), rows);
    }

    #[test]
    fn visual_row_count_ignores_ansi_and_overwritten_progress_rows() {
        let apt_like = concat!(
            "\r0% [Working]",
            "\r\x1b[K\x1b[32mHit:1 repo\x1b[0m\r\n",
            "\r50% [Working]",
            "\r\x1b[KDone\r\n",
        );
        assert_eq!(super::output_visual_row_count(apt_like, 20), 2);
    }

    #[test]
    fn filter_output_lines_includes_context_without_extra_alloc_join() {
        assert_eq!(
            filter_output_lines("one\ntwo\nthree\nfour", "three", false, true, false, 1).unwrap(),
            "two\nthree\nfour"
        );
    }
}
