//! Durable session persistence.
//!
//! Only serializable session intent is stored: pane specs, layout, focus, and
//! selected project. Live PTY handles, parser state, threads, and process exits
//! stay runtime-only.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::session::Session;
use crate::xdg;

const SESSION_FILE: &str = "aetherspace/session.toml";

pub(crate) fn load() -> Option<Session> {
    match load_from_path(&default_path()) {
        Ok(session) => session,
        Err(e) => {
            crate::log::warn(&format!("session load skipped: {e}"));
            None
        }
    }
}

pub(crate) fn save(session: &Session) {
    let path = default_path();
    if let Err(e) = save_to_path(session, &path) {
        crate::log::warn(&format!("session save failed: {e}"));
    } else {
        crate::log::info(&format!("session saved: {}", path.display()));
    }
}

fn default_path() -> PathBuf {
    xdg::home("XDG_STATE_HOME", ".local/state").join(SESSION_FILE)
}

fn load_from_path(path: &Path) -> Result<Option<Session>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let session: Session =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    if session.pane_specs().is_empty() {
        Ok(None)
    } else {
        Ok(Some(session))
    }
}

fn save_to_path(session: &Session, path: &Path) -> Result<()> {
    if session.pane_specs().is_empty() {
        if let Err(e) = fs::remove_file(path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(e).with_context(|| format!("remove {}", path.display()));
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(session).context("serialize session")?;
    fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::layout::{FloatGeom, SplitDir};
    use crate::session::{PaneId, ProjectSelection, Session};

    #[test]
    fn missing_session_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing.toml");
        assert!(load_from_path(&path).unwrap().is_none());
    }

    #[test]
    fn round_trips_session_intent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/aetherspace/session.toml");
        let mut session = Session::single_shell_for_project(
            PathBuf::from("/work/aetherspace"),
            Some(ProjectSelection {
                name: "aetherspace".into(),
                path: PathBuf::from("/work/aetherspace"),
            }),
        );
        session
            .split_focused_shell(PathBuf::from("/work/aetherspace"), SplitDir::Horizontal)
            .expect("split");
        assert!(session.toggle_float_focused(FloatGeom {
            x: 2,
            y: 3,
            width: 40,
            height: 12,
        }));

        save_to_path(&session, &path).unwrap();
        let loaded = load_from_path(&path).unwrap().expect("session");
        assert_eq!(loaded, session);
    }

    #[test]
    fn empty_session_removes_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.toml");
        let mut session = Session::single_shell(PathBuf::from("/work/aetherspace"));
        fs::write(&path, "stale").unwrap();

        assert!(!session.close_pane(PaneId(0)));
        save_to_path(&session, &path).unwrap();
        assert!(!path.exists());
    }
}
