use std::collections::HashSet;

use serde::Deserialize;
use serde_json::json;

use crate::ai::{AiClient, Role, Turn};

use super::util::{bounded, safe_candidate, truncate};
use super::{Candidate, Context, Evidence, Failure};

const MAX_AI_CONTEXT_BYTES: usize = 8 * 1024;
const MAX_CANDIDATES: usize = 3;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum AiReply {
    Suggest {
        message: String,
        candidates: Vec<AiCandidate>,
    },
    #[serde(rename = "none")]
    NoSuggestion { message: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AiCandidate {
    command: String,
    reason: String,
}

pub(super) fn ai_suggestions(
    context: &Context,
    failure: &Failure,
    client: &AiClient,
) -> Vec<Candidate> {
    let system = "Correct failed shell commands. Reply with exactly one JSON object and no markdown. \
Use {\"action\":\"suggest\",\"message\":\"short explanation\",\"candidates\":[{\"command\":\"one line\",\"reason\":\"short reason\"}]} \
or {\"action\":\"none\",\"message\":\"short reason\"}. Return at most three commands. Preserve the \
original structure and change the smallest possible part. Never add sudo, doas, su, a new remote \
host, redirection, command substitution, or a pipe unless it already existed. Never claim the \
command ran. Unverified package and executable names must be described as unverified.";

    let user = json!({
        "failure_kind": failure.label(),
        "command": context.command.as_str(),
        "cwd": context.cwd.as_str(),
        "exit_code": context.exit_code,
        "remote_target": context.remote,
        "output": bounded(&context.output, MAX_AI_CONTEXT_BYTES),
    })
    .to_string();

    let Ok(raw) = client.send_turns_blocking(
        Some(system),
        &[Turn {
            role: Role::User,
            text: user,
        }],
    ) else {
        return Vec::new();
    };
    let Some(reply) = parse_ai_reply(&raw) else {
        return Vec::new();
    };
    let (message, candidates) = match reply {
        AiReply::Suggest {
            message,
            candidates,
        } => (message, candidates),
        AiReply::NoSuggestion { message } => {
            log::debug!("AI command correction declined: {message}");
            return Vec::new();
        }
    };

    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter_map(|candidate| {
            let command = candidate.command.trim().to_string();
            if !safe_candidate(&context.command, &command) || !seen.insert(command.clone()) {
                return None;
            }
            let reason = if candidate.reason.trim().is_empty() {
                message.clone()
            } else {
                candidate.reason
            };
            Some(Candidate {
                command,
                reason: truncate(reason.trim(), 320),
                evidence: Evidence::AiUnverified,
            })
        })
        .take(MAX_CANDIDATES)
        .collect()
}

fn parse_ai_reply(raw: &str) -> Option<AiReply> {
    let mut payload = raw.trim();
    if payload.starts_with("```") {
        let newline = payload.find('\n')?;
        let language = payload[3..newline].trim();
        if !language.is_empty() && !language.eq_ignore_ascii_case("json") {
            return None;
        }
        payload = payload[newline + 1..].strip_suffix("```")?.trim();
    }
    serde_json::from_str(payload).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_protocol_is_strict() {
        assert!(parse_ai_reply(
            r#"{"action":"suggest","message":"typo","candidates":[{"command":"git status","reason":"close match"}]}"#
        )
        .is_some());
        assert!(parse_ai_reply(
            r#"result: {"action":"suggest","message":"typo","candidates":[]}"#
        )
        .is_none());
        assert!(parse_ai_reply(
            r#"{"action":"suggest","message":"typo","candidates":[],"extra":true}"#
        )
        .is_none());
    }
}
