//! redact — high-confidence secret scrubbing for AI-bound text.
//!
//! Scoped narrowly: this runs over BlockContext payloads (cmd + output) and
//! chat-turn text just before they're serialized into an Anthropic request.
//! Goal is to stop "I pasted my .env" / "I ran `aws sts get-session-token`"
//! accidents — not to be a general DLP. Patterns are conservative: we only
//! match shapes whose false-positive rate is essentially zero (AWS access
//! key ids, GitHub PATs, Slack tokens, JWTs, PEM block headers). Generic
//! "looks like a hex string" detection would gut routine command output
//! (git SHAs, hashes) so we deliberately avoid it.
//!
//! Replacement format: `[REDACTED:<kind>]` — short enough to keep the
//! surrounding token context legible for the model, distinctive enough to
//! survive copy/paste through the AI panel back to the user.

use regex::Regex;
use std::sync::OnceLock;

/// Each pattern is (kind tag, compiled regex). Order is unimportant — the
/// patterns are pairwise disjoint in practice — but PEM block headers come
/// first so the multi-line `-----BEGIN ... PRIVATE KEY-----` match wins
/// over any accidental sub-match of the inner base64 body.
fn patterns() -> &'static [(&'static str, Regex)] {
    static CELL: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    CELL.get_or_init(|| {
        let pats: &[(&str, &str)] = &[
            // PEM private key block (any flavor): RSA, EC, OPENSSH, plain
            // PRIVATE KEY. Span includes body so the whole secret is gone.
            ("private-key", r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----"),
            // AWS access key ids (long-lived + STS). Format is fixed.
            ("aws-access-key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
            // GitHub fine-grained PAT (long form).
            ("github-pat", r"\bgithub_pat_[A-Za-z0-9_]{82}\b"),
            // GitHub classic tokens: ghp_, gho_, ghu_, ghs_, ghr_.
            ("github-token", r"\bgh[opusr]_[A-Za-z0-9]{36,}\b"),
            // Slack bot / user / app / refresh tokens.
            ("slack-token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}\b"),
            // JWT (header.payload.signature). Loose but the three-segment
            // base64url structure with `eyJ` header prefix is distinctive.
            ("jwt", r"\beyJ[A-Za-z0-9_=-]{8,}\.eyJ[A-Za-z0-9_=-]{8,}\.[A-Za-z0-9_=.+/-]{8,}\b"),
            // Anthropic API keys — protect the user's own key if it shows
            // up in `env | grep` output etc. Format: sk-ant-<base64ish>.
            ("anthropic-key", r"\bsk-ant-[A-Za-z0-9_\-]{20,}\b"),
            // OpenAI keys (sk-, sk-proj-). The 20+ tail catches both.
            ("openai-key", r"\bsk-(?:proj-)?[A-Za-z0-9_\-]{20,}\b"),
        ];
        pats.iter()
            .map(|(k, p)| (*k, Regex::new(p).expect("redact pattern compiles")))
            .collect()
    })
}

/// Walk the input through every pattern, replacing matches with
/// `[REDACTED:<kind>]`. Allocates only when something matches — short
/// circuits on a clean string so the common case (most block output) is
/// just a couple of regex `is_match` probes.
pub fn redact_secrets(input: &str) -> String {
    let mut current = std::borrow::Cow::Borrowed(input);
    for (kind, re) in patterns() {
        if re.is_match(&current) {
            let replacement = format!("[REDACTED:{kind}]");
            current = std::borrow::Cow::Owned(re.replace_all(&current, replacement.as_str()).into_owned());
        }
    }
    current.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_when_nothing_to_redact() {
        let s = "the quick brown fox 1234567890 deadbeefcafef00d";
        assert_eq!(redact_secrets(s), s);
    }

    #[test]
    fn redacts_aws_access_key() {
        let s = "export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE";
        let out = redact_secrets(s);
        assert!(out.contains("[REDACTED:aws-access-key]"), "got {out}");
        assert!(!out.contains("AKIA"));
    }

    #[test]
    fn redacts_aws_sts_access_key() {
        let s = "ASIAY34FZKBOKMUTVV7A is current STS";
        let out = redact_secrets(s);
        assert!(out.contains("[REDACTED:aws-access-key]"), "got {out}");
    }

    #[test]
    fn redacts_github_classic_token() {
        // Classic GitHub PATs are exactly 36 chars after the prefix.
        let s = "git remote set-url origin https://x:ghp_1234567890abcdefghijABCDEFGHIJ123456@github.com/";
        let out = redact_secrets(s);
        assert!(out.contains("[REDACTED:github-token]"), "got {out}");
        assert!(!out.contains("ghp_"));
    }

    #[test]
    fn redacts_github_fine_grained_pat() {
        let body = "X".repeat(82);
        let s = format!("token: github_pat_{body}");
        let out = redact_secrets(&s);
        assert!(out.contains("[REDACTED:github-pat]"), "got {out}");
    }

    #[test]
    fn redacts_slack_token() {
        let s = "SLACK_TOKEN=xoxb-12345-67890-abcdefghijklmnop";
        let out = redact_secrets(s);
        assert!(out.contains("[REDACTED:slack-token]"), "got {out}");
    }

    #[test]
    fn redacts_jwt() {
        // Realistic-shape JWT (header.payload.signature, base64url).
        let s = "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4ifQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let out = redact_secrets(s);
        assert!(out.contains("[REDACTED:jwt]"), "got {out}");
        assert!(!out.contains("eyJzdWIi"));
    }

    #[test]
    fn redacts_pem_private_key_block_inclusive() {
        let s = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\nLOTSOFGARBAGE==\n-----END OPENSSH PRIVATE KEY-----\nafter";
        let out = redact_secrets(s);
        assert!(out.contains("[REDACTED:private-key]"));
        assert!(!out.contains("b3BlbnNzaC1rZXktdjE"));
        assert!(out.contains("before"));
        assert!(out.contains("after"));
    }

    #[test]
    fn redacts_anthropic_key() {
        let s = "ANTHROPIC_API_KEY=sk-ant-api03-AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHH";
        let out = redact_secrets(s);
        assert!(out.contains("[REDACTED:anthropic-key]"), "got {out}");
    }

    #[test]
    fn does_not_redact_short_git_sha_or_plain_uuid() {
        // Common content we MUST leave alone.
        let s = "commit deadbeefcafef00d1234567890abcdef01234567 (HEAD -> main)\nuuid: 550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(redact_secrets(s), s);
    }

    #[test]
    fn multiple_secrets_in_same_input_all_redacted() {
        let s = "AKIAIOSFODNN7EXAMPLE then ghp_1234567890abcdefghijABCDEFGHIJ123456 done";
        let out = redact_secrets(s);
        assert!(out.contains("[REDACTED:aws-access-key]"));
        assert!(out.contains("[REDACTED:github-token]"));
        assert!(!out.contains("AKIA"));
        assert!(!out.contains("ghp_"));
    }
}
