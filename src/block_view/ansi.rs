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
