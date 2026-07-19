use gtk4::prelude::*;
use gtk4::TextBuffer;

pub(super) const MAX_AI_VISIBLE_TRANSCRIPT_BYTES: usize = 1024 * 1024;
const OMITTED_MARKER: &str = "[Earlier visible activity was omitted by the live display budget]";

/// Keep a GTK transcript bounded without rebuilding its surviving tagged
/// suffix. Prefer an event boundary, but always fall back to a UTF-8 boundary.
pub(super) fn trim_ai_transcript(buffer: &TextBuffer) -> bool {
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    let text = buffer.text(&start, &end, false);
    let Some(trim_byte) = transcript_trim_byte(&text, MAX_AI_VISIBLE_TRANSCRIPT_BYTES) else {
        return false;
    };
    let trim_chars = text[..trim_byte].chars().count() as i32;
    let mut start = buffer.start_iter();
    let mut trim_end = buffer.iter_at_offset(trim_chars);
    buffer.delete(&mut start, &mut trim_end);
    let mut start = buffer.start_iter();
    buffer.insert(&mut start, OMITTED_MARKER);
    buffer.insert(&mut start, "\n\n");
    true
}

fn transcript_trim_byte(text: &str, max_bytes: usize) -> Option<usize> {
    if text.len() <= max_bytes {
        return None;
    }
    let prefix_bytes = OMITTED_MARKER.len() + 2;
    if max_bytes <= prefix_bytes {
        return Some(text.len());
    }
    let retain_bytes = max_bytes - prefix_bytes;
    let mut trim_byte = text.len().saturating_sub(retain_bytes);
    while trim_byte < text.len() && !text.is_char_boundary(trim_byte) {
        trim_byte += 1;
    }
    if let Some(boundary) = text[trim_byte..].find("\n\n") {
        trim_byte += boundary + 2;
    }
    Some(trim_byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_offset_keeps_a_bounded_utf8_event_suffix() {
        let text = format!(
            "Old\n{}\n\nRecent\n{}",
            "旧🙂".repeat(100),
            "新🙂".repeat(20)
        );
        let limit = 256;
        let trim_byte = transcript_trim_byte(&text, limit).unwrap();
        assert!(text.is_char_boundary(trim_byte));
        let bounded = format!("{OMITTED_MARKER}\n\n{}", &text[trim_byte..]);
        assert!(bounded.len() <= limit);
        assert!(bounded.contains("Recent"));
    }

    #[test]
    fn trim_offset_is_none_at_and_below_the_limit() {
        assert_eq!(transcript_trim_byte("1234", 4), None);
        assert_eq!(transcript_trim_byte("123", 4), None);
    }
}
