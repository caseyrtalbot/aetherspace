//! Serializable session model.
//!
//! A session describes durable pane intent: pane ids, pane kind, cwd/title, focus,
//! and the next id allocator. It deliberately contains no PTY handles, parser
//! state, terminal rectangles, or threads.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::layout::{
    CloseOutcome, FloatGeom, SplitDir, TileNode, close_leaf, contains_leaf, dock_leaf, leaves,
    nudge_ratio, split_leaf,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct PaneId(pub(crate) u64);

/// The session-format version this binary writes and the highest it can read.
/// Bumped when the serialized shape changes in a way an older binary cannot
/// faithfully read (B2 raises this to 2 for `TileNode::Stack`). The load guard in
/// `session_store` refuses to overwrite a file whose `format_version` exceeds this.
pub(crate) const CURRENT_SESSION_FORMAT: u32 = 1;

/// `#[serde(default)]` target for `format_version`. Returns 1 (NOT 0) on purpose:
/// every `session.toml` written before the guard lacks the key and is a legitimate
/// version-1 file, so a missing field must classify as 1, the pre-guard baseline.
fn session_format_v1() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Session {
    /// Serialized FIRST so a leading scalar precedes the `[floating.N]` tables.
    /// The default is load-bearing: without it every pre-guard file becomes a
    /// parse error the load path silently drops, triggering the exact data loss
    /// the guard exists to prevent.
    #[serde(default = "session_format_v1")]
    format_version: u32,
    panes: Vec<PaneSpec>,
    focused: Option<PaneId>,
    next_pane_id: u64,
    tiled: Option<TileNode>,
    floating: std::collections::BTreeMap<PaneId, FloatGeom>,
    zoomed: Option<PaneId>,
    selected_project: Option<ProjectSelection>,
}

impl Session {
    #[cfg(test)]
    pub(crate) fn single_shell(cwd: PathBuf) -> Self {
        Self::single_shell_for_project(cwd, None)
    }

    pub(crate) fn single_shell_for_project(
        cwd: PathBuf,
        selected_project: Option<ProjectSelection>,
    ) -> Self {
        let id = PaneId(0);
        Self {
            format_version: CURRENT_SESSION_FORMAT,
            panes: vec![PaneSpec::shell(id, cwd)],
            focused: Some(id),
            next_pane_id: 1,
            tiled: Some(TileNode::Leaf(id)),
            floating: std::collections::BTreeMap::new(),
            zoomed: None,
            selected_project,
        }
    }

    pub(crate) fn format_version(&self) -> u32 {
        self.format_version
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

    pub(crate) fn focused_shell_cwd(&self) -> Option<PathBuf> {
        let spec = self.focused_spec()?;
        match &spec.kind {
            PaneKind::Shell(shell) => Some(shell.cwd.clone()),
            PaneKind::Viewer(_) => None,
        }
    }

    pub(crate) fn selected_project(&self) -> Option<&ProjectSelection> {
        self.selected_project.as_ref()
    }

    pub(crate) fn select_project(&mut self, project: ProjectSelection) {
        self.selected_project = Some(project);
    }

    pub(crate) fn spec(&self, id: PaneId) -> Option<&PaneSpec> {
        self.panes.iter().find(|spec| spec.id == id)
    }

    pub(crate) fn tiled(&self) -> Option<&TileNode> {
        self.tiled.as_ref()
    }

    pub(crate) fn floating(&self) -> &std::collections::BTreeMap<PaneId, FloatGeom> {
        &self.floating
    }

    pub(crate) fn zoomed(&self) -> Option<PaneId> {
        self.zoomed
    }

    pub(crate) fn is_floating(&self, id: PaneId) -> bool {
        self.floating.contains_key(&id)
    }

    pub(crate) fn is_tiled(&self, id: PaneId) -> bool {
        self.tiled
            .as_ref()
            .map(|tree| contains_leaf(tree, id))
            .unwrap_or(false)
    }

    pub(crate) fn split_focused_shell(&mut self, cwd: PathBuf, dir: SplitDir) -> Option<PaneSpec> {
        let target = self.focused?;
        if self.is_floating(target) {
            return None;
        }
        let tree = self.tiled.as_mut()?;
        let id = PaneId(self.next_pane_id);
        if !split_leaf(tree, target, id, dir) {
            return None;
        }
        self.next_pane_id += 1;
        let spec = PaneSpec::shell(id, cwd);
        self.panes.push(spec.clone());
        self.focused = Some(id);
        self.zoomed = None;
        Some(spec)
    }

    pub(crate) fn focus_next(&mut self) {
        self.focus_by(1);
    }

    pub(crate) fn focus_prev(&mut self) {
        self.focus_by(-1);
    }

    pub(crate) fn focus_pane(&mut self, id: PaneId) -> bool {
        if self.spec(id).is_none() {
            return false;
        }
        self.focused = Some(id);
        self.zoomed = None;
        true
    }

    pub(crate) fn resize_focused(&mut self, delta: i16) -> bool {
        let Some(id) = self.focused else {
            return false;
        };
        let Some(tree) = self.tiled.as_mut() else {
            return false;
        };
        nudge_ratio(tree, id, delta)
    }

    pub(crate) fn toggle_zoom_focused(&mut self) -> bool {
        let Some(id) = self.focused else {
            return false;
        };
        if !self.is_tiled(id) {
            return false;
        }
        self.zoomed = if self.zoomed == Some(id) {
            None
        } else {
            Some(id)
        };
        true
    }

    pub(crate) fn toggle_float_focused(&mut self, geom: FloatGeom) -> bool {
        let Some(id) = self.focused else {
            return false;
        };
        if self.floating.remove(&id).is_some() {
            let tree = self.tiled.take();
            self.tiled = Some(dock_leaf(tree, id, SplitDir::Horizontal));
            self.zoomed = None;
            return true;
        }

        let Some(tree) = self.tiled.as_mut() else {
            return false;
        };
        match close_leaf(tree, id) {
            CloseOutcome::Closed(_) => {}
            CloseOutcome::WasLast => self.tiled = None,
            CloseOutcome::NotFound => return false,
        }
        self.floating.insert(id, geom);
        self.focused = Some(id);
        self.zoomed = None;
        true
    }

    pub(crate) fn close_pane(&mut self, id: PaneId) -> bool {
        let Some(pos) = self.panes.iter().position(|spec| spec.id == id) else {
            return !self.panes.is_empty();
        };
        self.panes.remove(pos);
        let mut preferred_focus = None;
        if self.floating.remove(&id).is_none()
            && let Some(tree) = self.tiled.as_mut()
        {
            match close_leaf(tree, id) {
                CloseOutcome::Closed(focus) => preferred_focus = Some(focus),
                CloseOutcome::WasLast => self.tiled = None,
                CloseOutcome::NotFound => {}
            }
        }
        if self.zoomed == Some(id) {
            self.zoomed = None;
        }
        if self.focused == Some(id) {
            self.focused = preferred_focus.or_else(|| self.focus_order().first().copied());
        }
        !self.panes.is_empty()
    }

    #[allow(dead_code)]
    pub(crate) fn allocate_shell(&mut self, cwd: PathBuf) -> PaneId {
        self.open_shell(cwd, "SHELL".to_string()).id
    }

    pub(crate) fn open_shell(&mut self, cwd: PathBuf, title: String) -> PaneSpec {
        let id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        let spec = PaneSpec::shell_titled(id, cwd, title);
        self.panes.push(spec.clone());
        self.focused = Some(id);
        let tree = self.tiled.take();
        self.tiled = Some(dock_leaf(tree, id, SplitDir::Horizontal));
        self.zoomed = None;
        spec
    }

    pub(crate) fn open_viewer(&mut self, path: PathBuf, title: String) -> PaneSpec {
        let id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        let spec = PaneSpec::viewer(id, path, title);
        self.panes.push(spec.clone());
        self.focused = Some(id);
        let tree = self.tiled.take();
        self.tiled = Some(dock_leaf(tree, id, SplitDir::Horizontal));
        self.zoomed = None;
        spec
    }

    fn focus_by(&mut self, delta: isize) {
        let order = self.focus_order();
        if order.is_empty() {
            self.focused = None;
            return;
        }
        let current = self
            .focused
            .and_then(|id| order.iter().position(|pane| *pane == id))
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(order.len() as isize) as usize;
        self.focused = Some(order[next]);
        self.zoomed = None;
    }

    fn focus_order(&self) -> Vec<PaneId> {
        let mut order = self.tiled.as_ref().map(leaves).unwrap_or_default();
        order.extend(self.floating.keys().copied());
        order.retain(|id| self.spec(*id).is_some());
        order
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PaneSpec {
    pub(crate) id: PaneId,
    pub(crate) title: String,
    pub(crate) kind: PaneKind,
}

impl PaneSpec {
    pub(crate) fn shell(id: PaneId, cwd: PathBuf) -> Self {
        Self::shell_titled(id, cwd, "SHELL".to_string())
    }

    pub(crate) fn shell_titled(id: PaneId, cwd: PathBuf, title: String) -> Self {
        Self {
            id,
            title,
            kind: PaneKind::Shell(ShellSpec { cwd }),
        }
    }

    pub(crate) fn viewer(id: PaneId, path: PathBuf, title: String) -> Self {
        Self {
            id,
            title,
            kind: PaneKind::Viewer(ViewerSpec { path }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PaneKind {
    Shell(ShellSpec),
    Viewer(ViewerSpec),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ShellSpec {
    pub(crate) cwd: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ViewerSpec {
    pub(crate) path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProjectSelection {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_session_carries_current_format_version() {
        let session = Session::single_shell(PathBuf::from("/work/project"));
        assert_eq!(session.format_version(), CURRENT_SESSION_FORMAT);
    }

    #[test]
    fn single_shell_session_has_serializable_spec_not_runtime_state() {
        let session = Session::single_shell(PathBuf::from("/work/project"));
        assert_eq!(session.focused(), Some(PaneId(0)));
        assert_eq!(session.pane_specs().len(), 1);
        assert_eq!(session.pane_specs()[0].title, "SHELL");
        assert_eq!(session.next_pane_id, 1);
        assert_eq!(session.tiled(), Some(&TileNode::Leaf(PaneId(0))));
        assert!(session.floating().is_empty());
        assert!(session.selected_project().is_none());
    }

    #[test]
    fn selected_project_is_durable_session_context() {
        let mut session = Session::single_shell_for_project(
            PathBuf::from("/work/project"),
            Some(ProjectSelection {
                name: "project".into(),
                path: PathBuf::from("/work/project"),
            }),
        );
        assert_eq!(
            session.selected_project(),
            Some(&ProjectSelection {
                name: "project".into(),
                path: PathBuf::from("/work/project"),
            })
        );

        session.select_project(ProjectSelection {
            name: "other".into(),
            path: PathBuf::from("/work/other"),
        });
        assert_eq!(
            session.selected_project().map(|p| p.name.as_str()),
            Some("other")
        );
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

    #[test]
    fn splitting_focused_shell_updates_layout_and_focus() {
        let mut session = Session::single_shell(PathBuf::from("/work/one"));
        let spec = session
            .split_focused_shell(PathBuf::from("/work/two"), SplitDir::Horizontal)
            .expect("split");
        assert_eq!(spec.id, PaneId(1));
        assert_eq!(session.focused(), Some(PaneId(1)));
        assert_eq!(
            session.tiled().map(leaves),
            Some(vec![PaneId(0), PaneId(1)])
        );
    }

    #[test]
    fn opening_viewer_docks_a_non_shell_pane() {
        let mut session = Session::single_shell(PathBuf::from("/work/one"));
        let spec = session.open_viewer(PathBuf::from("/work/one/README.md"), "VIEWER".into());
        assert_eq!(spec.id, PaneId(1));
        assert_eq!(session.focused(), Some(PaneId(1)));
        assert_eq!(
            session.tiled().map(leaves),
            Some(vec![PaneId(0), PaneId(1)])
        );
        assert!(matches!(spec.kind, PaneKind::Viewer(_)));
        assert_eq!(session.focused_shell_cwd(), None);
    }

    #[test]
    fn floating_focus_can_be_docked_without_losing_runtime_identity() {
        let mut session = Session::single_shell(PathBuf::from("/work/one"));
        session
            .split_focused_shell(PathBuf::from("/work/two"), SplitDir::Horizontal)
            .expect("split");

        assert!(session.toggle_float_focused(FloatGeom {
            x: 4,
            y: 3,
            width: 20,
            height: 10,
        }));
        assert!(session.is_floating(PaneId(1)));
        assert_eq!(session.tiled().map(leaves), Some(vec![PaneId(0)]));

        assert!(session.toggle_float_focused(FloatGeom {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        }));
        assert!(!session.is_floating(PaneId(1)));
        assert_eq!(
            session.tiled().map(leaves),
            Some(vec![PaneId(0), PaneId(1)])
        );
    }

    #[test]
    fn focus_cycles_through_tiled_then_floating_panes() {
        let mut session = Session::single_shell(PathBuf::from("/work/one"));
        session
            .split_focused_shell(PathBuf::from("/work/two"), SplitDir::Horizontal)
            .expect("split");
        assert!(session.toggle_float_focused(FloatGeom {
            x: 4,
            y: 3,
            width: 20,
            height: 10,
        }));

        session.focus_next();
        assert_eq!(session.focused(), Some(PaneId(0)));
        session.focus_next();
        assert_eq!(session.focused(), Some(PaneId(1)));
        session.focus_prev();
        assert_eq!(session.focused(), Some(PaneId(0)));
    }

    #[test]
    fn changing_focus_clears_zoom() {
        let mut session = Session::single_shell(PathBuf::from("/work/one"));
        session
            .split_focused_shell(PathBuf::from("/work/two"), SplitDir::Horizontal)
            .expect("split");
        assert!(session.toggle_zoom_focused());
        assert_eq!(session.zoomed(), Some(PaneId(1)));

        session.focus_prev();
        assert_eq!(session.focused(), Some(PaneId(0)));
        assert_eq!(session.zoomed(), None);
    }

    #[test]
    fn focus_pane_selects_existing_pane_and_clears_zoom() {
        let mut session = Session::single_shell(PathBuf::from("/work/one"));
        session
            .split_focused_shell(PathBuf::from("/work/two"), SplitDir::Horizontal)
            .expect("split");
        assert!(session.toggle_zoom_focused());

        assert!(session.focus_pane(PaneId(0)));
        assert_eq!(session.focused(), Some(PaneId(0)));
        assert_eq!(session.zoomed(), None);
        assert!(!session.focus_pane(PaneId(99)));
        assert_eq!(session.focused(), Some(PaneId(0)));
    }
}
