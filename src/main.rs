//! Aetherspace Phase 1 boot path.
//!
//! The current app is intentionally small: one shell pane, a guarded terminal
//! boundary, a unified runtime queue, and input routed through actions.

mod action;
mod config;
mod event;
mod input;
mod layout;
mod log;
mod pane;
mod pty;
mod runtime;
mod session;
mod shell;
mod terminal;
mod theme;
mod viewer;
mod xdg;

use anyhow::Result;

fn main() -> Result<()> {
    log::init();
    log::info("aetherspace starting");

    runtime::run(config::Config::load())
}
