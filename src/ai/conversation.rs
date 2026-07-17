//! Versioned, bounded persistence model for the AI chat panel.
//!
//! The live panel may temporarily contain a trailing user turn while a request
//! is in flight. Persistence deliberately captures only complete successful
//! `user -> assistant` pairs, so a crash can never restore a request as though
//! it had received a response. The encoded and decoded forms are independently
//! bounded before they are allowed into the per-window state snapshot.

use super::{BlockContext, Role, Turn};
use serde::{Deserialize, Serialize};
use std::fmt;

const CONVERSATION_SNAPSHOT_VERSION: u32 = 1;
const MAX_PERSISTED_TURNS: usize = 100;
const MAX_TURN_BYTES: usize = 256 * 1024;
const MAX_TURN_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_BLOCK_CONTEXT_BYTES: usize = 512 * 1024;

/// Hard upper bound for the compact JSON value embedded in window state.
/// Escaping it for the line-oriented state format can at most double this.
pub(crate) const MAX_CONVERSATION_SNAPSHOT_JSON_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct ConversationSnapshot {
    version: u32,
    turns: Vec<Turn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    block_context: Option<BlockContext>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConversationSnapshotError {
    EncodedTooLarge,
    InvalidJson(String),
    UnsupportedVersion(u32),
    InvalidTurnSequence,
    TurnTooLarge,
    ConversationTooLarge,
    BlockContextTooLarge,
}

impl fmt::Display for ConversationSnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EncodedTooLarge => write!(f, "encoded conversation snapshot is too large"),
            Self::InvalidJson(error) => write!(f, "invalid conversation snapshot JSON: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported conversation snapshot version {version}")
            }
            Self::InvalidTurnSequence => {
                write!(f, "conversation must contain complete user/assistant pairs")
            }
            Self::TurnTooLarge => write!(f, "a conversation turn exceeds the size limit"),
            Self::ConversationTooLarge => write!(f, "conversation exceeds the retention limit"),
            Self::BlockContextTooLarge => write!(f, "block context exceeds the size limit"),
        }
    }
}

impl std::error::Error for ConversationSnapshotError {}

impl ConversationSnapshot {
    /// Build a persistable snapshot from live provider history.
    ///
    /// A trailing in-flight user turn and any malformed/oversized pair are not
    /// persisted. When retention limits are reached, the newest complete pairs
    /// win while chronological order is preserved.
    pub(crate) fn from_completed_history(
        history: &[Turn],
        block_context: Option<&BlockContext>,
    ) -> Option<Self> {
        let mut complete_pairs: Vec<(&Turn, &Turn)> = Vec::new();
        for pair in history.chunks(2) {
            if pair.len() != 2 {
                break;
            }
            let (user, assistant) = (&pair[0], &pair[1]);
            if user.role != Role::User || assistant.role != Role::Assistant {
                break;
            }
            if !valid_turn_text(&user.text) || !valid_turn_text(&assistant.text) {
                continue;
            }
            complete_pairs.push((user, assistant));
        }

        let mut retained_reversed: Vec<(Turn, Turn)> = Vec::new();
        let mut retained_bytes = 0usize;
        for (user, assistant) in complete_pairs.into_iter().rev() {
            if retained_reversed.len() * 2 == MAX_PERSISTED_TURNS {
                break;
            }
            let pair_bytes = user.text.len() + assistant.text.len();
            if retained_bytes + pair_bytes > MAX_TURN_TEXT_BYTES {
                break;
            }
            retained_bytes += pair_bytes;
            retained_reversed.push((user.clone(), assistant.clone()));
        }

        if retained_reversed.is_empty() {
            return None;
        }
        retained_reversed.reverse();
        let mut turns = Vec::with_capacity(retained_reversed.len() * 2);
        for (user, assistant) in retained_reversed {
            turns.push(user);
            turns.push(assistant);
        }

        let block_context = block_context
            .filter(|context| valid_context(context))
            .cloned();
        let mut snapshot = Self {
            version: CONVERSATION_SNAPSHOT_VERSION,
            turns,
            block_context,
        };

        // Raw text can nearly double after JSON escaping (for example, a
        // transcript dominated by backslashes). Enforce the actual encoded
        // size here so every constructed snapshot is serializable and can
        // never block the surrounding window snapshot.
        loop {
            let encoded_len = serde_json::to_vec(&snapshot).ok()?.len();
            if encoded_len <= MAX_CONVERSATION_SNAPSHOT_JSON_BYTES {
                return Some(snapshot);
            }
            if snapshot.turns.len() > 2 {
                snapshot.turns.drain(..2);
            } else if snapshot.block_context.take().is_none() {
                return None;
            }
        }
    }

    pub(crate) fn turns(&self) -> &[Turn] {
        &self.turns
    }

    pub(crate) fn block_context(&self) -> Option<&BlockContext> {
        self.block_context.as_ref()
    }

    pub(crate) fn into_parts(self) -> (Vec<Turn>, Option<BlockContext>) {
        (self.turns, self.block_context)
    }

    pub(crate) fn to_json(&self) -> Result<String, ConversationSnapshotError> {
        self.validate()?;
        let encoded = serde_json::to_string(self)
            .map_err(|error| ConversationSnapshotError::InvalidJson(error.to_string()))?;
        if encoded.len() > MAX_CONVERSATION_SNAPSHOT_JSON_BYTES {
            return Err(ConversationSnapshotError::EncodedTooLarge);
        }
        Ok(encoded)
    }

    pub(crate) fn from_json(encoded: &str) -> Result<Self, ConversationSnapshotError> {
        if encoded.len() > MAX_CONVERSATION_SNAPSHOT_JSON_BYTES {
            return Err(ConversationSnapshotError::EncodedTooLarge);
        }
        let snapshot: Self = serde_json::from_str(encoded)
            .map_err(|error| ConversationSnapshotError::InvalidJson(error.to_string()))?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    fn validate(&self) -> Result<(), ConversationSnapshotError> {
        if self.version != CONVERSATION_SNAPSHOT_VERSION {
            return Err(ConversationSnapshotError::UnsupportedVersion(self.version));
        }
        if self.turns.is_empty()
            || self.turns.len() > MAX_PERSISTED_TURNS
            || !self.turns.len().is_multiple_of(2)
        {
            return Err(ConversationSnapshotError::InvalidTurnSequence);
        }

        let mut total_bytes = 0usize;
        for (index, turn) in self.turns.iter().enumerate() {
            let expected = if index.is_multiple_of(2) {
                Role::User
            } else {
                Role::Assistant
            };
            if turn.role != expected || turn.text.trim().is_empty() {
                return Err(ConversationSnapshotError::InvalidTurnSequence);
            }
            if turn.text.len() > MAX_TURN_BYTES {
                return Err(ConversationSnapshotError::TurnTooLarge);
            }
            total_bytes = total_bytes.saturating_add(turn.text.len());
            if total_bytes > MAX_TURN_TEXT_BYTES {
                return Err(ConversationSnapshotError::ConversationTooLarge);
            }
        }
        if self
            .block_context
            .as_ref()
            .is_some_and(|context| !valid_context(context))
        {
            return Err(ConversationSnapshotError::BlockContextTooLarge);
        }
        Ok(())
    }
}

fn valid_turn_text(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= MAX_TURN_BYTES
}

fn valid_context(context: &BlockContext) -> bool {
    context
        .cmd
        .len()
        .saturating_add(context.output.len())
        .saturating_add(context.cwd.as_ref().map_or(0, String::len))
        <= MAX_BLOCK_CONTEXT_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: Role, text: impl Into<String>) -> Turn {
        Turn {
            role,
            text: text.into(),
        }
    }

    fn pair(index: usize) -> [Turn; 2] {
        [
            turn(Role::User, format!("question {index}")),
            turn(Role::Assistant, format!("answer {index}")),
        ]
    }

    #[test]
    fn persists_only_complete_successful_pairs() {
        let mut history = pair(1).to_vec();
        history.push(turn(Role::User, "still in flight"));
        let context = BlockContext {
            cmd: "cargo test".into(),
            output: "ok".into(),
            cwd: Some("/tmp/project".into()),
            exit_code: 0,
        };

        let snapshot =
            ConversationSnapshot::from_completed_history(&history, Some(&context)).unwrap();
        assert_eq!(snapshot.turns(), &history[..2]);
        assert_eq!(snapshot.block_context(), Some(&context));
    }

    #[test]
    fn json_round_trip_preserves_unicode_newlines_and_context() {
        let history = vec![
            turn(Role::User, "为什么失败？\n路径是 C:\\tmp"),
            turn(Role::Assistant, "因为权限不足。\n请重试。"),
        ];
        let context = BlockContext {
            cmd: "printf '你好\\n'".into(),
            output: "你好\n".into(),
            cwd: Some("/tmp/项目".into()),
            exit_code: 1,
        };
        let snapshot =
            ConversationSnapshot::from_completed_history(&history, Some(&context)).unwrap();
        let decoded = ConversationSnapshot::from_json(&snapshot.to_json().unwrap()).unwrap();
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn decoder_rejects_unknown_versions_and_incomplete_sequences() {
        let unknown =
            r#"{"version":2,"turns":[{"role":"user","text":"q"},{"role":"assistant","text":"a"}]}"#;
        assert!(matches!(
            ConversationSnapshot::from_json(unknown),
            Err(ConversationSnapshotError::UnsupportedVersion(2))
        ));

        let incomplete = r#"{"version":1,"turns":[{"role":"user","text":"q"}]}"#;
        assert!(matches!(
            ConversationSnapshot::from_json(incomplete),
            Err(ConversationSnapshotError::InvalidTurnSequence)
        ));
    }

    #[test]
    fn retention_keeps_the_newest_complete_pairs() {
        let mut history = Vec::new();
        for index in 0..(MAX_PERSISTED_TURNS / 2 + 3) {
            history.extend(pair(index));
        }
        let snapshot = ConversationSnapshot::from_completed_history(&history, None).unwrap();
        assert_eq!(snapshot.turns().len(), MAX_PERSISTED_TURNS);
        assert_eq!(snapshot.turns()[0].text, "question 3");
        assert_eq!(snapshot.turns().last().unwrap().text, "answer 52");
    }

    #[test]
    fn oversized_turn_and_context_never_enter_a_snapshot() {
        let oversized = "x".repeat(MAX_TURN_BYTES + 1);
        let history = vec![
            turn(Role::User, oversized.clone()),
            turn(Role::Assistant, "answer"),
        ];
        assert!(ConversationSnapshot::from_completed_history(&history, None).is_none());

        let valid_history = pair(1);
        let context = BlockContext {
            cmd: "cmd".into(),
            output: "x".repeat(MAX_BLOCK_CONTEXT_BYTES),
            cwd: None,
            exit_code: 0,
        };
        let snapshot =
            ConversationSnapshot::from_completed_history(&valid_history, Some(&context)).unwrap();
        assert!(snapshot.block_context().is_none());

        let encoded = format!(
            r#"{{"version":1,"turns":[{{"role":"user","text":"{}"}},{{"role":"assistant","text":"answer"}}]}}"#,
            oversized
        );
        assert!(matches!(
            ConversationSnapshot::from_json(&encoded),
            Err(ConversationSnapshotError::TurnTooLarge)
        ));
    }

    #[test]
    fn json_escape_expansion_drops_old_pairs_until_snapshot_is_encodable() {
        let escaped = "\\".repeat(MAX_TURN_BYTES);
        let mut history = Vec::new();
        for index in 0..(MAX_TURN_TEXT_BYTES / MAX_TURN_BYTES) {
            history.push(turn(
                if index.is_multiple_of(2) {
                    Role::User
                } else {
                    Role::Assistant
                },
                escaped.clone(),
            ));
        }

        let snapshot = ConversationSnapshot::from_completed_history(&history, None).unwrap();
        assert!(snapshot.turns().len() < history.len());
        assert!(snapshot.to_json().is_ok());
    }
}
