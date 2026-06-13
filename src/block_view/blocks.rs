//! blocks — extracted from block_view (mechanical split, no logic changes)
use gtk4::gdk::RGBA;
use gtk4::prelude::*;
use gtk4::{glib, EventControllerKey, Orientation, TextBuffer, TextView};
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
        || (0x200B..=0x200F).contains(&cp)  // zero-width space .. RLM
        || cp == 0x200D                      // zero-width joiner
        || (0xFE00..=0xFE0F).contains(&cp)  // variation selectors
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
                let next_stop = (col / 8 + 1) * 8;
                if next_stop > cols {
                    out.push('\n');
                    col = 0;
                } else {
                    for _ in col..next_stop {
                        out.push(' ');
                    }
                    col = next_stop;
                }
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
    pub(crate) prompt_view: gtk4::TextView,
    pub(crate) prompt_buffer: gtk4::TextBuffer,
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
            copy_cmd_btn: self.copy_cmd_btn.clone(),
            copy_output_btn: self.copy_output_btn.clone(),
            rerun_btn: self.rerun_btn.clone(),
        }
    }
}

impl FinishedBlock {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
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
        Self::new_with_pool(prompt, cmd, cmd_ansi, output, exit_code, config, duration_ms, end_time_ms, cwd, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_pool(
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

        // Status icon: ✓ for success, ✗ for failure.
        // Nerd Font glyphs: nf-fa-check () on success, nf-fa-times () on failure.
        let status_icon = gtk4::Label::new(Some(if exit_code == 0 { "\u{f00c}" } else { "\u{f00d}" }));
        status_icon.add_css_class(if exit_code == 0 { "block-status-ok" } else { "block-status-bad" });
        status_icon.set_halign(gtk4::Align::Start);
        header_row.append(&status_icon);

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
        for btn in [&copy_cmd_btn, &copy_output_btn, &rerun_btn] {
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
            action_box_for_leave.set_visible(false);
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

        let (prompt_view, prompt_buffer) = create_textview("block-prompt-view");
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

        // Populate buffers
        set_active_prompt_buffer(&prompt_buffer, prompt);

        // Render the command line with the shell's own ANSI syntax highlighting
        // when it's available; otherwise fall back to plain text.
        match cmd_ansi {
            Some(ansi) if !ansi.is_empty() && !cmd.is_empty() => {
                set_active_output_buffer(&command_buffer, ansi, &config.palette, None);
            }
            _ => {
                let cmd_display = if cmd.is_empty() { "(empty)" } else { cmd };
                command_buffer.set_text(cmd_display);
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
            copy_cmd_btn,
            copy_output_btn,
            rerun_btn,
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
            let active = active_for_rerun.borrow();
            *active.pending_cmd.borrow_mut() = cmd_for_rerun.clone();
            active.cursor_offset.set(cmd_for_rerun.chars().count());
            if pty_synced_for_rerun.get() {
                pty_for_rerun.write_bytes(b"\x15");
            }
            pty_for_rerun.write_bytes(cmd_for_rerun.as_bytes());
            pty_synced_for_rerun.set(true);
            *active.pending_suggestion.borrow_mut() = String::new();
            active.cursor_visible.set(true);
            active.update_content_view();
            active.command_view.grab_focus();
        });
    }
}

// ─── ActiveBlock ──────────────────────────────────────────────────────────────

pub(crate) struct ActiveBlock {
    pub(crate) widget: gtk4::Box,
    pub(crate) prompt_buffer: gtk4::TextBuffer,
    pub(crate) command_view: gtk4::TextView,
    pub(crate) command_buffer: gtk4::TextBuffer,
    pub(crate) output_vte: Terminal,
    pub(crate) raw_output: Rc<RefCell<Vec<u8>>>,
    // Incremental counters mirroring raw_output, so feed_output never rescans the
    // whole accumulated buffer (which made large outputs quadratic).
    pub(crate) output_newlines: Rc<Cell<i64>>,
    pub(crate) output_bytes: Rc<Cell<i64>>,
    // High-water mark of grid rows the output has actually occupied (from VTE's
    // cursor row). Drives the exact widget height so the running block is flush
    // with its content like a normal terminal, instead of the byte-estimate that
    // over-grows (badly so for carriage-return progress bars with no newlines).
    pub(crate) output_max_row: Rc<Cell<i64>>,
    pub(crate) pending_cmd: Rc<RefCell<String>>,        // User input only
    pub(crate) pending_preedit: Rc<RefCell<String>>,    // IME composing text
    pub(crate) pending_suggestion: Rc<RefCell<String>>, // Shell suggestion/autocomplete
    pub(crate) cursor_visible: Rc<Cell<bool>>, // For blinking cursor animation
    pub(crate) cursor_offset: Rc<Cell<usize>>, // Cursor position in chars (editor mode)
    // True while a command is executing. The live cursor then belongs to the
    // output VTE, so the input line's blinking cursor is suppressed to match
    // VTE's single-cursor behaviour (otherwise two cursors show at once).
    pub(crate) command_running: Rc<Cell<bool>>,
    pub(crate) cursor_color: RGBA,
    pub(crate) cursor_foreground: RGBA,
    pub(crate) cwd_label: gtk4::Label,
    pub(crate) running_label: gtk4::Label,
    pub(crate) running_timer_handle: Rc<RefCell<Option<glib::SourceId>>>,
    pub(crate) blink_timer_handle: Rc<RefCell<Option<glib::SourceId>>>,
    // The exact column count last pushed to the PTY by the resize tick — the single
    // source of truth the running program (and a real terminal) wraps at. The live
    // output VTE is sized to this, and finished blocks are pre-wrapped at it, so the
    // grid, the PTY, and the completed render all agree. 0 until the first tick.
    pub(crate) pty_cols: Rc<Cell<u16>>,
}

impl ActiveBlock {
    pub(crate) fn new(_batch_min_ms: u32, _batch_max_ms: u32, config: &Config) -> Self {
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
        // The input row gets its leading indent from the accent prompt chevron, so
        // drop the command view's own left margin to avoid a double indent.
        command_view.set_left_margin(2);

        // Accent prompt chevron (❯), Warp-style, marking the live input line.
        let prompt_chevron = gtk4::Label::new(Some("\u{276f}"));
        prompt_chevron.add_css_class("block-prompt-chevron");
        prompt_chevron.set_valign(gtk4::Align::Start);
        let cmd_row = gtk4::Box::new(Orientation::Horizontal, 0);
        cmd_row.append(&prompt_chevron);
        cmd_row.append(&command_view);

        // Running timer label (shown during command execution)
        let running_label = gtk4::Label::new(None);
        running_label.add_css_class("block-running-label");
        running_label.set_halign(gtk4::Align::End);
        running_label.set_hexpand(false);
        running_label.set_visible(false);

        // Header row: cwd on the left, running timer on the right — mirrors the
        // finished block header so the active block reads consistently.
        let cwd_label = gtk4::Label::new(None);
        cwd_label.add_css_class("block-header-label");
        cwd_label.set_halign(gtk4::Align::Start);
        cwd_label.set_ellipsize(gtk4::pango::EllipsizeMode::Start);
        cwd_label.set_max_width_chars(40);

        let header_row = gtk4::Box::new(Orientation::Horizontal, 8);
        header_row.add_css_class("block-header");
        header_row.set_margin_start(12);
        header_row.set_margin_end(8);
        header_row.set_margin_top(6);
        header_row.set_margin_bottom(2);
        header_row.append(&cwd_label);
        let header_spacer = gtk4::Box::new(Orientation::Horizontal, 0);
        header_spacer.set_hexpand(true);
        header_row.append(&header_spacer);
        header_row.append(&running_label);

        // Output: use VTE widget for full terminal compatibility
        let output_vte = build_output_vte(config);
        output_vte.set_visible(false); // Hidden until there's output

        // Append to widget
        widget.append(&header_row);
        widget.append(&prompt_view);
        widget.append(&cmd_row);
        widget.append(&output_vte);

        // Grab focus on command_view when realized
        let command_view_clone = command_view.clone();
        command_view.connect_realize(move |_| {
            command_view_clone.grab_focus();
        });

        let cursor_visible = Rc::new(Cell::new(true));
        let cursor_offset: Rc<Cell<usize>> = Rc::new(Cell::new(0));
        let command_running = Rc::new(Cell::new(false));
        let pending_cmd = Rc::new(RefCell::new(String::new()));
        let pending_preedit = Rc::new(RefCell::new(String::new()));
        let pending_suggestion = Rc::new(RefCell::new(String::new()));

        let blink_timer_handle = Rc::new(RefCell::new(None::<glib::SourceId>));
        {
            // Manual cursor blink animation (both editor and non-editor modes)
            let cursor_visible_clone = cursor_visible.clone();
            let cursor_offset_clone = cursor_offset.clone();
            let command_running_clone = command_running.clone();
            let command_buffer_clone = command_buffer.clone();
            let command_view_for_timer = command_view.clone();
            let pending_cmd_clone = pending_cmd.clone();
            let pending_preedit_clone = pending_preedit.clone();
            let pending_suggestion_clone = pending_suggestion.clone();
            let cursor_color_for_timer = config.cursor;
            let cursor_foreground_for_timer = config.cursor_foreground;

            let handle = glib::timeout_add_local(std::time::Duration::from_millis(530), move || {
                // While a command is executing the live cursor lives in the output
                // VTE. Match VTE's single-cursor behaviour by hiding the input
                // cursor: draw the command text once without a cursor, then idle
                // until the command finishes (cursor_visible is restored by
                // reset_for_next_prompt).
                if command_running_clone.get() {
                    if !cursor_visible_clone.get() {
                        return glib::ControlFlow::Continue;
                    }
                    cursor_visible_clone.set(false);
                } else {
                    // Match VTE: when the toplevel window is not focused, show a steady
                    // (non-blinking) cursor and skip the buffer rebuild entirely once it's
                    // solid — no point burning redraws on a blink nobody can see.
                    let window_active = command_view_for_timer
                        .root()
                        .and_then(|r| r.downcast::<gtk4::Window>().ok())
                        .map(|w| w.is_active())
                        .unwrap_or(true);
                    if !window_active {
                        if cursor_visible_clone.get() {
                            return glib::ControlFlow::Continue;
                        }
                        cursor_visible_clone.set(true);
                    } else {
                        cursor_visible_clone.set(!cursor_visible_clone.get());
                    }
                }

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
            *blink_timer_handle.borrow_mut() = Some(handle);

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
            prompt_buffer,
            command_view,
            command_buffer,
            output_vte,
            raw_output: Rc::new(RefCell::new(Vec::new())),
            output_newlines: Rc::new(Cell::new(0)),
            output_bytes: Rc::new(Cell::new(0)),
            output_max_row: Rc::new(Cell::new(0)),
            pending_cmd,
            pending_preedit,
            pending_suggestion,
            cursor_visible,
            cursor_offset,
            command_running,
            cursor_color: config.cursor,
            cursor_foreground: config.cursor_foreground,
            cwd_label,
            running_label,
            running_timer_handle: Rc::new(RefCell::new(None)),
            blink_timer_handle,
            pty_cols: Rc::new(Cell::new(0)),
        }
    }

    pub(crate) fn cancel_blink_timer(&self) {
        if let Some(handle) = self.blink_timer_handle.borrow_mut().take() {
            handle.remove();
        }
    }

    pub(crate) fn set_prompt(&self, text: &str) {
        set_active_prompt_buffer(&self.prompt_buffer, text);
    }

    pub(crate) fn update_cwd(&self, cwd: &str) {
        if cwd.is_empty() {
            self.cwd_label.set_visible(false);
        } else {
            self.cwd_label.set_text(&shorten_path(cwd));
            self.cwd_label.set_visible(true);
        }
    }

    pub(crate) fn set_cmd(&self, text: &str) {
        log::debug!("set_cmd: text={:?}", text);
        *self.pending_cmd.borrow_mut() = text.to_string();
        self.pending_preedit.borrow_mut().clear();
        *self.pending_suggestion.borrow_mut() = String::new();
        self.cursor_offset.set(text.chars().count());
        self.update_content_view();
    }

    pub(crate) fn set_preedit(&self, text: &str) {
        *self.pending_preedit.borrow_mut() = text.to_string();
        self.update_content_view();
    }

    pub(crate) fn set_cmd_parts(&self, user_part: &str, suggestion_part: &str) {
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

    pub(crate) fn update_content_view(&self) {
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

        // Suppress the input cursor while a command runs: the live cursor then
        // belongs to the output VTE (see command_running).
        let cursor_visible = self.cursor_visible.get() && !self.command_running.get();

        set_active_command_buffer_at(
            &self.command_buffer,
            &cmd,
            &preedit,
            cursor_visible,
            &suggestion,
            &self.cursor_color,
            &self.cursor_foreground,
            explicit_pos,
        );
    }

    pub(crate) fn feed_output(&self, raw_bytes: &[u8]) {
        let _pf = if super::prof_enabled() { Some(std::time::Instant::now()) } else { None };
        if !self.output_vte.is_visible() {
            self.output_vte.set_visible(true);
        }
        // Derive the wrap width from the BLOCK CONTAINER's allocation, NOT the
        // output VTE's own. feed_output runs in the PTY reader callback, before
        // the layout pass that allocates the just-shown VTE, so the output VTE
        // still reports its pre-realization ~2px allocation here — and its
        // column_count stays pinned at VTE's 80-column default. Sizing the grid
        // from that stale 80 made every long line wrap at 80 while the shell was
        // told the real (narrower) PTY width by the resize tick, so raw output
        // wider than the screen wrapped in the wrong place. The block container
        // is realized and stable, so (container_w - chrome) / char_w yields the
        // SAME column count the tick sends to the PTY — keeping grid == PTY, the
        // way a real terminal does.
        let char_width = self.output_vte.char_width();
        let container_width = self.widget.allocated_width() as i64;
        let mut cols = self.output_vte.column_count();
        let _dbg_colcount = cols;
        // Use the exact PTY width the resize tick committed, so the live grid wraps
        // where the program (and a real terminal) does. Before the first tick, fall
        // back to the container-derived estimate.
        let pty_cols = self.pty_cols.get() as i64;
        if pty_cols >= 20 {
            cols = pty_cols;
        } else if char_width > 0 && container_width > char_width * 40 {
            cols = ((container_width - super::OUTPUT_GRID_CHROME_PX) / char_width).max(20);
        }
        if std::env::var("JT_WDBG").is_ok() {
            eprintln!("[WDBG feed] column_count={} char_w={} container_w={} -> cols={} (first={})",
                _dbg_colcount, char_width, container_width, cols, raw_bytes.len() < 4000 && self.output_bytes.get()==0);
        }
        // Accumulate raw bytes first so we can estimate from total content.
        // Track newline/byte totals incrementally — only scan the NEW chunk — so this
        // stays O(chunk) instead of O(total) on every feed (was quadratic for big output).
        {
            let mut buf = self.raw_output.borrow_mut();
            buf.extend_from_slice(raw_bytes);
            // Bound memory for runaway output: keep only the most recent tail.
            if buf.len() > super::MAX_RAW_OUTPUT_BYTES {
                let drop = buf.len() - super::MAX_RAW_OUTPUT_BYTES;
                buf.drain(..drop);
            }
        }
        let new_newlines = raw_bytes.iter().filter(|&&b| b == b'\n').count() as i64;
        let total_newlines = self.output_newlines.get() + new_newlines;
        self.output_newlines.set(total_newlines);
        let total_bytes = self.output_bytes.get() + raw_bytes.len() as i64;
        self.output_bytes.set(total_bytes);
        let wrap_extra = if cols > 0 {
            total_bytes / cols
        } else {
            0
        };
        // Pre-size the grid to a GENEROUS upper bound before feeding. Because bytes
        // include ANSI escapes and UTF-8 continuation bytes, total_bytes/cols can
        // only ever over-count the real number of wrapped rows, so this guarantees
        // all freshly fed content lands in the visible grid (never in scrollback) —
        // which is what makes the cursor-row read below an accurate height. Capped
        // at MAX_INLINE_OUTPUT_ROWS (the output VTE's scrollback bound) so a runaway
        // command can't try to make the widget millions of rows tall.
        let upper = (total_newlines + wrap_extra + 2).min(MAX_INLINE_OUTPUT_ROWS);
        let current_rows = self.output_vte.row_count();
        if upper > current_rows || cols != self.output_vte.column_count() {
            self.output_vte.set_size(cols, upper.max(current_rows));
        }
        self.output_vte.feed(raw_bytes);

        // Trim to the ACTUAL content height. VTE parses feed() synchronously, and
        // since the grid was sized to `upper` >= content, the whole output is in the
        // grid and the cursor row is the high-water mark of rows used. A real
        // terminal shows output flush with no trailing blank rows; the byte estimate
        // over-grows the block (badly for carriage-return progress bars that emit
        // megabytes with no newlines), so collapse the grid back to what's used.
        let (_ccol, crow) = self.output_vte.cursor_position();
        let high = self.output_max_row.get().max(crow + 1);
        self.output_max_row.set(high);
        // Floor at the newline count (a hard lower bound for line-oriented output) so
        // a program that parks the cursor higher up can't shrink the grid under real
        // content. Wrapped output exceeds this floor and is captured by `high`.
        let needed_rows = high.max(total_newlines + 1).min(MAX_INLINE_OUTPUT_ROWS);
        if needed_rows != self.output_vte.row_count() {
            self.output_vte.set_size(cols, needed_rows);
        }
        self.output_vte.queue_resize();
        if let Some(t) = _pf {
            super::prof!("    feed_output: {} bytes in {}us (newlines={}, rows={}, cursor_row={})",
                raw_bytes.len(), t.elapsed().as_micros(), total_newlines, needed_rows, crow);
        }
    }

    pub(crate) fn flush_output(&self) {
        // VTE renders immediately on feed(), no flush needed
    }

    /// The column count the live output VTE is wrapping at — computed exactly the
    /// way `feed_output` derives the grid width (and the way the resize tick sizes
    /// the PTY), so a finished block can be pre-wrapped at the SAME column the user
    /// just watched the running command wrap at. Keeps the finished render byte-for-
    /// byte aligned with the live one (and with a real terminal).
    pub(crate) fn output_grid_cols(&self) -> usize {
        // Prefer the exact PTY width the resize tick last sent — what the program
        // actually wrapped at. Fall back to the container-derived estimate only
        // before the first tick has run.
        let pty_cols = self.pty_cols.get();
        if pty_cols >= 20 {
            return pty_cols as usize;
        }
        let char_width = self.output_vte.char_width();
        let container_width = self.widget.allocated_width() as i64;
        let mut cols = self.output_vte.column_count();
        if char_width > 0 && container_width > char_width * 40 {
            cols = ((container_width - super::OUTPUT_GRID_CHROME_PX) / char_width).max(20);
        }
        cols.max(20) as usize
    }

    pub(crate) fn output_text(&self) -> String {
        let raw = self.raw_output.borrow();
        if raw.is_empty() {
            return String::new();
        }
        String::from_utf8_lossy(&raw).into_owned()
    }

    #[allow(dead_code)]
    pub(crate) fn visible_output_text(&self) -> String {
        let rows = self.output_vte.row_count();
        let cols = self.output_vte.column_count();
        if rows <= 0 || cols <= 0 {
            return String::new();
        }
        let (text, _) = self.output_vte.text_range_format(
            vte4::Format::Text,
            0,
            0,
            rows.saturating_sub(1),
            cols,
        );
        let raw = text.map(|s| s.to_string()).unwrap_or_default();
        // Trim trailing empty lines left by pre-grown VTE rows
        raw.trim_end().to_string()
    }

    pub(crate) fn append_output(&self, text: &str) {
        self.feed_output(text.as_bytes());
    }

    /// Clear the accumulated output buffer and its incremental counters without
    /// touching the VTE widget. Use when discarding transient content (e.g. a
    /// completion menu) that was fed to output_vte but isn't real command output.
    pub(crate) fn reset_output_buffer(&self) {
        self.raw_output.borrow_mut().clear();
        self.output_newlines.set(0);
        self.output_bytes.set(0);
        self.output_max_row.set(0);
    }

    pub(crate) fn clear_output(&self) {
        self.raw_output.borrow_mut().clear();
        self.output_newlines.set(0);
        self.output_bytes.set(0);
        self.output_max_row.set(0);
        self.output_vte.feed(b"\x1b[2J\x1b[H\x1b[3J");
        self.output_vte.reset(true, true);
        let cols = self.output_vte.column_count().max(80);
        self.output_vte.set_size(cols, 1);
        self.output_vte.set_visible(false);
    }

    pub(crate) fn start_command(&self, command: &str) {
        *self.pending_cmd.borrow_mut() = command.to_string();
        self.pending_preedit.borrow_mut().clear();
        self.pending_suggestion.borrow_mut().clear();
        self.command_running.set(true);
        self.clear_output();
        self.command_buffer.set_text("");
    }

    pub(crate) fn start_timer(&self) {
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

    pub(crate) fn stop_timer(&self) {
        if let Some(handle) = self.running_timer_handle.borrow_mut().take() {
            handle.remove();
        }
        self.running_label.set_visible(false);
    }

    pub(crate) fn reset_for_next_prompt(&self) {
        self.stop_timer();
        self.set_prompt("");
        *self.pending_cmd.borrow_mut() = String::new();
        self.pending_preedit.borrow_mut().clear();
        self.pending_suggestion.borrow_mut().clear();
        self.command_running.set(false);
        self.clear_output();
        self.cursor_visible.set(true);
        self.cursor_offset.set(0);
        self.command_buffer.set_text("");
    }

    pub(crate) fn widget(&self) -> &gtk4::Box {
        &self.widget
    }

    pub(crate) fn grab_focus(&self) {
        self.command_view.grab_focus();
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
