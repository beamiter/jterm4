//! select — semantic ("smart") double-click selection for command/output views.
//!
//! On double-click GTK selects a plain word (alnum run). This module instead
//! detects the semantic token under the cursor — URL, path, file:line:col,
//! IPv4, git SHA, key=value, quoted string, … — so a single double-click grabs
//! the whole meaningful unit.
use gtk4::prelude::*;
use gtk4::TextBuffer;
use regex::Regex;
use std::sync::LazyLock;

struct Pat {
    re: Regex,
    /// Capture group whose span is selected (0 = whole match).
    group: usize,
}

/// Ordered most-specific → least-specific. The first pattern whose chosen
/// capture group contains the click wins, so e.g. `src/main.rs:42` is grabbed
/// as a file:line before the bare-path or number rules get a chance.
static PATTERNS: LazyLock<Vec<Pat>> = LazyLock::new(|| {
    let p = |s: &str, g: usize| Pat {
        re: Regex::new(s).unwrap(),
        group: g,
    };
    vec![
        // Quoted strings → select the inner content (handles spaces inside).
        p(r#""([^"\n]*)""#, 1),
        p(r#"'([^'\n]*)'"#, 1),
        p(r#"`([^`\n]*)`"#, 1),
        // URL
        p(r#"((?:https?|ftp|file)://[^\s<>"'`)\]}]+)"#, 1),
        // email
        p(r#"([\w.+-]+@[\w-]+(?:\.[\w-]+)+)"#, 1),
        // file:line[:col]
        p(r#"((?:[~.]?[\w./+-]*\w):\d+(?::\d+)?)"#, 1),
        // absolute / home / relative path (must contain a slash)
        p(r#"((?:~|\.{1,2})?(?:/[\w.+@~-]+)+/?|(?:[\w.+-]+/)+[\w.+-]*)"#, 1),
        // IPv4[:port]
        p(r#"(\b\d{1,3}(?:\.\d{1,3}){3}(?::\d+)?)"#, 1),
        // key=value
        p(r#"([\w.-]+=[^\s'"]+)"#, 1),
        // git SHA
        p(r#"(\b[0-9a-f]{7,40}\b)"#, 1),
        // hex literal
        p(r#"(\b0x[0-9a-fA-F]+\b)"#, 1),
        // number
        p(r#"(\b\d+(?:\.\d+)?\b)"#, 1),
        // fallback: extended word
        p(r#"([\w@.+-]+)"#, 1),
    ]
});

/// Returns the char-offset span `[start, end)` within `line` of the semantic
/// token containing the char at `click_char`, or `None` if the click is on
/// whitespace / past the line end (caller should then fall back to default).
fn semantic_span(line: &str, click_char: usize) -> Option<(usize, usize)> {
    let click_byte = line
        .char_indices()
        .nth(click_char)
        .map(|(b, _)| b)
        .unwrap_or(line.len());

    for pat in PATTERNS.iter() {
        for caps in pat.re.captures_iter(line) {
            if let Some(m) = caps.get(pat.group) {
                if m.start() <= click_byte && click_byte < m.end() {
                    let s = line[..m.start()].chars().count();
                    let e = line[..m.end()].chars().count();
                    return Some((s, e));
                }
            }
        }
    }
    None
}

/// Resolve the semantic token at `iter` to a pair of buffer iters to select.
pub(crate) fn get_semantic_bounds_at_position(
    buffer: &TextBuffer,
    iter: &gtk4::TextIter,
) -> Option<(gtk4::TextIter, gtk4::TextIter)> {
    let mut line_start = *iter;
    line_start.set_line_offset(0);
    let mut line_end = *iter;
    if !line_end.ends_line() {
        line_end.forward_to_line_end();
    }
    let line_text = buffer.text(&line_start, &line_end, false).to_string();
    let click_char = iter.line_offset() as usize;

    let (s, e) = semantic_span(&line_text, click_char)?;

    let mut sel_start = line_start;
    sel_start.forward_chars(s as i32);
    let mut sel_end = line_start;
    sel_end.forward_chars(e as i32);
    Some((sel_start, sel_end))
}

#[cfg(test)]
mod tests {
    use super::semantic_span;

    fn sel(line: &str, at: usize) -> Option<&str> {
        semantic_span(line, at).map(|(s, e)| {
            let cs: Vec<char> = line.chars().collect();
            let slice: String = cs[s..e].iter().collect();
            Box::leak(slice.into_boxed_str()) as &str
        })
    }

    #[test]
    fn path() {
        assert_eq!(sel("cd /home/mm/projects/jterm4", 5), Some("/home/mm/projects/jterm4"));
    }

    #[test]
    fn file_line_col() {
        assert_eq!(sel("error at src/main.rs:42:7 here", 12), Some("src/main.rs:42:7"));
    }

    #[test]
    fn url() {
        assert_eq!(
            sel("see https://example.com/a?b=1 ok", 10),
            Some("https://example.com/a?b=1")
        );
    }

    #[test]
    fn ipv4_port() {
        assert_eq!(sel("conn 192.168.0.1:8080 up", 8), Some("192.168.0.1:8080"));
    }

    #[test]
    fn quoted_with_space() {
        assert_eq!(sel(r#"msg "hello world" end"#, 8), Some("hello world"));
    }

    #[test]
    fn key_value() {
        assert_eq!(sel("RUST_LOG=debug cargo", 3), Some("RUST_LOG=debug"));
    }

    #[test]
    fn whitespace_is_none() {
        assert_eq!(sel("a  b", 1), None);
    }

    #[test]
    fn plain_word() {
        assert_eq!(sel("just a word", 7), Some("word"));
    }
}
