//! ai — Anthropic Messages API client.
//!
//! Shells out to `curl` rather than linking a Rust HTTP client. Rationale:
//! - The dev box's TLS path can't fetch new crates from index.crates.io, so
//!   adding `ureq` / `reqwest` is currently impossible.
//! - `curl` is a Nix-provided system dep already on every machine that runs
//!   jterm4, and the request shape is simple enough that the subprocess
//!   round-trip cost (~10ms) is negligible against API latency (~hundreds of
//!   ms to seconds).
//!
//! The caller spawns this on a dedicated thread and posts the result back
//! to the GTK main loop via `glib::MainContext::channel`, so the UI never
//! blocks on a network call.

use serde::{Deserialize, Serialize};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// One turn in the conversation transcript posted to the API.
#[derive(Debug, Clone)]
pub(crate) struct Turn {
    pub(crate) role: Role,
    pub(crate) text: String,
}

#[derive(Serialize)]
struct ReqMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<ReqMessage<'a>>,
}

#[derive(Deserialize)]
struct RespContent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    content: Vec<RespContent>,
}

#[derive(Deserialize)]
struct ApiError {
    error: ApiErrorBody,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}

#[derive(Debug)]
pub(crate) enum AiError {
    /// `ANTHROPIC_API_KEY` env var not set or empty.
    MissingApiKey,
    /// Network / TLS / DNS / connection failure.
    Transport(String),
    /// HTTP non-2xx. Carries the API's `error.type / message` if parseable,
    /// else the raw body.
    Api { status: u16, message: String },
    /// 2xx but the response body had no `content[].text` we could surface —
    /// usually a protocol mismatch (model returned only tool_use blocks etc).
    Empty,
}

impl std::fmt::Display for AiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AiError::MissingApiKey => write!(
                f,
                "ANTHROPIC_API_KEY is not set — export it before launching jterm4."
            ),
            AiError::Transport(s) => write!(f, "network error: {s}"),
            AiError::Api { status, message } => write!(f, "API {status}: {message}"),
            AiError::Empty => write!(f, "API returned no text content"),
        }
    }
}

/// Issue one synchronous Messages request via `curl`. Blocks the calling
/// thread until the response (or error) is in hand — caller is responsible
/// for spawning a worker thread so the GTK main loop stays responsive.
pub(crate) fn send_blocking(
    model: &str,
    max_tokens: u32,
    system: Option<&str>,
    history: &[Turn],
) -> Result<String, AiError> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or(AiError::MissingApiKey)?;

    let msgs: Vec<ReqMessage> = history
        .iter()
        .map(|t| ReqMessage {
            role: t.role.as_str(),
            content: &t.text,
        })
        .collect();
    let body = Request {
        model,
        max_tokens,
        system,
        messages: msgs,
    };
    let body_json = serde_json::to_string(&body)
        .map_err(|e| AiError::Transport(format!("encode request: {e}")))?;

    use std::io::Write;
    use std::process::Stdio;

    // -w writes the HTTP status as the last line of stdout AFTER the body,
    // so we can split them deterministically without a `-D headers` tempfile.
    let mut child = crate::host::command("curl")
        .args([
            "--silent",
            "--show-error",
            "-X",
            "POST",
            API_URL,
            "-H",
            &format!("x-api-key: {api_key}"),
            "-H",
            &format!("anthropic-version: {API_VERSION}"),
            "-H",
            "content-type: application/json",
            "--data-binary",
            "@-",
            "-w",
            "\n__JTERM4_STATUS__:%{http_code}",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AiError::Transport(format!("spawn curl: {e}")))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| AiError::Transport("curl stdin unavailable".to_string()))?;
        stdin
            .write_all(body_json.as_bytes())
            .map_err(|e| AiError::Transport(format!("write body: {e}")))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| AiError::Transport(format!("wait curl: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        return Err(AiError::Transport(format!(
            "curl exit {}: {}",
            out.status.code().unwrap_or(-1),
            stderr.trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (body, status) = split_curl_w(&stdout)
        .ok_or_else(|| AiError::Transport("malformed curl output".to_string()))?;

    if !(200..300).contains(&status) {
        let message = serde_json::from_str::<ApiError>(body)
            .map(|e| format!("{}: {}", e.error.kind, e.error.message))
            .unwrap_or_else(|_| {
                if body.is_empty() {
                    format!("HTTP {status}")
                } else {
                    body.to_string()
                }
            });
        return Err(AiError::Api { status, message });
    }

    let parsed: Response = serde_json::from_str(body)
        .map_err(|e| AiError::Transport(format!("decode response: {e}")))?;
    let text: String = parsed
        .content
        .into_iter()
        .filter(|c| c.kind == "text")
        .map(|c| c.text)
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        Err(AiError::Empty)
    } else {
        Ok(text)
    }
}

/// Split curl `-w "\n__JTERM4_STATUS__:%{http_code}"` output into (body, code).
/// Returns `None` if the marker is missing (e.g. curl wrote nothing).
fn split_curl_w(stdout: &str) -> Option<(&str, u16)> {
    let marker = "\n__JTERM4_STATUS__:";
    let idx = stdout.rfind(marker)?;
    let body = &stdout[..idx];
    let status: u16 = stdout[idx + marker.len()..].trim().parse().ok()?;
    Some((body, status))
}

/// Build the per-request system prompt from the user's chat history + the
/// (optional) selected-block context. Keeping this here so the UI layer only
/// hands raw fields and the wire format lives next to the request types.
pub(crate) fn build_system_prompt(block: Option<&BlockContext>) -> Option<String> {
    let base = "You are an inline terminal assistant embedded in jterm4. \
                Answer in tight, terminal-friendly markdown. Prefer shell \
                commands and concrete next steps over long prose.";
    let Some(b) = block else {
        return Some(base.to_string());
    };
    let mut s = String::from(base);
    s.push_str("\n\nThe user has selected a finished command block:\n");
    if let Some(cwd) = &b.cwd {
        s.push_str(&format!("cwd: {cwd}\n"));
    }
    s.push_str(&format!("exit_code: {}\n", b.exit_code));
    s.push_str("command:\n```\n");
    s.push_str(&b.cmd);
    s.push_str("\n```\n");
    if !b.output.trim().is_empty() {
        s.push_str("output:\n```\n");
        s.push_str(&b.output);
        s.push_str("\n```\n");
    }
    Some(s)
}

/// Selected-block context that the UI passes to the AI worker. `pub` (not
/// `pub(crate)`) because `TermView::selected_block_context()` lives in
/// `block_view` and exposes this type at the lib crate's surface.
#[derive(Clone, Debug)]
pub struct BlockContext {
    pub cmd: String,
    pub output: String,
    pub cwd: Option<String>,
    pub exit_code: i32,
}

/// Trim block output to the head + tail when it's large. Keeps the API token
/// bill bounded for `cargo build` / `pytest` style spew while preserving the
/// most-likely-relevant lines (the first failure summary and the trailing
/// error). `max_lines_per_side` lines from each end; if the input fits, it's
/// returned unchanged.
pub(crate) fn truncate_for_context(output: &str, max_lines_per_side: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= max_lines_per_side * 2 + 1 {
        return output.to_string();
    }
    let head = &lines[..max_lines_per_side];
    let tail = &lines[lines.len() - max_lines_per_side..];
    let elided = lines.len() - max_lines_per_side * 2;
    format!(
        "{}\n… [{} lines elided] …\n{}",
        head.join("\n"),
        elided,
        tail.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_passthrough_small() {
        assert_eq!(truncate_for_context("a\nb\nc\n", 5), "a\nb\nc\n");
    }

    #[test]
    fn truncate_elides_middle_large() {
        let input: String = (0..100)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = truncate_for_context(&input, 3);
        assert!(out.starts_with("l0\nl1\nl2"));
        assert!(out.contains("[94 lines elided]"));
        assert!(out.ends_with("l97\nl98\nl99"));
    }

    #[test]
    fn system_prompt_without_block_is_just_base() {
        let s = build_system_prompt(None).unwrap();
        assert!(s.contains("inline terminal assistant"));
        assert!(!s.contains("selected a finished command block"));
    }

    #[test]
    fn split_curl_w_parses_body_and_status() {
        let out = "{\"ok\":true}\n__JTERM4_STATUS__:200";
        let (body, status) = split_curl_w(out).unwrap();
        assert_eq!(body, "{\"ok\":true}");
        assert_eq!(status, 200);
    }

    #[test]
    fn split_curl_w_parses_error_body() {
        let out = "{\"error\":{\"type\":\"invalid_request_error\",\"message\":\"bad\"}}\n__JTERM4_STATUS__:400";
        let (body, status) = split_curl_w(out).unwrap();
        assert!(body.contains("invalid_request_error"));
        assert_eq!(status, 400);
    }

    #[test]
    fn split_curl_w_missing_marker_returns_none() {
        assert!(split_curl_w("just a body").is_none());
    }

    #[test]
    fn system_prompt_with_block_includes_cmd_and_exit() {
        let s = build_system_prompt(Some(&BlockContext {
            cmd: "git push".into(),
            output: "fatal: oops\n".into(),
            cwd: Some("/tmp".into()),
            exit_code: 128,
        }))
        .unwrap();
        assert!(s.contains("cwd: /tmp"));
        assert!(s.contains("exit_code: 128"));
        assert!(s.contains("git push"));
        assert!(s.contains("fatal: oops"));
    }
}
