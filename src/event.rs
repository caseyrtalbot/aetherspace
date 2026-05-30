//! External events entering the runtime queue.

use ratatui::crossterm::event::{Event as CrosstermEvent, KeyEvent, MouseEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Paste(String),
    Resize(u16, u16),
    Pty,
    ChildExit,
    #[allow(dead_code)]
    Status,
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
