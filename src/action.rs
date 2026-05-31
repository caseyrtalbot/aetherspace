//! Intent-level actions emitted by the input router.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Quit,
    Render,
    Resize {
        cols: u16,
        rows: u16,
    },
    SendBytes(Vec<u8>),
    RestartFocusedPane,
    CloseFocusedPane,
    #[allow(dead_code)]
    FocusNext,
    #[allow(dead_code)]
    FocusPrev,
    Noop,
}

impl Action {
    pub(crate) fn is_render_request(&self) -> bool {
        matches!(self, Self::Render | Self::Resize { .. })
    }
}
