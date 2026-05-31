//! Serializable session model.
//!
//! A session describes durable pane intent: pane ids, pane kind, cwd/title, focus,
//! and the next id allocator. It deliberately contains no PTY handles, parser
//! state, terminal rectangles, or threads.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct PaneId(pub(crate) u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Session {
    panes: Vec<PaneSpec>,
    focused: Option<PaneId>,
    next_pane_id: u64,
}

impl Session {
    pub(crate) fn single_shell(cwd: PathBuf) -> Self {
        let id = PaneId(0);
        Self {
            panes: vec![PaneSpec::shell(id, cwd)],
            focused: Some(id),
            next_pane_id: 1,
        }
    }

    pub(crate) fn pane_specs(&self) -> &[PaneSpec] {
        &self.panes
    }

    pub(crate) fn focused(&self) -> Option<PaneId> {
        self.focused
    }

    pub(crate) fn focused_spec(&self) -> Option<&PaneSpec> {
        self.focused.and_then(|id| self.spec(id))
    }

    pub(crate) fn spec(&self, id: PaneId) -> Option<&PaneSpec> {
        self.panes.iter().find(|spec| spec.id == id)
    }

    pub(crate) fn close_pane(&mut self, id: PaneId) -> bool {
        let Some(pos) = self.panes.iter().position(|spec| spec.id == id) else {
            return !self.panes.is_empty();
        };
        self.panes.remove(pos);
        if self.focused == Some(id) {
            self.focused = self.panes.first().map(|spec| spec.id);
        }
        !self.panes.is_empty()
    }

    #[allow(dead_code)]
    pub(crate) fn allocate_shell(&mut self, cwd: PathBuf) -> PaneId {
        let id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        self.panes.push(PaneSpec::shell(id, cwd));
        self.focused = Some(id);
        id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PaneSpec {
    pub(crate) id: PaneId,
    pub(crate) title: String,
    pub(crate) kind: PaneKind,
}

impl PaneSpec {
    fn shell(id: PaneId, cwd: PathBuf) -> Self {
        Self {
            id,
            title: "SHELL".to_string(),
            kind: PaneKind::Shell(ShellSpec { cwd }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PaneKind {
    Shell(ShellSpec),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ShellSpec {
    pub(crate) cwd: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_shell_session_has_serializable_spec_not_runtime_state() {
        let session = Session::single_shell(PathBuf::from("/work/project"));
        assert_eq!(session.focused(), Some(PaneId(0)));
        assert_eq!(session.pane_specs().len(), 1);
        assert_eq!(session.pane_specs()[0].title, "SHELL");
        assert_eq!(session.next_pane_id, 1);
    }

    #[test]
    fn pane_ids_are_monotonic_and_not_reused() {
        let mut session = Session::single_shell(PathBuf::from("/work/one"));
        assert_eq!(
            session.allocate_shell(PathBuf::from("/work/two")),
            PaneId(1)
        );
        assert!(session.close_pane(PaneId(1)));
        assert_eq!(
            session.allocate_shell(PathBuf::from("/work/three")),
            PaneId(2)
        );
    }

    #[test]
    fn closing_focused_pane_moves_focus_or_empties_session() {
        let mut session = Session::single_shell(PathBuf::from("/work/one"));
        session.allocate_shell(PathBuf::from("/work/two"));

        assert!(session.close_pane(PaneId(1)));
        assert_eq!(session.focused(), Some(PaneId(0)));

        assert!(!session.close_pane(PaneId(0)));
        assert_eq!(session.focused(), None);
    }
}
