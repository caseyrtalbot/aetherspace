//! Restartable shell pane facade.
//!
//! `pty.rs` owns process handles and threads. This module owns the vt100 parser,
//! shell lifecycle state, and stale-process generation checks.

use std::sync::mpsc::Sender;

use anyhow::Result;
use tui_term::vt100;

use crate::event::{PaneProcessId, RuntimeEvent};
use crate::pty::{PtyDrainBudget, PtyProcess};
use crate::session::{PaneId, ShellSpec};

pub(crate) struct Shell {
    parser: vt100::Parser,
    process: Option<PtyProcess>,
    pane_id: PaneId,
    generation: u64,
    rows: u16,
    cols: u16,
    scrollback: usize,
    state: ShellState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ShellState {
    Running,
    Exited(String),
}

impl Shell {
    pub(crate) fn spawn(
        pane_id: PaneId,
        spec: &ShellSpec,
        rows: u16,
        cols: u16,
        scrollback: usize,
        notify: Sender<RuntimeEvent>,
    ) -> Result<Self> {
        let (rows, cols) = normalize_size(rows, cols);
        let generation = 0;
        let process = PtyProcess::spawn(
            PaneProcessId::new(pane_id, generation),
            spec,
            rows,
            cols,
            notify,
        )?;
        Ok(Self {
            parser: vt100::Parser::new(rows, cols, scrollback),
            process: Some(process),
            pane_id,
            generation,
            rows,
            cols,
            scrollback,
            state: ShellState::Running,
        })
    }

    pub(crate) fn restart(
        &mut self,
        pane_id: PaneId,
        spec: &ShellSpec,
        scrollback: usize,
        notify: Sender<RuntimeEvent>,
    ) -> Result<()> {
        self.terminate();
        self.generation += 1;
        self.pane_id = pane_id;
        self.scrollback = scrollback;
        let id = self.current_process_id();
        let process = PtyProcess::spawn(id, spec, self.rows, self.cols, notify)?;
        self.parser = vt100::Parser::new(self.rows, self.cols, self.scrollback);
        self.process = Some(process);
        self.state = ShellState::Running;
        Ok(())
    }

    pub(crate) fn process_pending(&mut self, id: PaneProcessId) -> bool {
        if id != self.current_process_id() {
            return false;
        }
        if let Some(process) = &mut self.process {
            return process.process_pending(PtyDrainBudget::interactive(), |chunk| {
                self.parser.process(chunk);
            });
        }
        false
    }

    pub(crate) fn mark_child_exit(&mut self, id: PaneProcessId) {
        if id != self.current_process_id() {
            return;
        }
        let _ = self.process_pending(id);
        let status = self
            .process
            .as_ref()
            .and_then(PtyProcess::exit_status_text)
            .unwrap_or_else(|| "exited".to_string());
        self.process = None;
        self.state = ShellState::Exited(status);
    }

    pub(crate) fn is_running(&self) -> bool {
        matches!(self.state, ShellState::Running)
            && self
                .process
                .as_ref()
                .map(PtyProcess::is_alive)
                .unwrap_or(false)
    }

    pub(crate) fn terminate(&mut self) {
        if let Some(process) = &mut self.process {
            process.terminate();
        }
        self.process = None;
    }

    pub(crate) fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = normalize_size(rows, cols);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        if let Some(process) = &mut self.process {
            process.resize(rows, cols);
        }
        self.parser.screen_mut().set_size(rows, cols);
    }

    pub(crate) fn send_input(&mut self, bytes: &[u8]) {
        if let Some(process) = &mut self.process {
            process.send_input(bytes);
        }
    }

    pub(crate) fn title(&self, base: &str) -> String {
        match &self.state {
            ShellState::Running => base.to_string(),
            ShellState::Exited(status) => format!("{base}  exited: {status}"),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn scroll_by(&mut self, delta: i64) {
        let target = scroll_delta(self.parser.screen().scrollback(), delta);
        self.parser.screen_mut().set_scrollback(target);
    }

    #[allow(dead_code)]
    pub(crate) fn scroll_to_bottom(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    #[allow(dead_code)]
    pub(crate) fn scrollback_offset(&self) -> usize {
        self.parser.screen().scrollback()
    }

    fn current_process_id(&self) -> PaneProcessId {
        PaneProcessId::new(self.pane_id, self.generation)
    }
}

fn normalize_size(rows: u16, cols: u16) -> (u16, u16) {
    (rows.max(1), cols.max(1))
}

fn scroll_delta(current: usize, delta: i64) -> usize {
    (current as i64 + delta).max(0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_delta_moves_into_history() {
        assert_eq!(scroll_delta(0, 10), 10);
        assert_eq!(scroll_delta(5, 3), 8);
    }

    #[test]
    fn scroll_delta_clamps_at_live_bottom() {
        assert_eq!(scroll_delta(3, -10), 0);
        assert_eq!(scroll_delta(0, -1), 0);
    }

    #[test]
    fn shell_size_never_reaches_zero() {
        assert_eq!(normalize_size(0, 0), (1, 1));
        assert_eq!(normalize_size(22, 80), (22, 80));
    }
}
