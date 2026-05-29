//! Zero-dependency file logger. A TUI owns the alternate screen, so anything we
//! print to stdout/stderr is invisible (or corrupts the frame). Diagnostics go to
//! a log file instead: `$XDG_STATE_HOME/aetherspace/aetherspace.log`, falling back
//! to `~/.local/state/aetherspace/`. Lines are `<epoch_millis> <LEVEL> <msg>`.
//!
//! `init()` is best-effort: if the directory or file can't be opened we simply
//! drop logs rather than failing startup. Writes take a short mutex; the volume
//! here (startup, panics, occasional warnings) makes contention a non-issue.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG: OnceLock<Mutex<File>> = OnceLock::new();

/// `$XDG_STATE_HOME` if set and non-empty, else `$HOME/.local/state`. Hand-rolled
/// so we take no `dirs`-style dependency (which would resolve macOS paths wrong).
fn state_home() -> PathBuf {
    match env::var("XDG_STATE_HOME") {
        Ok(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(env::var("HOME").unwrap_or_default()).join(".local/state"),
    }
}

/// Open the log file for appending, creating the directory if needed. Best-effort:
/// any failure leaves `LOG` unset and every `info/warn/error` becomes a no-op.
pub fn init() {
    let dir = state_home().join("aetherspace");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("aetherspace.log"))
    {
        let _ = LOG.set(Mutex::new(file));
    }
}

fn write(level: &str, msg: &str) {
    let Some(lock) = LOG.get() else { return };
    let Ok(mut file) = lock.lock() else { return };
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let _ = writeln!(file, "{millis} {level} {msg}");
}

pub fn info(msg: &str) {
    write("INFO", msg);
}

#[allow(dead_code)] // joins the API surface as warnings land in Phase 3+
pub fn warn(msg: &str) {
    write("WARN", msg);
}

pub fn error(msg: &str) {
    write("ERROR", msg);
}
