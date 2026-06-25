//! Live pane runtimes.
//!
//! Pane specs live in `session.rs`; this layer binds those specs to runtime-only
//! processes and parser state.

use std::sync::mpsc::Sender;

use anyhow::{Result, bail};
use ratatui::text::Line;
use tui_term::vt100::{self, MouseProtocolEncoding, MouseProtocolMode};

use crate::event::{PaneProcessId, RuntimeEvent};
use crate::session::{PaneId, PaneKind, PaneSpec};
use crate::shell::Shell;
use crate::viewer::Viewer;

pub(crate) enum PaneRuntime {
    Shell(Box<Shell>),
    Viewer(Viewer),
}

impl PaneRuntime {
    pub(crate) fn spawn(
        spec: &PaneSpec,
        rows: u16,
        cols: u16,
        scrollback: usize,
        notify: Sender<RuntimeEvent>,
    ) -> Result<Self> {
        match &spec.kind {
            PaneKind::Shell(shell) => Ok(Self::Shell(Box::new(Shell::spawn(
                spec.id, shell, rows, cols, scrollback, notify,
            )?))),
            PaneKind::Viewer(viewer) => Ok(Self::Viewer(Viewer::open(viewer))),
        }
    }

    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        match self {
            Self::Shell(shell) => shell.resize(rows, cols),
            Self::Viewer(_) => {
                let _ = (rows, cols);
            }
        }
    }

    pub(crate) fn process_pending(&mut self, id: PaneProcessId) -> bool {
        match self {
            Self::Shell(shell) => shell.process_pending(id),
            Self::Viewer(_) => false,
        }
    }

    pub(crate) fn mark_child_exit(&mut self, id: PaneProcessId) {
        match self {
            Self::Shell(shell) => shell.mark_child_exit(id),
            Self::Viewer(_) => {
                let _ = id;
            }
        }
    }

    pub(crate) fn restart(
        &mut self,
        spec: &PaneSpec,
        scrollback: usize,
        notify: Sender<RuntimeEvent>,
    ) -> Result<()> {
        match (self, &spec.kind) {
            (Self::Shell(shell), PaneKind::Shell(shell_spec)) => {
                shell.restart(spec.id, shell_spec, scrollback, notify)
            }
            (Self::Viewer(viewer), PaneKind::Viewer(viewer_spec)) => {
                let _ = (scrollback, notify);
                viewer.reload(viewer_spec);
                Ok(())
            }
            _ => bail!("pane runtime kind does not match session spec"),
        }
    }

    pub(crate) fn terminate(&mut self) {
        match self {
            Self::Shell(shell) => shell.terminate(),
            Self::Viewer(_) => {}
        }
    }

    pub(crate) fn send_input(&mut self, bytes: &[u8]) {
        match self {
            Self::Shell(shell) => shell.send_input(bytes),
            Self::Viewer(_) => {
                let _ = bytes;
            }
        }
    }

    pub(crate) fn shell_screen(&self) -> Option<&vt100::Screen> {
        match self {
            Self::Shell(shell) => Some(shell.screen()),
            Self::Viewer(_) => None,
        }
    }

    /// Plain-text visible screen of a shell pane, `None` for viewers. Feeds the
    /// EditScrollback (`$EDITOR`) scratch buffer.
    pub(crate) fn shell_screen_text(&self) -> Option<String> {
        match self {
            Self::Shell(shell) => Some(shell.screen_text()),
            Self::Viewer(_) => None,
        }
    }

    pub(crate) fn title(&self, spec: &PaneSpec) -> String {
        match self {
            Self::Shell(shell) => shell.title(&spec.title),
            Self::Viewer(_) => spec.title.clone(),
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        match self {
            Self::Shell(shell) => shell.is_running(),
            Self::Viewer(_) => false,
        }
    }

    pub(crate) fn is_viewer(&self) -> bool {
        matches!(self, Self::Viewer(_))
    }

    pub(crate) fn child_mouse_protocol(
        &self,
    ) -> Option<(MouseProtocolMode, MouseProtocolEncoding)> {
        match self {
            Self::Shell(shell) => Some(shell.mouse_protocol()),
            Self::Viewer(_) => None,
        }
    }

    pub(crate) fn scroll_viewer_by(&mut self, delta: isize, viewport_rows: u16) -> bool {
        match self {
            Self::Shell(_) => false,
            Self::Viewer(viewer) => {
                viewer.scroll_by(delta, viewport_rows);
                true
            }
        }
    }

    pub(crate) fn scroll_viewer_home(&mut self) -> bool {
        match self {
            Self::Shell(_) => false,
            Self::Viewer(viewer) => {
                viewer.scroll_home();
                true
            }
        }
    }

    pub(crate) fn scroll_viewer_end(&mut self, viewport_rows: u16) -> bool {
        match self {
            Self::Shell(_) => false,
            Self::Viewer(viewer) => {
                viewer.scroll_end(viewport_rows);
                true
            }
        }
    }

    pub(crate) fn viewer_lines(&self, height: u16) -> Option<Vec<Line<'static>>> {
        match self {
            Self::Shell(_) => None,
            Self::Viewer(viewer) => Some(viewer.visible_lines(height)),
        }
    }

    pub(crate) fn viewer_status(&self) -> Option<String> {
        match self {
            Self::Shell(_) => None,
            Self::Viewer(viewer) => Some(viewer.status()),
        }
    }

    pub(crate) fn begin_viewer_find(&mut self) -> bool {
        match self {
            Self::Shell(_) => false,
            Self::Viewer(viewer) => {
                viewer.begin_find();
                true
            }
        }
    }

    pub(crate) fn set_viewer_query(&mut self, query: &str, viewport_rows: u16) -> bool {
        match self {
            Self::Shell(_) => false,
            Self::Viewer(viewer) => {
                viewer.set_query(query, viewport_rows);
                true
            }
        }
    }

    pub(crate) fn viewer_find_next(&mut self, viewport_rows: u16) -> bool {
        match self {
            Self::Shell(_) => false,
            Self::Viewer(viewer) => {
                viewer.next_match(viewport_rows);
                true
            }
        }
    }

    pub(crate) fn viewer_find_prev(&mut self, viewport_rows: u16) -> bool {
        match self {
            Self::Shell(_) => false,
            Self::Viewer(viewer) => {
                viewer.prev_match(viewport_rows);
                true
            }
        }
    }

    pub(crate) fn cancel_viewer_find(&mut self) -> bool {
        match self {
            Self::Shell(_) => false,
            Self::Viewer(viewer) => {
                viewer.cancel_find();
                true
            }
        }
    }

    pub(crate) fn clear_viewer_find(&mut self) -> bool {
        match self {
            Self::Shell(_) => false,
            Self::Viewer(viewer) => {
                viewer.clear_find();
                true
            }
        }
    }

    pub(crate) fn viewer_find_status(&self) -> Option<String> {
        match self {
            Self::Shell(_) => None,
            Self::Viewer(viewer) => viewer.find_status(),
        }
    }

    pub(crate) fn viewer_find_query(&self) -> Option<String> {
        match self {
            Self::Shell(_) => None,
            Self::Viewer(viewer) => viewer.find_query().map(str::to_string),
        }
    }
}

pub(crate) fn ensure_spec_matches_runtime(
    id: PaneId,
    spec: Option<&PaneSpec>,
) -> Result<&PaneSpec> {
    let Some(spec) = spec else {
        bail!("pane runtime {id:?} has no session spec");
    };
    Ok(spec)
}
