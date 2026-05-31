//! External events entering the runtime queue.

use ratatui::crossterm::event::{Event as CrosstermEvent, KeyEvent, MouseEvent};

use crate::session::PaneId;
use crate::status::StatusSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaneProcessId {
    pub(crate) pane: PaneId,
    pub(crate) generation: u64,
}

impl PaneProcessId {
    pub(crate) fn new(pane: PaneId, generation: u64) -> Self {
        Self { pane, generation }
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
