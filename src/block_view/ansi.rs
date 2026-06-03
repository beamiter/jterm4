//! ansi — extracted from block_view (mechanical split, no logic changes)
use gtk4::gdk::RGBA;
use gtk4::glib::translate::IntoGlib;
use gtk4::prelude::*;
use gtk4::TextBuffer;


pub(crate) fn skip_osc_sequence(bytes: &[u8], mut i: usize) -> usize {
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

pub(crate) fn skip_escape_sequence(bytes: &[u8], i: usize) -> usize {
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

pub(crate) fn strip_ansi_with_clear_detect(input: &str) -> (String, bool) {
    let bytes = input.as_bytes();
    let mut lines: Vec<Vec<char>> = vec![Vec::new()];
    let mut cursor = 0usize;
    let mut i = 0;
    let mut should_clear = false;
    let mut param_buf: Vec<u8> = Vec::with_capacity(16);

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    i += 2;
                    param_buf.clear();
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        param_buf.push(bytes[i]);
                        i += 1;
                    }
                    if i < bytes.len() {
                        let final_byte = bytes[i];
                        i += 1;

                        let cells = lines.last_mut().unwrap();
                        match final_byte {
                            b'J' => {
                                if param_buf == b"2" || param_buf == b"3" {
                                    should_clear = true;
                                }
                            }
                            b'K' => {
                                match param_buf.as_slice() {
                                    b"" | b"0" => cells.truncate(cursor),
                                    b"1" => {
                                        for c in cells.iter_mut().take(cursor) {
                                            *c = ' ';
                                        }
                                    }
                                    b"2" => {
                                        cells.clear();
                                        cursor = 0;
                                    }
                                    _ => {}
                                }
                            }
                            b'C' => {
                                let count = parse_param_first(&param_buf, 1);
                                cursor = (cursor + count).min(cells.len());
                            }
                            b'D' => {
                                let count = parse_param_first(&param_buf, 1);
                                cursor = cursor.saturating_sub(count);
                            }
                            b'G' => {
                                let col = parse_param_first(&param_buf, 1);
                                cursor = col.saturating_sub(1).min(cells.len());
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
        } else if bytes[i] == b'\n' {
            lines.push(Vec::new());
            cursor = 0;
            i += 1;
        } else if bytes[i] == b'\r' {
            cursor = 0;
            i += 1;
        } else if bytes[i] == b'\x08' {
            cursor = cursor.saturating_sub(1);
            i += 1;
        } else {
            let ch = input[i..].chars().next().unwrap_or('\u{FFFD}');
            i += ch.len_utf8();
            let cells = lines.last_mut().unwrap();
            if cursor < cells.len() {
                cells[cursor] = ch;
            } else {
                cells.push(ch);
            }
            cursor += 1;
        }
    }
    let mut result = String::with_capacity(input.len());
    for (line_idx, cells) in lines.iter().enumerate() {
        if line_idx > 0 {
            result.push('\n');
        }
        for &ch in cells {
            result.push(ch);
        }
    }
    (result, should_clear)
}

pub(crate) fn contains_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    let first = needle[0];
    let finder = memchr::memchr_iter(first, haystack);
    for pos in finder {
        if pos + needle.len() > haystack.len() {
            return false;
        }
        let candidate = &haystack[pos..pos + needle.len()];
        if candidate.iter().zip(needle.iter()).all(|(&h, &n)| h.to_ascii_lowercase() == n) {
            return true;
        }
    }
    // Also check uppercase variant of first byte
    let first_upper = first.to_ascii_uppercase();
    if first_upper != first {
        let finder = memchr::memchr_iter(first_upper, haystack);
        for pos in finder {
            if pos + needle.len() > haystack.len() {
                return false;
            }
            let candidate = &haystack[pos..pos + needle.len()];
            if candidate.iter().zip(needle.iter()).all(|(&h, &n)| h.to_ascii_lowercase() == n) {
                return true;
            }
        }
    }
    false
}

pub(crate) fn parse_param_first(buf: &[u8], default: usize) -> usize {
    let end = buf.iter().position(|&b| b == b';').unwrap_or(buf.len());
    if end == 0 {
        return default;
    }
    let mut val = 0usize;
    for &b in &buf[..end] {
        if b.is_ascii_digit() {
            val = val * 10 + (b - b'0') as usize;
        } else {
            return default;
        }
    }
    if val == 0 { default } else { val }
}

pub(crate) fn strip_ansi(input: &str) -> String {
    strip_ansi_with_clear_detect(input).0
}


pub(crate) fn ansi256_to_rgb(idx: u8, palette: &[RGBA; 16]) -> (u8, u8, u8) {
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

pub(crate) fn skip_ansi_visible_chars(input: &str, mut count: usize) -> String {
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
            count = count.saturating_sub(1);
        }
    }
    input[i..].to_string()
}

pub(crate) fn separate_input_and_suggestion(input: &str, column_offset: usize) -> (String, String) {
    struct Cell {
        ch: char,
        in_dim: bool,
    }

    let mut cells: Vec<Cell> = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut in_dim = false;
    let mut cursor = 0usize;
    let mut param_buf: Vec<u8> = Vec::with_capacity(16);

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            param_buf.clear();

            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                param_buf.push(bytes[i]);
                i += 1;
            }

            if i < bytes.len() {
                let final_byte = bytes[i];
                i += 1;

                match final_byte {
                    b'm' => {
                        if param_buf.is_empty() {
                            in_dim = false;
                        } else {
                            for param in param_buf.split(|&b| b == b';') {
                                match param {
                                    b"0" | b"22" => in_dim = false,
                                    b"2" => in_dim = true,
                                    b"" => in_dim = false,
                                    _ => {}
                                }
                            }
                        }
                    }
                    b'D' => {
                        let count = parse_param_first(&param_buf, 1);
                        cursor = cursor.saturating_sub(count);
                    }
                    b'C' => {
                        let count = parse_param_first(&param_buf, 1);
                        cursor = (cursor + count).min(cells.len());
                    }
                    b'G' => {
                        let col = parse_param_first(&param_buf, 1);
                        cursor = if column_offset == 0 {
                            col.saturating_sub(1)
                        } else {
                            col.saturating_sub(column_offset)
                        }
                        .min(cells.len());
                    }
                    b'K' => {
                        match param_buf.as_slice() {
                            b"" | b"0" => cells.truncate(cursor),
                            b"1" => {
                                cells.drain(..cursor);
                                cursor = 0;
                            }
                            b"2" => {
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
            let ch = input[i..].chars().next().unwrap_or('\u{FFFD}');
            i += ch.len_utf8();
            if cursor < cells.len() {
                cells[cursor] = Cell { ch, in_dim };
            } else {
                cells.push(Cell { ch, in_dim });
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
            user_input.push(cell.ch);
        } else {
            suggestion.push(cell.ch);
        }
    }

    (user_input, suggestion)
}

pub(crate) fn command_line_plain_text(input: &str) -> String {
    let mut cells: Vec<char> = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut cursor = 0usize;
    let mut param_buf: Vec<u8> = Vec::with_capacity(16);

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            param_buf.clear();

            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                param_buf.push(bytes[i]);
                i += 1;
            }

            if i < bytes.len() {
                let final_byte = bytes[i];
                i += 1;

                match final_byte {
                    b'D' => {
                        let count = parse_param_first(&param_buf, 1);
                        cursor = cursor.saturating_sub(count);
                    }
                    b'C' => {
                        let count = parse_param_first(&param_buf, 1);
                        cursor = (cursor + count).min(cells.len());
                    }
                    b'G' => {
                        let col = parse_param_first(&param_buf, 1);
                        cursor = col.saturating_sub(1).min(cells.len());
                    }
                    b'K' => {
                        match param_buf.as_slice() {
                            b"" | b"0" => cells.truncate(cursor),
                            b"1" => {
                                cells.drain(..cursor);
                                cursor = 0;
                            }
                            b"2" => {
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
            let ch = input[i..].chars().next().unwrap_or('\u{FFFD}');
            i += ch.len_utf8();
            if cursor < cells.len() {
                cells[cursor] = ch;
            } else {
                cells.push(ch);
            }
            cursor += 1;
        }
    }

    cells.into_iter().collect()
}

pub(crate) fn plain_text_from_ansi(input: &str) -> String {
    command_line_plain_text(input)
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Clone, Default, PartialEq)]
pub(crate) struct AnsiStyleState {
    pub(crate) foreground: Option<RGBA>,
    pub(crate) background: Option<RGBA>,
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) underline_style: UnderlineStyle,
    pub(crate) underline_color: Option<RGBA>,
    pub(crate) strikethrough: bool,
    pub(crate) dim: bool,
    pub(crate) reverse: bool,
    pub(crate) hidden: bool,
    pub(crate) overline: bool,
    pub(crate) hyperlink: Option<String>,
}

#[derive(Clone)]
pub(crate) struct AnsiTextRun {
    pub(crate) text: String,
    pub(crate) style: AnsiStyleState,
}

pub(crate) fn ansi_tag_name(style: &AnsiStyleState) -> Option<String> {
    if style.foreground.is_none()
        && style.background.is_none()
        && !style.bold
        && !style.italic
        && style.underline_style == UnderlineStyle::None
        && style.underline_color.is_none()
        && !style.strikethrough
        && !style.dim
        && !style.reverse
        && !style.hidden
        && !style.overline
        && style.hyperlink.is_none()
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

    let ul_style = match style.underline_style {
        UnderlineStyle::None => 0,
        UnderlineStyle::Single => 1,
        UnderlineStyle::Double => 2,
        UnderlineStyle::Curly => 3,
        UnderlineStyle::Dotted => 4,
        UnderlineStyle::Dashed => 5,
    };

    let link_key = match &style.hyperlink {
        Some(uri) => {
            let mut h: u64 = 0;
            for b in uri.bytes() {
                h = h.wrapping_mul(31).wrapping_add(b as u64);
            }
            format!("{:016x}", h)
        }
        None => "none".to_string(),
    };

    Some(format!(
        "ansi-run-fg:{}-bg:{}-b{}-i{}-u{}-uc:{}-s{}-d{}-lk:{}",
        rgba_key(style.foreground.as_ref()),
        rgba_key(style.background.as_ref()),
        style.bold as u8,
        style.italic as u8,
        ul_style,
        rgba_key(style.underline_color.as_ref()),
        style.strikethrough as u8,
        style.dim as u8,
        link_key,
    ))
}

pub(crate) fn ensure_ansi_text_tag(buffer: &TextBuffer, style: &AnsiStyleState) -> Option<gtk4::TextTag> {
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
    if style.hyperlink.is_some() && style.foreground.is_none() {
        tag.set_foreground_rgba(Some(&RGBA::new(0.4, 0.6, 1.0, 1.0)));
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
    match style.underline_style {
        UnderlineStyle::None => {}
        UnderlineStyle::Single => tag.set_underline(gtk4::pango::Underline::Single),
        UnderlineStyle::Double => tag.set_underline(gtk4::pango::Underline::Double),
        UnderlineStyle::Curly => tag.set_underline(gtk4::pango::Underline::Error),
        UnderlineStyle::Dotted | UnderlineStyle::Dashed => {
            tag.set_underline(gtk4::pango::Underline::Single);
        }
    }
    if style.hyperlink.is_some() && style.underline_style == UnderlineStyle::None {
        tag.set_underline(gtk4::pango::Underline::Single);
    }
    if let Some(ul_color) = style.underline_color {
        tag.set_underline_rgba(Some(&ul_color));
    }
    if style.strikethrough {
        tag.set_strikethrough(true);
    }

    tag_table.add(&tag);
    Some(tag)
}


pub(crate) fn parse_sgr_params(style: &mut AnsiStyleState, params: &[String], palette: &[RGBA; 16]) {
    let mut index = 0;
    while index < params.len() {
        // Handle colon-separated subparams (e.g., "4:3" for curly underline, "58:2:r:g:b")
        if params[index].contains(':') {
            let sub_parts: Vec<&str> = params[index].split(':').collect();
            let base = sub_parts[0].parse::<u32>().unwrap_or(0);
            match base {
                4 => {
                    let sub = sub_parts.get(1).and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
                    style.underline_style = match sub {
                        0 => UnderlineStyle::None,
                        1 => UnderlineStyle::Single,
                        2 => UnderlineStyle::Double,
                        3 => UnderlineStyle::Curly,
                        4 => UnderlineStyle::Dotted,
                        5 => UnderlineStyle::Dashed,
                        _ => UnderlineStyle::Single,
                    };
                }
                58 => {
                    // 58:2:r:g:b or 58:5:n (underline color)
                    let mode = sub_parts.get(1).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
                    if mode == 5 && sub_parts.len() >= 3 {
                        if let Ok(idx) = sub_parts[2].parse::<u8>() {
                            let (r, g, b) = ansi256_to_rgb(idx, palette);
                            style.underline_color = Some(RGBA::new(
                                r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0,
                            ));
                        }
                    } else if mode == 2 && sub_parts.len() >= 5 {
                        if let (Ok(r), Ok(g), Ok(b)) = (
                            sub_parts[2].parse::<u8>(),
                            sub_parts[3].parse::<u8>(),
                            sub_parts[4].parse::<u8>(),
                        ) {
                            style.underline_color = Some(RGBA::new(
                                r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0,
                            ));
                        }
                    }
                }
                _ => {}
            }
            index += 1;
            continue;
        }

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
            4 => style.underline_style = UnderlineStyle::Single,
            9 => style.strikethrough = true,
            22 => {
                style.bold = false;
                style.dim = false;
            }
            23 => style.italic = false,
            24 => {
                style.underline_style = UnderlineStyle::None;
                style.underline_color = None;
            }
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
            58 => {
                if index + 2 < params.len() && params[index + 1] == "5" {
                    if let Ok(color_index) = params[index + 2].parse::<u8>() {
                        let (r, g, b) = ansi256_to_rgb(color_index, palette);
                        style.underline_color = Some(RGBA::new(
                            r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0,
                        ));
                    }
                    index += 2;
                } else if index + 4 < params.len() && params[index + 1] == "2" {
                    if let (Ok(r), Ok(g), Ok(b)) = (
                        params[index + 2].parse::<u8>(),
                        params[index + 3].parse::<u8>(),
                        params[index + 4].parse::<u8>(),
                    ) {
                        style.underline_color = Some(RGBA::new(
                            r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0,
                        ));
                    }
                    index += 4;
                }
            }
            59 => style.underline_color = None,
            7 => style.reverse = true,
            8 => style.hidden = true,
            27 => style.reverse = false,
            28 => style.hidden = false,
            53 => style.overline = true,
            55 => style.overline = false,
            _ => {}
        }

        index += 1;
    }
}

/// Parse ANSI text with proper cursor movement handling
/// This ensures colors align with the final text after \r and cursor movements
pub(crate) fn ansi_text_runs(input: &str, palette: &[RGBA; 16]) -> Vec<AnsiTextRun> {
    let bytes = input.as_bytes();
    let mut runs: Vec<AnsiTextRun> = Vec::new();
    let mut current_style = AnsiStyleState::default();

    let mut cells: Vec<(char, AnsiStyleState)> = Vec::new();
    let mut cursor = 0usize;
    let mut i = 0;
    let mut param_buf: Vec<u8> = Vec::with_capacity(32);

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            param_buf.clear();
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                param_buf.push(bytes[i]);
                i += 1;
            }

            if i < bytes.len() {
                let final_byte = bytes[i];
                i += 1;

                match final_byte {
                    b'm' => {
                        let params_str: Vec<String> = if param_buf.is_empty() {
                            vec!["0".to_string()]
                        } else {
                            param_buf.split(|&b| b == b';')
                                .map(|p| {
                                    if p.is_empty() { "0".to_string() }
                                    else { String::from_utf8_lossy(p).into_owned() }
                                })
                                .collect()
                        };
                        parse_sgr_params(&mut current_style, &params_str, palette);
                    }
                    b'D' => {
                        let count = parse_param_first(&param_buf, 1);
                        cursor = cursor.saturating_sub(count);
                    }
                    b'C' => {
                        let count = parse_param_first(&param_buf, 1);
                        cursor = (cursor + count).min(cells.len());
                    }
                    b'G' => {
                        let col = parse_param_first(&param_buf, 1);
                        cursor = col.saturating_sub(1).min(cells.len());
                    }
                    b'K' => {
                        match param_buf.as_slice() {
                            b"" | b"0" => cells.truncate(cursor),
                            b"1" => {
                                cells.drain(..cursor);
                                cursor = 0;
                            }
                            b"2" => {
                                cells.clear();
                                cursor = 0;
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        } else if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b']' {
            let osc_start = i + 2;
            let osc_end = skip_osc_sequence(bytes, i);
            let osc_payload_end = if osc_end > 0 && osc_end <= bytes.len() && bytes.get(osc_end - 1) == Some(&0x07) {
                osc_end - 1
            } else if osc_end >= 2 && bytes.get(osc_end - 2) == Some(&0x1b) && bytes.get(osc_end - 1) == Some(&b'\\') {
                osc_end - 2
            } else {
                osc_end
            };
            if osc_start < osc_payload_end && osc_payload_end <= bytes.len() {
                if let Ok(osc_str) = std::str::from_utf8(&bytes[osc_start..osc_payload_end]) {
                    if let Some(rest) = osc_str.strip_prefix("8;") {
                        if let Some(sep) = rest.find(';') {
                            let uri = &rest[sep + 1..];
                            if uri.is_empty() {
                                current_style.hyperlink = None;
                            } else {
                                current_style.hyperlink = Some(uri.to_string());
                            }
                        }
                    }
                }
            }
            i = osc_end;
        } else if bytes[i] == 0x1b && i + 1 < bytes.len() {
            i = skip_escape_sequence(bytes, i);
        } else if bytes[i] == b'\r' {
            cursor = 0;
            i += 1;
        } else if bytes[i] == b'\n' {
            for &(ch, ref style) in &cells {
                if runs.is_empty() || runs.last().unwrap().style != *style {
                    runs.push(AnsiTextRun {
                        text: String::from(ch),
                        style: style.clone(),
                    });
                } else {
                    runs.last_mut().unwrap().text.push(ch);
                }
            }
            cells.clear();
            runs.push(AnsiTextRun {
                text: "\n".to_string(),
                style: current_style.clone(),
            });
            cursor = 0;
            i += 1;
        } else if bytes[i] == b'\x08' {
            cursor = cursor.saturating_sub(1);
            i += 1;
        } else {
            let ch = input[i..].chars().next().unwrap_or('\u{FFFD}');
            i += ch.len_utf8();

            if cursor < cells.len() {
                cells[cursor] = (ch, current_style.clone());
            } else {
                cells.push((ch, current_style.clone()));
            }
            cursor += 1;
        }
    }

    // Convert remaining cells to runs by merging adjacent cells with the same style
    let mut current_run_text = String::new();
    let mut current_run_style = AnsiStyleState::default();
    let mut first = true;

    for (ch, style) in cells {
        if first {
            current_run_text.push(ch);
            current_run_style = style;
            first = false;
        } else if style == current_run_style {
            current_run_text.push(ch);
        } else {
            if !current_run_text.is_empty() {
                runs.push(AnsiTextRun {
                    text: std::mem::take(&mut current_run_text),
                    style: current_run_style.clone(),
                });
            }
            current_run_text.push(ch);
            current_run_style = style;
        }
    }

    if !current_run_text.is_empty() {
        runs.push(AnsiTextRun {
            text: current_run_text,
            style: current_run_style,
        });
    }

    runs
}

pub(crate) fn apply_ansi_runs_to_buffer(buffer: &TextBuffer, start_offset: usize, runs: &[AnsiTextRun]) {
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

        if let Some(ref uri) = run.style.hyperlink {
            let link_tag_name = format!("osc8-link:{}", uri);
            let tag_table = buffer.tag_table();
            let link_tag = if let Some(existing) = tag_table.lookup(&link_tag_name) {
                existing
            } else {
                let new_tag = gtk4::TextTag::new(Some(&link_tag_name));
                tag_table.add(&new_tag);
                new_tag
            };
            let start_iter = buffer.iter_at_offset(offset as i32);
            let end_iter = buffer.iter_at_offset((offset + len) as i32);
            buffer.apply_tag(&link_tag, &start_iter, &end_iter);
        }

        offset += len;
    }
}


pub(crate) fn set_active_prompt_buffer(buffer: &TextBuffer, prompt: &str) {
    buffer.set_text(prompt);
}


#[allow(clippy::too_many_arguments)]
pub(crate) fn set_active_command_buffer_at(
    buffer: &TextBuffer,
    cmd: &str,
    preedit: &str,
    cursor_visible: bool,
    suggestion: &str,
    cursor_color: &RGBA,
    cursor_foreground: &RGBA,
    explicit_cursor_pos: Option<usize>,
) {
    let cursor_pos = explicit_cursor_pos.unwrap_or_else(|| cmd.chars().count() + preedit.chars().count());
    let text = format!("{}{} {}", cmd, preedit, suggestion);
    buffer.set_text(&text);
    let cursor_iter = buffer.iter_at_offset(cursor_pos as i32);
    buffer.place_cursor(&cursor_iter);

    let tag_table = buffer.tag_table();

    if tag_table.lookup("cursor").is_none() {
        let tag = gtk4::TextTag::new(Some("cursor"));
        tag_table.add(&tag);
    }

    if cursor_visible {
        if let Some(tag) = tag_table.lookup("cursor") {
            tag.set_background_rgba(Some(cursor_color));
            tag.set_foreground_rgba(Some(cursor_foreground));
            let start_iter = buffer.iter_at_offset(cursor_pos as i32);
            let end_iter = buffer.iter_at_offset((cursor_pos + 1) as i32);
            buffer.apply_tag(&tag, &start_iter, &end_iter);
        }
    }

    if !preedit.is_empty() {
        if tag_table.lookup("preedit").is_none() {
            let tag = gtk4::TextTag::new(Some("preedit"));
            tag.set_underline(gtk4::pango::Underline::Single);
            tag_table.add(&tag);
        }

        if let Some(tag) = tag_table.lookup("preedit") {
            let start_pos = cmd.chars().count();
            let end_pos = start_pos + preedit.chars().count();
            let start_iter = buffer.iter_at_offset(start_pos as i32);
            let end_iter = buffer.iter_at_offset(end_pos as i32);
            buffer.apply_tag(&tag, &start_iter, &end_iter);
        }
    }

    if suggestion.is_empty() {
        return;
    }

    if tag_table.lookup("suggestion").is_none() {
        let tag = gtk4::TextTag::new(Some("suggestion"));
        tag.set_style(gtk4::pango::Style::Italic);
        tag.set_foreground_rgba(Some(&RGBA::new(0.5, 0.5, 0.5, 0.7)));
        tag_table.add(&tag);
    }

    if let Some(tag) = tag_table.lookup("suggestion") {
        let start_pos = cursor_pos + 1;
        let end_pos = start_pos + suggestion.chars().count();
        let start_iter = buffer.iter_at_offset(start_pos as i32);
        let end_iter = buffer.iter_at_offset(end_pos as i32);
        buffer.apply_tag(&tag, &start_iter, &end_iter);
    }
}

/// Returns (cursor_col_in_last_line, after_newline).
/// after_newline=true means cursor is at start of a new line following \n
/// (buffer may not render that empty trailing line).
pub(crate) fn output_cursor_col(output: &str) -> (usize, bool) {
    let bytes = output.as_bytes();
    let mut cells_len = 0usize;
    let mut cursor = 0usize;
    let mut after_newline = false;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            let mut params: Vec<String> = Vec::new();
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
                        let count = params.first().and_then(|p| p.parse::<usize>().ok()).unwrap_or(1);
                        cursor = cursor.saturating_sub(count);
                        after_newline = false;
                    }
                    b'C' => {
                        let count = params.first().and_then(|p| p.parse::<usize>().ok()).unwrap_or(1);
                        cursor = (cursor + count).min(cells_len);
                        after_newline = false;
                    }
                    b'G' => {
                        let col = params.first().and_then(|p| p.parse::<usize>().ok()).unwrap_or(1);
                        cursor = col.saturating_sub(1).min(cells_len);
                        after_newline = false;
                    }
                    b'K' => {
                        let mode = params.first().map(String::as_str).unwrap_or("0");
                        match mode {
                            "" | "0" => { cells_len = cursor; }
                            "1" => { cursor = 0; after_newline = false; }
                            "2" => { cells_len = 0; cursor = 0; after_newline = false; }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        } else if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b']' {
            i = skip_osc_sequence(bytes, i);
        } else if bytes[i] == 0x1b {
            i = skip_escape_sequence(bytes, i);
        } else if bytes[i] == b'\n' {
            cursor = 0;
            cells_len = 0;
            after_newline = true;
            i += 1;
        } else if bytes[i] == b'\r' {
            cursor = 0;
            after_newline = false;
            i += 1;
        } else if bytes[i] == b'\x08' {
            cursor = cursor.saturating_sub(1);
            after_newline = false;
            i += 1;
        } else {
            let ch_len = if bytes[i] & 0x80 == 0 { 1 }
                else if bytes[i] & 0xe0 == 0xc0 { 2 }
                else if bytes[i] & 0xf0 == 0xe0 { 3 }
                else if bytes[i] & 0xf8 == 0xf0 { 4 }
                else { 1 };
            i = (i + ch_len).min(bytes.len());
            if cursor >= cells_len { cells_len += 1; }
            cursor += 1;
            after_newline = false;
        }
    }
    (cursor, after_newline)
}

pub(crate) fn apply_output_cursor(
    buffer: &TextBuffer,
    output: &str,
    cursor_color: &RGBA,
    cursor_foreground: &RGBA,
) {
    let (cursor_col, after_newline) = output_cursor_col(output);

    let tag_table = buffer.tag_table();
    if tag_table.lookup("output-cursor").is_none() {
        let tag = gtk4::TextTag::new(Some("output-cursor"));
        tag_table.add(&tag);
    }
    if let Some(tag) = tag_table.lookup("output-cursor") {
        tag.set_background_rgba(Some(cursor_color));
        tag.set_foreground_rgba(Some(cursor_foreground));
    }

    let buffer_len = buffer.char_count() as usize;
    let cursor_abs: usize = if after_newline {
        buffer_len
    } else {
        let last_line = (buffer.line_count() - 1).max(0);
        if let Some(line_start) = buffer.iter_at_line(last_line) {
            line_start.offset() as usize + cursor_col
        } else {
            buffer_len
        }
    };

    if cursor_abs >= buffer.char_count() as usize {
        let mut end_iter = buffer.end_iter();
        buffer.insert(&mut end_iter, " ");
    }

    if let Some(tag) = tag_table.lookup("output-cursor") {
        let start = buffer.iter_at_offset(cursor_abs as i32);
        let end = buffer.iter_at_offset(cursor_abs as i32 + 1);
        buffer.apply_tag(&tag, &start, &end);
    }
}

pub(crate) fn set_active_output_buffer(
    buffer: &TextBuffer,
    output: &str,
    palette: &[RGBA; 16],
    cursor_colors: Option<(&RGBA, &RGBA)>,
) {
    let output_no_ansi = strip_ansi(output);
    let output_plain = output_no_ansi
        .lines()
        .map(command_line_plain_text)
        .collect::<Vec<_>>()
        .join("\n");
    buffer.set_text(&output_plain);

    let output_runs = ansi_text_runs(output, palette);
    apply_ansi_runs_to_buffer(buffer, 0, &output_runs);

    if let Some((cursor_color, cursor_foreground)) = cursor_colors {
        apply_output_cursor(buffer, output, cursor_color, cursor_foreground);
    }
}



pub(crate) fn ansi_to_pango(input: &str, palette: &[RGBA; 16]) -> String {
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
                        // Handle colon-separated subparams (e.g., "4:3")
                        if param_str.contains(':') {
                            let sub_parts: Vec<&str> = param_str.split(':').collect();
                            if let Ok(base) = sub_parts[0].parse::<u32>() {
                                if base == 4 {
                                    let sub = sub_parts.get(1).and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
                                    let ul = match sub {
                                        0 => { continue; }
                                        2 => "double",
                                        3 => "error",
                                        _ => "single",
                                    };
                                    out.push_str(&format!("<span underline=\"{}\">", ul));
                                    open_spans += 1;
                                }
                            }
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
                                out.push_str("<span alpha=\"60%\">");
                                open_spans += 1;
                            }
                            Ok(9) => {
                                out.push_str("<span strikethrough=\"true\">");
                                open_spans += 1;
                            }
                            Ok(7) => {
                                // Reverse video - for now use background/foreground swap via CSS
                                out.push_str("<span style=\"reverse\">");
                                open_spans += 1;
                            }
                            Ok(8) => {
                                // Hidden/conceal text - use very low opacity
                                out.push_str("<span alpha=\"5%\">");
                                open_spans += 1;
                            }
                            Ok(27) => {
                                out.push_str("</span>");
                                if open_spans > 0 { open_spans -= 1; }
                            }
                            Ok(28) => {
                                out.push_str("</span>");
                                if open_spans > 0 { open_spans -= 1; }
                            }
                            Ok(53) => {
                                // Overline - use overline attribute (if supported)
                                out.push_str("<span overline=\"single\">");
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
