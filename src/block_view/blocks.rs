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
        let output_buffer_for_copy = self.output_buffer.clone();
        self.copy_output_btn.connect_clicked(move |_| {
            let text = output_buffer_for_copy.text(
                &output_buffer_for_copy.start_iter(),
                &output_buffer_for_copy.end_iter(),
                true,
            );
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
        if !self.output_vte.is_visible() {
            self.output_vte.set_visible(true);
        }
        // Sync column count with actual widget width to avoid line wrap mismatch
        let char_width = self.output_vte.char_width();
        let widget_width = self.output_vte.allocated_width() as i64;
        let mut cols = self.output_vte.column_count();
        if char_width > 0 && widget_width > 0 {
            let actual_cols = (widget_width / char_width).max(40);
            if actual_cols != cols {
                cols = actual_cols;
            }
        }
        // Accumulate raw bytes first so we can estimate from total content.
        // Track newline/byte totals incrementally — only scan the NEW chunk — so this
        // stays O(chunk) instead of O(total) on every feed (was quadratic for big output).
        self.raw_output.borrow_mut().extend_from_slice(raw_bytes);
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
        // Cap the inline VTE height. The output VTE only retains a bounded scrollback
        // (see build_output_vte), so growing set_size beyond that just produces blank
        // rows for content VTE no longer holds — and a runaway command (`yes`, a giant
        // build log) would otherwise try to make the widget millions of rows tall,
        // choking layout where a real terminal stays smooth. Normal-sized output is far
        // below this cap, so this is a no-op for typical commands.
        let needed_rows = (total_newlines + wrap_extra + 2).min(MAX_INLINE_OUTPUT_ROWS);
        let current_rows = self.output_vte.row_count();
        if needed_rows > current_rows || cols != self.output_vte.column_count() {
            self.output_vte.set_size(cols, needed_rows.max(current_rows));
            self.output_vte.queue_resize();
        }
        self.output_vte.feed(raw_bytes);
    }

    pub(crate) fn flush_output(&self) {
        // VTE renders immediately on feed(), no flush needed
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
    }

    pub(crate) fn clear_output(&self) {
        self.raw_output.borrow_mut().clear();
        self.output_newlines.set(0);
        self.output_bytes.set(0);
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
}
