//! Intent-level actions emitted by the input router.

use crate::layout::SplitDir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Quit,
    Render,
    Resize { cols: u16, rows: u16 },
    SendBytes(Vec<u8>),
    SplitFocusedPane { dir: SplitDir },
    RestartFocusedPane,
    CloseFocusedPane,
    FocusNext,
    FocusPrev,
    ResizeFocusedPane { delta: i16 },
    ToggleZoomFocusedPane,
    ToggleFloatFocusedPane,
    OpenCommandPalette,
    OpenProjectPalette,
    OpenProjectViewer,
    OpenProjectShell,
    Noop,
}

impl Action {
    pub(crate) fn is_render_request(&self) -> bool {
        matches!(self, Self::Render | Self::Resize { .. })
    }
}
