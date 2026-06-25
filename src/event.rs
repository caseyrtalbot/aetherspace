//! External events entering the runtime queue.

use std::sync::atomic::{AtomicU64, Ordering};

use ratatui::crossterm::event::{Event as CrosstermEvent, KeyEvent, MouseEvent};

use crate::session::PaneId;
use crate::status::StatusSnapshot;

static NEXT_PROCESS_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaneProcessId {
    pub(crate) pane: PaneId,
    pub(crate) generation: u64,
}

impl PaneProcessId {
    pub(crate) fn new(pane: PaneId, generation: u64) -> Self {
        Self { pane, generation }
    }

    pub(crate) fn for_spawn(pane: PaneId) -> Self {
        let generation = NEXT_PROCESS_GENERATION.fetch_add(1, Ordering::Relaxed);
        Self::new(pane, generation)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize(u16, u16),
    Pty(PaneProcessId),
    ChildExit(PaneProcessId),
    Status(StatusSnapshot),
    #[allow(dead_code)]
    Tick,
    #[allow(dead_code)]
    QuitRequested,
}

impl RuntimeEvent {
    pub(crate) fn from_crossterm(event: CrosstermEvent) -> Option<Self> {
        match event {
            CrosstermEvent::Key(key) => Some(Self::Key(key)),
            CrosstermEvent::Mouse(mouse) => Some(Self::Mouse(mouse)),
            CrosstermEvent::Paste(text) => Some(Self::Paste(text)),
            CrosstermEvent::Resize(cols, rows) => Some(Self::Resize(cols, rows)),
            CrosstermEvent::FocusGained | CrosstermEvent::FocusLost => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_ids_for_reused_pane_ids_do_not_collide() {
        let old_process = PaneProcessId::for_spawn(PaneId(1));
        let replacement_process = PaneProcessId::for_spawn(PaneId(1));

        assert_ne!(old_process, replacement_process);
    }
}
