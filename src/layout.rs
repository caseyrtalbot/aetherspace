//! Pure layout primitives for tiled and floating panes.
//!
//! The session stores pane ids, split ratios, and generic floating geometry.
//! This module is the only place that turns that durable model into Ratatui
//! `Rect`s for rendering and PTY resize commands.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use serde::{Deserialize, Serialize};

use crate::session::PaneId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SplitDir {
    Horizontal,
    Vertical,
}

impl SplitDir {
    fn ratatui(self) -> Direction {
        match self {
            Self::Horizontal => Direction::Horizontal,
            Self::Vertical => Direction::Vertical,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum TileNode {
    Leaf(PaneId),
    Split {
        dir: SplitDir,
        ratio: u16,
        a: Box<TileNode>,
        b: Box<TileNode>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SolvedPane {
    pub(crate) id: PaneId,
    pub(crate) rect: Rect,
}

pub(crate) fn solve_tiled(node: &TileNode, area: Rect) -> Vec<SolvedPane> {
    let mut out = Vec::new();
    solve_into(node, area, &mut out);
    out
}

fn solve_into(node: &TileNode, area: Rect, out: &mut Vec<SolvedPane>) {
    match node {
        TileNode::Leaf(id) => out.push(SolvedPane {
            id: *id,
            rect: area,
        }),
        TileNode::Split { dir, ratio, a, b } => {
            let (ra, _sep, rb) = split_rects(*dir, *ratio, area);
            solve_into(a, ra, out);
            solve_into(b, rb, out);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SepLine {
    pub(crate) rect: Rect,
    pub(crate) horizontal: bool,
}

pub(crate) fn separators(node: &TileNode, area: Rect) -> Vec<SepLine> {
    let mut out = Vec::new();
    separators_into(node, area, &mut out);
    out
}

fn separators_into(node: &TileNode, area: Rect, out: &mut Vec<SepLine>) {
    if let TileNode::Split { dir, ratio, a, b } = node {
        let (ra, sep, rb) = split_rects(*dir, *ratio, area);
        out.push(SepLine {
            rect: sep,
            horizontal: *dir == SplitDir::Vertical,
        });
        separators_into(a, ra, out);
        separators_into(b, rb, out);
    }
}

fn split_rects(dir: SplitDir, ratio: u16, area: Rect) -> (Rect, Rect, Rect) {
    let parts = Layout::default()
        .direction(dir.ratatui())
        .constraints([
            Constraint::Percentage(ratio.clamp(1, 99)),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);
    (parts[0], parts[1], parts[2])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseOutcome {
    Closed(PaneId),
    WasLast,
    NotFound,
}

pub(crate) fn split_leaf(
    node: &mut TileNode,
    target: PaneId,
    new_id: PaneId,
    dir: SplitDir,
) -> bool {
    match node {
        TileNode::Leaf(id) if *id == target => {
            let original = TileNode::Leaf(*id);
            *node = TileNode::Split {
                dir,
                ratio: 50,
                a: Box::new(original),
                b: Box::new(TileNode::Leaf(new_id)),
            };
            true
        }
        TileNode::Leaf(_) => false,
        TileNode::Split { a, b, .. } => {
            split_leaf(a, target, new_id, dir) || split_leaf(b, target, new_id, dir)
        }
    }
}

pub(crate) fn dock_leaf(root: Option<TileNode>, id: PaneId, dir: SplitDir) -> TileNode {
    match root {
        None => TileNode::Leaf(id),
        Some(node) => TileNode::Split {
            dir,
            ratio: 65,
            a: Box::new(node),
            b: Box::new(TileNode::Leaf(id)),
        },
    }
}

pub(crate) fn close_leaf(node: &mut TileNode, target: PaneId) -> CloseOutcome {
    if matches!(node, TileNode::Leaf(id) if *id == target) {
        return CloseOutcome::WasLast;
    }
    close_in(node, target)
}

fn close_in(node: &mut TileNode, target: PaneId) -> CloseOutcome {
    let TileNode::Split { a, b, .. } = node else {
        return CloseOutcome::NotFound;
    };
    let a_is_target = matches!(a.as_ref(), TileNode::Leaf(id) if *id == target);
    let b_is_target = matches!(b.as_ref(), TileNode::Leaf(id) if *id == target);
    if a_is_target || b_is_target {
        let survivor = if a_is_target {
            std::mem::replace(b.as_mut(), TileNode::Leaf(target))
        } else {
            std::mem::replace(a.as_mut(), TileNode::Leaf(target))
        };
        let focus = first_leaf(&survivor);
        *node = survivor;
        return CloseOutcome::Closed(focus);
    }
    match close_in(a, target) {
        CloseOutcome::NotFound => close_in(b, target),
        other => other,
    }
}

pub(crate) fn nudge_ratio(node: &mut TileNode, target: PaneId, delta: i16) -> bool {
    let TileNode::Split { ratio, a, b, .. } = node else {
        return false;
    };
    let a_is_target = matches!(a.as_ref(), TileNode::Leaf(id) if *id == target);
    let b_is_target = matches!(b.as_ref(), TileNode::Leaf(id) if *id == target);
    if a_is_target || b_is_target {
        let signed = if a_is_target { delta } else { -delta };
        *ratio = (*ratio as i16 + signed).clamp(1, 99) as u16;
        return true;
    }
    nudge_ratio(a, target, delta) || nudge_ratio(b, target, delta)
}

pub(crate) fn contains_leaf(node: &TileNode, id: PaneId) -> bool {
    match node {
        TileNode::Leaf(pane) => *pane == id,
        TileNode::Split { a, b, .. } => contains_leaf(a, id) || contains_leaf(b, id),
    }
}

pub(crate) fn leaves(node: &TileNode) -> Vec<PaneId> {
    let mut out = Vec::new();
    collect_leaves(node, &mut out);
    out
}

fn collect_leaves(node: &TileNode, out: &mut Vec<PaneId>) {
    match node {
        TileNode::Leaf(id) => out.push(*id),
        TileNode::Split { a, b, .. } => {
            collect_leaves(a, out);
            collect_leaves(b, out);
        }
    }
}

fn first_leaf(node: &TileNode) -> PaneId {
    match node {
        TileNode::Leaf(id) => *id,
        TileNode::Split { a, .. } => first_leaf(a),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FloatGeom {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) width: u16,
    pub(crate) height: u16,
}

impl FloatGeom {
    pub(crate) fn centered(area: Rect) -> Self {
        let width = preferred_float_dim(area.width, 70, 24);
        let height = preferred_float_dim(area.height, 65, 8);
        Self {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        }
    }
}

fn preferred_float_dim(total: u16, percent: u16, min: u16) -> u16 {
    if total <= min {
        total
    } else {
        ((total as u32 * percent as u32 / 100) as u16)
            .max(min)
            .min(total)
    }
}

pub(crate) fn resolve_float(geom: FloatGeom, area: Rect) -> Rect {
    if area.width == 0 || area.height == 0 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    let width = geom.width.max(1).min(area.width);
    let height = geom.height.max(1).min(area.height);
    let max_x = area.right().saturating_sub(width);
    let max_y = area.bottom().saturating_sub(height);
    Rect::new(
        geom.x.clamp(area.x, max_x),
        geom.y.clamp(area.y, max_y),
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_leaf() -> TileNode {
        TileNode::Split {
            dir: SplitDir::Vertical,
            ratio: 50,
            a: Box::new(TileNode::Leaf(PaneId(0))),
            b: Box::new(TileNode::Leaf(PaneId(1))),
        }
    }

    #[test]
    fn solve_single_leaf_fills_area() {
        let area = Rect::new(2, 3, 40, 20);
        assert_eq!(
            solve_tiled(&TileNode::Leaf(PaneId(0)), area),
            vec![SolvedPane {
                id: PaneId(0),
                rect: area
            }]
        );
    }

    #[test]
    fn solve_split_carves_separator() {
        let area = Rect::new(0, 0, 80, 40);
        let out = solve_tiled(&two_leaf(), area);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].rect.width, 80);
        assert_eq!(out[1].rect.width, 80);
        assert_eq!(out[0].rect.height + out[1].rect.height + 1, 40);
        let seps = separators(&two_leaf(), area);
        assert_eq!(seps.len(), 1);
        assert!(seps[0].horizontal);
    }

    #[test]
    fn split_close_and_leaf_order_are_stable() {
        let mut tree = TileNode::Leaf(PaneId(0));
        assert!(split_leaf(
            &mut tree,
            PaneId(0),
            PaneId(1),
            SplitDir::Horizontal
        ));
        assert_eq!(leaves(&tree), vec![PaneId(0), PaneId(1)]);
        assert_eq!(
            close_leaf(&mut tree, PaneId(0)),
            CloseOutcome::Closed(PaneId(1))
        );
        assert_eq!(tree, TileNode::Leaf(PaneId(1)));
    }

    #[test]
    fn close_last_leaf_reports_without_mutating() {
        let mut tree = TileNode::Leaf(PaneId(0));
        assert_eq!(close_leaf(&mut tree, PaneId(0)), CloseOutcome::WasLast);
        assert_eq!(tree, TileNode::Leaf(PaneId(0)));
    }

    #[test]
    fn nudge_ratio_grows_focused_leaf_on_either_side() {
        let mut tree = two_leaf();
        assert!(nudge_ratio(&mut tree, PaneId(0), 10));
        let TileNode::Split { ratio, .. } = tree else {
            panic!("expected split");
        };
        assert_eq!(ratio, 60);

        let mut tree = two_leaf();
        assert!(nudge_ratio(&mut tree, PaneId(1), 10));
        let TileNode::Split { ratio, .. } = tree else {
            panic!("expected split");
        };
        assert_eq!(ratio, 40);
    }

    #[test]
    fn dock_leaf_adds_to_existing_tree_or_starts_one() {
        assert_eq!(
            dock_leaf(None, PaneId(2), SplitDir::Horizontal),
            TileNode::Leaf(PaneId(2))
        );

        let docked = dock_leaf(
            Some(TileNode::Leaf(PaneId(0))),
            PaneId(1),
            SplitDir::Horizontal,
        );
        assert_eq!(leaves(&docked), vec![PaneId(0), PaneId(1)]);
    }

    #[test]
    fn float_geometry_resolves_inside_workspace() {
        let area = Rect::new(10, 5, 100, 40);
        let geom = FloatGeom {
            x: 0,
            y: 0,
            width: 200,
            height: 200,
        };
        assert_eq!(resolve_float(geom, area), area);

        let centered = FloatGeom::centered(area);
        let resolved = resolve_float(centered, area);
        assert!(resolved.width <= area.width);
        assert!(resolved.height <= area.height);
        assert!(resolved.x >= area.x);
        assert!(resolved.y >= area.y);
    }
}
