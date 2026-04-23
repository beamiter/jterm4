/// TermView — block-based terminal widget.
///
/// Layout:
///   root (gtk4::Box, Vertical)
///     ├── block_scroll (ScrolledWindow)  — shown in block mode
///     │   └── block_list (gtk4::Box, Vertical)
///     │       ├── finished blocks …
///     │       └── active_block (gtk4::Box, Vertical)
///     │           ├── prompt_row (gtk4::Box, Horizontal)
///     │           │   └── prompt_label
///     │           ├── cmd_row (gtk4::Box, Horizontal)
///     │           │   └── cmd_label
///     │           └── live_view (gtk4::TextView) — live output
///     └── vte_box (gtk4::Box)            — shown in alt-screen mode
///         └── vte4::Terminal + Scrollbar
use gtk4::gdk::RGBA;
use gtk4::pango::FontDescription;
use gtk4::{glib, Orientation, ScrolledWindow, WrapMode};
use gtk4::prelude::*;
use lru::LruCache;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::rc::Rc;
use vte4::{CursorBlinkMode, CursorShape, Terminal};
use vte4::{TerminalExt, TerminalExtManual};
use serde::{Serialize, Deserialize};

use crate::config::Config;
use crate::parser::{Parser, ParserEvent};
use crate::pty::OwnedPty;

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Coalesces repeated scroll requests into a single scroll event.
/// Eliminates cascade of timers and provides smooth scrolling under rapid output.
struct ScrollDebouncer {
    dirty: Rc<Cell<bool>>,
    pending_handle: Rc<RefCell<Option<glib::source::SourceId>>>,
}

impl ScrollDebouncer {
    fn new() -> Self {
        Self {
            dirty: Rc::new(Cell::new(false)),
            pending_handle: Rc::new(RefCell::new(None)),
        }
    }

    fn mark_dirty(&self, scroll: &ScrolledWindow) {
        if self.dirty.get() {
            return;
        }
        self.dirty.set(true);

        let scroll = scroll.clone();
        let dirty = self.dirty.clone();
        let pending = self.pending_handle.clone();

        // Cancel existing timer if present (ignore error if already fired)
        if let Some(handle) = pending.borrow_mut().take() {
            let _ = handle.remove();
        }

        let pending_for_clear = pending.clone();
        let handle = glib::timeout_add_local_once(
            std::time::Duration::from_millis(50),
            move || {
                let adj = scroll.vadjustment();
                let target = adj.upper() - adj.page_size();
                if adj.value() < target {
                    adj.set_value(target);
                }
                dirty.set(false);
                // Clear the handle after firing to prevent double-remove
                pending_for_clear.borrow_mut().take();
            },
        );
        pending.borrow_mut().replace(handle);
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

fn dim_rgba(c: &RGBA, alpha: f32) -> RGBA {
    RGBA::new(c.red(), c.green(), c.blue(), alpha)
}

fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    // CSI sequence: skip until final byte 0x40..0x7e
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() { i += 1; }
                }
                b']' => {
                    // OSC sequence: skip until BEL or ST
                    i += 2;
                    while i < bytes.len() && bytes[i] != 0x07 && bytes[i] != 0x1b {
                        i += 1;
                    }
                    if i < bytes.len() && bytes[i] == 0x07 { i += 1; }
                }
                _ => {
                    // Other ESC sequence: skip ESC + one byte
                    i += 2;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn strip_ansi_cached(input: &str, cache: &std::collections::HashMap<String, String>) -> (String, bool) {
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
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() { i += 1; }
                }
                b']' => {
                    i += 2;
                    while i < bytes.len() && bytes[i] != 0x07 && bytes[i] != 0x1b {
                        i += 1;
                    }
                    if i < bytes.len() && bytes[i] == 0x07 { i += 1; }
                }
                _ => {
                    i += 2;
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
            if count > 0 { count -= 1; }
        }
    }
    input[i..].to_string()
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
                        if param_str.is_empty() { continue; }
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
                            Ok(30..=37) => {
                                let idx = (param_str.parse::<u32>().unwrap() - 30) as usize;
                                let (r, g, b) = ansi256_to_rgb(idx as u8, palette);
                                out.push_str(&format!("<span foreground=\"#{:02x}{:02x}{:02x}\">", r, g, b));
                                open_spans += 1;
                            }
                            Ok(40..=47) => {
                                let idx = (param_str.parse::<u32>().unwrap() - 40) as usize;
                                let (r, g, b) = ansi256_to_rgb(idx as u8, palette);
                                out.push_str(&format!("<span background=\"#{:02x}{:02x}{:02x}\">", r, g, b));
                                open_spans += 1;
                            }
                            Ok(90..=97) => {
                                let idx = (param_str.parse::<u32>().unwrap() - 90 + 8) as u8;
                                let (r, g, b) = ansi256_to_rgb(idx, palette);
                                out.push_str(&format!("<span foreground=\"#{:02x}{:02x}{:02x}\">", r, g, b));
                                open_spans += 1;
                            }
                            Ok(100..=107) => {
                                let idx = (param_str.parse::<u32>().unwrap() - 100 + 8) as u8;
                                let (r, g, b) = ansi256_to_rgb(idx, palette);
                                out.push_str(&format!("<span background=\"#{:02x}{:02x}{:02x}\">", r, g, b));
                                open_spans += 1;
                            }
                            Ok(38) => {
                                let j = params.iter().position(|p| p == param_str).unwrap_or(0);
                                if j + 2 < params.len() {
                                    if params[j + 1] == "5" {
                                        if let Ok(idx) = params[j + 2].parse::<u8>() {
                                            let (r, g, b) = ansi256_to_rgb(idx, palette);
                                            out.push_str(&format!("<span foreground=\"#{:02x}{:02x}{:02x}\">", r, g, b));
                                            open_spans += 1;
                                        }
                                    } else if params[j + 1] == "2" && j + 4 < params.len() {
                                        if let (Ok(r), Ok(g), Ok(b)) = (
                                            params[j + 2].parse::<u8>(),
                                            params[j + 3].parse::<u8>(),
                                            params[j + 4].parse::<u8>(),
                                        ) {
                                            out.push_str(&format!("<span foreground=\"#{:02x}{:02x}{:02x}\">", r, g, b));
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
                                            out.push_str(&format!("<span background=\"#{:02x}{:02x}{:02x}\">", r, g, b));
                                            open_spans += 1;
                                        }
                                    } else if params[j + 1] == "2" && j + 4 < params.len() {
                                        if let (Ok(r), Ok(g), Ok(b)) = (
                                            params[j + 2].parse::<u8>(),
                                            params[j + 3].parse::<u8>(),
                                            params[j + 4].parse::<u8>(),
                                        ) {
                                            out.push_str(&format!("<span background=\"#{:02x}{:02x}{:02x}\">", r, g, b));
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
            // OSC sequence: skip until BEL or ST (ESC \)
            i += 2;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    i += 1;
                    break;
                } else if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                    i += 2;
                    break;
                }
                i += 1;
            }
        } else if bytes[i] == 0x1b && i + 1 < bytes.len() {
            // Skip unknown escape sequences (e.g. ESC + single char like ESC D)
            i += 2;
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
    prompt: String,
    cmd: String,
    cmd_markup: Option<String>,
    output: String,
    exit_code: i32,
    estimated_height: i32,
    line_count: usize,
}

struct FinishedBlock {
    widget: gtk4::Box,
}

impl FinishedBlock {
    fn new(prompt: &str, cmd: &str, cmd_markup: Option<&str>, output: &str, exit_code: i32, _config: &Config) -> Self {

        // Outer frame
        let outer = gtk4::Box::new(Orientation::Vertical, 0);
        outer.add_css_class("block-finished");
        outer.set_margin_top(4);
        outer.set_margin_bottom(4);

        // Prompt row
        let prompt_label = gtk4::Label::new(Some(prompt));
        prompt_label.add_css_class("block-prompt");
        prompt_label.set_xalign(0.0);
        prompt_label.set_selectable(true);
        prompt_label.set_margin_start(12);
        prompt_label.set_margin_top(2);
        prompt_label.set_margin_bottom(0);
        prompt_label.set_single_line_mode(true);
        outer.append(&prompt_label);

        // Command row with wrap support and collapse button
        let cmd_box = gtk4::Box::new(Orientation::Horizontal, 8);
        cmd_box.set_margin_start(12);
        cmd_box.set_margin_top(0);
        cmd_box.set_margin_bottom(0);
        cmd_box.set_spacing(0);

        let cmd_label = gtk4::Label::new(None);
        cmd_label.add_css_class("block-cmd");
        cmd_label.set_xalign(0.0);
        cmd_label.set_hexpand(true);
        cmd_label.set_valign(gtk4::Align::Start);
        cmd_label.set_selectable(true);
        cmd_label.set_wrap(true);
        cmd_label.set_wrap_mode(gtk4::pango::WrapMode::Char);
        if cmd.is_empty() {
            cmd_label.set_text("(empty)");
        } else if let Some(markup) = cmd_markup {
            cmd_label.set_markup(markup);
        } else {
            cmd_label.set_text(cmd);
        }
        cmd_box.append(&cmd_label);

        if exit_code != 0 {
            let badge = gtk4::Label::new(Some(&format!(" {exit_code} ")));
            badge.add_css_class("block-exit-bad");
            badge.set_valign(gtk4::Align::Start);
            cmd_box.append(&badge);
        }

        // Add collapse button if there's output
        let has_output = !output.is_empty();
        if has_output {
            let collapse_btn = gtk4::Button::from_icon_name("go-down-symbolic");
            collapse_btn.add_css_class("flat");
            collapse_btn.set_focus_on_click(false);
            collapse_btn.set_valign(gtk4::Align::Start);
            collapse_btn.set_tooltip_text(Some("Collapse output"));
            cmd_box.append(&collapse_btn);

            // Store collapse state
            unsafe {
                outer.set_data("collapsed", false);
            }

            // Wire collapse button (will connect after output is created)
            let outer_for_collapse = outer.clone();
            collapse_btn.connect_clicked(move |btn| {
                let collapsed = unsafe {
                    outer_for_collapse.data::<bool>("collapsed")
                        .map(|ptr| *ptr.as_ref())
                        .unwrap_or(false)
                };
                let new_state = !collapsed;

                // Toggle visibility of all children except first two (prompt + cmd)
                let mut child_idx = 0;
                let mut child = outer_for_collapse.first_child();
                while let Some(c) = child {
                    if child_idx >= 2 {  // Skip prompt and cmd rows
                        c.set_visible(!new_state);
                    }
                    child = c.next_sibling();
                    child_idx += 1;
                }

                unsafe {
                    outer_for_collapse.set_data("collapsed", new_state);
                }

                if new_state {
                    btn.set_icon_name("go-next-symbolic");
                    btn.set_tooltip_text(Some("Expand output"));
                } else {
                    btn.set_icon_name("go-down-symbolic");
                    btn.set_tooltip_text(Some("Collapse output"));
                }
            });
        }

        outer.append(&cmd_box);

        // Output area (only if there is output)
        if !output.is_empty() {
            let line_count = output.lines().count();
            let byte_size = output.as_bytes().len();

            // Use Label for small outputs (faster rendering), TextView for large ones
            if line_count < 100 && byte_size < 10240 {
                let output_label = gtk4::Label::new(Some(output));
                output_label.set_selectable(true);
                output_label.set_wrap(true);
                output_label.set_wrap_mode(gtk4::pango::WrapMode::Char);
                output_label.set_xalign(0.0);
                output_label.set_margin_start(12);
                output_label.set_margin_end(8);
                output_label.set_margin_top(0);
                output_label.set_margin_bottom(2);
                output_label.add_css_class("block-output");
                outer.append(&output_label);
            } else {
                // Check if output exceeds lazy-load threshold
                let threshold = _config.lazy_load_threshold as usize;
                if line_count > threshold {
                    // Show first 50 lines and last 10 lines with "Show more" button
                    let lines: Vec<&str> = output.lines().collect();
                    let first_50 = lines.iter().take(50).map(|s| *s).collect::<Vec<_>>().join("\n");
                    let last_10: Vec<&str> = lines.iter().rev().take(10).collect();
                    let last_10_text = last_10.into_iter().rev().map(|s| *s).collect::<Vec<_>>().join("\n");

                    let preview = format!("{}\n\n[... {} lines hidden ...]\n\n{}",
                        first_50,
                        line_count - 60,
                        last_10_text);

                    let preview_label = gtk4::Label::new(Some(&preview));
                    preview_label.set_selectable(true);
                    preview_label.set_wrap(true);
                    preview_label.set_wrap_mode(WrapMode::Char);
                    preview_label.add_css_class("monospace");
                    preview_label.set_xalign(0.0);
                    preview_label.set_margin_start(12);
                    preview_label.set_margin_end(8);
                    preview_label.set_margin_top(0);
                    preview_label.set_margin_bottom(2);
                    preview_label.add_css_class("block-output");
                    outer.append(&preview_label);

                    // "Show all" button
                    let show_all_btn = gtk4::Button::with_label(
                        &format!("Show all {} lines", line_count)
                    );
                    show_all_btn.add_css_class("flat");
                    show_all_btn.add_css_class("block-show-more");
                    show_all_btn.set_margin_start(12);
                    show_all_btn.set_margin_top(4);

                    let full_output = output.to_string();
                    let outer_ref = outer.clone();
                    show_all_btn.connect_clicked(move |btn| {
                        // Remove the preview label and button
                        btn.unparent();
                        let first_child = outer_ref.first_child();
                        if let Some(child) = first_child {
                            if child.downcast_ref::<gtk4::Label>().is_some() {
                                if let Some(prev) = child.prev_sibling() {
                                    if let Some(prev_label) = prev.downcast_ref::<gtk4::Label>() {
                                        if prev_label.has_css_class("block-output") {
                                            prev_label.unparent();
                                        }
                                    }
                                }
                            }
                        }

                        // Insert full output TextView
                        let tv = gtk4::TextView::new();
                        tv.set_editable(false);
                        tv.set_cursor_visible(false);
                        tv.set_wrap_mode(WrapMode::Char);
                        tv.set_monospace(true);
                        tv.set_margin_start(12);
                        tv.set_margin_end(8);
                        tv.set_margin_top(0);
                        tv.set_margin_bottom(2);
                        tv.add_css_class("block-output");
                        tv.buffer().set_text(&full_output);
                        outer_ref.append(&tv);
                    });

                    outer.append(&show_all_btn);
                } else {
                    // For large outputs under threshold, just use TextView
                    let tv = gtk4::TextView::new();
                    tv.set_editable(false);
                    tv.set_cursor_visible(false);
                    tv.set_wrap_mode(WrapMode::Char);
                    tv.set_monospace(true);
                    tv.set_margin_start(12);
                    tv.set_margin_end(8);
                    tv.set_margin_top(0);
                    tv.set_margin_bottom(2);
                    tv.add_css_class("block-output");
                    let buf = tv.buffer();
                    buf.set_text(output);
                    outer.append(&tv);
                }
            }
        }

        // Separator line
        let sep_box = gtk4::Separator::new(Orientation::Horizontal);
        outer.append(&sep_box);

        FinishedBlock { widget: outer }
    }

    fn widget(&self) -> &gtk4::Box {
        &self.widget
    }
}

// ─── ActiveBlock ──────────────────────────────────────────────────────────────

struct ActiveBlock {
    widget: gtk4::Box,
    prompt_label: gtk4::Label,
    cmd_label: gtk4::Label,
    output_buf: gtk4::TextBuffer,
    pending_output: Rc<RefCell<String>>,
    flush_pending: Rc<Cell<bool>>,
    // Adaptive batching state
    bytes_since_last_flush: Rc<Cell<usize>>,
    last_flush_time: Rc<Cell<std::time::Instant>>,
    current_batch_ms: Rc<Cell<u32>>,
    config_batch_min: u32,
    config_batch_max: u32,
}

impl ActiveBlock {
    fn new(batch_min_ms: u32, batch_max_ms: u32) -> Self {
        let widget = gtk4::Box::new(Orientation::Vertical, 0);
        widget.add_css_class("block-active");
        widget.set_margin_top(4);
        widget.set_margin_bottom(4);

        // Prompt row
        let prompt_label = gtk4::Label::new(Some(""));
        prompt_label.add_css_class("block-prompt");
        prompt_label.set_xalign(0.0);
        prompt_label.set_margin_start(12);
        prompt_label.set_margin_top(2);
        prompt_label.set_margin_bottom(0);
        prompt_label.set_single_line_mode(true);
        widget.append(&prompt_label);

        // Command row with wrap support
        let cmd_label = gtk4::Label::new(Some(""));
        cmd_label.add_css_class("block-cmd-active");
        cmd_label.set_xalign(0.0);
        cmd_label.set_hexpand(true);
        cmd_label.set_valign(gtk4::Align::Start);
        cmd_label.set_selectable(true);
        cmd_label.set_wrap(true);
        cmd_label.set_wrap_mode(gtk4::pango::WrapMode::Char);
        cmd_label.set_margin_start(12);
        cmd_label.set_margin_top(0);
        cmd_label.set_margin_bottom(2);
        widget.append(&cmd_label);

        // Live output
        let tv = gtk4::TextView::new();
        tv.set_editable(false);
        tv.set_cursor_visible(false);
        tv.set_wrap_mode(WrapMode::Char);
        tv.set_monospace(true);
        tv.set_margin_start(12);
        tv.set_margin_end(8);
        tv.set_margin_top(0);
        tv.set_margin_bottom(2);
        tv.add_css_class("block-output");
        let output_buf = tv.buffer();

        widget.append(&tv);

        let initial_batch_ms = batch_min_ms;

        ActiveBlock {
            widget,
            prompt_label,
            cmd_label,
            output_buf,
            pending_output: Rc::new(RefCell::new(String::new())),
            flush_pending: Rc::new(Cell::new(false)),
            bytes_since_last_flush: Rc::new(Cell::new(0)),
            last_flush_time: Rc::new(Cell::new(std::time::Instant::now())),
            current_batch_ms: Rc::new(Cell::new(initial_batch_ms)),
            config_batch_min: batch_min_ms,
            config_batch_max: batch_max_ms,
        }
    }

    fn set_prompt(&self, text: &str) {
        self.prompt_label.set_text(text);
    }

    fn set_cmd(&self, text: &str) {
        self.cmd_label.set_text(text);
    }

    fn set_cmd_markup(&self, markup: &str) {
        self.cmd_label.set_markup(markup);
    }

    fn append_output(&self, text: &str) {
        let text_len = text.len();
        self.pending_output.borrow_mut().push_str(text);

        // Track throughput for adaptive batching
        self.bytes_since_last_flush.set(self.bytes_since_last_flush.get() + text_len);

        // Schedule flush if not already pending
        if !self.flush_pending.get() {
            self.flush_pending.set(true);
            let pending = self.pending_output.clone();
            let output_buf = self.output_buf.clone();
            let flush_flag = self.flush_pending.clone();
            let bytes_tracker = self.bytes_since_last_flush.clone();
            let last_flush_time = self.last_flush_time.clone();
            let current_batch_ms = self.current_batch_ms.clone();
            let min_ms = self.config_batch_min;
            let max_ms = self.config_batch_max;

            let batch_interval = current_batch_ms.get();
            glib::timeout_add_local_once(
                std::time::Duration::from_millis(batch_interval as u64),
                move || {
                    let text = pending.borrow_mut().drain(..).collect::<String>();
                    if !text.is_empty() {
                        let mut end = output_buf.end_iter();
                        output_buf.insert(&mut end, &text);
                    }

                    // Calculate adaptive batch interval based on throughput
                    let now = std::time::Instant::now();
                    let elapsed = now.duration_since(last_flush_time.get());
                    let elapsed_ms = elapsed.as_millis().max(1) as u64;
                    let bytes = bytes_tracker.get();

                    // throughput in bytes/ms
                    let throughput = bytes as f64 / elapsed_ms as f64;

                    let new_interval = if throughput > 100.0 {
                        // High throughput (>100KB/s) - batch aggressively
                        max_ms
                    } else if throughput < 1.0 {
                        // Low throughput (<1KB/s) - flush quickly for responsiveness
                        min_ms
                    } else {
                        // Linear interpolation between min and max
                        let t = (throughput - 1.0) / 99.0;
                        min_ms + ((max_ms - min_ms) as f64 * t) as u32
                    };

                    current_batch_ms.set(new_interval);
                    last_flush_time.set(now);
                    bytes_tracker.set(0);
                    flush_flag.set(false);
                },
            );
        }
    }

    fn flush_output(&self) {
        let text = self.pending_output.borrow_mut().drain(..).collect::<String>();
        if !text.is_empty() {
            let mut end = self.output_buf.end_iter();
            self.output_buf.insert(&mut end, &text);
        }
        self.flush_pending.set(false);
    }

    fn output_text(&self) -> String {
        self.output_buf.text(
            &self.output_buf.start_iter(),
            &self.output_buf.end_iter(),
            false,
        ).to_string()
    }

    fn widget(&self) -> &gtk4::Box {
        &self.widget
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
    config: Config,
    block_data: Rc<RefCell<VecDeque<BlockData>>>,
    finished_blocks: Rc<RefCell<Vec<gtk4::Box>>>,  // Rendered widgets (will phase out with virtual scrolling)
    ansi_cache: Rc<RefCell<LruCache<String, String>>>,
    viewport: Rc<RefCell<ViewportState>>,
    widget_pool: Rc<RefCell<WidgetPool>>,
}

impl TermView {
    pub fn new(config: &Config, shell_argv: &[String], cwd: Option<&str>) -> Self {
        // ── Build widget tree ──────────────────────────────────────────────
        let root = gtk4::Box::new(Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.add_css_class("term-view-root");

        // Block list inside a scrolled window
        let block_list = gtk4::Box::new(Orientation::Vertical, 0);
        block_list.set_vexpand(false);  // Don't expand - only take space needed
        block_list.set_valign(gtk4::Align::Start);  // Align to top
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
        )));
        block_list.append(active.borrow().widget());

        // VTE fallback for alt-screen mode
        let vte = build_vte(config);
        let vte_scrollbar = gtk4::Scrollbar::new(
            Orientation::Vertical,
            vte.vadjustment().as_ref(),
        );
        let vte_box = gtk4::Box::new(Orientation::Horizontal, 0);
        vte_box.set_hexpand(true);
        vte_box.set_vexpand(true);
        vte_box.append(&vte);
        vte_box.append(&vte_scrollbar);
        vte_box.set_visible(false); // hidden until alt-screen

        root.append(&block_scroll);
        root.append(&vte_box);

        // ── PTY ───────────────────────────────────────────────────────────
        let argv: Vec<&str> = shell_argv.iter().map(|s| s.as_str()).collect();
        let pty = Rc::new(
            OwnedPty::spawn(&argv, cwd, &[]).expect("PTY spawn failed"),
        );

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
        let cwd_callbacks: Rc<RefCell<Vec<Box<dyn Fn(&str)>>>> = Rc::new(RefCell::new(vec![]));
        let exited_callbacks: Rc<RefCell<Vec<Box<dyn Fn(i32)>>>> = Rc::new(RefCell::new(vec![]));
        let block_data_rc: Rc<RefCell<VecDeque<BlockData>>> = Rc::new(RefCell::new(VecDeque::new()));
        let finished_blocks_rc: Rc<RefCell<Vec<gtk4::Box>>> = Rc::new(RefCell::new(Vec::new()));
        let ansi_cache: Rc<RefCell<LruCache<String, String>>> = Rc::new(RefCell::new(
            LruCache::new(NonZeroUsize::new(config.ansi_cache_capacity as usize).unwrap())
        ));

        // ── Wire PTY → parser → block events ─────────────────────────────
        {
            let active_rc = active.clone();
            let bstate_rc = bstate.clone();
            let prompt_buf_rc = prompt_buf.clone();
            let cmd_buf_rc = cmd_buf.clone();
            let cmd_display_raw_rc = cmd_display_raw.clone();
            let block_list_rc = block_list.clone();
            let block_scroll_rc = block_scroll.clone();
            let vte_for_alt = vte.clone();
            let vte_box_rc = vte_box.clone();
            let pty_for_resize = pty.clone();
            let cwd_cbs = cwd_callbacks.clone();
            let exited_cbs = exited_callbacks.clone();
            let config_for_cb = config.clone();
            let parser = Rc::new(RefCell::new(Parser::new()));
            let block_data_for_cb = block_data_rc.clone();
            let finished_blocks_for_cb = finished_blocks_rc.clone();
            let scroll_debouncer = ScrollDebouncer::new();
            let ansi_cache_for_cb = ansi_cache.clone();

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
                                        let mut raw_cmd = if !prompt_clean.is_empty() {
                                            if let Some(_after_prompt) = current_stripped.strip_prefix(prompt_clean) {
                                                // Calculate visible chars to skip in raw text
                                                skip_ansi_visible_chars(&current_raw_buf, prompt_char_count)
                                            } else if let Some(pos) = current_stripped.find(prompt_clean) {
                                                let pos_chars = current_stripped[..pos].chars().count();
                                                skip_ansi_visible_chars(&current_raw_buf, pos_chars + prompt_char_count)
                                            } else {
                                                current_raw_buf.clone()
                                            }
                                        } else {
                                            current_raw_buf.clone()
                                        };

                                        raw_cmd = raw_cmd.trim_start().to_string();
                                        let display = raw_cmd.trim_end_matches('\n').trim_end();

                                        // Use LRU cache for ANSI → Pango conversion
                                        let markup = {
                                            let mut cache = ansi_cache_for_cb.borrow_mut();
                                            if let Some(cached) = cache.get(display) {
                                                cached.clone()
                                            } else {
                                                let result = ansi_to_pango(display, &config_for_cb.palette);
                                                // LRU automatically evicts least-recently-used entry
                                                cache.put(display.to_string(), result.clone());
                                                result
                                            }
                                        };

                                        active_rc.borrow().set_cmd_markup(&markup);
                                        // Save the raw command for CommandEnd
                                        *cmd_display_raw_rc.borrow_mut() = display.to_string();

                                        // Auto-scroll to bottom while typing command
                                        scroll_debouncer.mark_dirty(&block_scroll_rc);
                                    }
                                    BlockState::CollectingOutput => {
                                        let clean = strip_ansi(&text);
                                        active_rc.borrow().append_output(&clean);
                                        // Auto-scroll to bottom
                                        scroll_debouncer.mark_dirty(&block_scroll_rc);
                                    }
                                    BlockState::AltScreen => {
                                        // Feed raw bytes directly to VTE
                                        vte_for_alt.feed(bytes);
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
                                active_rc.borrow().set_cmd("");
                                // Auto-scroll to bottom when prompt ends (ready for command)
                                scroll_debouncer.mark_dirty(&block_scroll_rc);
                            }

                            ParserEvent::CommandStart => {
                                bstate_rc.set(BlockState::CollectingOutput);
                                // Auto-scroll to bottom when command starts executing
                                scroll_debouncer.mark_dirty(&block_scroll_rc);
                            }

                            ParserEvent::CommandEnd(code) => {
                                // Flush any pending output first
                                active_rc.borrow().flush_output();

                                // Freeze the active block into a finished block
                                let prompt = strip_ansi(&prompt_buf_rc.borrow()).trim().to_string();

                                // Use the last displayed command (saved in AwaitingCommand)
                                // This avoids issues when the shell redraws with just the prompt after Enter
                                let raw_cmd_with_ansi = cmd_display_raw_rc.borrow().clone();
                                let cmd = strip_ansi(&raw_cmd_with_ansi).trim().to_string();

                                let cmd_markup = if !raw_cmd_with_ansi.is_empty() {
                                    ansi_to_pango(&raw_cmd_with_ansi, &config_for_cb.palette)
                                } else {
                                    String::new()
                                };

                                let output = active_rc.borrow().output_text();
                                let output_trimmed = output.trim_end().to_string();
                                log::debug!("CommandEnd: cmd={:?}, output_len={}, output_empty={}", cmd, output_trimmed.len(), output_trimmed.is_empty());

                                // Create BlockData (logical representation)
                                let line_count = output_trimmed.lines().count();
                                let estimated_height = (line_count as i32 * 20).max(60);  // Rough estimate

                                let block_data = BlockData {
                                    prompt: prompt.clone(),
                                    cmd: cmd.clone(),
                                    cmd_markup: if cmd_markup.is_empty() { None } else { Some(cmd_markup.clone()) },
                                    output: output_trimmed.clone(),
                                    exit_code: *code,
                                    estimated_height,
                                    line_count,
                                };

                                block_data_for_cb.borrow_mut().push_back(block_data);

                                // Create widget (physical representation)
                                let finished = FinishedBlock::new(
                                    &prompt, &cmd, if cmd_markup.is_empty() { None } else { Some(&cmd_markup) }, &output_trimmed, *code, &config_for_cb,
                                );

                                // Insert before the active block (which is always last)
                                let active_widget = active_rc.borrow().widget().clone().upcast::<gtk4::Widget>();
                                finished.widget().insert_before(&block_list_rc, Some(&active_widget));

                                // Track finished blocks and limit history
                                let max_blocks = config_for_cb.max_visible_blocks as usize;
                                finished_blocks_for_cb.borrow_mut().push(finished.widget().clone());

                                // Remove oldest block if we exceed the limit
                                if finished_blocks_for_cb.borrow().len() > max_blocks {
                                    let oldest = finished_blocks_for_cb.borrow_mut().remove(0);
                                    block_list_rc.remove(&oldest);
                                }

                                // Also evict from block_data if needed
                                if block_data_for_cb.borrow().len() > max_blocks {
                                    block_data_for_cb.borrow_mut().pop_front();
                                }

                                // Reset active block for next command
                                active_rc.borrow().set_prompt("");
                                active_rc.borrow().set_cmd("");
                                active_rc.borrow().output_buf.set_text("");

                                // Scroll to bottom after layout updates
                                scroll_debouncer.mark_dirty(&block_scroll_rc);

                                bstate_rc.set(BlockState::Idle);
                            }

                            ParserEvent::CwdUpdate(path) => {
                                for cb in cwd_cbs.borrow().iter() {
                                    cb(&path);
                                }
                            }

                            ParserEvent::AltScreenEnter => {
                                bstate_rc.set(BlockState::AltScreen);
                                // Hide block view and expand VTE to fill all space
                                block_scroll_rc.set_visible(false);
                                block_scroll_rc.set_vexpand(false);
                                vte_box_rc.set_vexpand(true);
                                vte_box_rc.set_visible(true);

                                // Resize PTY to match VTE widget size
                                let pty_resize = pty_for_resize.clone();
                                let vte_for_resize = vte_for_alt.clone();
                                glib::idle_add_local_once(move || {
                                    let width = vte_for_resize.allocated_width() as i64;
                                    let height = vte_for_resize.allocated_height() as i64;
                                    if width > 0 && height > 0 {
                                        let char_width = vte_for_resize.char_width();
                                        let char_height = vte_for_resize.char_height();
                                        if char_width > 0 && char_height > 0 {
                                            let cols = (width / char_width) as u16;
                                            let rows = (height / char_height) as u16;
                                            log::debug!("Resizing PTY to {}x{} (widget {}x{}, char {}x{})",
                                                cols, rows, width, height, char_width, char_height);
                                            pty_resize.resize(cols, rows);
                                        }
                                    }
                                });

                                vte_for_alt.grab_focus();
                            }

                            ParserEvent::AltScreenLeave => {
                                vte_box_rc.set_visible(false);
                                vte_box_rc.set_vexpand(false);
                                block_scroll_rc.set_vexpand(true);
                                block_scroll_rc.set_visible(true);
                                bstate_rc.set(BlockState::Idle);
                                // Reset active block ready for next prompt
                                active_rc.borrow().set_prompt("");
                                active_rc.borrow().set_cmd("");
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

        // ── VTE is used as a display-only widget (fed via feed() in alt-screen mode)
        //    so we do NOT attach it to the PTY. Our reader thread handles all I/O.

        // ── Keyboard input → PTY ──────────────────────────────────────────
        {
            let pty_for_key = pty.clone();
            let vte_box_for_key = vte_box.clone();
            let bstate_for_key = bstate.clone();
            let key_ctrl = gtk4::EventControllerKey::new();
            key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);

            key_ctrl.connect_key_pressed(move |_, keyval, _keycode, modifiers| {
                // All keyboard input goes through here to the PTY.
                // VTE has no PTY attached — it's display-only (fed via feed()).
                // Main app's key_controller on the window (also Capture phase) runs first
                // and will intercept keybindings before we get here.
                let ctrl = modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
                let bytes: Option<Vec<u8>> = match keyval {
                    v if v == gtk4::gdk::Key::Return || v == gtk4::gdk::Key::KP_Enter => {
                        Some(b"\r".to_vec())
                    }
                    v if v == gtk4::gdk::Key::BackSpace => Some(b"\x7f".to_vec()),
                    v if v == gtk4::gdk::Key::Tab => Some(b"\t".to_vec()),
                    v if v == gtk4::gdk::Key::Escape => Some(b"\x1b".to_vec()),
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
            root.add_controller(key_ctrl);
            root.set_focusable(true);
        }

        TermView {
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
            config: config.clone(),
            block_data: block_data_rc,
            finished_blocks: finished_blocks_rc,
            ansi_cache,  // Use the cache we created earlier, not a new empty one
            viewport: Rc::new(RefCell::new(ViewportState {
                first_visible: 0,
                last_visible: 0,
                total_height: 0,
            })),
            widget_pool: Rc::new(RefCell::new(WidgetPool::new())),
        };

        // Load history if configured
        let _ = term_view.load_history();
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

    pub fn connect_cwd_changed<F: Fn(&str) + 'static>(&self, f: F) {
        self.cwd_callbacks.borrow_mut().push(Box::new(f));
    }

    pub fn connect_exited<F: Fn(i32) + 'static>(&self, f: F) {
        self.exited_callbacks.borrow_mut().push(Box::new(f));
    }

    /// Apply updated theme colors to the block widgets.
    pub fn apply_theme(&self) {
        install_block_css(&self.config);
    }

    /// Update virtual scrolling viewport state based on scroll position.
    pub fn update_viewport(&self) {
        let adj = self.block_scroll.vadjustment();
        let scroll_top = adj.value() as i32;
        let viewport_height = adj.page_size() as i32;
        let margin = (self.config.virtual_scroll_margin as i32) * viewport_height;

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

    /// Search blocks for a query string (case-insensitive).
    /// Returns indices of matching blocks.
    pub fn search_blocks(&self, query: &str) -> Vec<usize> {
        let q = query.to_lowercase();
        self.block_data.borrow().iter().enumerate()
            .filter(|(_, b)| {
                b.prompt.to_lowercase().contains(&q) ||
                b.cmd.to_lowercase().contains(&q) ||
                b.output.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn scroll_to_block(&self, block_index: usize) {
        let finished = self.finished_blocks.borrow();
        if block_index >= finished.len() {
            return;
        }
        if let Some(widget) = finished.get(block_index) {
            widget.grab_focus();
            let adj = self.block_scroll.vadjustment();
            if let Some(value) = widget.compute_point(&self.block_scroll, &gtk4::graphene::Point::new(0.0, 0.0)) {
                adj.set_value(value.y() as f64);
            }
        }
    }

    /// Save block history to file (if configured).
    pub fn save_history(&self) -> std::io::Result<()> {
        let path_opt = self.config.block_history_path.as_ref();
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
            let serialized = bincode::serialize(block).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
            })?;

            if self.config.block_history_compress {
                let compressed = zstd::encode_all(serialized.as_slice(), 3).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                })?;
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
        let path_opt = self.config.block_history_path.as_ref();
        if path_opt.is_none() {
            return Ok(());
        }

        let path = path_opt.unwrap();
        if !std::path::Path::new(path).exists() {
            return Ok(());
        }

        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut blocks = self.block_data.borrow_mut();

        loop {
            let mut len_bytes = [0u8; 4];
            if file.read_exact(&mut len_bytes).is_err() {
                break;
            }

            let len = u32::from_le_bytes(len_bytes) as usize;
            let mut data = vec![0u8; len];
            file.read_exact(&mut data)?;

            let decoded = if self.config.block_history_compress {
                zstd::decode_all(data.as_slice()).map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                })?
            } else {
                data
            };

            if let Ok(block) = bincode::deserialize::<BlockData>(&decoded) {
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
    terminal.set_colors(Some(&config.foreground), Some(&config.background), &palette_refs);
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
    // Slightly lighter bg for header
    let header_bg = format!(
        "rgba({},{},{},0.08)",
        (fg.red() * 255.0) as u8,
        (fg.green() * 255.0) as u8,
        (fg.blue() * 255.0) as u8,
    );
    let dim_fg = format!(
        "rgba({},{},{},0.55)",
        (fg.red() * 255.0) as u8,
        (fg.green() * 255.0) as u8,
        (fg.blue() * 255.0) as u8,
    );
    // Accent color for active chevron (use palette color 2 = green-ish)
    let accent = rgba_to_hex(&config.palette[2]);

    let fg_r = (fg.red() * 255.0) as u8;
    let fg_g = (fg.green() * 255.0) as u8;
    let fg_b = (fg.blue() * 255.0) as u8;

    // Parse font description to extract font family and size
    // Format: "FontName Style Size" e.g. "SauceCodePro Nerd Font Regular 14"
    let parts: Vec<&str> = config.font_desc.split_whitespace().collect();
    let (font_family, font_size) = if parts.len() >= 2 {
        // Last part is usually the size
        if let Ok(size) = parts[parts.len() - 1].parse::<i32>() {
            let family = parts[..parts.len() - 1].join(" ");
            (family, format!("{}pt", size))
        } else {
            (config.font_desc.clone(), "14pt".to_string())
        }
    } else {
        (config.font_desc.clone(), "14pt".to_string())
    };

    let css = format!(
        r#"
        .block-scroll {{
            background-color: {bg_hex};
        }}
        .block-list {{
            background-color: {bg_hex};
            contain: style;
        }}
        .block-finished {{
            border-bottom: 1px solid rgba({fg_r},{fg_g},{fg_b},0.12);
            border-radius: 0;
            margin: 0;
            background-color: {bg_hex};
            contain: content;
        }}
        .block-active {{
            border-radius: 0;
            margin: 0;
            background-color: {bg_hex};
        }}
        .block-header {{
            background-color: {header_bg};
            border-radius: 5px 5px 0 0;
            padding-top: 8px;
            padding-bottom: 8px;
        }}
        .block-prompt {{
            color: {dim_fg};
            font-family: "{font_family}";
            font-size: {font_size};
            line-height: 1.0;
            margin: 0;
        }}
        .block-cmd {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            padding: 0;
            line-height: 1.0;
            margin: 0;
        }}
        .block-cmd-active {{
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            font-weight: bold;
            padding: 0;
            line-height: 1.0;
            margin: 0;
            border-left: 3px solid {accent};
            padding-left: 9px;
        }}
        .block-exit-bad {{
            color: #ff5555;
            background-color: rgba(255,85,85,0.18);
            border-radius: 3px;
            font-size: 0.8em;
        }}
        .block-output {{
            background-color: {bg_hex};
            color: {fg_hex};
            font-family: "{font_family}";
            font-size: {font_size};
            contain: content;
        }}
        .block-show-more {{
            color: {accent};
            margin-start: 12px;
            margin-top: 4px;
            margin-bottom: 4px;
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
