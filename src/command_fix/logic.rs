mod classify;
mod resolve;
mod util;

pub(crate) use classify::classify;
pub(crate) use resolve::local_suggestions;
pub(crate) use util::safe_candidate;

#[derive(Clone, Debug)]
pub(crate) struct Context {
    pub(crate) command: String,
    pub(crate) output: String,
    pub(crate) remote: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    AptPackageNotFound(String),
    CommandNotFound(String),
    ToolSuggestion { old: String, new: String },
    UnknownSubcommand(String),
    UnknownOption(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Evidence {
    AptIndex,
    Path,
    TargetOutput,
}

impl Evidence {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::AptIndex => "Verified in this host's APT package index",
            Self::Path => "Verified in this host's executable PATH",
            Self::TargetOutput => "Suggested by the command's own error output",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) command: String,
    pub(crate) reason: String,
    pub(crate) evidence: Evidence,
}
