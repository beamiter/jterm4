//! ansi — small ANSI helpers used to build the plain-text shadow
//! (`BlockData.output`) consumed by search / export / "copy block".
//! All SGR→TextTag rendering lives in VTE now; see `block_view/blocks.rs`.

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

/// Upper bounds on the reconstructed screen so a bogus absolute-position
/// parameter (`\x1b[9999999999H`) can never make us allocate an unbounded grid.
/// Real full-screen redraws (top, htop, watch) stay well under these.
const MAX_GRID_ROWS: usize = 100_000;
const MAX_GRID_COLS: usize = 10_000;

/// Grow `grid` so `row` is a valid index and return a mutable handle to it.
fn ensure_row(grid: &mut Vec<Vec<char>>, row: usize) -> &mut Vec<char> {
    while grid.len() <= row {
        grid.push(Vec::new());
    }
    &mut grid[row]
}

/// Reconstruct the on-screen text a stream of bytes would leave behind, applying
/// a full two-dimensional cursor model (home/absolute positioning, vertical
/// moves, and screen clears) in addition to the horizontal CR/erase semantics.
///
/// This matters for programs that repaint in place without switching to the
/// alternate screen — `top`, `watch`, and multi-line progress UIs emit
/// cursor-home (`\x1b[H`) before every frame. A horizontal-only model treats
/// each frame's lines as fresh output and concatenates them, so a long `top`
/// session grows an unbounded block. Modelling vertical position collapses the
/// repeated frames down to the final one, matching what the live VTE showed.
///
/// The `bool` reports whether a full-screen clear (`\x1b[2J`/`\x1b[3J`) was
/// seen — retained for callers that key off it.
pub(crate) fn strip_ansi_with_clear_detect(input: &str) -> (String, bool) {
    let bytes = input.as_bytes();
    if memchr::memchr3(0x1b, b'\r', b'\x08', bytes).is_none() {
        return (input.to_string(), false);
    }

    let mut grid: Vec<Vec<char>> = vec![Vec::new()];
    let mut row = 0usize;
    let mut col = 0usize;
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

                        // Private-mode sequences (`\x1b[?…`, `\x1b[>…`) never move
                        // the cursor or edit cells in this shadow; skip them so a
                        // trailing `h`/`l` is not mistaken for a real command.
                        if matches!(param_buf.first(), Some(0x3c..=0x3f)) {
                            continue;
                        }

                        match final_byte {
                            b'H' | b'f' => {
                                let r = parse_param_nth(&param_buf, 0, 1);
                                let c = parse_param_nth(&param_buf, 1, 1);
                                row = r.saturating_sub(1).min(MAX_GRID_ROWS);
                                col = c.saturating_sub(1).min(MAX_GRID_COLS);
                            }
                            b'A' => {
                                let n = parse_param_first(&param_buf, 1);
                                row = row.saturating_sub(n);
                            }
                            b'B' | b'e' => {
                                let n = parse_param_first(&param_buf, 1);
                                row = (row + n).min(MAX_GRID_ROWS);
                            }
                            b'E' => {
                                let n = parse_param_first(&param_buf, 1);
                                row = (row + n).min(MAX_GRID_ROWS);
                                col = 0;
                            }
                            b'F' => {
                                let n = parse_param_first(&param_buf, 1);
                                row = row.saturating_sub(n);
                                col = 0;
                            }
                            b'd' => {
                                let r = parse_param_first(&param_buf, 1);
                                row = r.saturating_sub(1).min(MAX_GRID_ROWS);
                            }
                            b'C' | b'a' => {
                                let n = parse_param_first(&param_buf, 1);
                                col = (col + n).min(MAX_GRID_COLS);
                            }
                            b'D' => {
                                let n = parse_param_first(&param_buf, 1);
                                col = col.saturating_sub(n);
                            }
                            b'G' | b'`' => {
                                let c = parse_param_first(&param_buf, 1);
                                col = c.saturating_sub(1).min(MAX_GRID_COLS);
                            }
                            b'J' => match param_buf.as_slice() {
                                b"" | b"0" => {
                                    // Erase from the cursor to the end of screen.
                                    if row < grid.len() && col < grid[row].len() {
                                        grid[row].truncate(col);
                                    }
                                    grid.truncate(row + 1);
                                }
                                b"1" => {
                                    // Erase from the start of screen up to the cursor.
                                    for r in grid.iter_mut().take(row) {
                                        r.clear();
                                    }
                                    if row < grid.len() {
                                        let upto = col.min(grid[row].len());
                                        for c in grid[row].iter_mut().take(upto) {
                                            *c = ' ';
                                        }
                                    }
                                }
                                b"2" | b"3" => {
                                    should_clear = true;
                                    grid.clear();
                                    grid.push(Vec::new());
                                    row = 0;
                                    col = 0;
                                }
                                _ => {}
                            },
                            b'K' => {
                                let cells = ensure_row(&mut grid, row);
                                match param_buf.as_slice() {
                                    b"" | b"0" => {
                                        if col < cells.len() {
                                            cells.truncate(col);
                                        }
                                    }
                                    b"1" => {
                                        let upto = col.min(cells.len());
                                        for c in cells.iter_mut().take(upto) {
                                            *c = ' ';
                                        }
                                    }
                                    b"2" => cells.clear(),
                                    _ => {}
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
        } else if bytes[i] == b'\n' {
            row = (row + 1).min(MAX_GRID_ROWS);
            col = 0;
            // Materialise the destination row so a trailing newline still emits
            // its blank line, matching a real terminal's line feed.
            ensure_row(&mut grid, row);
            i += 1;
        } else if bytes[i] == b'\r' {
            col = 0;
            i += 1;
        } else if bytes[i] == b'\x08' {
            col = col.saturating_sub(1);
            i += 1;
        } else {
            let ch = input[i..].chars().next().unwrap_or('\u{FFFD}');
            i += ch.len_utf8();
            if col < MAX_GRID_COLS {
                let cells = ensure_row(&mut grid, row);
                if col < cells.len() {
                    cells[col] = ch;
                } else {
                    while cells.len() < col {
                        cells.push(' ');
                    }
                    cells.push(ch);
                }
            }
            col += 1;
        }
    }
    let mut result = String::with_capacity(input.len());
    for (line_idx, cells) in grid.iter().enumerate() {
        if line_idx > 0 {
            result.push('\n');
        }
        for &ch in cells {
            result.push(ch);
        }
    }
    (result, should_clear)
}

/// Whether a captured output stream repaints in place using vertical cursor
/// motion — the fingerprint of a full-screen program that does *not* switch to
/// the alternate screen (`top`, `watch`, multi-line progress bars). Such a
/// stream must be collapsed to its final frame before it feeds a scrollback-
/// backed VTE, or every frame stacks. A lone leading `\x1b[H` (some tools emit
/// one before ordinary output) is deliberately not counted: repaint sequences
/// only matter once real lines already exist above the cursor.
pub(crate) fn output_has_vertical_repaint(input: &str) -> bool {
    let bytes = input.as_bytes();
    let mut seen_newline = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                seen_newline = true;
                i += 1;
            }
            0x1b if i + 1 < bytes.len() => match bytes[i + 1] {
                b'[' => {
                    let params_start = i + 2;
                    let mut j = params_start;
                    while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                        j += 1;
                    }
                    if j >= bytes.len() {
                        return false;
                    }
                    let is_private = matches!(bytes.get(params_start), Some(0x3c..=0x3f));
                    if seen_newline && !is_private {
                        match bytes[j] {
                            b'A' | b'H' | b'f' | b'd' | b'F' => return true,
                            b'J' => {
                                let params = &bytes[params_start..j];
                                if matches!(params, b"1" | b"2" | b"3") {
                                    return true;
                                }
                            }
                            _ => {}
                        }
                    }
                    i = j + 1;
                }
                b']' => i = skip_osc_sequence(bytes, i),
                _ => i = skip_escape_sequence(bytes, i),
            },
            _ => i += 1,
        }
    }
    false
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SgrColor {
    Default,
    Palette(u8),
    Rgb(u8, u8, u8),
}

/// The subset of SGR state a repaint snapshot needs to reproduce. `top`'s look
/// (bold values, reverse-video header/footer bars, colored columns) is entirely
/// these attributes plus the 16-colour and 256/truecolour palettes.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Sgr {
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    reverse: bool,
    hidden: bool,
    strike: bool,
    fg: SgrColor,
    bg: SgrColor,
}

impl Default for Sgr {
    fn default() -> Self {
        Sgr {
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            blink: false,
            reverse: false,
            hidden: false,
            strike: false,
            fg: SgrColor::Default,
            bg: SgrColor::Default,
        }
    }
}

/// Apply an SGR parameter list (`\x1b[…m`) to the running attribute state.
fn apply_sgr(cur: &mut Sgr, params: &[u8]) {
    if params.is_empty() {
        *cur = Sgr::default();
        return;
    }
    let parts: Vec<&[u8]> = params.split(|&b| b == b';').collect();
    let num = |p: &[u8]| -> usize {
        let mut v = 0usize;
        for &b in p {
            if b.is_ascii_digit() {
                v = v.saturating_mul(10).saturating_add((b - b'0') as usize);
            } else {
                return usize::MAX;
            }
        }
        v
    };
    let mut i = 0;
    while i < parts.len() {
        match num(parts[i]) {
            0 | usize::MAX => *cur = Sgr::default(), // bare/`;`/garbage → reset
            1 => cur.bold = true,
            2 => cur.dim = true,
            3 => cur.italic = true,
            4 => cur.underline = true,
            5 | 6 => cur.blink = true,
            7 => cur.reverse = true,
            8 => cur.hidden = true,
            9 => cur.strike = true,
            21 | 22 => {
                cur.bold = false;
                cur.dim = false;
            }
            23 => cur.italic = false,
            24 => cur.underline = false,
            25 => cur.blink = false,
            27 => cur.reverse = false,
            28 => cur.hidden = false,
            29 => cur.strike = false,
            n @ 30..=37 => cur.fg = SgrColor::Palette((n - 30) as u8),
            39 => cur.fg = SgrColor::Default,
            n @ 40..=47 => cur.bg = SgrColor::Palette((n - 40) as u8),
            49 => cur.bg = SgrColor::Default,
            n @ 90..=97 => cur.fg = SgrColor::Palette((n - 90 + 8) as u8),
            n @ 100..=107 => cur.bg = SgrColor::Palette((n - 100 + 8) as u8),
            sel @ (38 | 48) => {
                // Extended colour: `38;5;n` (256) or `38;2;r;g;b` (truecolour).
                let mode = parts.get(i + 1).map(|p| num(p)).unwrap_or(usize::MAX);
                let color = if mode == 5 {
                    let idx = parts.get(i + 2).map(|p| num(p)).unwrap_or(0);
                    i += 2;
                    SgrColor::Palette(idx.min(255) as u8)
                } else if mode == 2 {
                    let c = |k: usize| parts.get(i + k).map(|p| num(p)).unwrap_or(0).min(255) as u8;
                    let rgb = SgrColor::Rgb(c(2), c(3), c(4));
                    i += 4;
                    rgb
                } else {
                    SgrColor::Default
                };
                if sel == 38 {
                    cur.fg = color;
                } else {
                    cur.bg = color;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Serialise attribute state to SGR parameters, always rebuilt from a `0` reset
/// so each transition is self-contained.
fn sgr_codes(s: &Sgr) -> String {
    let mut v: Vec<String> = vec!["0".into()];
    for (on, code) in [
        (s.bold, "1"),
        (s.dim, "2"),
        (s.italic, "3"),
        (s.underline, "4"),
        (s.blink, "5"),
        (s.reverse, "7"),
        (s.hidden, "8"),
        (s.strike, "9"),
    ] {
        if on {
            v.push(code.into());
        }
    }
    let mut push_color = |c: SgrColor, base: usize, ext: usize| match c {
        SgrColor::Default => {}
        SgrColor::Palette(i) if i < 8 => v.push((base + i as usize).to_string()),
        SgrColor::Palette(i) if i < 16 => v.push((base + 60 + (i as usize - 8)).to_string()),
        SgrColor::Palette(i) => {
            v.push(ext.to_string());
            v.push("5".into());
            v.push(i.to_string());
        }
        SgrColor::Rgb(r, g, b) => {
            v.push(ext.to_string());
            v.push("2".into());
            v.push(r.to_string());
            v.push(g.to_string());
            v.push(b.to_string());
        }
    };
    push_color(s.fg, 30, 38);
    push_color(s.bg, 40, 48);
    v.join(";")
}

/// Collapse a full-screen repaint stream (see [`output_has_vertical_repaint`])
/// to a single clean frame, **preserving colour**.
///
/// The whole stream is replayed through a 2D screen model that stores each
/// cell's character *and* SGR attributes. `top` and friends repaint
/// *incrementally* — a refresh rewrites only the lines that changed (clock,
/// CPU%, times) and leaves static rows untouched — so accumulating every write
/// reconstructs the final screen exactly; isolating a single frame would drop
/// the unchanged rows. `\x1b[K`/`\x1b[J` erases fill with the active background,
/// which is how top's reverse-video header/footer bars extend to the screen
/// edge, so those are reproduced too.
///
/// The result is serialised with `\r\n` line breaks (a finished VTE treats a
/// bare `\n` as line-feed only, which would stair-step the frame) and minimal
/// SGR transitions. Trailing blank rows and trailing *default-styled* blank
/// cells are trimmed (coloured trailing cells — the bars — are kept), so the
/// snapshot is tight without wrapping at the VTE's right edge.
pub(crate) fn collapse_repaint_output(input: &str, cols: usize) -> String {
    let bytes = input.as_bytes();
    let width = cols.clamp(1, MAX_GRID_COLS);
    let blank = (' ', Sgr::default());

    let mut grid: Vec<Vec<(char, Sgr)>> = vec![Vec::new()];
    let mut row = 0usize;
    let mut col = 0usize;
    let mut cur = Sgr::default();
    let mut i = 0;

    let ensure = |grid: &mut Vec<Vec<(char, Sgr)>>, row: usize| {
        while grid.len() <= row {
            grid.push(Vec::new());
        }
    };

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'[' => {
                    i += 2;
                    let ps = i;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i >= bytes.len() {
                        break;
                    }
                    let final_byte = bytes[i];
                    let params = &bytes[ps..i];
                    i += 1;
                    if matches!(params.first(), Some(0x3c..=0x3f)) {
                        continue; // private mode — no cursor/text effect here
                    }
                    match final_byte {
                        b'm' => apply_sgr(&mut cur, params),
                        b'H' | b'f' => {
                            row = parse_param_nth(params, 0, 1)
                                .saturating_sub(1)
                                .min(MAX_GRID_ROWS);
                            col = parse_param_nth(params, 1, 1).saturating_sub(1).min(width);
                        }
                        b'A' => row = row.saturating_sub(parse_param_first(params, 1)),
                        b'B' | b'e' => {
                            row = (row + parse_param_first(params, 1)).min(MAX_GRID_ROWS)
                        }
                        b'E' => {
                            row = (row + parse_param_first(params, 1)).min(MAX_GRID_ROWS);
                            col = 0;
                        }
                        b'F' => {
                            row = row.saturating_sub(parse_param_first(params, 1));
                            col = 0;
                        }
                        b'd' => {
                            row = parse_param_first(params, 1)
                                .saturating_sub(1)
                                .min(MAX_GRID_ROWS)
                        }
                        b'C' | b'a' => col = (col + parse_param_first(params, 1)).min(width),
                        b'D' => col = col.saturating_sub(parse_param_first(params, 1)),
                        b'G' | b'`' => {
                            col = parse_param_first(params, 1).saturating_sub(1).min(width)
                        }
                        b'K' => {
                            ensure(&mut grid, row);
                            let cells = &mut grid[row];
                            let fill = (' ', cur);
                            match params {
                                b"" | b"0" => {
                                    cells.truncate(col);
                                    while cells.len() < width {
                                        cells.push(fill);
                                    }
                                }
                                b"1" => {
                                    while cells.len() < col {
                                        cells.push(blank);
                                    }
                                    for c in cells.iter_mut().take(col) {
                                        *c = fill;
                                    }
                                }
                                b"2" => {
                                    cells.clear();
                                    for _ in 0..width {
                                        cells.push(fill);
                                    }
                                }
                                _ => {}
                            }
                        }
                        b'J' => match params {
                            b"" | b"0" => {
                                ensure(&mut grid, row);
                                let cells = &mut grid[row];
                                cells.truncate(col);
                                while cells.len() < width {
                                    cells.push((' ', cur));
                                }
                                grid.truncate(row + 1);
                            }
                            b"1" => {
                                for r in grid.iter_mut().take(row) {
                                    r.clear();
                                }
                                ensure(&mut grid, row);
                                let cells = &mut grid[row];
                                while cells.len() < col {
                                    cells.push(blank);
                                }
                                for c in cells.iter_mut().take(col) {
                                    *c = (' ', cur);
                                }
                            }
                            b"2" | b"3" => {
                                grid.clear();
                                grid.push(Vec::new());
                                row = 0;
                                col = 0;
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
                b']' => i = skip_osc_sequence(bytes, i),
                _ => i = skip_escape_sequence(bytes, i),
            }
        } else if bytes[i] == b'\n' {
            row = (row + 1).min(MAX_GRID_ROWS);
            col = 0;
            ensure(&mut grid, row);
            i += 1;
        } else if bytes[i] == b'\r' {
            col = 0;
            i += 1;
        } else if bytes[i] == b'\x08' {
            col = col.saturating_sub(1);
            i += 1;
        } else {
            let ch = input[i..].chars().next().unwrap_or('\u{FFFD}');
            i += ch.len_utf8();
            if col < width {
                ensure(&mut grid, row);
                let cells = &mut grid[row];
                while cells.len() <= col {
                    cells.push(blank);
                }
                cells[col] = (ch, cur);
            }
            col += 1;
        }
    }

    // Drop screen-padding rows at the bottom that carry no visible content.
    let is_blank_row =
        |r: &[(char, Sgr)]| r.iter().all(|&(ch, s)| ch == ' ' && s == Sgr::default());
    while grid.len() > 1 && grid.last().is_some_and(|r| is_blank_row(r)) {
        grid.pop();
    }

    let mut out = String::with_capacity(input.len().min(64 * 1024));
    for (ri, cells) in grid.iter().enumerate() {
        if ri > 0 {
            out.push_str("\r\n");
        }
        // Keep coloured trailing cells (the bars); trim default-styled padding.
        let mut end = cells.len();
        while end > 0 {
            let (ch, s) = cells[end - 1];
            if ch == ' ' && s == Sgr::default() {
                end -= 1;
            } else {
                break;
            }
        }
        let mut emitted = Sgr::default();
        for &(ch, s) in &cells[..end] {
            if s != emitted {
                out.push_str("\x1b[");
                out.push_str(&sgr_codes(&s));
                out.push('m');
                emitted = s;
            }
            out.push(ch);
        }
        if emitted != Sgr::default() {
            out.push_str("\x1b[0m");
        }
    }
    out
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
        // memchr_iter yields ascending positions, so once a candidate would run
        // past the end every later one does too: stop scanning this byte, but
        // still fall through to the uppercase-variant search below.
        if pos + needle.len() > haystack.len() {
            break;
        }
        let candidate = &haystack[pos..pos + needle.len()];
        if candidate
            .iter()
            .zip(needle.iter())
            .all(|(&h, &n)| h.to_ascii_lowercase() == n)
        {
            return true;
        }
    }
    // Also check uppercase variant of first byte
    let first_upper = first.to_ascii_uppercase();
    if first_upper != first {
        let finder = memchr::memchr_iter(first_upper, haystack);
        for pos in finder {
            if pos + needle.len() > haystack.len() {
                break;
            }
            let candidate = &haystack[pos..pos + needle.len()];
            if candidate
                .iter()
                .zip(needle.iter())
                .all(|(&h, &n)| h.to_ascii_lowercase() == n)
            {
                return true;
            }
        }
    }
    false
}

pub(crate) fn parse_param_first(buf: &[u8], default: usize) -> usize {
    parse_param_nth(buf, 0, default)
}

/// Parse the `n`-th semicolon-separated numeric parameter, saturating rather
/// than overflowing on absurdly long digit runs. An empty or non-numeric field
/// yields `default`.
pub(crate) fn parse_param_nth(buf: &[u8], n: usize, default: usize) -> usize {
    let Some(field) = buf.split(|&b| b == b';').nth(n) else {
        return default;
    };
    if field.is_empty() {
        return default;
    }
    let mut val = 0usize;
    for &b in field {
        if b.is_ascii_digit() {
            val = val.saturating_mul(10).saturating_add((b - b'0') as usize);
        } else {
            return default;
        }
    }
    if val == 0 {
        default
    } else {
        val
    }
}

pub(crate) fn strip_ansi(input: &str) -> String {
    strip_ansi_with_clear_detect(input).0
}

#[cfg(test)]
mod tests {
    use super::{contains_case_insensitive as cci, strip_ansi_with_clear_detect};

    #[test]
    fn matches_regardless_of_case() {
        assert!(cci(b"Hello World", b"hello"));
        assert!(cci(b"hello world", b"world"));
        assert!(cci(b"MiXeD", b"mixed"));
    }

    #[test]
    fn reports_absent_needle() {
        assert!(!cci(b"abcdef", b"xyz"));
    }

    #[test]
    fn empty_needle_always_matches() {
        assert!(cci(b"anything", b""));
    }

    #[test]
    fn finds_uppercase_first_byte_after_oob_lowercase_hit() {
        // Regression: the lowercase-first-byte scan finds 'f' at the final
        // position (out of bounds for a 2-byte needle). The old code returned
        // false there, skipping the uppercase-variant scan that matches "Fo"
        // at position 0.
        assert!(cci(b"Foxf", b"fo"));
    }

    #[test]
    fn strip_plain_multiline_output_uses_passthrough_result() {
        assert_eq!(
            strip_ansi_with_clear_detect("plain\ntext\n"),
            ("plain\ntext\n".to_string(), false)
        );
    }
}
