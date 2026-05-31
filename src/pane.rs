//! Live pane runtimes.
//!
//! Pane specs live in `session.rs`; this layer binds those specs to runtime-only
//! processes and parser state.

use std::sync::mpsc::Sender;

use anyhow::{Result, bail};
use tui_term::vt100;

use crate::event::{PaneProcessId, RuntimeEvent};
use crate::session::{PaneId, PaneKind, PaneSpec};
use crate::shell::Shell;

pub(crate) enum PaneRuntime {
    Shell(Shell),
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
            PaneKind::Shell(shell) => Ok(Self::Shell(Shell::spawn(
                spec.id, shell, rows, cols, scrollback, notify,
            )?)),
        }
    }

    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        match self {
            Self::Shell(shell) => shell.resize(rows, cols),
        }
    }

    pub(crate) fn process_pending(&mut self, id: PaneProcessId) {
        match self {
            Self::Shell(shell) => shell.process_pending(id),
        }
    }

    pub(crate) fn mark_child_exit(&mut self, id: PaneProcessId) {
        match self {
            Self::Shell(shell) => shell.mark_child_exit(id),
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
        }
    }

    pub(crate) fn terminate(&mut self) {
        match self {
            Self::Shell(shell) => shell.terminate(),
        }
    }

    pub(crate) fn send_input(&mut self, bytes: &[u8]) {
        match self {
            Self::Shell(shell) => shell.send_input(bytes),
        }
    }

    pub(crate) fn shell_screen(&self) -> &vt100::Screen {
        match self {
            Self::Shell(shell) => shell.screen(),
        }
    }

    pub(crate) fn title(&self, spec: &PaneSpec) -> String {
        match self {
            Self::Shell(shell) => shell.title(&spec.title),
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        match self {
            Self::Shell(shell) => shell.is_running(),
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
