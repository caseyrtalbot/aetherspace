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
mod session_store;
mod shell;
mod status;
mod terminal;
mod theme;
mod viewer;
mod xdg;

use anyhow::Result;

/// Env var carrying the aetherspace nesting depth across spawned shells.
const NEST_ENV: &str = "AETHERSPACE_NEST";

fn main() -> Result<()> {
    log::init();
    log::info("aetherspace starting");

    // Depth this process is AT (0 = top-level). Read before we increment so the
    // statusline reflects our own level, not the children's.
    let nest_depth = parse_nest_depth(std::env::var(NEST_ENV).ok().as_deref());
    // Stamp the incremented value so shells spawned later (which inherit our env,
    // see pty.rs new_default_prog) report depth+1. SAFETY: edition 2024 marks
    // set_var unsafe; main is still single-threaded here (before runtime::run
    // spawns any pane, reader, or input thread), so there is no data race.
    unsafe {
        std::env::set_var(NEST_ENV, (nest_depth + 1).to_string());
    }

    let (config, config_warning) = config::Config::load_with_warning();
    runtime::run(config, config_warning, nest_depth)
}

/// Parse the nesting depth from the env var, defaulting to 0 when absent or
/// unparseable. Permissive: a garbage value never refuses launch.
fn parse_nest_depth(raw: Option<&str>) -> u32 {
    raw.and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::parse_nest_depth;

    #[test]
    fn nest_depth_defaults_to_zero_when_absent_or_garbage() {
        assert_eq!(parse_nest_depth(None), 0);
        assert_eq!(parse_nest_depth(Some("")), 0);
        assert_eq!(parse_nest_depth(Some("not-a-number")), 0);
        assert_eq!(parse_nest_depth(Some("-1")), 0);
    }

    #[test]
    fn nest_depth_parses_and_trims() {
        assert_eq!(parse_nest_depth(Some("0")), 0);
        assert_eq!(parse_nest_depth(Some("1")), 1);
        assert_eq!(parse_nest_depth(Some("  3  ")), 3);
    }
}
