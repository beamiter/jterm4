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
                            b'K' => match param_buf.as_slice() {
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
                            },
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
    use super::contains_case_insensitive as cci;

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
}
