//! This frontend's agent/task surface.
//!
//! The domain half — tasks, worktrees, drivers, validation, the native session
//! — lives in [`jterm_core::agent_task`], shared with the sibling terminals.
//! What stays here is diff *rendering*, the one part of the subsystem that
//! touches a toolkit.
pub use jterm_core::agent_task::*;

pub mod diff;
#[allow(unused_imports)] // the panel's siblings are used by the UI layer only
pub use diff::{AgentDiffPanel, AgentDiffState, DiffRequestError};
