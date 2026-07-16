#![allow(dead_code)]

pub mod agent;
pub mod ai;
pub mod block_view;
pub mod cli;
mod command_history;
pub mod config;
pub mod config_store;
pub mod git_meta;
pub mod host;
pub mod keybindings;
pub mod logging;
pub mod notebook;
pub mod notify;
mod palette;
pub mod parser;
pub mod pty;
pub mod redact;
mod review_input;
pub mod state;
pub mod terminal;
pub mod workflows;

#[path = "main.rs"]
pub mod app;
mod ui;
