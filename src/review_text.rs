//! Forge-specific review-text helpers layered on the shared
//! `jterm_core::review_input` boundary.
//!
//! The visual-spoofing character class itself lives in core; this module keeps
//! only forge's per-surface byte budgets, the parameterized strict validator,
//! and the prompt/history sanitizers whose multiline semantics are
//! app-specific.

use std::fmt;

use jterm_core::review_input::{contains_visual_spoofing, is_visual_spoofing_character};

/// Pinned jagent's existing command budget; the compatibility layer may only
/// tighten validation, never enlarge its accepted protocol surface.
pub(crate) const MAX_AGENT_COMMAND_BYTES: usize = 16 * 1024;
/// Existing OSC 133 / execution-journal command budget.
#[allow(dead_code)] // consumed by the binary-only command timeline module
pub(crate) const MAX_HISTORY_COMMAND_BYTES: usize = 64 * 1024;
#[allow(dead_code)] // consumed by binary-only prompt insertion paths
pub(crate) const MAX_PROMPT_INSERT_BYTES: usize = 256 * 1024;

#[allow(dead_code)] // consumed by binary-only prompt insertion paths
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VisualSpoofDisposition {
    Reject,
    PreserveForConfirmation,
}

#[allow(dead_code)] // consumed by binary-only prompt insertion paths
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SanitizedPromptPayload {
    pub(crate) text: String,
    pub(crate) had_visual_spoofing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReviewTextError {
    Empty,
    TooLarge { limit: usize },
    ControlCharacter,
    VisualSpoof,
}

impl fmt::Display for ReviewTextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("the command is empty"),
            Self::TooLarge { limit } => {
                write!(formatter, "the command exceeds the {limit}-byte limit")
            }
            Self::ControlCharacter => {
                formatter.write_str("the command contains a terminal control character")
            }
            Self::VisualSpoof => formatter
                .write_str("the command contains invisible or bidirectional formatting characters"),
        }
    }
}

/// Strict single-line Agent/review validator. Tabs and every non-ASCII-space
/// whitespace character are intentionally rejected along with C0/C1.
pub(crate) fn validate_single_line(text: &str, max_bytes: usize) -> Result<&str, ReviewTextError> {
    if text.len() > max_bytes {
        return Err(ReviewTextError::TooLarge { limit: max_bytes });
    }
    if text.trim().is_empty() {
        return Err(ReviewTextError::Empty);
    }
    if text.chars().any(char::is_control) {
        return Err(ReviewTextError::ControlCharacter);
    }
    if contains_visual_spoofing(text) {
        return Err(ReviewTextError::VisualSpoof);
    }
    Ok(text)
}

#[allow(dead_code)] // consumed by the binary-only command timeline module
fn is_c0_or_c1(character: char) -> bool {
    matches!(character as u32, 0x00..=0x1f | 0x7f..=0x9f)
}

/// Prepare text that will enter the shell's editable prompt.
///
/// LF/tab retain forge's structural paste semantics and CR/CRLF normalize to
/// LF. Other C0/C1 characters are stripped. Search/sidebar insertions reject
/// non-control visual spoofing; a real clipboard may retain it only when the
/// caller routes the result through a mandatory, explicit confirmation.
#[allow(dead_code)] // consumed by binary-only prompt insertion paths
pub(crate) fn sanitize_prompt_payload(
    text: &str,
    max_bytes: usize,
    visual_spoofing: VisualSpoofDisposition,
) -> Result<SanitizedPromptPayload, ReviewTextError> {
    if text.len() > max_bytes {
        return Err(ReviewTextError::TooLarge { limit: max_bytes });
    }
    let mut sanitized = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    let mut had_visual_spoofing = false;
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                sanitized.push('\n');
            }
            '\n' | '\t' => sanitized.push(character),
            control if is_c0_or_c1(control) => {}
            visual if is_visual_spoofing_character(visual) => {
                had_visual_spoofing = true;
                if visual_spoofing == VisualSpoofDisposition::Reject {
                    return Err(ReviewTextError::VisualSpoof);
                }
                sanitized.push(visual);
            }
            visible => sanitized.push(visible),
        }
    }
    Ok(SanitizedPromptPayload {
        text: sanitized,
        had_visual_spoofing,
    })
}

/// Prepare an OSC-133/journal command for Fill/Run.
///
/// History Fill deliberately supports multiline commands and literal tabs, so
/// those two ASCII controls retain their product semantics. CR/CRLF normalize
/// to LF; every other C0/C1 byte is removed. All other non-space whitespace
/// and default-ignorable formatting characters fail closed.
#[allow(dead_code)] // consumed by the binary-only command timeline module
pub(crate) fn sanitize_history_replay(
    text: &str,
    max_bytes: usize,
) -> Result<String, ReviewTextError> {
    // One implementation, in the crate that owns the replay channel. The
    // wrapper survives only to keep this module's error vocabulary.
    jterm_core::agent_task::replay_text::sanitize_history_replay(text, max_bytes).map_err(|error| {
        match error {
            jterm_core::agent_task::replay_text::ReplayTextError::Empty => ReviewTextError::Empty,
            jterm_core::agent_task::replay_text::ReplayTextError::TooLarge { limit } => {
                ReviewTextError::TooLarge { limit }
            }
            jterm_core::agent_task::replay_text::ReplayTextError::ControlCharacter => {
                ReviewTextError::ControlCharacter
            }
            jterm_core::agent_task::replay_text::ReplayTextError::VisualSpoof => {
                ReviewTextError::VisualSpoof
            }
        }
    })
}

/// Make dangerous-to-display code points explicit without retaining their
/// formatting effect. Output is byte-bounded even when each scalar expands to
/// an ASCII `\\u{...}` spelling.
#[allow(dead_code)] // consumed by binary-only UI modules
pub(crate) fn visible_bounded(text: &str, max_bytes: usize) -> String {
    let mut visible = String::with_capacity(text.len().min(max_bytes));
    let mut truncated = false;
    for character in text.chars() {
        let replacement = match character {
            '\n' => "\\n".to_string(),
            '\r' => "\\r".to_string(),
            '\t' => "\\t".to_string(),
            unsafe_character
                if unsafe_character.is_control()
                    || is_visual_spoofing_character(unsafe_character) =>
            {
                format!("\\u{{{:X}}}", unsafe_character as u32)
            }
            safe => safe.to_string(),
        };
        if replacement.len() > max_bytes.saturating_sub(visible.len()) {
            truncated = true;
            break;
        }
        visible.push_str(&replacement);
    }
    if truncated && max_bytes >= 3 {
        while !visible.is_empty() && "…".len() > max_bytes.saturating_sub(visible.len()) {
            visible.pop();
            while !visible.is_char_boundary(visible.len()) {
                visible.pop();
            }
        }
        visible.push('…');
    }
    visible
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_validator_delegates_spoofing_to_core_and_keeps_forge_budgets() {
        // The full character class is covered by jterm_core::review_input;
        // here only the wiring and forge's parameterized budget are checked.
        for spoofed in ["echo safe\u{202e}txt", "echo\u{00a0}not-a-space"] {
            assert_eq!(
                validate_single_line(spoofed, 64 * 1024),
                Err(ReviewTextError::VisualSpoof),
                "{spoofed:?}"
            );
        }
        assert!(validate_single_line("printf '编译🙂'", 64 * 1024).is_ok());
        assert_eq!(
            validate_single_line("echo\tsecret", 64 * 1024),
            Err(ReviewTextError::ControlCharacter)
        );
        assert_eq!(
            validate_single_line(&"x".repeat(64 * 1024 + 1), 64 * 1024),
            Err(ReviewTextError::TooLarge { limit: 64 * 1024 })
        );
        assert_eq!(
            validate_single_line("  ", 64 * 1024),
            Err(ReviewTextError::Empty)
        );
    }

    #[test]
    fn history_replay_keeps_only_product_controls_and_rejects_spoofing() {
        assert_eq!(
            sanitize_history_replay("one\r\ntwo\tthree\u{7}", 64 * 1024).unwrap(),
            "one\ntwo\tthree"
        );
        assert_eq!(
            sanitize_history_replay("echo safe\u{202e}txt", 64 * 1024),
            Err(ReviewTextError::VisualSpoof)
        );
    }

    #[test]
    fn prompt_payload_rejects_spoofing_unless_confirmation_will_make_it_visible() {
        let safe = sanitize_prompt_payload(
            "one\r\ntwo\tthree\u{1b}[31m雪🙂",
            4096,
            VisualSpoofDisposition::Reject,
        )
        .unwrap();
        assert_eq!(safe.text, "one\ntwo\tthree[31m雪🙂");
        assert!(!safe.had_visual_spoofing);

        assert_eq!(
            sanitize_prompt_payload(
                "echo safe\u{202e}hidden",
                4096,
                VisualSpoofDisposition::Reject,
            ),
            Err(ReviewTextError::VisualSpoof)
        );
        let pending = sanitize_prompt_payload(
            "echo safe\u{202e}hidden",
            4096,
            VisualSpoofDisposition::PreserveForConfirmation,
        )
        .unwrap();
        assert!(pending.had_visual_spoofing);
        assert_eq!(pending.text, "echo safe\u{202e}hidden");
    }

    #[test]
    fn visible_text_escapes_formatting_and_stays_bounded() {
        let shown = visible_bounded("safe\u{202e}\ttext", 64);
        assert_eq!(shown, "safe\\u{202E}\\ttext");
        assert!(visible_bounded(&"\u{202e}".repeat(100), 32).len() <= 32);
    }
}
