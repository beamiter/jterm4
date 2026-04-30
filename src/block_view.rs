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
use vte4::{CursorBlinkMode, CursorShape, Terminal};
use vte4::{TerminalExt, TerminalExtManual};

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
        let handle =
            glib::timeout_add_local_once(std::time::Duration::from_millis(50), move || {
                let adj = scroll.vadjustment();
                let target = adj.upper() - adj.page_size();
                if adj.value() < target {
                    adj.set_value(target);
                }
                dirty.set(false);
                // Clear the handle after firing to prevent double-remove
                pending_for_clear.borrow_mut().take();
            });
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

fn contains_full_screen_redraw(bytes: &[u8]) -> bool {
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

                if is_alt_screen_mode(params) {
                    return true;
                }

                match final_byte {
                    b'H' | b'f' | b'J' => return true,
                    _ => {}
                }
            }
            _ => {
                i = skip_escape_sequence(bytes, i);
            }
        }
    }

    false
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

                        // Detect clear-screen sequences
                        match final_byte {
                            b'J' => {
                                // CSI J, CSI 1J, CSI 2J — any erase in display
                                should_clear = true;
                            }
                            b'H' | b'f' => {
                                // CSI H or CSI f — cursor movement to position
                                // If no params or params = "1;1", it's home position (clear-like behavior)
                                if params.is_empty() || params == b"" || params == b"1;1" {
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

#[derive(Clone, Default)]
struct AnsiStyleState {
    foreground: Option<RGBA>,
    background: Option<RGBA>,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    dim: bool,
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
            _ => {}
        }

        index += 1;
    }
}

fn ansi_text_runs(input: &str, palette: &[RGBA; 16]) -> Vec<AnsiTextRun> {
    let bytes = input.as_bytes();
    let mut runs = Vec::new();
    let mut current_style = AnsiStyleState::default();
    let mut current_text = String::new();
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
                if final_byte == b'm' {
                    if params.is_empty() || params[0].is_empty() {
                        params = vec!["0".to_string()];
                    }
                    flush_ansi_run(&mut runs, &mut current_text, &current_style);
                    parse_sgr_params(&mut current_style, &params, palette);
                }
            }
        } else if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b']' {
            i = skip_osc_sequence(bytes, i);
        } else if bytes[i] == 0x1b && i + 1 < bytes.len() {
            i = skip_escape_sequence(bytes, i);
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
            let end = (i + ch_len).min(bytes.len());
            current_text.push_str(&String::from_utf8_lossy(&bytes[i..end]));
            i = end;
        }
    }

    flush_ansi_run(&mut runs, &mut current_text, &current_style);
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
    content_view: gtk4::TextView,
    content_buffer: gtk4::TextBuffer,
}

impl Clone for FinishedBlock {
    fn clone(&self) -> Self {
        Self {
            widget: self.widget.clone(),
            content_view: self.content_view.clone(),
            content_buffer: self.content_buffer.clone(),
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
    ) -> Self {
        // Output is already trimmed by caller, but be defensive
        let block_outer_margin_top = 4;
        let block_outer_margin_bottom = 2;
        let prompt_margin_top = 4;
        let prompt_margin_bottom = 2;
        let content_margin_top = 2;
        let content_margin_bottom = 6;

        // Outer frame
        let outer = gtk4::Box::new(Orientation::Vertical, 0);
        outer.add_css_class("block-finished");
        outer.set_margin_top(block_outer_margin_top);
        outer.set_margin_bottom(block_outer_margin_bottom);

        // Prompt row
        let prompt_label = gtk4::Label::new(Some(prompt));
        prompt_label.add_css_class("block-prompt");
        prompt_label.set_xalign(0.0);
        prompt_label.set_selectable(true);
        prompt_label.set_margin_start(12);
        prompt_label.set_margin_top(prompt_margin_top);
        prompt_label.set_margin_bottom(prompt_margin_bottom);
        prompt_label.set_single_line_mode(true);
        outer.append(&prompt_label);

        // Command + output content row using TextView
        let cmd_box = gtk4::Box::new(Orientation::Horizontal, 8);
        cmd_box.set_margin_start(0); // TextView has its own left margin
        cmd_box.set_margin_top(0);
        cmd_box.set_margin_bottom(0);
        cmd_box.set_spacing(0);
        cmd_box.set_can_focus(false);
        cmd_box.set_focusable(false);
        // Don't set can_target(false) - need to allow mouse events for TextView selection

        // Create TextView + TextBuffer for content
        let content_buffer = gtk4::TextBuffer::new(None);
        let content_view = gtk4::TextView::with_buffer(&content_buffer);

        // Basic styling
        content_view.add_css_class("block-cmd-finished");
        content_view.set_wrap_mode(gtk4::WrapMode::Char);
        content_view.set_monospace(true);

        // Margins (matching the previous cmd_label)
        content_view.set_left_margin(12);
        content_view.set_right_margin(8);
        content_view.set_top_margin(content_margin_top);
        content_view.set_bottom_margin(content_margin_bottom);

        // Layout
        content_view.set_hexpand(true);
        content_view.set_vexpand(false);
        content_view.set_valign(gtk4::Align::Start);

        // Non-editable
        content_view.set_editable(false);
        content_view.set_cursor_visible(false);
        content_view.set_accepts_tab(false); // Don't capture Tab key

        // Focus management - allow focus for text selection
        // Non-editable TextView won't capture keyboard input
        content_view.set_can_focus(true);
        content_view.set_focusable(true);
        content_view.set_can_target(true);

        // Combine cmd and output text
        let output_runs = ansi_text_runs(output, &config.palette);
        let output_plain = output_runs
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>();

        let combined_text = if output_plain.is_empty() {
            if cmd.is_empty() {
                "(empty)".to_string()
            } else {
                cmd.to_string()
            }
        } else if cmd.is_empty() {
            // Historical blocks may have empty cmd - show a subtle indicator
            format!("[?]\n{}", output_plain)
        } else {
            format!("{}\n{}", cmd, output_plain)
        };

        content_buffer.set_text(&combined_text);
        if !output_plain.is_empty() {
            let output_start = if cmd.is_empty() { 4 } else { cmd.chars().count() + 1 };
            apply_ansi_runs_to_buffer(&content_buffer, output_start, &output_runs);
        }
        cmd_box.append(&content_view);

        // Exit code badge
        if exit_code != 0 {
            let badge = gtk4::Label::new(Some(&format!(" {exit_code} ")));
            badge.add_css_class("block-exit-bad");
            badge.set_valign(gtk4::Align::Start);
            cmd_box.append(&badge);
        }

        outer.append(&cmd_box);

        // Separator line
        let sep_box = gtk4::Separator::new(Orientation::Horizontal);
        outer.append(&sep_box);

        FinishedBlock {
            widget: outer,
            content_view,
            content_buffer,
        }
    }

    fn widget(&self) -> &gtk4::Box {
        &self.widget
    }
}

// ─── ActiveBlock ──────────────────────────────────────────────────────────────

struct ActiveBlock {
    widget: gtk4::Box,
    prompt_label: gtk4::Label,
    content_view: gtk4::TextView, // TextView for cmd + output
    content_buffer: gtk4::TextBuffer,
    pending_output: Rc<RefCell<String>>,
    pending_cmd: Rc<RefCell<String>>,        // User input only
    pending_suggestion: Rc<RefCell<String>>, // Shell suggestion/autocomplete
    flush_pending: Rc<Cell<bool>>,
    // Adaptive batching state
    bytes_since_last_flush: Rc<Cell<usize>>,
    last_flush_time: Rc<Cell<std::time::Instant>>,
    current_batch_ms: Rc<Cell<u32>>,
    config_batch_min: u32,
    config_batch_max: u32,
    last_flushed_size: Rc<Cell<usize>>,
    cursor_visible: Rc<Cell<bool>>, // For blinking cursor animation
    palette: [RGBA; 16],
}

impl ActiveBlock {
    fn new(batch_min_ms: u32, batch_max_ms: u32, config: &Config) -> Self {
        let block_outer_margin_top = 4;
        let block_outer_margin_bottom = 2;
        let prompt_margin_top = 4;
        let prompt_margin_bottom = 2;
        let content_margin_top = 2;
        let content_margin_bottom = 6;

        let widget = gtk4::Box::new(Orientation::Vertical, 0);
        widget.add_css_class("block-active");
        widget.set_margin_top(block_outer_margin_top);
        widget.set_margin_bottom(block_outer_margin_bottom);
        widget.set_can_focus(false); // Don't steal focus from labels
        widget.set_can_target(false); // Let events pass through to children
        widget.set_focusable(false); // Prevent any focus interception

        // Prompt label
        let prompt_label = gtk4::Label::new(Some(""));
        prompt_label.add_css_class("block-prompt");
        prompt_label.set_xalign(0.0);
        prompt_label.set_selectable(true);
        prompt_label.set_can_target(true); // Ensure it can receive mouse events
        prompt_label.set_can_focus(true); // Ensure it can receive focus
        prompt_label.set_margin_start(12);
        prompt_label.set_margin_top(prompt_margin_top);
        prompt_label.set_margin_bottom(prompt_margin_bottom);
        prompt_label.set_single_line_mode(true);
        widget.append(&prompt_label);

        // Content view (TextView for cmd + output combined)
        let content_buffer = TextBuffer::new(None);
        let content_view = TextView::with_buffer(&content_buffer);
        content_view.add_css_class("block-cmd-active");
        content_view.set_hexpand(true);
        content_view.set_vexpand(false);
        content_view.set_editable(true); // Enable editing to show blinking cursor
        content_view.set_cursor_visible(true);
        content_view.set_can_focus(true);
        content_view.set_focusable(true);

        // Ensure GTK cursor blink is enabled
        if let Some(settings) = gtk4::Settings::default() {
            settings.set_property("gtk-cursor-blink", true);
            settings.set_property("gtk-cursor-blink-time", 1200i32); // 1200ms blink cycle
        }

        // Block keyboard input to keep it read-only while showing cursor
        let key_controller = EventControllerKey::new();
        key_controller.connect_key_pressed(|_controller, _key, _code, _modifier| {
            glib::Propagation::Stop // Block all keyboard input
        });
        content_view.add_controller(key_controller);
        content_view.set_wrap_mode(gtk4::WrapMode::Char);
        content_view.set_left_margin(12);
        content_view.set_right_margin(8);
        content_view.set_top_margin(content_margin_top);
        content_view.set_bottom_margin(content_margin_bottom);
        content_view.set_monospace(true);
        widget.append(&content_view);

        // Place cursor at the beginning of the buffer and grab focus for cursor blinking
        let start_iter = content_buffer.start_iter();
        content_buffer.place_cursor(&start_iter);

        // Schedule focus grab after widget is realized
        let content_view_clone = content_view.clone();
        content_view.connect_realize(move |_| {
            content_view_clone.grab_focus();
        });

        let cursor_visible = Rc::new(Cell::new(true));
        let pending_cmd = Rc::new(RefCell::new(String::new()));
        let pending_suggestion = Rc::new(RefCell::new(String::new()));
        let pending_output = Rc::new(RefCell::new(String::new()));

        // Start cursor blink animation
        let cursor_visible_clone = cursor_visible.clone();
        let content_buffer_clone = content_buffer.clone();
        let pending_cmd_clone = pending_cmd.clone();
        let pending_suggestion_clone = pending_suggestion.clone();
        let pending_output_clone = pending_output.clone();
        let palette_for_cursor = config.palette;

        glib::timeout_add_local(std::time::Duration::from_millis(530), move || {
            // Toggle cursor visibility
            cursor_visible_clone.set(!cursor_visible_clone.get());

            // Update display
            let cmd = pending_cmd_clone.borrow();
            let suggestion = pending_suggestion_clone.borrow();
            let output = pending_output_clone.borrow();
            log::debug!(
                "cursor_blink_timer: cmd={:?}, suggestion={:?}, cursor_visible={}",
                cmd,
                suggestion,
                cursor_visible_clone.get()
            );

            set_active_buffer_text(
                &content_buffer_clone,
                &cmd,
                &suggestion,
                &output,
                cursor_visible_clone.get(),
                &palette_for_cursor,
            );

            glib::ControlFlow::Continue
        });

        ActiveBlock {
            widget,
            prompt_label,
            content_view,
            content_buffer,
            pending_output,
            pending_cmd,
            pending_suggestion,
            flush_pending: Rc::new(Cell::new(false)),
            bytes_since_last_flush: Rc::new(Cell::new(0)),
            last_flush_time: Rc::new(Cell::new(std::time::Instant::now())),
            current_batch_ms: Rc::new(Cell::new(batch_min_ms)),
            config_batch_min: batch_min_ms,
            config_batch_max: batch_max_ms,
            last_flushed_size: Rc::new(Cell::new(0)),
            cursor_visible,
            palette: config.palette,
        }
    }

    fn set_prompt(&self, text: &str) {
        self.prompt_label.set_text(text);
    }

    fn set_cmd(&self, text: &str) {
        log::debug!("set_cmd: text={:?}", text);
        *self.pending_cmd.borrow_mut() = text.to_string();
        *self.pending_suggestion.borrow_mut() = String::new(); // Clear suggestion when user types
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
        *self.pending_suggestion.borrow_mut() = suggestion_plain;
        self.update_content_view();
    }

    // Helper to update content_view with cmd + output
    fn update_content_view(&self) {
        let cmd = self.pending_cmd.borrow();
        let suggestion = self.pending_suggestion.borrow();
        let output = self.pending_output.borrow();
        log::debug!(
            "update_content_view: cmd={:?}, suggestion={:?}, cursor_visible={}",
            cmd,
            suggestion,
            self.cursor_visible.get()
        );

        set_active_buffer_text(
            &self.content_buffer,
            &cmd,
            &suggestion,
            &output,
            self.cursor_visible.get(),
            &self.palette,
        );
    }

    fn append_output(&self, text: &str) {
        let text_len = text.len();
        self.pending_output.borrow_mut().push_str(text);

        // Track throughput for adaptive batching
        self.bytes_since_last_flush
            .set(self.bytes_since_last_flush.get() + text_len);

        // Schedule flush if not already pending
        if !self.flush_pending.get() {
            self.flush_pending.set(true);
            let pending_cmd = self.pending_cmd.clone();
            let pending_output = self.pending_output.clone();
            let content_buffer = self.content_buffer.clone();
            let flush_flag = self.flush_pending.clone();
            let bytes_tracker = self.bytes_since_last_flush.clone();
            let last_flush_time = self.last_flush_time.clone();
            let current_batch_ms = self.current_batch_ms.clone();
            let min_ms = self.config_batch_min;
            let max_ms = self.config_batch_max;
            let last_flushed_size = self.last_flushed_size.clone();
            let palette = self.palette;

            let batch_interval = current_batch_ms.get();
            glib::timeout_add_local_once(
                std::time::Duration::from_millis(batch_interval as u64),
                move || {
                    // Get current cmd and output
                    let cmd = pending_cmd.borrow();
                    let output = pending_output.borrow();

                    // Combine and display in TextBuffer
                    set_active_buffer_text(&content_buffer, &cmd, "", &output, false, &palette);
                    let end_iter = content_buffer.end_iter();
                    content_buffer.place_cursor(&end_iter);

                    last_flushed_size.set(output.len());

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
        let cmd = self.pending_cmd.borrow();
        let output = self.pending_output.borrow();

        set_active_buffer_text(&self.content_buffer, &cmd, "", &output, false, &self.palette);
        let end_iter = self.content_buffer.end_iter();
        self.content_buffer.place_cursor(&end_iter);
        self.last_flushed_size.set(output.len());
    }

    fn output_text(&self) -> String {
        self.pending_output.borrow().clone()
    }

    fn widget(&self) -> &gtk4::Box {
        &self.widget
    }

    fn grab_focus(&self) {
        self.content_view.grab_focus();
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
    config: Rc<RefCell<Config>>,
    block_data: Rc<RefCell<VecDeque<BlockData>>>,
    finished_blocks: Rc<RefCell<Vec<FinishedBlock>>>,
    ansi_cache: Rc<RefCell<LruCache<String, String>>>,
    viewport: Rc<RefCell<ViewportState>>,
    widget_pool: Rc<RefCell<WidgetPool>>,
    visible_indices: Rc<RefCell<std::collections::HashSet<usize>>>,
    search_cache: Rc<std::sync::Mutex<std::collections::HashMap<String, Vec<usize>>>>, // Cache search results
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
        block_list.set_vexpand(false); // Don't expand - only take space needed
        block_list.set_valign(gtk4::Align::Start); // Align to top
        block_list.set_margin_bottom(28); // Leave more room below the current block at the bottom edge
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
        block_list.append(active.borrow().widget());

        // VTE fallback for alt-screen mode
        let vte = build_vte(config);
        let vte_scrollbar = gtk4::Scrollbar::new(Orientation::Vertical, vte.vadjustment().as_ref());
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
        let pty = Rc::new(OwnedPty::spawn(&argv, cwd, &[]).expect("PTY spawn failed"));

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
        let block_data_rc: Rc<RefCell<VecDeque<BlockData>>> =
            Rc::new(RefCell::new(VecDeque::new()));
        let finished_blocks_rc: Rc<RefCell<Vec<FinishedBlock>>> = Rc::new(RefCell::new(Vec::new()));
        let ansi_cache: Rc<RefCell<LruCache<String, String>>> = Rc::new(RefCell::new(
            LruCache::new(NonZeroUsize::new(config.ansi_cache_capacity as usize).unwrap()),
        ));

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
            let config_for_cb = Rc::new(RefCell::new(config.clone()));
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
                                        let user_markup = {
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
                                        if contains_full_screen_redraw(bytes) {
                                            bstate_rc.set(BlockState::AltScreen);
                                            show_alt_screen(
                                                &block_scroll_rc,
                                                &vte_box_rc,
                                                &vte_for_alt,
                                                pty_for_resize.clone(),
                                                Some(bytes),
                                            );
                                            continue;
                                        }

                                        let (_, should_clear) = strip_ansi_with_clear_detect(&text);
                                        if should_clear {
                                            // Clear-screen sequence detected; clear output buffer
                                            active_rc.borrow().pending_output.borrow_mut().clear();
                                            active_rc.borrow().update_content_view();
                                        }
                                        active_rc.borrow().append_output(&text);
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
                                cmd_display_markup_rc.borrow_mut().clear();
                                active_rc.borrow().set_cmd("");
                                // Auto-scroll to bottom when prompt ends (ready for command)
                                scroll_debouncer.mark_dirty(&block_scroll_rc);
                            }

                            ParserEvent::CommandStart => {
                                bstate_rc.set(BlockState::CollectingOutput);
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
                                // Clear output buffer for new command
                                active_rc.borrow().pending_suggestion.borrow_mut().clear();
                                active_rc.borrow().pending_output.borrow_mut().clear();
                                active_rc.borrow().update_content_view();
                                // Auto-scroll to bottom when command starts executing
                                scroll_debouncer.mark_dirty(&block_scroll_rc);
                            }

                            ParserEvent::CommandEnd(code) => {
                                if bstate_rc.get() == BlockState::AltScreen || vte_box_rc.is_visible() {
                                    hide_alt_screen(&block_scroll_rc, &vte_box_rc);
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
                                    &prompt, &cmd, if cmd_markup.is_empty() { None } else { Some(&cmd_markup) }, &output_display, *code, &config_for_cb.borrow(),
                                );

                                // Insert before the active block (which is always last)
                                let active_widget = active_rc.borrow().widget().clone().upcast::<gtk4::Widget>();
                                finished.widget().insert_before(&block_list_rc, Some(&active_widget));

                                // Track finished blocks and limit history
                                let max_blocks = config_for_cb.borrow().max_visible_blocks as usize;
                                finished_blocks_for_cb.borrow_mut().push(finished.clone());

                                // Remove oldest block if we exceed the limit
                                if finished_blocks_for_cb.borrow().len() > max_blocks {
                                    let oldest = finished_blocks_for_cb.borrow_mut().remove(0);
                                    block_list_rc.remove(oldest.widget());
                                }

                                // Also evict from block_data if needed
                                if block_data_for_cb.borrow().len() > max_blocks {
                                    block_data_for_cb.borrow_mut().pop_front();
                                }

                                // Reset active block for next command
                                active_rc.borrow().set_prompt("");
                                active_rc.borrow().set_cmd("");
                                active_rc.borrow().pending_output.borrow_mut().clear();
                                active_rc.borrow().update_content_view();

                                executing_cmd_raw_rc.borrow_mut().clear();
                                executing_cmd_markup_rc.borrow_mut().clear();
                                last_nonempty_cmd_raw_rc.borrow_mut().clear();
                                last_nonempty_cmd_markup_rc.borrow_mut().clear();

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
                                show_alt_screen(
                                    &block_scroll_rc,
                                    &vte_box_rc,
                                    &vte_for_alt,
                                    pty_for_resize.clone(),
                                    None,
                                );
                            }

                            ParserEvent::AltScreenLeave => {
                                hide_alt_screen(&block_scroll_rc, &vte_box_rc);
                                bstate_rc.set(BlockState::Idle);
                                // Reset active block ready for next prompt
                                active_rc.borrow().set_prompt("");
                                active_rc.borrow().set_cmd("");
                                // Give focus to active block's TextView
                                active_rc.borrow().content_view.grab_focus();
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
            let vte_for_key = vte.clone();
            let root_for_key = root.clone();
            let key_ctrl = gtk4::EventControllerKey::new();
            key_ctrl.set_propagation_phase(gtk4::PropagationPhase::Capture);

            key_ctrl.connect_key_pressed(move |_, keyval, _keycode, modifiers| {
                // All keyboard input goes through here to the PTY.
                // VTE has no PTY attached — it's display-only (fed via feed()).
                // Main app's key_controller on the window (also Capture phase) runs first
                // and will intercept keybindings before we get here.
                let ctrl = modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
                let shift = modifiers.contains(gtk4::gdk::ModifierType::SHIFT_MASK);
                let alt = modifiers.contains(gtk4::gdk::ModifierType::ALT_MASK);

                log::debug!("KEY: keyval={:?}, ctrl={}, shift={}, alt={}", keyval, ctrl, shift, alt);

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
                            log::warn!(">>> Paste: got clipboard, calling read_text_async");
                            clipboard.read_text_async(None::<&gtk4::gio::Cancellable>, move |result| {
                                log::warn!(">>> Paste callback: result={:?}", result.as_ref().map(|opt| opt.as_ref().map(|s| s.len())));
                                match result {
                                    Ok(text_opt) => {
                                        if let Some(text_str) = text_opt {
                                            log::warn!(">>> Paste: got {} chars from clipboard", text_str.len());
                                            pty_for_paste.write_bytes(text_str.as_bytes());
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
            config: Rc::new(RefCell::new(config.clone())),
            block_data: block_data_rc,
            finished_blocks: finished_blocks_rc,
            ansi_cache,
            viewport: Rc::new(RefCell::new(ViewportState {
                first_visible: 0,
                last_visible: 0,
                total_height: 0,
            })),
            widget_pool: Rc::new(RefCell::new(WidgetPool::new())),
            visible_indices: Rc::new(RefCell::new(std::collections::HashSet::new())),
            search_cache: Rc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
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
                );
                term_view.block_list.append(finished.widget());
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
        term_view.active.borrow().content_view.grab_focus();

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
                        pty.write_bytes(text_str.as_bytes());
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
        let q = query.to_lowercase();

        // Check cache first
        if let Ok(cache) = self.search_cache.lock() {
            if let Some(cached) = cache.get(&q) {
                return cached.clone();
            }
        }

        // Perform search
        let results: Vec<usize> = self
            .block_data
            .borrow()
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                b.prompt.to_lowercase().contains(&q)
                    || b.cmd.to_lowercase().contains(&q)
                    || b.output.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();

        // Cache results (ignore lock errors)
        if let Ok(mut cache) = self.search_cache.lock() {
            cache.insert(q, results.clone());
        }

        results
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
        let mut blocks = self.block_data.borrow_mut();

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
                log::debug!("Loaded historical block: prompt={:?}, cmd={:?}, output_len={}, exit_code={}",
                    &block.prompt, &block.cmd, block.output.len(), block.exit_code);
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
            border-bottom: 1px solid rgba({fg_r},{fg_g},{fg_b},0.12);
            border-radius: 0;
            margin: 0;
            background-color: {bg_hex};
            min-height: 40px;
        }}
        .block-active {{
            border-radius: 0;
            margin: 0;
            background-color: {bg_hex};
            min-height: 40px;
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
    use super::{ansi_text_runs, ansi_to_pango, command_line_plain_text, skip_ansi_visible_chars, strip_ansi};
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
        assert_eq!(runs.iter().map(|run| run.text.as_str()).collect::<String>(), "aredz");
        assert!(runs.iter().any(|run| run.text == "red" && run.style.foreground.is_some()));
    }
}
