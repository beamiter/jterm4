#![allow(dead_code)]

pub mod agent;
pub(crate) mod agent_task;
pub(crate) mod agent_task_ui;
pub mod ai;
pub mod block_view;
pub mod cli;
pub mod config;
pub mod config_store;
pub(crate) mod font;
pub mod git_meta;
/// The OSC/CSI stream parser lives in [`jterm_core::parser`], shared with the
/// other terminals; re-exported here so `crate::parser::…` paths keep working.
pub mod parser {
    pub use jterm_core::parser::*;
}
pub mod exit_status {
    pub use jterm_core::exit_status::*;
}
pub mod host;
mod image_drop;

pub mod identity {
    pub use jterm_core::identity::*;
}
pub mod keybindings;
pub mod logging;
pub mod notebook;
mod palette;
mod persistence;
pub mod process {
    pub use jterm_core::process::*;

    /// Refuse path text whose terminal rendering can disagree with the bytes
    /// inserted into the interactive shell editor.
    pub fn try_shell_quote_path(path: &str) -> Option<String> {
        if path.chars().any(char::is_control)
            || jterm_core::review_input::contains_visual_spoofing(path)
        {
            return None;
        }
        if path.is_empty() {
            return Some("''".to_string());
        }
        let safe = path
            .chars()
            .all(|character| character.is_alphanumeric() || "._-/~".contains(character));
        Some(if safe {
            path.to_string()
        } else {
            jterm_core::process::shell_single_quote(path)
        })
    }

    pub fn shell_quote_path(path: &str) -> String {
        try_shell_quote_path(path).unwrap_or_else(|| "''".to_string())
    }

    pub fn foreground_process_name(pty_fd: i32, shell_pid: i32) -> Option<String> {
        let name = jterm_core::process::foreground_process_name(pty_fd, shell_pid)?;
        let visible = jterm_core::review_input::safe_inline_display(&name, 4 * 1024);
        let mut characters = visible.chars();
        let mut bounded = characters.by_ref().take(128).collect::<String>();
        if characters.next().is_some() {
            bounded.push('…');
        }
        (!bounded.trim().is_empty()).then_some(bounded)
    }

    /// Restored commands are typed through an interactive PTY, so keep both
    /// the snapshot representation and the quoting amplification well below
    /// the platform's argv/input limits. The pinned shared classifier predates
    /// these bounds; retain them locally until the next shared-core release is
    /// published and consumed.
    pub const MAX_RESTORABLE_ARG_COUNT_LOCAL: usize = 256;
    pub const MAX_RESTORABLE_ARG_BYTES_LOCAL: usize = 64 * 1024;
    pub const MAX_RESTORABLE_ARGV_BYTES_LOCAL: usize = 256 * 1024;
    pub const MAX_RESTORABLE_QUOTED_COMMAND_BYTES_LOCAL: usize = 512 * 1024;

    pub fn restorable_argv_within_local_limits(args: &[String]) -> bool {
        if args.is_empty() || args.len() > MAX_RESTORABLE_ARG_COUNT_LOCAL {
            return false;
        }
        let mut total = 0usize;
        for argument in args {
            if argument.len() > MAX_RESTORABLE_ARG_BYTES_LOCAL
                || argument.chars().any(char::is_control)
                || jterm_core::review_input::contains_visual_spoofing(argument)
            {
                return false;
            }
            let Some(next) = total
                .checked_add(argument.len())
                .and_then(|bytes| bytes.checked_add(1))
            else {
                return false;
            };
            if next > MAX_RESTORABLE_ARGV_BYTES_LOCAL {
                return false;
            }
            total = next;
        }
        true
    }

    /// Re-run both the byte budget and the narrow command allowlist at every
    /// execution/persistence boundary. Structured argv preserves boundaries;
    /// it does not make arbitrary snapshot content trustworthy.
    pub fn match_restorable_command_bounded(args: &[String]) -> Option<Vec<String>> {
        restorable_argv_within_local_limits(args)
            .then(|| jterm_core::process::match_restorable_command(args))
            .flatten()
    }

    /// Load old joined-string snapshots without replaying them, and discard
    /// unknown or oversized structured argv before any quoting or PTY spawn.
    /// The visitor retains at most 256 elements while consuming the JSON, so a
    /// tiny-array-element attack cannot first allocate millions of `String`s
    /// and only then discover the structural limit.
    pub fn deserialize_restorable_argv_bounded<'de, D>(
        deserializer: D,
    ) -> Result<Option<Vec<String>>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct BoundedArgument(Option<String>);

        impl<'de> serde::Deserialize<'de> for BoundedArgument {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                struct ArgumentVisitor;

                impl serde::de::Visitor<'_> for ArgumentVisitor {
                    type Value = BoundedArgument;

                    fn expecting(
                        &self,
                        formatter: &mut std::fmt::Formatter<'_>,
                    ) -> std::fmt::Result {
                        formatter.write_str("a bounded command argument string")
                    }

                    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
                    where
                        E: serde::de::Error,
                    {
                        Ok(BoundedArgument(
                            (value.len() <= MAX_RESTORABLE_ARG_BYTES_LOCAL
                                && !value.chars().any(char::is_control)
                                && !jterm_core::review_input::contains_visual_spoofing(value))
                            .then(|| value.to_string()),
                        ))
                    }

                    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
                    where
                        E: serde::de::Error,
                    {
                        Ok(BoundedArgument(
                            (value.len() <= MAX_RESTORABLE_ARG_BYTES_LOCAL
                                && !value.chars().any(char::is_control)
                                && !jterm_core::review_input::contains_visual_spoofing(&value))
                            .then_some(value),
                        ))
                    }
                }

                deserializer.deserialize_string(ArgumentVisitor)
            }
        }

        struct StoredCommandVisitor;

        impl<'de> serde::de::Visitor<'de> for StoredCommandVisitor {
            type Value = Option<Vec<String>>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("null, a legacy command string, or a bounded argv array")
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_str<E>(self, joined: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let _ = joined;
                log::warn!("Ignoring legacy session restore command without argv boundaries");
                Ok(None)
            }

            fn visit_string<E>(self, joined: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_str(&joined)
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let capacity = sequence
                    .size_hint()
                    .unwrap_or(0)
                    .min(MAX_RESTORABLE_ARG_COUNT_LOCAL);
                let mut argv = Vec::with_capacity(capacity);
                let mut total = 0usize;
                let mut valid = true;
                let mut count = 0usize;
                while let Some(BoundedArgument(argument)) =
                    sequence.next_element::<BoundedArgument>()?
                {
                    count = count.saturating_add(1);
                    let Some(argument) = argument else {
                        valid = false;
                        continue;
                    };
                    let next_total = total
                        .checked_add(argument.len())
                        .and_then(|bytes| bytes.checked_add(1));
                    if count > MAX_RESTORABLE_ARG_COUNT_LOCAL
                        || next_total.is_none_or(|bytes| bytes > MAX_RESTORABLE_ARGV_BYTES_LOCAL)
                    {
                        valid = false;
                        continue;
                    }
                    total = next_total.unwrap_or(total);
                    argv.push(argument);
                }
                Ok(valid
                    .then(|| match_restorable_command_bounded(&argv))
                    .flatten())
            }
        }

        deserializer.deserialize_any(StoredCommandVisitor)
    }
}
pub mod pty;
pub mod redact {
    pub use jterm_core::redact::*;
}
pub mod jsh_install;
pub(crate) mod review_text;
pub mod state;
pub mod terminal;
pub mod workflows;

#[path = "main.rs"]
pub mod app;
mod ui;
