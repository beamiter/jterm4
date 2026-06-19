//! blocks — extracted from block_view (mechanical split, no logic changes)
use gtk4::prelude::*;
use gtk4::{Orientation, TextView};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use vte4::Terminal;
use vte4::TerminalExt;
use crate::config::Config;
use crate::terminal::open_uri;
use super::*;


// ─── Cursor Shape ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[derive(Default)]
pub enum TermCursorShape {
    #[default]
    Block,      // 0 or 1: block cursor
    Underline,  // 3 or 4: underline cursor
    Bar,        // 5 or 6: bar/vertical cursor
}


/// Display width of a single character in terminal cells. Coarse but covers the
/// common cases: zero-width combining marks / joiners, double-width CJK & emoji,
/// everything else single-width. Used only to reproduce the terminal's wrap column.
pub(crate) fn char_display_width(c: char) -> usize {
    let cp = c as u32;
    if cp == 0 {
        return 0;
    }
    if (0x0300..=0x036F).contains(&cp)      // combining diacriticals
        || (0x0483..=0x0489).contains(&cp)  // Cyrillic combining
        || (0x0591..=0x05BD).contains(&cp)  // Hebrew points
        || cp == 0x05BF || cp == 0x05C1 || cp == 0x05C2 || cp == 0x05C4 || cp == 0x05C5 || cp == 0x05C7
        || (0x0610..=0x061A).contains(&cp)  // Arabic combining
        || (0x064B..=0x065F).contains(&cp)  // Arabic diacritics
        || cp == 0x0670                      // Arabic superscript alef
        || (0x06D6..=0x06DC).contains(&cp)  // Arabic small high marks
        || (0x06DF..=0x06E4).contains(&cp)
        || (0x06E7..=0x06E8).contains(&cp)
        || (0x06EA..=0x06ED).contains(&cp)
        || (0x0900..=0x0902).contains(&cp)  // Devanagari combining (subset)
        || cp == 0x093C || (0x0941..=0x0948).contains(&cp) || cp == 0x094D
        || (0x0951..=0x0957).contains(&cp)
        || (0x1AB0..=0x1AFF).contains(&cp)  // combining diacriticals extended
        || (0x1DC0..=0x1DFF).contains(&cp)  // combining diacriticals supplement
        || (0x200B..=0x200F).contains(&cp)  // zero-width space .. RLM
        || cp == 0x200D                      // zero-width joiner
        || (0x20D0..=0x20FF).contains(&cp)  // combining marks for symbols
        || (0xFE00..=0xFE0F).contains(&cp)  // variation selectors
        || (0xFE20..=0xFE2F).contains(&cp)  // combining half marks
    {
        return 0;
    }
    if (0x1100..=0x115F).contains(&cp)       // Hangul Jamo
        || (0x2E80..=0xA4CF).contains(&cp)   // CJK radicals .. Yi
        || (0xAC00..=0xD7A3).contains(&cp)   // Hangul syllables
        || (0xF900..=0xFAFF).contains(&cp)   // CJK compatibility ideographs
        || (0xFE30..=0xFE4F).contains(&cp)   // CJK compatibility forms
        || (0xFF00..=0xFF60).contains(&cp)   // fullwidth forms
        || (0xFFE0..=0xFFE6).contains(&cp)   // fullwidth signs
        || (0x1F300..=0x1FAFF).contains(&cp) // emoji & symbols
        || (0x20000..=0x3FFFD).contains(&cp) // CJK ext B+
    {
        return 2;
    }
    1
}

/// Soft-wrap ANSI-bearing text at `cols` display columns, inserting a hard newline
/// at each wrap point. ANSI/OSC escape sequences pass through untouched and don't
/// count toward the column, tabs expand to 8-column stops, and double-width glyphs
/// count as two — exactly matching how the live output VTE (and a real terminal)
/// wrapped the same bytes. The result is rendered in the finished block's TextView
/// with no further reflow, so a completed command keeps the identical line breaks
/// the user just watched, instead of the TextView's own pixel-based wrap column.
pub(crate) fn wrap_ansi_at(input: &str, cols: usize) -> String {
    if cols == 0 {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len() + input.len() / cols + 8);
    let mut col = 0usize;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                out.push(c);
                match chars.peek() {
                    Some('[') => {
                        out.push(chars.next().unwrap());
                        // CSI: consume until a final byte in 0x40..=0x7E.
                        while let Some(&p) = chars.peek() {
                            out.push(chars.next().unwrap());
                            if ('\x40'..='\x7e').contains(&p) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        out.push(chars.next().unwrap());
                        // OSC: consume until BEL or ST (ESC \).
                        while let Some(&p) = chars.peek() {
                            if p == '\x07' {
                                out.push(chars.next().unwrap());
                                break;
                            }
                            if p == '\x1b' {
                                out.push(chars.next().unwrap());
                                if let Some('\\') = chars.peek() {
                                    out.push(chars.next().unwrap());
                                }
                                break;
                            }
                            out.push(chars.next().unwrap());
                        }
                    }
                    Some('(') | Some(')') => {
                        // Charset designation ESC(<f> / ESC)<f>: two more bytes,
                        // zero display width.
                        out.push(chars.next().unwrap());
                        if let Some(f) = chars.next() {
                            out.push(f);
                        }
                    }
                    Some(_) => {
                        out.push(chars.next().unwrap());
                    }
                    None => {}
                }
            }
            // SI / SO (charset shift): zero width.
            '\x0e' | '\x0f' => {
                out.push(c);
            }
            '\n' => {
                out.push('\n');
                col = 0;
            }
            '\r' => {
                out.push('\r');
                col = 0;
            }
            '\t' => {
                // VTE clamps a tab to the right margin rather than wrapping: it fills
                // spaces to the line edge and parks the cursor there; the *next* glyph
                // wraps. Discarding the filler used to make the finished line shorter
                // than the live render. Fill to min(next_stop, cols).
                let next_stop = ((col / 8 + 1) * 8).min(cols);
                for _ in col..next_stop {
                    out.push(' ');
                }
                col = next_stop;
            }
            _ => {
                let w = char_display_width(c);
                if w == 0 {
                    out.push(c);
                } else {
                    if col + w > cols {
                        out.push('\n');
                        col = 0;
                    }
                    out.push(c);
                    col += w;
                }
            }
        }
    }
    out
}

/// Reserve the correct height on a finished block's output TextView *before* it is
/// realized. A freshly appended GtkTextView reports a too-small natural height
/// (~1 line) until its layout is validated on a later frame, which made multi-line
/// output visibly "expand" row by row after the block appeared. Setting an explicit
/// height request makes the natural height correct from the first measure, so the
/// block snaps to full size in one shot.
fn fit_output_height(view: &TextView, display_output: &str, config: &Config) {
    let line_count = display_output.lines().count().max(1) as i32;

    // Mirror css.rs: derive the scaled font (family + size) used for these views.
    let parts: Vec<&str> = config.font_desc.split_whitespace().collect();
    let (family, base_size) = if parts.len() >= 2 {
        if let Ok(size) = parts[parts.len() - 1].parse::<i32>() {
            (parts[..parts.len() - 1].join(" "), size)
        } else {
            (config.font_desc.clone(), 14)
        }
    } else {
        (config.font_desc.clone(), 14)
    };
    let scaled_size = (base_size as f64 * config.default_font_scale).round().max(1.0) as i32;
    let mut font_desc = gtk4::pango::FontDescription::from_string(&family);
    font_desc.set_size(scaled_size * gtk4::pango::SCALE);

    // Measure via a private context that inherits the widget's resolution/DPI.
    let metrics = view.create_pango_context().metrics(Some(&font_desc), None);
    let line_units = if metrics.height() > 0 {
        metrics.height()
    } else {
        metrics.ascent() + metrics.descent()
    };
    // CSS line-height: 1.2 on .block-output-view.
    let per_line = ((line_units as f64 / gtk4::pango::SCALE as f64) * 1.2).ceil() as i32;
    let per_line = per_line.max(1);

    // top + bottom view margins, plus 1px slack against rounding.
    let height = per_line * line_count + 4 + 1;
    view.set_size_request(-1, height);
}

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
    pub(crate) command_view: gtk4::TextView,
    pub(crate) command_buffer: gtk4::TextBuffer,
    pub(crate) output_view: gtk4::TextView,
    pub(crate) output_buffer: gtk4::TextBuffer,
    pub(crate) show_more_btn: Option<gtk4::Button>,
    pub(crate) full_output: Rc<RefCell<String>>,
    pub(crate) cmd_text: String,
    pub(crate) copy_cmd_btn: gtk4::Button,
    pub(crate) copy_output_btn: gtk4::Button,
    pub(crate) rerun_btn: gtk4::Button,
    pub(crate) header_row: gtk4::Box,
    pub(crate) action_box: gtk4::Box,
    pub(crate) bookmark_star: gtk4::Label,
}

impl Clone for FinishedBlock {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            widget: self.widget.clone(),
            prompt_text: self.prompt_text.clone(),
            command_view: self.command_view.clone(),
            command_buffer: self.command_buffer.clone(),
            output_view: self.output_view.clone(),
            output_buffer: self.output_buffer.clone(),
            show_more_btn: self.show_more_btn.clone(),
            cmd_text: self.cmd_text.clone(),
            full_output: self.full_output.clone(),
            copy_cmd_btn: self.copy_cmd_btn.clone(),
            copy_output_btn: self.copy_output_btn.clone(),
            rerun_btn: self.rerun_btn.clone(),
            header_row: self.header_row.clone(),
            action_box: self.action_box.clone(),
            bookmark_star: self.bookmark_star.clone(),
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

impl FinishedBlock {
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
    ) -> Self {
        Self::new_with_pool(id, prompt, cmd, cmd_ansi, output, exit_code, config, duration_ms, end_time_ms, cwd, None)
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
        recycled: Option<gtk4::Box>,
    ) -> Self {
        let view_margin_top = 2;
        let view_margin_bottom = 2;

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
        outer.add_css_class(if exit_code == 0 { "block-success" } else { "block-failed" });

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
        let status_icon = gtk4::Label::new(Some(if exit_code == 0 { "\u{f00c}" } else { "\u{f00d}" }));
        status_icon.add_css_class(if exit_code == 0 { "block-status-ok" } else { "block-status-bad" });
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
        let copy_cmd_btn = gtk4::Button::with_label("\u{f0c5}"); // nf-fa-copy  copy command
        copy_cmd_btn.set_tooltip_text(Some("Copy command"));
        let copy_output_btn = gtk4::Button::with_label("\u{f0ea}"); // nf-fa-clipboard  copy output
        copy_output_btn.set_tooltip_text(Some("Copy output"));
        let rerun_btn = gtk4::Button::with_label("\u{f021}"); // nf-fa-refresh  re-run
        rerun_btn.set_tooltip_text(Some("Re-run command"));
        let filter_btn = gtk4::Button::with_label("\u{f0b0}"); // nf-fa-filter  filter output
        filter_btn.set_tooltip_text(Some("Filter output"));
        for btn in [&copy_cmd_btn, &copy_output_btn, &rerun_btn, &filter_btn] {
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

        let (command_view, command_buffer) = create_textview("block-command-view");
        let (output_view, output_buffer) = create_textview("block-output-view");
        // The live output is rendered in a VTE appended flush to the block's left
        // edge (no indent), while prompt/command keep the 12px indent. Match that
        // here so a finished block's output starts at the same x the user just saw
        // and gets the full container width — its lines are already pre-wrapped at
        // the live grid's column count, so the wider area prevents the TextView's
        // own pixel wrap from re-breaking them.
        output_view.set_left_margin(2);
        output_view.set_right_margin(2);
        // The text is already wrapped at the live grid's exact column, so the view
        // must NOT re-wrap: GtkTextView's pixel-based Char wrap breaks a hair earlier
        // than the VTE cell grid (its glyph advance is slightly wider), which would
        // re-break each full-width line one column early. Disabling wrap keeps the
        // identical line structure the user saw live.
        output_view.set_wrap_mode(gtk4::WrapMode::None);

        // Render the command line with the shell's own ANSI syntax highlighting
        // when it's available; otherwise fall back to plain text.
        match cmd_ansi {
            Some(ansi) if !ansi.is_empty() && !cmd.is_empty() => {
                set_active_output_buffer(&command_buffer, ansi, &config.palette, None);
            }
            _ => {
                if cmd.is_empty() {
                    command_buffer.set_text("(empty)");
                } else {
                    // No shell-provided ANSI echo: apply our own lightweight
                    // syntax highlighting so finished commands look like Warp's.
                    let highlighted = highlight_command_to_ansi(cmd);
                    set_active_output_buffer(&command_buffer, &highlighted, &config.palette, None);
                }
            }
        }

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
        fit_output_height(&output_view, &display_output, config);

        // Add Ctrl+Click handler to open URLs in command and output views
        for (view, buffer) in [(&command_view, &command_buffer), (&output_view, &output_buffer)] {
            let click_controller = gtk4::GestureClick::new();
            click_controller.set_button(1); // left click
            click_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);

            let buffer_clone = buffer.clone();
            let view_clone = view.clone();
            click_controller.connect_pressed(move |controller, n_press, x, y| {
                let (bx, by) = view_clone.window_to_buffer_coords(
                    gtk4::TextWindowType::Widget,
                    x as i32,
                    y as i32,
                );
                let iter = view_clone.iter_at_location(bx, by);
                if n_press == 1 {
                    let state = controller.current_event_state();
                    if state.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
                        if let Some(iter) = iter {
                            if let Some(url) = get_url_at_position(&buffer_clone, &iter) {
                                open_uri(&url);
                                controller.set_state(gtk4::EventSequenceState::Claimed);
                                return;
                            }
                        }
                    }
                } else if n_press == 2 {
                    // Smart selection: grab the whole semantic token (path, URL,
                    // file:line, …) instead of GTK's default plain-word select.
                    if let Some(iter) = iter {
                        if let Some((start, end)) =
                            get_semantic_bounds_at_position(&buffer_clone, &iter)
                        {
                            buffer_clone.select_range(&start, &end);
                            controller.set_state(gtk4::EventSequenceState::Claimed);
                            return;
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

        // Command row: Warp-style accent prompt chevron + the command text.
        // The chevron supplies the left indent, so trim the command view's own
        // left margin to keep them visually adjacent.
        command_view.set_left_margin(2);
        let cmd_row = gtk4::Box::new(Orientation::Horizontal, 0);
        let chevron = gtk4::Label::new(Some("\u{276f}")); // ❯
        chevron.add_css_class("block-prompt-chevron");
        chevron.set_valign(gtk4::Align::Start);
        cmd_row.append(&chevron);
        cmd_row.append(&command_view);

        // Append views to outer box
        outer.append(&cmd_row);
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
            let output_view_clone = output_view.clone();
            let config_clone = config.clone();
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
                    fit_output_height(&output_view_clone, &truncated, &config_clone);
                    let remaining = lines.len() - threshold;
                    btn.set_label(&format!("Show more ({} more lines)", remaining));
                    is_expanded_clone.set(false);
                } else {
                    let full = full_output_clone.borrow();
                    set_active_output_buffer(&output_buffer_clone, &full, &palette, None);
                    fit_output_height(&output_view_clone, &full, &config_clone);
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
            for w in [&regex_tg, &case_tg, &invert_tg] {
                w.add_css_class("flat");
                w.add_css_class("block-filter-toggle");
            }
            filter_row.append(&filter_entry);
            filter_row.append(&regex_tg);
            filter_row.append(&case_tg);
            filter_row.append(&invert_tg);
            filter_row.append(&ctx_spin);

            outer.append(&filter_row);
            outer.reorder_child_after(&filter_row, Some(&cmd_row));

            let apply = {
                let output_buffer = output_buffer.clone();
                let output_view = output_view.clone();
                let config = config.clone();
                let palette = config.palette;
                let full_output = full_output.clone();
                let filter_entry = filter_entry.clone();
                let regex_tg = regex_tg.clone();
                let case_tg = case_tg.clone();
                let invert_tg = invert_tg.clone();
                let ctx_spin = ctx_spin.clone();
                let show_more = show_more_btn.clone();
                move || {
                    let q = filter_entry.text().to_string();
                    let full = full_output.borrow();
                    if q.is_empty() {
                        // Restore the initial (collapsed) view.
                        let lines: Vec<&str> = full.lines().collect();
                        let shown = if lines.len() > threshold {
                            lines[..threshold].join("\n")
                        } else {
                            full.to_string()
                        };
                        set_active_output_buffer(&output_buffer, &shown, &palette, None);
                        fit_output_height(&output_view, &shown, &config);
                        if let Some(ref smb) = show_more {
                            smb.set_visible(lines.len() > threshold);
                            smb.set_label(&format!(
                                "Show more ({} more lines)",
                                lines.len().saturating_sub(threshold)
                            ));
                        }
                    } else {
                        let filtered = filter_output_lines(
                            full.as_str(),
                            &q,
                            regex_tg.is_active(),
                            case_tg.is_active(),
                            invert_tg.is_active(),
                            ctx_spin.value() as usize,
                        );
                        set_active_output_buffer(&output_buffer, &filtered, &palette, None);
                        fit_output_height(&output_view, &filtered, &config);
                        if let Some(ref smb) = show_more {
                            smb.set_visible(false);
                        }
                    }
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
            filter_btn.connect_clicked(move |_| {
                let show = !filter_row_for_btn.is_visible();
                filter_row_for_btn.set_visible(show);
                if show {
                    entry_for_btn.grab_focus();
                } else {
                    entry_for_btn.set_text("");
                    apply_for_btn();
                }
            });
        }

        FinishedBlock {
            id,
            widget: outer,
            prompt_text: prompt.to_string(),
            command_view,
            command_buffer,
            output_view,
            output_buffer,
            show_more_btn,
            full_output,
            cmd_text: cmd.to_string(),
            copy_cmd_btn,
            copy_output_btn,
            rerun_btn,
            header_row,
            action_box,
            bookmark_star,
        }
    }

    pub(crate) fn widget(&self) -> &gtk4::Box {
        &self.widget
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
        self.copy_cmd_btn.connect_clicked(move |_| {
            vte_for_cmd.clipboard().set_text(&cmd_for_copy);
        });

        let vte_for_out = vte.clone();
        // Copy the FULL output (ANSI stripped), not just the collapsed first-N
        // lines shown in output_buffer before "Show more" is clicked.
        let full_output_for_copy = self.full_output.clone();
        self.copy_output_btn.connect_clicked(move |_| {
            let text = strip_ansi(&full_output_for_copy.borrow());
            vte_for_out.clipboard().set_text(&text);
        });

        let pty_for_rerun = Rc::clone(pty);
        let pty_synced_for_rerun = pty_synced.clone();
        let active_for_rerun = active.clone();
        let cmd_for_rerun = self.cmd_text.clone();
        self.rerun_btn.connect_clicked(move |_| {
            // Clear any partial line at the live prompt (Ctrl+U) then type the
            // command bytes into the shell, leaving the user to press Enter
            // (jterm1 rerun model).
            if pty_synced_for_rerun.get() {
                pty_for_rerun.write_bytes(b"\x15");
            }
            pty_for_rerun.write_bytes(cmd_for_rerun.as_bytes());
            pty_synced_for_rerun.set(true);
            active_for_rerun.borrow().grab_focus();
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
    pub(crate) fn reset_active(&self) {
        self.active_vte.reset(true, true);
        self.active_vte.feed(b"\x1b[H\x1b[2J\x1b[3J");
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

#[cfg(test)]
mod char_width_tests {
    use super::char_display_width;

    #[test]
    fn ascii_is_one() {
        assert_eq!(char_display_width('a'), 1);
        assert_eq!(char_display_width('Z'), 1);
        assert_eq!(char_display_width('5'), 1);
    }

    #[test]
    fn cjk_and_emoji_are_two() {
        assert_eq!(char_display_width('中'), 2);
        assert_eq!(char_display_width('한'), 2);
        assert_eq!(char_display_width('\u{1F600}'), 2); // 😀
    }

    #[test]
    fn combining_marks_are_zero() {
        assert_eq!(char_display_width('\u{0301}'), 0); // combining acute accent
        assert_eq!(char_display_width('\u{200D}'), 0); // zero-width joiner
        assert_eq!(char_display_width('\u{0591}'), 0); // Hebrew accent (newly added range)
        assert_eq!(char_display_width('\u{064B}'), 0); // Arabic fathatan (newly added range)
        assert_eq!(char_display_width('\u{FE0F}'), 0); // variation selector-16
    }
}

#[cfg(test)]
mod wrap_ansi_tests {
    use super::wrap_ansi_at;

    #[test]
    fn wraps_at_column_boundary() {
        assert_eq!(wrap_ansi_at("abcdef", 3), "abc\ndef");
    }

    #[test]
    fn zero_cols_is_passthrough() {
        assert_eq!(wrap_ansi_at("abcdef", 0), "abcdef");
    }

    #[test]
    fn ansi_escapes_do_not_count_toward_width() {
        // The SGR sequence is zero-width: the 6 visible chars wrap at col 3.
        let input = "\x1b[31mabcdef\x1b[0m";
        assert_eq!(wrap_ansi_at(input, 3), "\x1b[31mabc\ndef\x1b[0m");
    }

    #[test]
    fn tab_fills_to_stop_not_past_edge() {
        // Tab from col 0 fills to next 8-stop, clamped to cols=5: 5 spaces, then 'x'
        // wraps onto a new line.
        assert_eq!(wrap_ansi_at("\tx", 5), "     \nx");
    }

    #[test]
    fn double_width_glyph_wraps_as_two_columns() {
        // cols=3: '中'(2) + 'a'(1) fills the line, second '中' wraps.
        assert_eq!(wrap_ansi_at("中a中", 3), "中a\n中");
    }
}
