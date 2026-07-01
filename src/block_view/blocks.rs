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
#[archive(check_bytes)]
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
    pub(crate) widget: gtk4::Box,
    pub(crate) prompt_text: String,
    /// Read-only VTE displaying the executed command line (single-row typically).
    pub(crate) command_vte: vte4::Terminal,
    /// Read-only VTE displaying the captured output. Full output is fed once;
    /// rows beyond viewport_cap live in this VTE's own scrollback so the user
    /// can scroll inside long blocks (e.g. `git log`).
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
    pub(crate) bookmark_star: gtk4::Label,
    pub(crate) status_icon: gtk4::Label,
    /// Column count the output VTE is sized to — needed for re-feed (filter).
    pub(crate) cols: i64,
    /// Visible-row cap (config.finished_block_viewport_rows).
    pub(crate) viewport_cap: i64,
}

impl Clone for FinishedBlock {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            widget: self.widget.clone(),
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
            bookmark_star: self.bookmark_star.clone(),
            status_icon: self.status_icon.clone(),
            cols: self.cols,
            viewport_cap: self.viewport_cap,
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
) -> String {
    if query.is_empty() {
        return full.to_string();
    }
    let re = if use_regex {
        match regex::RegexBuilder::new(query)
            .case_insensitive(!case_sensitive)
            .build()
        {
            Ok(re) => Some(re),
            Err(_) => return full.to_string(),
        }
    } else {
        None
    };
    let lc_query = if case_sensitive {
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
    lines
        .iter()
        .zip(keep.iter())
        .filter_map(|(l, k)| if *k { Some(*l) } else { None })
        .collect::<Vec<_>>()
        .join("\n")
}

fn output_row_count(text: &str) -> i64 {
    if text.is_empty() {
        1
    } else {
        text.lines().count().max(1) as i64
    }
}

fn line_count_text(rows: i64) -> String {
    if rows == 1 {
        "1 line".to_string()
    } else {
        format!("{rows} lines")
    }
}

pub(crate) fn estimated_cell_height_px(config: &Config) -> i32 {
    let parts: Vec<&str> = config.font_desc.split_whitespace().collect();
    let base_size = parts
        .last()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(14.0);
    (base_size * config.default_font_scale * (96.0 / 72.0) * 1.2)
        .ceil()
        .max(1.0) as i32
}

pub(crate) fn estimated_finished_block_height(config: &Config, output_rows: usize) -> i32 {
    let cell = estimated_cell_height_px(config);
    // Header + command row + output rows + margins/borders/filter slack.
    let rows = output_rows.max(1) as i32;
    (rows + 2) * cell + 34
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

/// Render `bytes` into a read-only finished VTE: reset the grid, resize to the
/// new visible-row count (so the chrome shrinks/grows on filter), then feed
/// the bytes in one shot. Used for filter changes — the initial feed happens
/// once at construction.
pub(crate) fn render_bytes_into_finished_vte(
    vte: &vte4::Terminal,
    bytes: &[u8],
    cols: i64,
    output_rows: i64,
    viewport_cap: i64,
) {
    let visible_rows = output_rows.min(viewport_cap).max(1);
    let scrollback = (output_rows.max(visible_rows) as u32).saturating_add(64);
    // Set size BEFORE reset/feed so VTE's internal grid is sized correctly when
    // bytes are processed. If we feed first and resize later, VTE wraps lines
    // at the pre-resize default width (root cause of the ls-output misalignment
    // bug: `ls` formats for N cols but VTE wrapped at a narrower width,
    // producing mid-word splits like "ta\nuri-sandbox").
    vte.set_size(cols.max(1), visible_rows);
    vte.set_scrollback_lines(scrollback as i64);
    vte.reset(true, true);
    // reset() can clamp dimensions on some VTE builds — re-assert.
    vte.set_size(cols.max(1), visible_rows);
    vte.feed(bytes);
}

impl FinishedBlock {
    /// Returns the ANSI-stripped view of `full_output`, populating the cache on
    /// first call. Caller passes a closure to handle the cached string by ref to
    /// avoid an extra clone — `stripped_output` lives in a `RefCell` so we can't
    /// hand out a `Ref` that outlives the borrow.
    pub(crate) fn with_stripped_output<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        if self.stripped_output.borrow().is_none() {
            let s = strip_ansi(&self.full_output.borrow());
            *self.stripped_output.borrow_mut() = Some(s);
        }
        let guard = self.stripped_output.borrow();
        f(guard.as_deref().unwrap_or(""))
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
        let viewport_cap = config.finished_block_viewport_rows.max(3) as i64;
        let max_expanded_cap = (config.finished_block_max_expanded_rows as i64).max(viewport_cap);

        let outer = if let Some(reused) = recycled {
            while let Some(child) = reused.first_child() {
                reused.remove(&child);
            }
            reused.remove_css_class("block-hovered");
            reused.remove_css_class("block-selected");
            reused.remove_css_class("block-success");
            reused.remove_css_class("block-failed");
            reused
        } else {
            let b = gtk4::Box::new(Orientation::Vertical, 0);
            b.add_css_class("block-finished");
            b.set_margin_top(4);
            b.set_margin_bottom(4);
            b.set_margin_start(8);
            b.set_margin_end(8);
            b
        };

        // Status stripe: green left border on success, red on failure.
        outer.add_css_class(if exit_code == 0 {
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
        header_row.set_margin_start(12);
        header_row.set_margin_end(8);
        header_row.set_margin_top(6);
        header_row.set_margin_bottom(2);

        // Bookmark star (gutter marker), hidden until the block is bookmarked.
        let bookmark_star = gtk4::Label::new(Some("\u{f02e}")); // nf-fa-bookmark
        bookmark_star.add_css_class("block-bookmark-star");
        bookmark_star.set_halign(gtk4::Align::Start);
        bookmark_star.set_visible(false);
        header_row.append(&bookmark_star);

        // Status icon: ✓ for success, ✗ for failure.
        // Nerd Font glyphs: nf-fa-check () on success, nf-fa-times () on failure.
        let status_icon = gtk4::Label::new(Some(if exit_code == 0 {
            "\u{f00c}"
        } else {
            "\u{f00d}"
        }));
        status_icon.add_css_class(if exit_code == 0 {
            "block-status-ok"
        } else {
            "block-status-bad"
        });
        status_icon.set_halign(gtk4::Align::Start);
        header_row.append(&status_icon);

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
        if exit_code != 0 {
            let badge = gtk4::Label::new(Some(&format!("exit:{}", exit_code)));
            badge.add_css_class("block-exit-bad");
            header_row.append(&badge);
        }

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
        rerun_btn.set_tooltip_text(Some("Re-run command"));
        let filter_btn = gtk4::Button::with_label("\u{f0b0}"); // nf-fa-filter  filter output
        filter_btn.set_tooltip_text(Some("Filter output"));
        // Expand button: appears only when output_rows > viewport_cap; toggles
        // the output VTE between the capped height and a roomier expanded height
        // (`finished_block_max_expanded_rows`). Wired below once output_rows and
        // the output VTE exist.
        let expand_btn = gtk4::Button::with_label("\u{f065}"); // nf-fa-expand
        expand_btn.set_tooltip_text(Some("Expand block"));
        for btn in [
            &copy_cmd_btn,
            &copy_output_btn,
            &rerun_btn,
            &filter_btn,
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
            // Keep the quick actions visible while the block is selected so they
            // stay reachable without re-hovering.
            if !outer_for_leave.has_css_class("block-selected") {
                action_box_for_leave.set_visible(false);
            }
        });
        outer.add_controller(hover_ctrl);

        // Collapse toggle button
        let collapse_btn = gtk4::Button::with_label("\u{f078}"); // nf-fa-chevron_down
        collapse_btn.add_css_class("block-collapse-btn");
        collapse_btn.add_css_class("flat");
        header_row.append(&collapse_btn);

        outer.append(&header_row);

        // ── VTE-rendered command + output ─────────────────────────────────
        // Command VTE: single-row read-only renderer for the executed command.
        let cmd_bytes: Vec<u8> = match cmd_ansi {
            Some(ansi) if !ansi.is_empty() && !cmd.is_empty() => ansi.as_bytes().to_vec(),
            _ if cmd.is_empty() => b"(empty)".to_vec(),
            _ => highlight_command_to_ansi(cmd).into_bytes(),
        };
        // Command typically fits one line; allow a few in case of multiline pastes.
        let cmd_rows = cmd_bytes.iter().filter(|&&b| b == b'\n').count().max(0) as i64 + 1;
        let command_vte = create_finished_terminal(config, cols, cmd_rows.max(1), 5);
        // Defer feeds until the widget is actually mapped — VTE's internal
        // grid resize from set_size() doesn't take effect until the widget is
        // realized, so feeding immediately wraps content at a smaller default
        // width (the ls-output misalignment bug). connect_map fires once the
        // widget has been allocated, when the grid actually matches set_size.
        // One-shot: re-mapping during scroll must not re-feed.
        {
            let cmd_bytes_for_map = cmd_bytes.clone();
            let cols_for_map = cols.max(1);
            let cmd_rows_for_map = cmd_rows.max(1).min(5);
            let fed = Cell::new(false);
            command_vte.connect_map(move |w| {
                if fed.get() {
                    return;
                }
                fed.set(true);
                w.set_size(cols_for_map, cmd_rows_for_map);
                w.feed(&cmd_bytes_for_map);
            });
        }

        // Output VTE: full output fed once on first map; rows beyond the
        // viewport cap live in this widget's scrollback so the user can
        // scroll inside the block. When the block scrolls out of view it's
        // unmapped — at that point we reset the VTE's grid + scrollback so
        // its per-widget buffer memory is reclaimed. A subsequent map (block
        // scrolls back in) re-feeds from `displayed_output`, which the filter
        // path keeps in sync with whatever the user wants to see.
        let full_output: Rc<RefCell<String>> = Rc::new(RefCell::new(output.to_string()));
        let displayed_output: Rc<RefCell<String>> = Rc::new(RefCell::new(output.to_string()));
        let output_rows = output_row_count(output);
        let output_vte = create_finished_terminal(config, cols, output_rows, viewport_cap);
        let initial_visible_rows = output_rows.min(viewport_cap).max(1);
        output_vte
            .set_height_request(initial_visible_rows as i32 * estimated_cell_height_px(config));
        // Tracks whether the user has toggled this block to its expanded
        // height. Survives unmap/remap so re-feeding picks the right cap.
        let expanded: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        {
            let cols_for_map = cols.max(1);
            let cap_for_map = viewport_cap;
            let max_for_map = max_expanded_cap;
            let displayed_for_map = displayed_output.clone();
            let expanded_for_map = expanded.clone();
            output_vte.connect_map(move |w| {
                let text = displayed_for_map.borrow();
                let rows = output_row_count(&text);
                let cap = if expanded_for_map.get() {
                    max_for_map
                } else {
                    cap_for_map
                };
                let visible_rows = rows.min(cap).max(1);
                render_bytes_into_finished_vte(w, text.as_bytes(), cols_for_map, rows, cap);
                // Pin a minimum pixel height so GTK's vertical Box layout cannot
                // shrink this VTE below what set_size requested. Without this,
                // finished VTEs can be allocated at ~1 row and VTE scrolls their
                // content into internal scrollback. Do not clear on unmap: GTK
                // virtual scrolling and ordinary layout churn can unmap visible
                // blocks transiently, and clearing there loses output if a later
                // remap is skipped or coalesced.
                let ch = w.char_height() as i32;
                if ch > 0 {
                    w.set_height_request((visible_rows as i32) * ch);
                }
            });
        }

        // Show the expand toggle only when there's content beyond the cap.
        // Click swaps the output VTE between capped and expanded heights and
        // updates the icon (expand ↔ compress). The map handler reads the
        // shared `expanded` flag so a re-feed after scroll-off/on respects it.
        if output_rows > viewport_cap {
            let expand_for_btn = expanded.clone();
            let output_vte_for_btn = output_vte.clone();
            let displayed_for_btn = displayed_output.clone();
            let cols_for_btn = cols.max(1);
            expand_btn.connect_clicked(move |btn| {
                let now_expanded = !expand_for_btn.get();
                expand_for_btn.set(now_expanded);
                let cap = if now_expanded {
                    max_expanded_cap
                } else {
                    viewport_cap
                };
                let rows = output_row_count(&displayed_for_btn.borrow());
                let visible_rows = rows.min(cap).max(1);
                output_vte_for_btn.set_size(cols_for_btn, visible_rows);
                let ch = output_vte_for_btn.char_height() as i32;
                if ch > 0 {
                    output_vte_for_btn.set_height_request((visible_rows as i32) * ch);
                }
                btn.set_label(if now_expanded { "\u{f066}" } else { "\u{f065}" });
                btn.set_tooltip_text(Some(if now_expanded {
                    "Collapse to default height"
                } else {
                    "Expand block"
                }));
            });
        } else {
            expand_btn.set_visible(false);
        }

        // Command row: Warp-style accent prompt chevron + the command VTE.
        let cmd_row = gtk4::Box::new(Orientation::Horizontal, 0);
        let chevron = gtk4::Label::new(Some("\u{276f}")); // ❯
        chevron.add_css_class("block-prompt-chevron");
        chevron.set_valign(gtk4::Align::Start);
        cmd_row.append(&chevron);
        cmd_row.append(&command_vte);

        outer.append(&cmd_row);
        outer.append(&output_vte);

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
        if !has_output {
            output_vte.set_visible(false);
            collapse_btn.set_sensitive(false);
            collapse_btn.set_tooltip_text(Some("No output"));
        } else {
            collapse_btn.set_tooltip_text(Some(&format!(
                "Toggle output ({})",
                line_count_text(output_rows)
            )));
        }
        // Wire collapse button to toggle output visibility.
        let output_vte_for_collapse = output_vte.clone();
        collapse_btn.connect_clicked(move |btn| {
            let visible = output_vte_for_collapse.is_visible();
            output_vte_for_collapse.set_visible(!visible);
            btn.set_label(if visible { "\u{f054}" } else { "\u{f078}" }); // chevron right / down
        });
        if !has_output {
            collapse_btn.set_label("\u{f054}"); // nf-fa-chevron_right
        }

        // Per-block output filter (Warp's BlockFilterQuery): the funnel button in
        // the action box toggles a compact row that narrows the output to lines
        // matching the query, honoring regex / case / invert / context-lines.
        {
            let filter_row = gtk4::Box::new(Orientation::Horizontal, 4);
            filter_row.add_css_class("block-filter-row");
            filter_row.set_visible(false);
            filter_row.set_margin_start(12);
            filter_row.set_margin_end(8);
            filter_row.set_margin_top(2);
            filter_row.set_margin_bottom(2);

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

            outer.append(&filter_row);
            outer.reorder_child_after(&filter_row, Some(&cmd_row));

            let apply = {
                let output_vte = output_vte.clone();
                let full_output = full_output.clone();
                let displayed_output = displayed_output.clone();
                let filter_entry = filter_entry.clone();
                let regex_tg = regex_tg.clone();
                let case_tg = case_tg.clone();
                let invert_tg = invert_tg.clone();
                let ctx_spin = ctx_spin.clone();
                let filter_status = filter_status.clone();
                let expand_btn = expand_btn.clone();
                let expanded = expanded.clone();
                let filter_btn = filter_btn.clone();
                move || {
                    let q = filter_entry.text().to_string();
                    let full = full_output.borrow();
                    let full_rows = output_row_count(&full);
                    let shown = if q.is_empty() {
                        full.to_string()
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
                    let shown_rows = output_row_count(&shown);
                    let active_cap = if expanded.get() {
                        max_expanded_cap
                    } else {
                        viewport_cap
                    };
                    render_bytes_into_finished_vte(
                        &output_vte,
                        shown.as_bytes(),
                        cols,
                        shown_rows,
                        active_cap,
                    );
                    let ch = output_vte.char_height() as i32;
                    if ch > 0 {
                        output_vte
                            .set_height_request((shown_rows.min(active_cap).max(1) as i32) * ch);
                    }
                    let has_query = !q.trim().is_empty();
                    if has_query {
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
                    expand_btn.set_visible(shown_rows > viewport_cap);
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

            let filter_row_for_btn = filter_row.clone();
            let entry_for_btn = filter_entry.clone();
            let apply_for_btn = apply.clone();
            let filter_btn_for_toggle = filter_btn.clone();
            filter_btn.connect_clicked(move |_| {
                let show = !filter_row_for_btn.is_visible();
                filter_row_for_btn.set_visible(show);
                if show {
                    filter_btn_for_toggle.add_css_class("block-action-active");
                    entry_for_btn.grab_focus();
                } else {
                    filter_btn_for_toggle.remove_css_class("block-action-active");
                    entry_for_btn.set_text("");
                    apply_for_btn();
                }
            });
        }

        FinishedBlock {
            id,
            widget: outer,
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
            bookmark_star,
            status_icon,
            cols,
            viewport_cap,
        }
    }

    pub(crate) fn widget(&self) -> &gtk4::Box {
        &self.widget
    }

    /// Forward wheel events on the output VTE to the outer ScrolledWindow once
    /// the VTE's internal scrollback can't move further in the wheel direction.
    /// Without this the user's scroll "sticks" at a long block's edge: VTE
    /// silently swallows wheels that no longer scroll its own buffer, and the
    /// page never resumes. Closes the perceptual gap with a single-scrollback
    /// VTE pane (terminator/xterm).
    pub(crate) fn connect_scroll_forwarding(&self, outer: &gtk4::ScrolledWindow) {
        let scroll_ctrl =
            gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        // Bubble phase: VTE's own controller runs first and consumes the event
        // when it can scroll. We only see what's left over.
        let vte = self.output_vte.clone();
        let outer = outer.clone();
        scroll_ctrl.connect_scroll(move |_, _dx, dy| {
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
            let outer_adj = outer.vadjustment();
            let step = outer_adj.step_increment().max(outer_adj.page_size() * 0.1);
            let target = (outer_adj.value() + dy * step)
                .clamp(outer_adj.lower(), outer_adj.upper() - outer_adj.page_size());
            outer_adj.set_value(target);
            glib::Propagation::Stop
        });
        self.output_vte.add_controller(scroll_ctrl);
    }

    /// Wire the hover quick-action buttons (copy command, copy output, re-run).
    /// Kept separate from construction because handlers need the clipboard, PTY,
    /// and active block, which only the owning `TermView` has.
    pub(crate) fn connect_actions(
        &self,
        vte: &Terminal,
        pty: &Rc<crate::pty::OwnedPty>,
        pty_synced: &Rc<Cell<bool>>,
        active: &Rc<RefCell<ActiveBlock>>,
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
        self.copy_output_btn.connect_clicked(move |btn| {
            let text = strip_ansi(&full_output_for_copy.borrow());
            vte_for_out.clipboard().set_text(&text);
            flash_button_label(btn, "\u{f00c}", "Output copied");
        });

        let pty_for_rerun = Rc::clone(pty);
        let pty_synced_for_rerun = pty_synced.clone();
        let active_for_rerun = active.clone();
        let cmd_for_rerun = self.cmd_text.clone();
        self.rerun_btn.connect_clicked(move |btn| {
            // Clear any partial line at the live prompt (Ctrl+U) then type the
            // command bytes into the shell, leaving the user to press Enter
            // (jterm1 rerun model).
            if pty_synced_for_rerun.get() {
                pty_for_rerun.write_bytes(b"\x15");
            }
            pty_for_rerun.write_bytes(cmd_for_rerun.as_bytes());
            pty_synced_for_rerun.set(true);
            active_for_rerun.borrow().grab_focus();
            flash_button_label(btn, "\u{f00c}", "Command inserted");
        });
    }
}

// ─── ActiveBlock ──────────────────────────────────────────────────────────────

/// The live area: a single persistent input-enabled VTE pinned to the viewport
/// height (jterm1 model). The shell's prompt, the user's typing, and command
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
        // focusable(false) keeps the holder Box from being a focus target, but we
        // must NOT set can_focus(false): in GTK4 that blocks all descendants
        // (including active_vte) from ever receiving focus.
        widget.set_focusable(false);
        widget.set_hexpand(true);
        // NOT vexpand: the input cell hugs its content (warp model). Its exact
        // height is driven by `update_input_height` in block_view/mod.rs via
        // height_request. With vexpand the cell would fill the whole viewport
        // regardless of the requested height.
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
    /// and SGR state is soft-reset). This mirrors a traditional VTE where PageUp
    /// at a prompt reveals the previous command's output tail. The default (false)
    /// wipes the live VTE on every PromptStart, since the finished blocks above
    /// already hold the authoritative scrollback.
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
