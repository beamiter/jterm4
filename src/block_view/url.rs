//! url — extracted from block_view (mechanical split, no logic changes)
use gtk4::prelude::*;
use gtk4::TextBuffer;



// ─── helpers ─────────────────────────────────────────────────────────────────

/// Simple URL detection regex (http/https/file URLs)
pub(crate) fn is_url(text: &str) -> bool {
    text.starts_with("http://") || text.starts_with("https://") || text.starts_with("file://")
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

    let text = buffer.text(&start, &end, false).to_string();
    if is_url(&text) {
        Some((start, end, text))
    } else {
        None
    }
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
