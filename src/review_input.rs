//! Safety boundary for text inserted into a live shell editor for review.
//!
//! Review-first surfaces promise not to submit a command. A carriage return,
//! line feed, NUL, escape, or other control character would break that promise
//! when written to a PTY, so every such surface shares this validator.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReviewInputError {
    Empty,
    ControlCharacter,
}

impl fmt::Display for ReviewInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("the command is empty"),
            Self::ControlCharacter => formatter
                .write_str("the command contains a line break, NUL, or terminal control character"),
        }
    }
}

/// Validate text before inserting it into an interactive prompt without Enter.
pub(crate) fn validate(text: &str) -> Result<&str, ReviewInputError> {
    if text.trim().is_empty() {
        return Err(ReviewInputError::Empty);
    }
    if text.chars().any(char::is_control) {
        return Err(ReviewInputError::ControlCharacter);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_single_line_shell_text_and_unicode() {
        assert_eq!(
            validate("printf '%s' '你好 🙂'").unwrap(),
            "printf '%s' '你好 🙂'"
        );
    }

    #[test]
    fn rejects_empty_and_every_pty_control_vector() {
        assert_eq!(validate("  "), Err(ReviewInputError::Empty));
        for unsafe_text in [
            "echo one\necho two",
            "echo one\recho two",
            "printf '\0'",
            "echo\tsecret",
            "echo \u{1b}[31mred",
        ] {
            assert_eq!(
                validate(unsafe_text),
                Err(ReviewInputError::ControlCharacter),
                "{unsafe_text:?} must never be written through a review-only path"
            );
        }
    }
}
