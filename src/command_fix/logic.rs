mod classify;
mod model;
mod resolve;
mod util;

use crate::ai::AiClient;

pub(super) use classify::classify;

#[derive(Clone, Debug)]
pub(super) struct Context {
    pub(super) command: String,
    pub(super) cwd: String,
    pub(super) exit_code: i32,
    pub(super) output: String,
    pub(super) remote: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Failure {
    AptPackageNotFound(String),
    CommandNotFound(String),
    ToolSuggestion { old: String, new: String },
    UnknownSubcommand(String),
    UnknownOption(String),
}

impl Failure {
    fn label(&self) -> &'static str {
        match self {
            Self::AptPackageNotFound(_) => "package_name_not_found",
            Self::CommandNotFound(_) => "command_not_found",
            Self::ToolSuggestion { .. } => "tool_provided_suggestion",
            Self::UnknownSubcommand(_) => "unknown_subcommand",
            Self::UnknownOption(_) => "unknown_option",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Evidence {
    AptIndex,
    Path,
    TargetOutput,
    AiUnverified,
}

impl Evidence {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::AptIndex => "Verified in this host's APT package index",
            Self::Path => "Verified in this host's executable PATH",
            Self::TargetOutput => "Suggested by the command's own error output",
            Self::AiUnverified => "AI suggestion; not verified on this target",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Candidate {
    pub(super) command: String,
    pub(super) reason: String,
    pub(super) evidence: Evidence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SuggestionResult {
    None,
    Candidates(Vec<Candidate>),
}

pub(super) fn suggest_blocking(
    context: &Context,
    failure: &Failure,
    ai: Option<&AiClient>,
) -> SuggestionResult {
    let local = resolve::local_suggestions(context, failure);
    if !local.is_empty() {
        return SuggestionResult::Candidates(local);
    }
    let Some(ai) = ai else {
        return SuggestionResult::None;
    };
    let candidates = model::ai_suggestions(context, failure, ai);
    if candidates.is_empty() {
        SuggestionResult::None
    } else {
        SuggestionResult::Candidates(candidates)
    }
}
