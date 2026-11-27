//! swarmy: mission control for Claude Code agents.
//!
//! The library holds everything; the `swarmy` binary only parses the command line and calls
//! [`cli::run`].

pub(crate) mod agy;
pub(crate) mod app;
pub(crate) mod claude;
pub mod cli;
pub(crate) mod codex;
pub(crate) mod config;
pub mod core;
pub(crate) mod demo;
pub(crate) mod diag;
pub mod errors;
pub(crate) mod fleet;
pub(crate) mod fsutil;
pub(crate) mod index;
pub(crate) mod limits;
pub(crate) mod live;
pub(crate) mod model;
pub(crate) mod power;
pub(crate) mod procs;
pub(crate) mod statusline;
pub(crate) mod terminal;
pub(crate) mod tui;
pub(crate) mod ui;
pub(crate) mod vault;
pub(crate) mod web;
