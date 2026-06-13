//! url — extracted from block_view (mechanical split, no logic changes)
use gtk4::prelude::*;
use gtk4::TextBuffer;



// ─── helpers ─────────────────────────────────────────────────────────────────

/// URL detection: common schemes plus bare `www.` hosts.
pub(crate) fn is_url(text: &str) -> bool {
    const SCHEMES: [&str; 7] = [
        "http://", "https://", "file://", "ftp://", "git://", "ssh://", "mailto:",
    ];
    SCHEMES.iter().any(|s| text.starts_with(s))
}

/// Trailing characters that are almost always sentence punctuation rather than
/// part of a URL (e.g. `see http://x.com.` → drop the period).
fn trim_trailing(text: &str) -> &str {
    text.trim_end_matches(|c| matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '>' | '\'' | '"'))
}

/// Extract URL at cursor position in a TextView's buffer, returning bounds and text
pub(crate) fn get_url_bounds_at_position(
    buffer: &TextBuffer,
    iter: &gtk4::TextIter,
) -> Option<(gtk4::TextIter, gtk4::TextIter, String)> {
    let mut start = *iter;
    let mut end = *iter;

    // Expand backwards to find URL start
    while !start.starts_line() {
        let ch = start.char();
        if ch == ' ' || ch == '\n' || ch == '\t' || ch == '<' || ch == '>' {
            start.forward_char();
            break;
        }
        if !start.backward_char() {
            break;
        }
    }

    // Expand forwards to find URL end
    while !end.ends_line() {
        let ch = end.char();
        if ch == ' ' || ch == '\n' || ch == '\t' || ch == '<' || ch == '>' {
            break;
        }
        if !end.forward_char() {
            break;
        }
    }

    let raw = buffer.text(&start, &end, false).to_string();
    let trimmed = trim_trailing(&raw);
    if !is_url(trimmed) {
        return None;
    }
    // Pull `end` back over any trailing punctuation we stripped, so the hover
    // highlight and click target match the actual URL.
    let trimmed_chars = trimmed.chars().count();
    let raw_chars = raw.chars().count();
    for _ in 0..(raw_chars - trimmed_chars) {
        end.backward_char();
    }
    Some((start, end, trimmed.to_string()))
}

pub(crate) fn get_url_at_position(buffer: &TextBuffer, iter: &gtk4::TextIter) -> Option<String> {
    for tag in iter.tags() {
        if let Some(name) = tag.name() {
            if let Some(uri) = name.strip_prefix("osc8-link:") {
                return Some(uri.to_string());
            }
        }
    }
    get_url_bounds_at_position(buffer, iter).map(|(_, _, url)| url)
}
