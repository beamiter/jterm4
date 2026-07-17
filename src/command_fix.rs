//! Deterministic, review-first correction helpers for failed shell commands.
//!
//! UI policy stays in `ui::command_correction`; this module only classifies
//! typo-shaped failures, resolves target-verified local candidates, and checks
//! that model-generated replacements do not silently expand authority.

mod logic;

pub(crate) use logic::{
    classify, local_suggestions, safe_candidate, Candidate, Context, Failure,
};
