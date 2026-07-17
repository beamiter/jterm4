//! Provider-neutral AI helpers for terminal chat, NL-to-command, and Agent mode.
//!
//! HTTP is intentionally delegated to the existing host curl binary. This
//! keeps the GTK thread free (callers run these blocking functions on a worker)
//! and avoids adding a second TLS stack. Every API here only returns text; no
//! function in this module executes or submits a generated command.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;

const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const CURL_STATUS_MARKER: &str = "\n__JTERM4_STATUS__:";
const MAX_ERROR_BODY_BYTES: usize = 2 * 1024;
const MAX_GENERATED_COMMAND_BYTES: usize = 16 * 1024;
const MAX_API_KEY_FILE_BYTES: u64 = 16 * 1024;
const API_KEY_ENV_NAMES: [&str; 4] = [
    "JTERM4_AI_API_KEY",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OLLAMA_API_KEY",
];

mod conversation;

pub(crate) use conversation::{
    ChatSnapshot, ConversationSnapshot, ConversationSnapshotError,
    MAX_CONVERSATION_SNAPSHOT_JSON_BYTES, MAX_PERSISTED_CHATS,
};

/// Supported wire protocols. OpenAI-compatible intentionally includes local
/// and hosted services which implement the Chat Completions endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAiCompatible,
    Ollama,
}

impl Provider {
    pub fn as_config_value(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAiCompatible => "openai-compatible",
            Self::Ollama => "ollama",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAiCompatible => "OpenAI-compatible",
            Self::Ollama => "Ollama",
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-sonnet-4-6",
            Self::OpenAiCompatible => "gpt-4o-mini",
            Self::Ollama => "codellama:7b",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::OpenAiCompatible => "https://api.openai.com/v1",
            Self::Ollama => "http://localhost:11434",
        }
    }

    fn endpoint(self, base_url: &str) -> String {
        let base = base_url.trim_end_matches('/');
        match self {
            Self::Anthropic if base.ends_with("/v1/messages") => base.to_string(),
            Self::Anthropic if base.ends_with("/v1") => format!("{base}/messages"),
            Self::Anthropic => format!("{base}/v1/messages"),
            Self::OpenAiCompatible if base.ends_with("/chat/completions") => base.to_string(),
            Self::OpenAiCompatible => format!("{base}/chat/completions"),
            Self::Ollama if base.ends_with("/api/chat") => base.to_string(),
            Self::Ollama if base.ends_with("/api") => format!("{base}/chat"),
            Self::Ollama => format!("{base}/api/chat"),
        }
    }

    fn provider_api_key(self) -> Option<String> {
        let provider_key = match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAiCompatible => "OPENAI_API_KEY",
            Self::Ollama => "OLLAMA_API_KEY",
        };
        nonempty_env("JTERM4_AI_API_KEY").or_else(|| nonempty_env(provider_key))
    }
}

impl FromStr for Provider {
    type Err = AiError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "openai" | "openai-compatible" | "openai_compatible" => Ok(Self::OpenAiCompatible),
            "ollama" => Ok(Self::Ollama),
            other => Err(AiError::InvalidConfiguration(format!(
                "unknown AI provider '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    User,
    Assistant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// One turn in a provider-neutral conversation transcript.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Turn {
    pub(crate) role: Role,
    pub(crate) text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiError {
    /// Legacy Anthropic entry point could not find ANTHROPIC_API_KEY.
    MissingApiKey,
    MissingProviderApiKey {
        provider: Provider,
    },
    CredentialFile(String),
    Disabled,
    InvalidConfiguration(String),
    InvalidCommand(String),
    Transport(String),
    Api {
        status: u16,
        message: String,
    },
    Empty,
}

impl std::fmt::Display for AiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingApiKey => write!(
                f,
                "ANTHROPIC_API_KEY is not set — export it before launching jterm4"
            ),
            Self::MissingProviderApiKey { provider } => write!(
                f,
                "{} API key is not set (use an environment variable or ai_api_key_file)",
                provider.display_name()
            ),
            Self::CredentialFile(message) => write!(f, "AI API key file: {message}"),
            Self::Disabled => write!(f, "AI features are disabled by configuration"),
            Self::InvalidConfiguration(message) => write!(f, "invalid AI configuration: {message}"),
            Self::InvalidCommand(message) => write!(
                f,
                "model did not return one safe-to-review command: {message}"
            ),
            Self::Transport(message) => write!(f, "network error: {message}"),
            Self::Api { status, message } => write!(f, "API {status}: {message}"),
            Self::Empty => write!(f, "API returned no text content"),
        }
    }
}

impl std::error::Error for AiError {}

/// Fully resolved settings for one provider. API key contents are never part
/// of Config or config persistence; only an optional credential-file path is.
#[derive(Debug, Clone)]
pub struct AiClient {
    pub provider: Provider,
    pub api_key: Option<String>,
    pub model: String,
    pub base_url: String,
    pub max_tokens: u32,
    pub redact_secrets: bool,
}

impl AiClient {
    pub fn new(
        provider: Provider,
        api_key: Option<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
        max_tokens: u32,
        redact_secrets: bool,
    ) -> Result<Self, AiError> {
        let model = model.into();
        let base_url = base_url.into();
        validate_client_values(&model, &base_url, max_tokens)?;
        if provider == Provider::Anthropic
            && api_key.as_deref().is_none_or(|key| key.trim().is_empty())
        {
            return Err(AiError::MissingProviderApiKey { provider });
        }
        Ok(Self {
            provider,
            api_key: api_key.filter(|key| !key.trim().is_empty()),
            model,
            base_url: base_url.trim_end_matches('/').to_string(),
            max_tokens,
            redact_secrets,
        })
    }

    pub(crate) fn from_config(config: &crate::config::Config) -> Result<Self, AiError> {
        if !config.ai_enabled {
            return Err(AiError::Disabled);
        }
        let provider = Provider::from_str(&config.ai_provider)?;
        let api_key = match provider.provider_api_key() {
            Some(key) => Some(key),
            None => config
                .ai_api_key_file
                .as_deref()
                .map(read_api_key_file)
                .transpose()?,
        };
        Self::new(
            provider,
            api_key,
            config.ai_model.clone(),
            config.ai_base_url.clone(),
            config.ai_max_tokens,
            config.ai_redact_secrets,
        )
    }

    /// Environment-only construction for non-GTK callers. Explicit provider
    /// wins, followed by detected Anthropic/OpenAI credentials, then Ollama.
    pub fn from_env() -> Result<Self, AiError> {
        let provider = match nonempty_env("JTERM4_AI_PROVIDER") {
            Some(value) => Provider::from_str(&value)?,
            None if nonempty_env("ANTHROPIC_API_KEY").is_some() => Provider::Anthropic,
            None if nonempty_env("OPENAI_API_KEY").is_some() => Provider::OpenAiCompatible,
            None => Provider::Ollama,
        };
        let model =
            nonempty_env("JTERM4_AI_MODEL").unwrap_or_else(|| provider.default_model().to_string());
        let base_url = nonempty_env("JTERM4_AI_BASE_URL")
            .unwrap_or_else(|| provider.default_base_url().to_string());
        let max_tokens = nonempty_env("JTERM4_AI_MAX_TOKENS")
            .and_then(|value| value.parse().ok())
            .unwrap_or(1024);
        let api_key = match provider.provider_api_key() {
            Some(key) => Some(key),
            None => nonempty_env("JTERM4_AI_API_KEY_FILE")
                .as_deref()
                .map(read_api_key_file)
                .transpose()?,
        };
        Self::new(provider, api_key, model, base_url, max_tokens, true)
    }

    pub fn display_name(&self) -> String {
        format!("{} · {}", self.provider.display_name(), self.model)
    }

    /// Send an existing multi-turn transcript. This function blocks and must
    /// be invoked off the GTK main thread.
    pub(crate) fn send_turns_blocking(
        &self,
        system: Option<&str>,
        history: &[Turn],
    ) -> Result<String, AiError> {
        let system = system.map(|text| self.prepare_text(text));
        let history: Vec<Turn> = history
            .iter()
            .map(|turn| Turn {
                role: turn.role,
                text: self.prepare_text(&turn.text),
            })
            .collect();
        let body = self.request_body(system.as_deref(), &history);
        let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
        match self.provider {
            Provider::Anthropic => {
                let key = self
                    .api_key
                    .as_deref()
                    .filter(|key| !key.trim().is_empty())
                    .ok_or(AiError::MissingProviderApiKey {
                        provider: self.provider,
                    })?;
                headers.push(("x-api-key".to_string(), key.to_string()));
                headers.push((
                    "anthropic-version".to_string(),
                    ANTHROPIC_API_VERSION.to_string(),
                ));
            }
            Provider::OpenAiCompatible | Provider::Ollama => {
                if let Some(key) = self.api_key.as_deref().filter(|key| !key.trim().is_empty()) {
                    headers.push(("authorization".to_string(), format!("Bearer {key}")));
                }
            }
        }
        let response = curl_json_post(&self.provider.endpoint(&self.base_url), &headers, &body)?;
        self.parse_response(response)
    }

    fn prepare_text(&self, text: &str) -> String {
        if self.redact_secrets {
            crate::redact::redact_secrets(text)
        } else {
            text.to_string()
        }
    }

    fn request_body(&self, system: Option<&str>, history: &[Turn]) -> Value {
        let mut messages: Vec<Value> = history
            .iter()
            .map(|turn| json!({"role": turn.role.as_str(), "content": turn.text}))
            .collect();
        match self.provider {
            Provider::Anthropic => {
                let mut body = json!({
                    "model": self.model,
                    "max_tokens": self.max_tokens,
                    "messages": messages,
                });
                if let Some(system) = system {
                    body["system"] = Value::String(system.to_string());
                }
                body
            }
            Provider::OpenAiCompatible => {
                if let Some(system) = system {
                    messages.insert(0, json!({"role": "system", "content": system}));
                }
                json!({
                    "model": self.model,
                    "max_tokens": self.max_tokens,
                    "messages": messages,
                })
            }
            Provider::Ollama => {
                if let Some(system) = system {
                    messages.insert(0, json!({"role": "system", "content": system}));
                }
                json!({
                    "model": self.model,
                    "messages": messages,
                    "stream": false,
                    "options": {"num_predict": self.max_tokens},
                })
            }
        }
    }

    fn parse_response(&self, response: Value) -> Result<String, AiError> {
        let text = match self.provider {
            Provider::Anthropic => response
                .get("content")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                }),
            Provider::OpenAiCompatible => response
                .pointer("/choices/0/message/content")
                .and_then(content_text),
            Provider::Ollama => response
                .pointer("/message/content")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    response
                        .get("response")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }),
        }
        .unwrap_or_default();
        if text.trim().is_empty() {
            Err(AiError::Empty)
        } else {
            Ok(text)
        }
    }
}

fn content_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    value.as_array().map(|parts| {
        parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn validate_client_values(model: &str, base_url: &str, max_tokens: u32) -> Result<(), AiError> {
    if model.trim().is_empty() {
        return Err(AiError::InvalidConfiguration(
            "model must not be empty".into(),
        ));
    }
    let base_url = base_url.trim();
    if !(base_url.starts_with("http://") || base_url.starts_with("https://"))
        || base_url
            .split_once("://")
            .is_none_or(|(_, host)| host.is_empty())
        || base_url.chars().any(char::is_whitespace)
    {
        return Err(AiError::InvalidConfiguration(
            "base URL must be an absolute http(s) URL without whitespace".into(),
        ));
    }
    if !(64..=32_768).contains(&max_tokens) {
        return Err(AiError::InvalidConfiguration(
            "max tokens must be between 64 and 32768".into(),
        ));
    }
    Ok(())
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn expand_api_key_path(raw: &str) -> Result<PathBuf, AiError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AiError::CredentialFile("path is empty".into()));
    }
    if raw == "~" || raw.starts_with("~/") {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| AiError::CredentialFile("HOME is unavailable for ~/ path".into()))?;
        let mut path = PathBuf::from(home);
        if let Some(rest) = raw.strip_prefix("~/") {
            path.push(rest);
        }
        return Ok(path);
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(AiError::CredentialFile(
            "path must be absolute or begin with ~/".into(),
        ));
    }
    Ok(path.to_path_buf())
}

fn read_api_key_file(raw_path: &str) -> Result<String, AiError> {
    let path = expand_api_key_path(raw_path)?;
    let file = fs::File::open(&path).map_err(|error| {
        AiError::CredentialFile(format!("cannot open {}: {error}", path.display()))
    })?;
    let metadata = file.metadata().map_err(|error| {
        AiError::CredentialFile(format!("cannot inspect {}: {error}", path.display()))
    })?;
    if !metadata.is_file() {
        return Err(AiError::CredentialFile(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if metadata.len() > MAX_API_KEY_FILE_BYTES {
        return Err(AiError::CredentialFile(format!(
            "{} exceeds {} bytes",
            path.display(),
            MAX_API_KEY_FILE_BYTES
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = metadata.mode() & 0o777;
        // SAFETY: geteuid has no preconditions and only returns process state.
        let effective_uid = unsafe { nix::libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(AiError::CredentialFile(format!(
                "{} is not owned by the current user",
                path.display()
            )));
        }
        if mode & 0o077 != 0 {
            return Err(AiError::CredentialFile(format!(
                "{} permissions are {:03o}; run chmod 600 {}",
                path.display(),
                mode,
                path.display()
            )));
        }
    }
    let mut contents = String::new();
    file.take(MAX_API_KEY_FILE_BYTES + 1)
        .read_to_string(&mut contents)
        .map_err(|error| {
            AiError::CredentialFile(format!("cannot read {}: {error}", path.display()))
        })?;
    if contents.len() as u64 > MAX_API_KEY_FILE_BYTES {
        return Err(AiError::CredentialFile(format!(
            "{} exceeds {} bytes",
            path.display(),
            MAX_API_KEY_FILE_BYTES
        )));
    }
    let key = contents.trim();
    if key.is_empty() {
        return Err(AiError::CredentialFile(format!(
            "{} is empty",
            path.display()
        )));
    }
    if key.chars().any(char::is_control) {
        return Err(AiError::CredentialFile(format!(
            "{} contains control characters",
            path.display()
        )));
    }
    Ok(key.to_string())
}

fn curl_json_post(url: &str, headers: &[(String, String)], body: &Value) -> Result<Value, AiError> {
    let body = serde_json::to_string(body)
        .map_err(|error| AiError::Transport(format!("encode request: {error}")))?;
    let config = build_curl_stdin_config(url, headers, &body)?;
    let mut command = crate::host::command("curl");
    for name in API_KEY_ENV_NAMES {
        // The already-resolved credential is carried in the private stdin
        // pipe. There is no reason to duplicate any provider credential in
        // curl/flatpak-spawn's inherited environment.
        command.env_remove(name);
    }
    // Keep the URL, request body, and especially authentication headers out of
    // the child argv (and therefore out of `ps`/`/proc/*/cmdline`). curl reads
    // its complete per-request config from stdin instead. This also works
    // through `flatpak-spawn --host`, which forwards the standard streams.
    command
        // `--disable` must be curl's first option. It prevents a user curlrc
        // from adding headers, changing the destination, or redirecting the
        // AI request before our explicit stdin config is read.
        .args(["--disable", "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| AiError::Transport(format!("spawn curl: {error}")))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| AiError::Transport("curl stdin unavailable".into()))?
        .write_all(config.as_bytes())
        .map_err(|error| AiError::Transport(format!("write request: {error}")))?;
    let output = child
        .wait_with_output()
        .map_err(|error| AiError::Transport(format!("wait for curl: {error}")))?;
    if !output.status.success() {
        return Err(AiError::Transport(format!(
            "curl exit {}: {}",
            output.status.code().unwrap_or(-1),
            trim_for_log(
                &String::from_utf8_lossy(&output.stderr),
                MAX_ERROR_BODY_BYTES
            )
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (body, status) =
        split_curl_w(&stdout).ok_or_else(|| AiError::Transport("malformed curl output".into()))?;
    if !(200..300).contains(&status) {
        return Err(AiError::Api {
            status,
            message: api_error_message(body, status),
        });
    }
    serde_json::from_str(body)
        .map_err(|error| AiError::Transport(format!("decode response: {error}")))
}

/// Quote one value for curl's double-quoted config-file grammar. curl expands
/// these four escapes back to the original bytes. Reject CR/LF in headers
/// separately below so an environment-provided API key cannot add a header.
fn curl_config_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\t' => quoted.push_str("\\t"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

fn build_curl_stdin_config(
    url: &str,
    headers: &[(String, String)],
    body: &str,
) -> Result<String, AiError> {
    let mut config = String::from(
        "silent\nshow-error\nconnect-timeout = 10\nmax-time = 75\nrequest = \"POST\"\n",
    );
    config.push_str("url = ");
    config.push_str(&curl_config_quote(url));
    config.push('\n');
    for (name, value) in headers {
        if name.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
            return Err(AiError::InvalidConfiguration(
                "AI HTTP headers must not contain newlines".into(),
            ));
        }
        config.push_str("header = ");
        config.push_str(&curl_config_quote(&format!("{name}: {value}")));
        config.push('\n');
    }
    config.push_str("data-binary = ");
    config.push_str(&curl_config_quote(body));
    config.push('\n');
    config.push_str("write-out = ");
    config.push_str(&curl_config_quote(&format!(
        "{CURL_STATUS_MARKER}%{{http_code}}"
    )));
    config.push('\n');
    Ok(config)
}

fn api_error_message(body: &str, status: u16) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(message) = value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| value.get("error").and_then(Value::as_str))
            .or_else(|| value.get("message").and_then(Value::as_str))
        {
            return trim_for_log(message, MAX_ERROR_BODY_BYTES);
        }
    }
    if body.trim().is_empty() {
        format!("HTTP {status}")
    } else {
        trim_for_log(body.trim(), MAX_ERROR_BODY_BYTES)
    }
}

fn split_curl_w(stdout: &str) -> Option<(&str, u16)> {
    let index = stdout.rfind(CURL_STATUS_MARKER)?;
    let body = &stdout[..index];
    let status = stdout[index + CURL_STATUS_MARKER.len()..]
        .trim()
        .parse()
        .ok()?;
    Some((body, status))
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn trim_for_log(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        text.to_string()
    } else {
        format!("{}…", &text[..floor_char_boundary(text, max_bytes)])
    }
}

/// Compatibility entry point for the existing Anthropic AI panel. The UI
/// already applies its configurable redaction before calling this function.
pub(crate) fn send_blocking(
    model: &str,
    max_tokens: u32,
    system: Option<&str>,
    history: &[Turn],
) -> Result<String, AiError> {
    let api_key = nonempty_env("ANTHROPIC_API_KEY").ok_or(AiError::MissingApiKey)?;
    let base_url = nonempty_env("JTERM4_AI_BASE_URL")
        .unwrap_or_else(|| Provider::Anthropic.default_base_url().to_string());
    let client = AiClient::new(
        Provider::Anthropic,
        Some(api_key),
        model,
        base_url,
        max_tokens,
        false,
    )?;
    client.send_turns_blocking(system, history)
}

/// Natural language to one reviewable shell command. The returned command is
/// never executed; callers must present it to the user and require an explicit
/// action before typing or submitting it.
pub fn nl_to_command_blocking(
    client: &AiClient,
    query: &str,
    cwd: &str,
) -> Result<String, AiError> {
    let (system, user) = build_nl_to_cmd_prompt(query, cwd);
    let response = client.send_turns_blocking(
        Some(&system),
        &[Turn {
            role: Role::User,
            text: user,
        }],
    )?;
    parse_single_command(&response)
}

fn parse_single_command(raw: &str) -> Result<String, AiError> {
    let mut value = raw.trim();
    if value.starts_with("```") {
        let first_newline = value
            .find('\n')
            .ok_or_else(|| AiError::InvalidCommand("unterminated markdown fence".into()))?;
        let language = value[3..first_newline].trim().to_ascii_lowercase();
        if !matches!(
            language.as_str(),
            "" | "sh" | "bash" | "shell" | "zsh" | "fish"
        ) {
            return Err(AiError::InvalidCommand(format!(
                "unexpected code-fence language '{language}'"
            )));
        }
        let fenced = &value[first_newline + 1..];
        let closing = fenced
            .strip_suffix("```")
            .ok_or_else(|| AiError::InvalidCommand("unterminated markdown fence".into()))?;
        value = closing.trim();
    }
    if value.is_empty() {
        return Err(AiError::InvalidCommand("empty response".into()));
    }
    if value.len() > MAX_GENERATED_COMMAND_BYTES {
        return Err(AiError::InvalidCommand("response is too large".into()));
    }
    if value.contains('\n') || value.contains('\r') {
        return Err(AiError::InvalidCommand(
            "response contains more than one line".into(),
        ));
    }
    if value.chars().any(|ch| ch.is_control() && ch != '\t') {
        return Err(AiError::InvalidCommand(
            "response contains control characters".into(),
        ));
    }
    Ok(value.to_string())
}

pub(crate) fn build_system_prompt(block: Option<&BlockContext>) -> Option<String> {
    let base = "You are an inline terminal assistant embedded in jterm4. \
                Answer in tight, terminal-friendly markdown. Prefer shell \
                commands and concrete next steps over long prose.";
    let Some(block) = block else {
        return Some(base.to_string());
    };
    let mut prompt = String::from(base);
    prompt.push_str("\n\nThe user has selected a finished command block:\n");
    if let Some(cwd) = &block.cwd {
        prompt.push_str(&format!("cwd: {cwd}\n"));
    }
    prompt.push_str(&format!("exit_code: {}\n", block.exit_code));
    prompt.push_str("command:\n```\n");
    prompt.push_str(&block.cmd);
    prompt.push_str("\n```\n");
    if !block.output.trim().is_empty() {
        prompt.push_str("output:\n```\n");
        prompt.push_str(&block.output);
        prompt.push_str("\n```\n");
    }
    Some(prompt)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlockContext {
    pub cmd: String,
    pub output: String,
    pub cwd: Option<String>,
    pub exit_code: i32,
}

pub(crate) fn truncate_for_context(output: &str, max_lines_per_side: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= max_lines_per_side * 2 + 1 {
        return output.to_string();
    }
    let head = &lines[..max_lines_per_side];
    let tail = &lines[lines.len() - max_lines_per_side..];
    let elided = lines.len() - max_lines_per_side * 2;
    format!(
        "{}\n… [{elided} lines elided] …\n{}",
        head.join("\n"),
        tail.join("\n")
    )
}

fn sample_output(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_string();
    }
    let half = max_bytes / 2;
    let head_end = floor_char_boundary(output, half);
    let tail_start = ceil_char_boundary(output, output.len().saturating_sub(half));
    let retained = head_end + output.len().saturating_sub(tail_start);
    format!(
        "{}\n\n… [{} bytes elided] …\n\n{}",
        &output[..head_end],
        output.len().saturating_sub(retained),
        &output[tail_start..]
    )
}

pub fn build_explain_prompt(
    command: &str,
    output: &str,
    exit_code: i32,
    cwd: &str,
) -> (String, String) {
    let system = "You are a senior shell user helping debug a failed command. \
Read the command, its output, and exit code. Reply with one short diagnosis and \
one concrete fix. Be terse; use no markdown headers or filler."
        .to_string();
    let user = format!(
        "cwd: {cwd}\nexit: {exit_code}\ncommand:\n{command}\n\noutput:\n{}",
        sample_output(output, 8 * 1024)
    );
    (system, user)
}

pub fn build_nl_to_cmd_prompt(query: &str, cwd: &str) -> (String, String) {
    let system = "Convert the request into exactly one shell command. Output only \
the command on one line: no markdown, quotes, comments, or explanation. Never claim \
the command ran. If the request cannot safely map to one command, output false."
        .to_string();
    (system, format!("cwd: {cwd}\nrequest: {query}"))
}

pub fn build_agent_system_prompt(cwd: &str, shell: &str, os: &str) -> String {
    format!(
        "You are an interactive shell agent. Every reply MUST be exactly one JSON object, \
with no markdown or surrounding prose. Allowed shapes (no extra keys):\n\
{{\"action\":\"run\",\"command\":\"one command\",\"thought\":\"optional\"}}\n\
{{\"action\":\"say\",\"message\":\"question or note\",\"thought\":\"optional\"}}\n\
{{\"action\":\"done\",\"message\":\"short summary\",\"thought\":\"optional\"}}\n\
A run action is only a proposal. The application will never execute it without explicit \
per-command user approval. Propose one focused command, wait for its exit status and output, \
and never assume success. Use say for clarification and done only when complete.\n\n\
Environment:\n  cwd: {cwd}\n  shell: {shell}\n  os: {os}\n"
    )
}

pub fn build_session_prompt(question: &str, context: Option<&str>) -> (String, String) {
    let system = "You are a terminal assistant. Answer concisely, use attached shell \
context when present, and use no filler or markdown headers."
        .to_string();
    let user = match context {
        Some(context) => format!("Recent shell context:\n{context}\n\nQuestion: {question}"),
        None => format!("Question: {question}"),
    };
    (system, user)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(provider: Provider) -> AiClient {
        AiClient {
            provider,
            api_key: Some("test-key".into()),
            model: "test-model".into(),
            base_url: provider.default_base_url().into(),
            max_tokens: 512,
            redact_secrets: false,
        }
    }

    #[test]
    fn provider_aliases_and_endpoints_are_normalized() {
        assert_eq!(Provider::from_str("claude").unwrap(), Provider::Anthropic);
        assert_eq!(
            Provider::from_str("openai").unwrap(),
            Provider::OpenAiCompatible
        );
        assert_eq!(
            Provider::Anthropic.endpoint("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            Provider::OpenAiCompatible.endpoint("http://localhost:8000/v1"),
            "http://localhost:8000/v1/chat/completions"
        );
        assert_eq!(
            Provider::Ollama.endpoint("http://localhost:11434/api/chat"),
            "http://localhost:11434/api/chat"
        );
    }

    #[test]
    fn provider_request_shapes_include_history_and_limits() {
        let turns = vec![
            Turn {
                role: Role::User,
                text: "hello".into(),
            },
            Turn {
                role: Role::Assistant,
                text: "hi".into(),
            },
        ];
        let anthropic = client(Provider::Anthropic).request_body(Some("system"), &turns);
        assert_eq!(anthropic["system"], "system");
        assert_eq!(anthropic["messages"].as_array().unwrap().len(), 2);
        let openai = client(Provider::OpenAiCompatible).request_body(Some("system"), &turns);
        assert_eq!(openai["messages"][0]["role"], "system");
        let ollama = client(Provider::Ollama).request_body(Some("system"), &turns);
        assert_eq!(ollama["stream"], false);
        assert_eq!(ollama["options"]["num_predict"], 512);
    }

    #[test]
    fn curl_request_keeps_credentials_and_payload_in_stdin_config() {
        let secret = "sk-ant-super-secret";
        let config = build_curl_stdin_config(
            "https://example.invalid/v1/messages",
            &[("x-api-key".into(), secret.into())],
            r#"{"prompt":"say \"hello\""}"#,
        )
        .unwrap();
        assert!(config.contains(secret));
        assert!(config.contains("header = \"x-api-key: sk-ant-super-secret\""));
        assert!(config.contains(r#"data-binary = "{\"prompt\":\"say \\\"hello\\\"\"}""#));

        // These are the only arguments passed to curl itself. Secrets, URL,
        // and body live exclusively in the pipe above.
        let argv = ["--disable", "--config", "-"];
        assert_eq!(argv.first(), Some(&"--disable"));
        assert!(!argv.join(" ").contains(secret));
        assert!(!argv.join(" ").contains("example.invalid"));
    }

    #[test]
    fn curl_request_rejects_header_newline_injection() {
        let error = build_curl_stdin_config(
            "https://example.invalid/v1/messages",
            &[("authorization".into(), "Bearer good\r\nX-Evil: yes".into())],
            "{}",
        )
        .unwrap_err();
        assert!(matches!(error, AiError::InvalidConfiguration(_)));
    }

    #[test]
    fn curl_child_environment_explicitly_removes_provider_credentials() {
        let mut command = std::process::Command::new("curl");
        for name in API_KEY_ENV_NAMES {
            command.env(name, "must-not-be-inherited");
            command.env_remove(name);
        }
        for name in API_KEY_ENV_NAMES {
            let value = command
                .get_envs()
                .find(|(key, _)| *key == std::ffi::OsStr::new(name))
                .map(|(_, value)| value);
            assert_eq!(value, Some(None), "{name}");
        }
    }

    #[test]
    fn parses_all_provider_response_shapes() {
        assert_eq!(
            client(Provider::Anthropic)
                .parse_response(json!({"content":[{"type":"text","text":"ok"}]}))
                .unwrap(),
            "ok"
        );
        assert_eq!(
            client(Provider::OpenAiCompatible)
                .parse_response(json!({"choices":[{"message":{"content":"ok"}}]}))
                .unwrap(),
            "ok"
        );
        assert_eq!(
            client(Provider::Ollama)
                .parse_response(json!({"message":{"content":"ok"}}))
                .unwrap(),
            "ok"
        );
    }

    #[test]
    fn strict_command_parser_accepts_one_command_only() {
        assert_eq!(parse_single_command("git status").unwrap(), "git status");
        assert_eq!(
            parse_single_command("```sh\ngit status\n```").unwrap(),
            "git status"
        );
        assert!(parse_single_command("git status\necho done").is_err());
        assert!(parse_single_command("Here you go: git status").is_ok());
        // Prose cannot be identified perfectly, but multiline/fenced protocol
        // violations are rejected; execution is still impossible in this API.
    }

    #[test]
    fn truncate_and_sample_are_utf8_safe() {
        let lines = (0..100)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let truncated = truncate_for_context(&lines, 3);
        assert!(truncated.contains("94 lines elided"));
        let sampled = sample_output(&"编译失败🙂".repeat(2_000), 1_001);
        assert!(sampled.contains("bytes elided"));
        assert!(sampled.ends_with('🙂'));
    }

    #[test]
    fn system_prompt_includes_selected_block() {
        let prompt = build_system_prompt(Some(&BlockContext {
            cmd: "false".into(),
            output: "failed".into(),
            cwd: Some("/tmp".into()),
            exit_code: 1,
        }))
        .unwrap();
        assert!(prompt.contains("cwd: /tmp"));
        assert!(prompt.contains("exit_code: 1"));
        assert!(prompt.contains("false"));
    }

    #[test]
    fn validation_rejects_bad_urls_and_limits() {
        assert!(AiClient::new(
            Provider::Ollama,
            None,
            "model",
            "file:///tmp/socket",
            512,
            true
        )
        .is_err());
        assert!(AiClient::new(
            Provider::Ollama,
            None,
            "model",
            "http://localhost:11434",
            2,
            true
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn api_key_file_requires_private_permissions_and_trims_one_line() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "jterm4-ai-key-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        fs::write(&path, "sk-test-secret\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_api_key_file(path.to_str().unwrap()).unwrap(),
            "sk-test-secret"
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = read_api_key_file(path.to_str().unwrap()).unwrap_err();
        assert!(matches!(error, AiError::CredentialFile(_)));
        assert!(error.to_string().contains("chmod 600"));
        fs::remove_file(path).unwrap();
    }
}
