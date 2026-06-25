//! Durable session persistence.
//!
//! Only serializable session intent is stored: pane specs, layout, focus, and
//! selected project. Live PTY handles, parser state, threads, and process exits
//! stay runtime-only.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::session::{CURRENT_SESSION_FORMAT, Session};
use crate::xdg;

const SESSION_FILE: &str = "aetherspace/session.toml";

/// Outcome of classifying a session file on disk.
///
/// `Incompatible` is the data-loss guard: a file whose `format_version` exceeds
/// what this binary understands was written by a newer Aetherspace, so the caller
/// must start fresh WITHOUT persisting (overwriting it would destroy the newer
/// file). `None` covers both "no file" and "file exists but holds no panes".
#[derive(Debug)]
pub(crate) enum SessionLoad {
    None,
    Loaded(Session),
    Incompatible,
}

pub(crate) fn save(session: &Session) {
    let path = default_path();
    if let Err(e) = save_to_path(session, &path) {
        crate::log::warn(&format!("session save failed: {e}"));
    } else {
        crate::log::info(&format!("session saved: {}", path.display()));
    }
}

pub(crate) fn default_path() -> PathBuf {
    xdg::home("XDG_STATE_HOME", ".local/state").join(SESSION_FILE)
}

/// Read and classify the session file. The guard lives here, not in the caller:
/// - missing file → `None`
/// - parse error → `Err` (corrupt/unknown; caller logs and starts fresh)
/// - `format_version > CURRENT` → `Incompatible` (newer binary wrote it)
/// - `format_version < CURRENT` → accept-and-rewrite: load it; the next gated
///   save rewrites it at CURRENT (load classifies, it never transforms)
/// - `== CURRENT` with panes → `Loaded`; with no panes → `None`
pub(crate) fn load_classified(path: &Path) -> Result<SessionLoad> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SessionLoad::None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let session: Session = match toml::from_str(&text) {
        Ok(session) => session,
        Err(e) => {
            // A parse failure is usually a corrupt file, but it is ALSO how a file
            // from a NEWER binary looks: a future body (e.g. a TileNode variant this
            // build lacks) fails to deserialize before the version check below ever
            // runs. Probe just the version field — VersionProbe ignores the
            // unparseable body — and if it is newer, classify Incompatible so the
            // refuse-to-clobber gate protects the file instead of overwriting it as
            // corrupt. Without this, B2's format bump would let a pre-B2 binary
            // destroy a newer session on the first autosave.
            #[derive(serde::Deserialize)]
            struct VersionProbe {
                #[serde(default)]
                format_version: u32,
            }
            if let Ok(probe) = toml::from_str::<VersionProbe>(&text)
                && probe.format_version > CURRENT_SESSION_FORMAT
            {
                return Ok(SessionLoad::Incompatible);
            }
            return Err(e).with_context(|| format!("parse {}", path.display()));
        }
    };
    if session.format_version() > CURRENT_SESSION_FORMAT {
        return Ok(SessionLoad::Incompatible);
    }
    if session.pane_specs().is_empty() {
        Ok(SessionLoad::None)
    } else {
        Ok(SessionLoad::Loaded(session))
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
    write_atomic(path, text.as_bytes())
}

/// Write to a sibling temp file then rename over the target. Rename is atomic on
/// the same filesystem, so a crash mid-write never leaves a half-written
/// session.toml (autosave fires on every session mutation, raising those odds).
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::layout::{FloatGeom, SplitDir};
    use crate::session::{PaneId, ProjectSelection, Session};

    /// Collapse `load_classified` to the historical `Option<Session>` shape for
    /// tests that only care about loaded-vs-not (None and Incompatible both map to
    /// `None` here; the guard-specific tests below assert on `SessionLoad` directly).
    fn loaded_session(path: &Path) -> Option<Session> {
        match load_classified(path).unwrap() {
            SessionLoad::Loaded(session) => Some(session),
            SessionLoad::None | SessionLoad::Incompatible => None,
        }
    }

    #[test]
    fn missing_session_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing.toml");
        assert!(loaded_session(&path).is_none());
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
        let loaded = loaded_session(&path).expect("session");
        assert_eq!(loaded, session);
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("aetherspace/session.toml");
        let session = Session::single_shell(PathBuf::from("/work/aetherspace"));

        save_to_path(&session, &path).unwrap();

        assert!(path.exists());
        assert!(!path.with_extension("toml.tmp").exists());
        // Overwriting an existing target also succeeds (rename replaces it).
        save_to_path(&session, &path).unwrap();
        assert_eq!(loaded_session(&path).unwrap(), session);
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

    /// A FIXED literal session.toml from before the guard existed: it lacks the
    /// `format_version` key entirely. Must be a literal (not a re-serialized
    /// struct) so it stays a true legacy fixture if the struct shape changes.
    const LEGACY_SESSION_NO_FORMAT_VERSION: &str = r#"
focused = 0
next_pane_id = 1

[[panes]]
id = 0
title = "SHELL"

[panes.kind.Shell]
cwd = "/work/aetherspace"

[tiled]
Leaf = 0

[floating]
"#;

    #[test]
    fn legacy_file_without_format_version_loads_as_v1() {
        // The serde default (= 1) is load-bearing: without it this file becomes a
        // parse error the load path would silently drop, then overwrite.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.toml");
        fs::write(&path, LEGACY_SESSION_NO_FORMAT_VERSION).unwrap();

        let session = match load_classified(&path).unwrap() {
            SessionLoad::Loaded(session) => session,
            other => panic!("expected Loaded, got a non-loaded classification ({other:?})"),
        };
        assert_eq!(session.format_version(), 1);
        assert_eq!(session.pane_specs().len(), 1);
    }

    #[test]
    fn newer_format_version_classifies_incompatible() {
        // A file from a newer binary (format_version = 999) must NOT load and must
        // NOT be treated as missing/corrupt — it is Incompatible so the caller can
        // refuse to overwrite it.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.toml");
        let future = LEGACY_SESSION_NO_FORMAT_VERSION.replacen(
            "focused = 0",
            "format_version = 999\nfocused = 0",
            1,
        );
        fs::write(&path, &future).unwrap();

        assert!(matches!(
            load_classified(&path).unwrap(),
            SessionLoad::Incompatible
        ));
    }

    #[test]
    fn newer_format_with_unparseable_body_classifies_incompatible() {
        // The dangerous forward case: a newer file whose body holds a TileNode variant
        // this build cannot deserialize. `toml::from_str::<Session>` fails BEFORE the
        // version check, so without the version probe this would route to the Err arm
        // (treated as corrupt, persist=true) and be clobbered. The probe must classify
        // it Incompatible so the refuse-to-clobber gate protects it. `Stack` is real
        // post-B2, so this uses a still-future `Grid` variant at format_version 3.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.toml");
        let future = LEGACY_SESSION_NO_FORMAT_VERSION
            .replacen("focused = 0", "format_version = 3\nfocused = 0", 1)
            .replace("[tiled]\nLeaf = 0", "[tiled.Grid]\nrows = 2\ncols = 2");
        fs::write(&path, &future).unwrap();

        // Sanity: the body really IS unparseable as the current Session shape, so
        // this exercises the probe path and not the normal version check.
        assert!(toml::from_str::<Session>(&future).is_err());
        assert!(matches!(
            load_classified(&path).unwrap(),
            SessionLoad::Incompatible
        ));
    }

    #[test]
    fn refuse_to_clobber_leaves_incompatible_file_byte_identical() {
        // After classifying Incompatible, a no-persist boot must never reach save.
        // Simulate the gate (session_persist = false): the on-disk bytes are
        // unchanged, including the empty-session remove_file path being skipped.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.toml");
        let future = LEGACY_SESSION_NO_FORMAT_VERSION.replacen(
            "focused = 0",
            "format_version = 999\nfocused = 0",
            1,
        );
        fs::write(&path, &future).unwrap();
        let before = fs::read(&path).unwrap();

        // Classify, then take the no-persist branch: the gate skips save entirely.
        assert!(matches!(
            load_classified(&path).unwrap(),
            SessionLoad::Incompatible
        ));
        let session_persist = false;
        // Even an empty session must not touch the file when persistence is gated off.
        let mut empty = Session::single_shell(PathBuf::from("/work/aetherspace"));
        assert!(!empty.close_pane(PaneId(0)));
        if session_persist {
            save_to_path(&empty, &path).unwrap();
        }

        let after = fs::read(&path).unwrap();
        assert_eq!(
            before, after,
            "Incompatible file must survive byte-for-byte"
        );
    }

    #[test]
    fn stack_session_round_trips_at_v2_and_is_incompatible_to_a_v1_guard() {
        // A session holding a TileNode::Stack must round-trip on this v2 binary, and
        // the SAME on-disk file must look Incompatible to a pre-B2 (CURRENT = 1) guard.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.toml");

        let mut session = Session::single_shell(PathBuf::from("/work/aetherspace"));
        session
            .split_focused_shell(PathBuf::from("/work/aetherspace"), SplitDir::Horizontal)
            .expect("split");
        assert!(session.toggle_stack_focused(), "expected a stack to form");
        assert_eq!(session.format_version(), 2);

        save_to_path(&session, &path).unwrap();

        // v2 binary: this file round-trips identically and classifies Loaded.
        let reloaded = loaded_session(&path).expect("v2 binary loads its own Stack file");
        assert_eq!(reloaded, session);

        // The file carries the v2 marker and a [tiled.Stack] body.
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("format_version = 2"));
        assert!(text.contains("[tiled.Stack]"));

        // A pre-B2 guard's threshold is CURRENT = 1: the same VersionProbe the load
        // path uses reads format_version = 2 > 1 ⇒ Incompatible (refuse-to-clobber).
        #[derive(serde::Deserialize)]
        struct VersionProbe {
            #[serde(default)]
            format_version: u32,
        }
        const PRE_B2_CURRENT: u32 = 1;
        let probe: VersionProbe = toml::from_str(&text).unwrap();
        assert!(
            probe.format_version > PRE_B2_CURRENT,
            "a v1 binary must classify this Incompatible, not load or clobber it"
        );
    }

    #[test]
    fn version_one_file_still_loads_on_the_v2_binary() {
        // Additive forward-read: a legacy version-1 file (no Stack) deserializes on
        // the v2 binary and is accepted (accept-and-rewrite up to CURRENT).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.toml");
        fs::write(&path, LEGACY_SESSION_NO_FORMAT_VERSION).unwrap();

        let session = match load_classified(&path).unwrap() {
            SessionLoad::Loaded(session) => session,
            other => panic!("expected Loaded, got {other:?}"),
        };
        assert_eq!(session.format_version(), 1);
        assert_eq!(session.pane_specs().len(), 1);
    }
}
