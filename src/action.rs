//! Intent-level actions emitted by the input router.

use crate::layout::SplitDir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Quit,
    Render,
    Resize {
        cols: u16,
        rows: u16,
    },
    SendBytes(Vec<u8>),
    SplitFocusedPane {
        dir: SplitDir,
    },
    RestartFocusedPane,
    CloseFocusedPane,
    FocusNext,
    FocusPrev,
    ResizeFocusedPane {
        delta: i16,
    },
    ToggleZoomFocusedPane,
    ToggleFloatFocusedPane,
    ToggleStack,
    ToggleCompactChrome,
    OpenHelp,
    OpenCommandPalette,
    OpenProjectPalette,
    OpenProjectViewer,
    OpenProjectShell,
    EditScrollback,
    ReloadConfig,
    /// Viewer incremental find: the live query as it is typed (re-search each edit).
    ViewerFind(String),
    /// Lock in the current find (exit the input loop; matches persist for n/N).
    ViewerFindCommit,
    /// Abandon the find and restore the pre-find scroll position.
    ViewerFindCancel,
    Noop,
}

impl Action {
    pub(crate) fn is_render_request(&self) -> bool {
        matches!(self, Self::Render | Self::Resize { .. })
    }
}
