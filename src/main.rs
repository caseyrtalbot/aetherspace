//! Aetherspace boot path.
//!
//! `main` stays intentionally thin: initialize logging, load config, and hand
//! control to the guarded Ratatui runtime.

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
