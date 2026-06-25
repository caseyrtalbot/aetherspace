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

/// Height in rows of the one-line indicator strip a `Stack` carves from the top of
/// its rect for the collapsed-member chip row. The expanded member fills the rest.
pub(crate) const STACK_STRIP_ROWS: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum TileNode {
    Leaf(PaneId),
    Split {
        dir: SplitDir,
        ratio: u16,
        a: Box<TileNode>,
        b: Box<TileNode>,
    },
    /// One expanded child (`children[active]`) filling the rect minus a one-row
    /// indicator strip; collapsed children keep their PTYs but get no content rect.
    /// `children` are flattened into the global focus cycle by `collect_leaves`.
    Stack {
        children: Vec<TileNode>,
        active: usize,
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
        TileNode::Stack { children, active } => {
            // One expanded child fills the rect minus the indicator strip; collapsed
            // children get no rect (idle). The strip carve lives HERE in the geometry
            // helper, not in draw, so the expanded PTY's winsize matches what is shown.
            if let Some(child) = children.get(*active) {
                solve_into(child, stack_body_rect(area), out);
            }
        }
    }
}

/// The rect the expanded stack member occupies: the stack's area minus the top
/// indicator strip. Shared by geometry and draw so the winsize and the painted
/// surface agree.
fn stack_body_rect(area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y.saturating_add(STACK_STRIP_ROWS),
        area.width,
        area.height.saturating_sub(STACK_STRIP_ROWS),
    )
}

/// The one-row indicator strip at the top of a stack's area where collapsed-member
/// chips are painted.
pub(crate) fn stack_strip_rect(area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y,
        area.width,
        area.height.min(STACK_STRIP_ROWS),
    )
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
    match node {
        TileNode::Leaf(_) => {}
        TileNode::Split { dir, ratio, a, b } => {
            let (ra, sep, rb) = split_rects(*dir, *ratio, area);
            out.push(SepLine {
                rect: sep,
                horizontal: *dir == SplitDir::Vertical,
            });
            separators_into(a, ra, out);
            separators_into(b, rb, out);
        }
        TileNode::Stack { children, active } => {
            // The strip replaces the stack's own separator. Only the active child is
            // visible, so only it draws separators (a Split nested inside it still
            // needs them); collapsed children draw nothing.
            if let Some(child) = children.get(*active) {
                separators_into(child, stack_body_rect(area), out);
            }
        }
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
        TileNode::Stack { children, .. } => children
            .iter_mut()
            .any(|child| split_leaf(child, target, new_id, dir)),
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
    match node {
        TileNode::Leaf(_) => CloseOutcome::NotFound,
        TileNode::Split { a, b, .. } => {
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
        TileNode::Stack { children, active } => {
            // Drop the matching direct-child leaf from the Vec. When one child
            // remains, the stack is degenerate: promote the SURVIVOR NODE in place
            // (Split OR Leaf — never flatten a surviving Split to a Leaf, which would
            // lose its subtree). Re-clamp `active` to the shrunken Vec either way.
            if let Some(pos) = children
                .iter()
                .position(|child| matches!(child, TileNode::Leaf(id) if *id == target))
            {
                children.remove(pos);
                if children.len() == 1 {
                    let survivor = children.remove(0);
                    let focus = first_leaf(&survivor);
                    *node = survivor;
                    return CloseOutcome::Closed(focus);
                }
                // A removal at `pos < active` shifts every later member's index
                // down by one, so decrement `active` to keep it pointing at the
                // SAME node (not the slot). Then clamp to handle removal of the
                // active member itself (its successor slides into the slot).
                if pos < *active {
                    *active -= 1;
                }
                *active = (*active).min(children.len().saturating_sub(1));
                let focus = first_leaf(node);
                return CloseOutcome::Closed(focus);
            }
            // Not a direct child: recurse into each member (a member may be a Split).
            for child in children.iter_mut() {
                match close_in(child, target) {
                    CloseOutcome::NotFound => continue,
                    other => return other,
                }
            }
            CloseOutcome::NotFound
        }
    }
}

pub(crate) fn nudge_ratio(node: &mut TileNode, target: PaneId, delta: i16) -> bool {
    match node {
        TileNode::Leaf(_) => false,
        TileNode::Split { ratio, a, b, .. } => {
            let a_is_target = matches!(a.as_ref(), TileNode::Leaf(id) if *id == target);
            let b_is_target = matches!(b.as_ref(), TileNode::Leaf(id) if *id == target);
            if a_is_target || b_is_target {
                let signed = if a_is_target { delta } else { -delta };
                *ratio = (*ratio as i16 + signed).clamp(1, 99) as u16;
                return true;
            }
            nudge_ratio(a, target, delta) || nudge_ratio(b, target, delta)
        }
        TileNode::Stack { children, active } => {
            // A stack has no ratio of its own. Only the active child is visible, so
            // only it can be resized.
            match children.get_mut(*active) {
                Some(child) => nudge_ratio(child, target, delta),
                None => false,
            }
        }
    }
}

pub(crate) fn contains_leaf(node: &TileNode, id: PaneId) -> bool {
    match node {
        TileNode::Leaf(pane) => *pane == id,
        TileNode::Split { a, b, .. } => contains_leaf(a, id) || contains_leaf(b, id),
        TileNode::Stack { children, .. } => children.iter().any(|c| contains_leaf(c, id)),
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
        TileNode::Stack { children, .. } => {
            // Push every member's leaves in order so collapsed panes stay in the
            // global focus cycle (leader-Tab steps through and auto-expands each).
            for child in children {
                collect_leaves(child, out);
            }
        }
    }
}

fn first_leaf(node: &TileNode) -> PaneId {
    match node {
        TileNode::Leaf(id) => *id,
        TileNode::Split { a, .. } => first_leaf(a),
        TileNode::Stack { children, .. } => {
            children.first().map(first_leaf).unwrap_or(PaneId(u64::MAX))
        }
    }
}

/// When focus lands on a leaf inside a `Stack`, expand it: set the enclosing stack's
/// `active` to the member that contains `id`. Walks the tree like `nudge_ratio`,
/// mutating only the directly-enclosing stack. Returns true if a stack was updated.
pub(crate) fn set_active_for_leaf(node: &mut TileNode, id: PaneId) -> bool {
    match node {
        TileNode::Leaf(_) => false,
        TileNode::Split { a, b, .. } => set_active_for_leaf(a, id) || set_active_for_leaf(b, id),
        TileNode::Stack { children, active } => {
            if let Some(pos) = children.iter().position(|c| contains_leaf(c, id)) {
                *active = pos;
                // Recurse so a stack nested inside this member also syncs.
                set_active_for_leaf(&mut children[pos], id);
                true
            } else {
                false
            }
        }
    }
}

/// Clamp every `Stack`'s `active` to a valid child index, recursively. A hand-edited
/// or corrupted `session.toml` can deserialize a Stack whose `active >= children.len()`;
/// the solver is panic-safe (`children.get(*active)` returns `None`) but renders an
/// empty stack. Normalizing at load keeps a member visible. Runtime mutation paths
/// re-clamp on their own (e.g. close), so this only matters at load. Returns true if
/// anything changed (informational; the load path does not branch on it).
pub(crate) fn clamp_active(node: &mut TileNode) -> bool {
    match node {
        TileNode::Leaf(_) => false,
        TileNode::Split { a, b, .. } => {
            let changed_a = clamp_active(a);
            let changed_b = clamp_active(b);
            changed_a || changed_b
        }
        TileNode::Stack { children, active } => {
            let mut changed = false;
            for child in children.iter_mut() {
                changed |= clamp_active(child);
            }
            let max = children.len().saturating_sub(1);
            if *active > max {
                *active = max;
                changed = true;
            }
            changed
        }
    }
}

/// Convert the focused leaf's parent `Split` into a `Stack` (collapsing the sibling
/// subtree alongside it), or collapse the enclosing `Stack` back to a `Split`. The
/// inverse uses survivor-promotion semantics: it does NOT reconstruct the original
/// split layout, it rebuilds a fresh left/right Split from the stack's two members
/// (defined behavior, not a perfect round-trip for 3+ members).
///
/// Returns true if the tree changed.
pub(crate) fn toggle_stack(node: &mut TileNode, target: PaneId) -> bool {
    // Already inside a stack → collapse back to splits.
    if leaf_is_in_stack(node, target) {
        return unstack(node, target);
    }
    make_stack(node, target)
}

fn leaf_is_in_stack(node: &TileNode, target: PaneId) -> bool {
    match node {
        TileNode::Leaf(_) => false,
        TileNode::Split { a, b, .. } => leaf_is_in_stack(a, target) || leaf_is_in_stack(b, target),
        TileNode::Stack { children, .. } => children.iter().any(|c| contains_leaf(c, target)),
    }
}

/// Fold the focused leaf's IMMEDIATE parent `Split` into a `Stack` of its two
/// children, expanded on the side holding `target`. "Immediate parent" means the
/// split with `target` as a DIRECT child leaf, so a 3-pane right-nested tree folds
/// the inner split (the focused pane and its nearest sibling), not the whole tree.
fn make_stack(node: &mut TileNode, target: PaneId) -> bool {
    match node {
        TileNode::Leaf(_) => false,
        TileNode::Split { a, b, .. } => {
            let a_is_target = matches!(a.as_ref(), TileNode::Leaf(id) if *id == target);
            let b_is_target = matches!(b.as_ref(), TileNode::Leaf(id) if *id == target);
            // Only fold here when target is a DIRECT child leaf of this split.
            if a_is_target || b_is_target {
                let TileNode::Split { a, b, .. } = std::mem::replace(node, TileNode::Leaf(target))
                else {
                    unreachable!("matched Split above");
                };
                let active = if a_is_target { 0 } else { 1 };
                *node = TileNode::Stack {
                    children: vec![*a, *b],
                    active,
                };
                true
            } else {
                make_stack(a, target) || make_stack(b, target)
            }
        }
        TileNode::Stack { children, .. } => {
            children.iter_mut().any(|child| make_stack(child, target))
        }
    }
}

/// Find the `Stack` enclosing `target` and rebuild a left/right `Split` from its
/// members so the panes tile again. For exactly two members the original two-way
/// split is restored; for 3+ members the trailing members nest as right-hand
/// splits (defined, documented behavior).
fn unstack(node: &mut TileNode, target: PaneId) -> bool {
    match node {
        TileNode::Leaf(_) => false,
        TileNode::Split { a, b, .. } => unstack(a, target) || unstack(b, target),
        TileNode::Stack { children, .. } => {
            if children.iter().any(|c| contains_leaf(c, target)) {
                let TileNode::Stack { children, .. } =
                    std::mem::replace(node, TileNode::Leaf(target))
                else {
                    unreachable!("matched Stack above");
                };
                *node = splits_from_members(children);
                true
            } else {
                children.iter_mut().any(|child| unstack(child, target))
            }
        }
    }
}

/// Fold a stack's members back into a right-nested chain of horizontal splits.
fn splits_from_members(mut members: Vec<TileNode>) -> TileNode {
    match members.len() {
        0 => TileNode::Leaf(PaneId(u64::MAX)),
        1 => members.remove(0),
        _ => {
            let first = members.remove(0);
            TileNode::Split {
                dir: SplitDir::Horizontal,
                ratio: 50,
                a: Box::new(first),
                b: Box::new(splits_from_members(members)),
            }
        }
    }
}

/// A collapsed-member chip in a stack's indicator strip: its pane id, the on-screen
/// rect to hit-test/paint, and whether it is the expanded (active) member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StackChip {
    pub(crate) id: PaneId,
    pub(crate) rect: Rect,
    pub(crate) active: bool,
}

/// Geometry for every stack's chip row: one chip per member, laid left-to-right
/// across the indicator strip, each labelled by its first leaf id. Used by draw and
/// by the mouse hit-test so clicking a chip focuses+expands that member.
pub(crate) fn stack_chips(node: &TileNode, area: Rect) -> Vec<StackChip> {
    let mut out = Vec::new();
    stack_chips_into(node, area, &mut out);
    out
}

fn stack_chips_into(node: &TileNode, area: Rect, out: &mut Vec<StackChip>) {
    match node {
        TileNode::Leaf(_) => {}
        TileNode::Split { dir, ratio, a, b } => {
            let (ra, _sep, rb) = split_rects(*dir, *ratio, area);
            stack_chips_into(a, ra, out);
            stack_chips_into(b, rb, out);
        }
        TileNode::Stack { children, active } => {
            let strip = stack_strip_rect(area);
            let count = children.len().max(1) as u16;
            let chip_w = (strip.width / count).max(1);
            for (i, child) in children.iter().enumerate() {
                let x = strip.x.saturating_add(chip_w * i as u16);
                if x >= strip.right() {
                    break;
                }
                let w = if i + 1 == children.len() {
                    strip.right().saturating_sub(x)
                } else {
                    chip_w
                };
                out.push(StackChip {
                    id: first_leaf(child),
                    rect: Rect::new(x, strip.y, w, strip.height),
                    active: i == *active,
                });
            }
            // The active member is visible below the strip; recurse so a stack nested
            // inside it also contributes its chips.
            if let Some(child) = children.get(*active) {
                stack_chips_into(child, stack_body_rect(area), out);
            }
        }
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

    fn stack_of(active: usize, ids: &[u64]) -> TileNode {
        TileNode::Stack {
            children: ids.iter().map(|i| TileNode::Leaf(PaneId(*i))).collect(),
            active,
        }
    }

    #[test]
    fn clamp_active_pulls_out_of_range_index_into_bounds() {
        // A hand-edited file with active past the last child would render an empty
        // stack; clamp pulls it to the last valid index and reports the change.
        let mut tree = stack_of(5, &[0, 1, 2]);
        assert!(clamp_active(&mut tree));
        let TileNode::Stack { active, .. } = &tree else {
            panic!("expected stack");
        };
        assert_eq!(*active, 2);

        // A valid index is untouched and reports no change (idempotent).
        let mut ok = stack_of(1, &[0, 1, 2]);
        assert!(!clamp_active(&mut ok));

        // Stacks nested inside a split are reached too.
        let mut nested = TileNode::Split {
            dir: SplitDir::Horizontal,
            ratio: 50,
            a: Box::new(TileNode::Leaf(PaneId(9))),
            b: Box::new(stack_of(9, &[0, 1])),
        };
        assert!(clamp_active(&mut nested));
        let TileNode::Split { b, .. } = &nested else {
            panic!("expected split");
        };
        let TileNode::Stack { active, .. } = b.as_ref() else {
            panic!("expected nested stack");
        };
        assert_eq!(*active, 1);
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

    #[test]
    fn stack_solves_only_active_child_minus_strip() {
        let area = Rect::new(0, 0, 80, 24);
        let tree = stack_of(1, &[0, 1, 2]);
        let solved = solve_tiled(&tree, area);
        assert_eq!(solved.len(), 1, "only the active child gets a rect");
        assert_eq!(solved[0].id, PaneId(1));
        // Body rect is the area minus the one-row strip.
        assert_eq!(solved[0].rect.y, area.y + STACK_STRIP_ROWS);
        assert_eq!(solved[0].rect.height, area.height - STACK_STRIP_ROWS);
        assert_eq!(solved[0].rect.width, area.width);
    }

    #[test]
    fn stack_emits_no_separator_but_a_nested_split_does() {
        let area = Rect::new(0, 0, 80, 24);
        // Stack of one Leaf and one Split, active on the Split.
        let nested_split = TileNode::Split {
            dir: SplitDir::Vertical,
            ratio: 50,
            a: Box::new(TileNode::Leaf(PaneId(1))),
            b: Box::new(TileNode::Leaf(PaneId(2))),
        };
        let tree = TileNode::Stack {
            children: vec![TileNode::Leaf(PaneId(0)), nested_split],
            active: 1,
        };
        // Active is the nested split → exactly its one separator, none for the stack.
        assert_eq!(separators(&tree, area).len(), 1);

        // Active on the bare Leaf → zero separators.
        let tree_leaf_active = TileNode::Stack {
            children: vec![
                TileNode::Leaf(PaneId(0)),
                TileNode::Split {
                    dir: SplitDir::Vertical,
                    ratio: 50,
                    a: Box::new(TileNode::Leaf(PaneId(1))),
                    b: Box::new(TileNode::Leaf(PaneId(2))),
                },
            ],
            active: 0,
        };
        assert!(separators(&tree_leaf_active, area).is_empty());
    }

    #[test]
    fn closing_a_stack_member_shrinks_the_vec_and_reclamps_active() {
        let mut tree = stack_of(2, &[0, 1, 2]);
        // Remove the active (last) member: Vec shrinks to 2, active re-clamps to 1.
        assert_eq!(
            close_leaf(&mut tree, PaneId(2)),
            CloseOutcome::Closed(PaneId(0))
        );
        match &tree {
            TileNode::Stack { children, active } => {
                assert_eq!(children.len(), 2);
                assert_eq!(*active, 1);
                assert_eq!(leaves(&tree), vec![PaneId(0), PaneId(1)]);
            }
            other => panic!("expected Stack, got {other:?}"),
        }
    }

    #[test]
    fn closing_a_member_before_the_active_keeps_the_active_node_expanded() {
        // Stack{[L0,L1,L2,L3], active:2} (L2 expanded). Closing the COLLAPSED L0
        // shifts every later member's index down by one, so `active` must decrement
        // to keep pointing at L2 — not blindly clamp and slide onto L3.
        let mut tree = stack_of(2, &[0, 1, 2, 3]);
        assert_eq!(
            close_leaf(&mut tree, PaneId(0)),
            CloseOutcome::Closed(PaneId(1))
        );
        match &tree {
            TileNode::Stack { children, active } => {
                assert_eq!(children.len(), 3);
                assert_eq!(
                    children[*active],
                    TileNode::Leaf(PaneId(2)),
                    "the expanded member stays L2 after a lower index is removed"
                );
            }
            other => panic!("expected Stack, got {other:?}"),
        }
    }

    #[test]
    fn closing_to_one_member_promotes_the_survivor_node() {
        // Survivor is a Leaf → Stack collapses to Leaf.
        let mut tree = stack_of(0, &[0, 1]);
        close_leaf(&mut tree, PaneId(0));
        assert_eq!(tree, TileNode::Leaf(PaneId(1)));

        // Survivor is a SPLIT → Stack collapses to that Split (NOT flattened to Leaf).
        let survivor_split = TileNode::Split {
            dir: SplitDir::Horizontal,
            ratio: 60,
            a: Box::new(TileNode::Leaf(PaneId(1))),
            b: Box::new(TileNode::Leaf(PaneId(2))),
        };
        let mut tree = TileNode::Stack {
            children: vec![TileNode::Leaf(PaneId(0)), survivor_split.clone()],
            active: 0,
        };
        close_leaf(&mut tree, PaneId(0));
        assert_eq!(tree, survivor_split, "surviving Split keeps its subtree");
        assert_eq!(leaves(&tree), vec![PaneId(1), PaneId(2)]);
    }

    #[test]
    fn closing_a_pane_inside_a_stack_actually_removes_it() {
        // Guards the old close_in NotFound silent-drop (orphaned PTY): a stack member
        // nested in a Split must be reachable and removable.
        let mut tree = TileNode::Split {
            dir: SplitDir::Horizontal,
            ratio: 50,
            a: Box::new(stack_of(0, &[0, 1, 2])),
            b: Box::new(TileNode::Leaf(PaneId(3))),
        };
        assert!(matches!(
            close_leaf(&mut tree, PaneId(1)),
            CloseOutcome::Closed(_)
        ));
        assert_eq!(leaves(&tree), vec![PaneId(0), PaneId(2), PaneId(3)]);
    }

    #[test]
    fn collect_leaves_flattens_a_split_containing_stack_in_order() {
        let tree = TileNode::Split {
            dir: SplitDir::Horizontal,
            ratio: 50,
            a: Box::new(stack_of(1, &[0, 1, 2])),
            b: Box::new(TileNode::Leaf(PaneId(3))),
        };
        assert_eq!(
            leaves(&tree),
            vec![PaneId(0), PaneId(1), PaneId(2), PaneId(3)]
        );
    }

    #[test]
    fn split_contains_first_leaf_handle_stack_arms() {
        // split_leaf: a stack member that is a Leaf splits like any leaf.
        let mut tree = stack_of(0, &[0, 1]);
        assert!(split_leaf(
            &mut tree,
            PaneId(0),
            PaneId(9),
            SplitDir::Horizontal
        ));
        assert_eq!(leaves(&tree), vec![PaneId(0), PaneId(9), PaneId(1)]);

        // contains_leaf reaches into stack members.
        let tree = stack_of(0, &[0, 1, 2]);
        assert!(contains_leaf(&tree, PaneId(2)));
        assert!(!contains_leaf(&tree, PaneId(7)));

        // first_leaf returns the first member's first leaf.
        assert_eq!(first_leaf(&tree), PaneId(0));
    }

    #[test]
    fn nudge_ratio_recurses_into_the_active_stack_member_only() {
        // Active member is a split; nudging its focused leaf changes that split's ratio.
        let active_split = TileNode::Split {
            dir: SplitDir::Vertical,
            ratio: 50,
            a: Box::new(TileNode::Leaf(PaneId(1))),
            b: Box::new(TileNode::Leaf(PaneId(2))),
        };
        let mut tree = TileNode::Stack {
            children: vec![TileNode::Leaf(PaneId(0)), active_split],
            active: 1,
        };
        assert!(nudge_ratio(&mut tree, PaneId(1), 10));
        match &tree {
            TileNode::Stack { children, .. } => match &children[1] {
                TileNode::Split { ratio, .. } => assert_eq!(*ratio, 60),
                other => panic!("expected split member, got {other:?}"),
            },
            other => panic!("expected stack, got {other:?}"),
        }

        // A collapsed member's leaf is NOT reachable by nudge (active-only).
        let mut tree2 = stack_of(0, &[0, 1, 2]);
        assert!(!nudge_ratio(&mut tree2, PaneId(2), 10));
    }

    #[test]
    fn toggle_stack_folds_a_split_and_unfolds_it() {
        // Fold a two-leaf split into a stack expanded on the targeted member.
        let mut tree = two_leaf();
        assert!(toggle_stack(&mut tree, PaneId(1)));
        match &tree {
            TileNode::Stack { children, active } => {
                assert_eq!(children.len(), 2);
                assert_eq!(*active, 1, "fold expands the targeted member");
            }
            other => panic!("expected stack, got {other:?}"),
        }
        // Unfold back to a tiling split (same two leaves, no longer stacked).
        assert!(toggle_stack(&mut tree, PaneId(1)));
        assert!(matches!(tree, TileNode::Split { .. }));
        assert_eq!(leaves(&tree), vec![PaneId(0), PaneId(1)]);
    }

    #[test]
    fn toggle_stack_folds_the_immediate_parent_split_not_the_whole_tree() {
        // Right-nested 3-pane tree; focus on the inner Leaf(1). Folding stacks only
        // its immediate parent (1 and 2), leaving Leaf(0) tiled beside the stack.
        let mut tree = TileNode::Split {
            dir: SplitDir::Horizontal,
            ratio: 50,
            a: Box::new(TileNode::Leaf(PaneId(0))),
            b: Box::new(TileNode::Split {
                dir: SplitDir::Horizontal,
                ratio: 50,
                a: Box::new(TileNode::Leaf(PaneId(1))),
                b: Box::new(TileNode::Leaf(PaneId(2))),
            }),
        };
        assert!(toggle_stack(&mut tree, PaneId(1)));
        match &tree {
            TileNode::Split { a, b, .. } => {
                assert_eq!(a.as_ref(), &TileNode::Leaf(PaneId(0)), "Leaf 0 stays tiled");
                match b.as_ref() {
                    TileNode::Stack { children, active } => {
                        assert_eq!(children.len(), 2);
                        assert_eq!(*active, 0, "focused inner-left member expands");
                    }
                    other => panic!("expected inner stack, got {other:?}"),
                }
            }
            other => panic!("expected outer split, got {other:?}"),
        }
        assert_eq!(leaves(&tree), vec![PaneId(0), PaneId(1), PaneId(2)]);
    }

    #[test]
    fn set_active_for_leaf_expands_the_enclosing_stack() {
        let mut tree = stack_of(0, &[0, 1, 2]);
        assert!(set_active_for_leaf(&mut tree, PaneId(2)));
        match &tree {
            TileNode::Stack { active, .. } => assert_eq!(*active, 2),
            other => panic!("expected stack, got {other:?}"),
        }
        // A leaf not in any stack is a no-op (returns false).
        let mut plain = two_leaf();
        assert!(!set_active_for_leaf(&mut plain, PaneId(0)));
    }

    #[test]
    fn stack_chips_lay_out_one_chip_per_member() {
        let area = Rect::new(0, 0, 80, 24);
        let tree = stack_of(1, &[0, 1, 2]);
        let chips = stack_chips(&tree, area);
        assert_eq!(chips.len(), 3);
        assert!(chips[1].active);
        assert!(!chips[0].active && !chips[2].active);
        // Chips sit in the top strip row and tile left-to-right without gaps.
        assert!(
            chips
                .iter()
                .all(|c| c.rect.y == area.y && c.rect.height == STACK_STRIP_ROWS)
        );
        assert_eq!(chips[0].rect.x, area.x);
        assert_eq!(chips[1].rect.x, chips[0].rect.right());
    }
}
