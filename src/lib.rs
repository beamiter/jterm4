#![allow(dead_code)]

pub mod ai;
pub mod block_view;
pub mod cli;
pub mod config;
pub mod git_meta;
pub mod keybindings;
pub mod logging;
pub mod notify;
pub mod parser;
pub mod pty;
pub mod redact;
pub mod state;
pub mod terminal;
pub mod workflows;

#[path = "main.rs"]
pub mod app;
mod ui;
