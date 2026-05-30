//! The workspace split-tree: a recursive binary `Node` and a `solve()` that flattens
//! it into `(PaneId, Rect)` pairs.
//!
//! This is the foundational primitive of the Zellij-convergence refactor. A real
//! pane collection keyed by `PaneId` plus this tree replaces the hardcoded single
//! shell, so splits, tree-zoom, and floating panes all become reachable. The hard
//! part — proportional shares to exact integer cells with correct rounding — is
//! delegated to ratatui's own `Layout` (kasuari solver) by recursing through it at
//! each split, so the tree inherits the same discretization the rest of the UI uses.
//!
//! For the Phase 2b tracer the live tree is a single `Leaf`, so `solve` returns the
//! whole area unchanged and the app renders pixel-identical to before. The split
//! arm is exercised by the unit tests and cashed in by Phase 3.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Stable identifier for a pane in the workspace collection. Allocated
/// monotonically so a closed pane's id is never reused within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PaneId(pub u64);

/// A binary split tree over the workspace area. A `Leaf` names one pane; a `Split`
/// divides its area in two along `dir`, the first child taking `ratio` percent.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Leaf(PaneId),
    Split {
        dir: Direction,
        /// First child's share as a percentage (1..=99); the second child takes the
        /// remainder after a 1-cell hairline separator is carved between them, so the
        /// divider never overpaints live pane content.
        ratio: u16,
        a: Box<Node>,
        b: Box<Node>,
    },
}

/// Flatten the tree into `(pane, rect)` pairs, recursing through ratatui's `Layout`
/// at each split so cell rounding matches the rest of the UI. Each split carves a
/// 1-cell separator between its children (drawn by `separators`); a single leaf
/// returns `area` unchanged — the behavior-preserving anchor the tracer relies on.
pub fn solve(node: &Node, area: Rect) -> Vec<(PaneId, Rect)> {
    let mut out = Vec::new();
    solve_into(node, area, &mut out);
    out
}

fn solve_into(node: &Node, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
    match node {
        Node::Leaf(id) => out.push((*id, area)),
        Node::Split { dir, ratio, a, b } => {
            let (ra, _sep, rb) = split_rects(*dir, *ratio, area);
            solve_into(a, ra, out);
            solve_into(b, rb, out);
        }
    }
}

/// Split `area` along `dir` into (first child, separator, second child): the first
/// child takes `ratio` percent, a 1-cell hairline separator is carved out, and the
/// second child takes the remainder. Shared by `solve` and `separators` so their
/// geometry can never drift.
fn split_rects(dir: Direction, ratio: u16, area: Rect) -> (Rect, Rect, Rect) {
    let parts = Layout::default()
        .direction(dir)
        .constraints([
            Constraint::Percentage(ratio),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);
    (parts[0], parts[1], parts[2])
}

/// A carved separator track: a 1-cell-thick rect and whether its hairline runs
/// horizontally (a `Vertical` split, panes stacked top/bottom) or vertically (a
/// `Horizontal` split, panes side by side). `horizontal` selects the box-drawing
/// glyph and drives how perpendicular separators connect at junctions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SepLine {
    pub rect: Rect,
    pub horizontal: bool,
}

/// The separator tracks carved between split children, in tree order. A single leaf
/// has none. The drawer paints these as hairlines, resolving the box-drawing
/// junction glyph where perpendicular separators meet.
pub fn separators(node: &Node, area: Rect) -> Vec<SepLine> {
    let mut out = Vec::new();
    separators_into(node, area, &mut out);
    out
}

fn separators_into(node: &Node, area: Rect, out: &mut Vec<SepLine>) {
    if let Node::Split { dir, ratio, a, b } = node {
        let (ra, sep, rb) = split_rects(*dir, *ratio, area);
        out.push(SepLine {
            rect: sep,
            horizontal: *dir == Direction::Vertical,
        });
        separators_into(a, ra, out);
        separators_into(b, rb, out);
    }
}

/// The result of `close_leaf`, telling the caller how to update focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseOutcome {
    /// The leaf was removed and its parent collapsed; focus this leaf next (the
    /// surviving sibling subtree's first leaf).
    Closed(PaneId),
    /// `target` was the only leaf — the tree is left intact; the caller decides
    /// whether to quit or keep an empty workspace.
    WasLast,
    /// `target` was not in the tree; nothing changed.
    NotFound,
}

/// Replace `Leaf(target)` with a `Split` dividing along `dir` at 50%: the original
/// leaf becomes child `a`, a fresh `Leaf(new_id)` becomes child `b`. Returns true
/// if the target leaf was found (and split), false otherwise.
pub fn split_leaf(node: &mut Node, target: PaneId, new_id: PaneId, dir: Direction) -> bool {
    match node {
        Node::Leaf(id) if *id == target => {
            let original = Node::Leaf(*id);
            *node = Node::Split {
                dir,
                ratio: 50,
                a: Box::new(original),
                b: Box::new(Node::Leaf(new_id)),
            };
            true
        }
        Node::Leaf(_) => false,
        Node::Split { a, b, .. } => {
            split_leaf(a, target, new_id, dir) || split_leaf(b, target, new_id, dir)
        }
    }
}

/// Remove `Leaf(target)`, collapsing its parent `Split` so the surviving sibling
/// takes the parent's slot. The tree has no parent pointers, so the parent is found
/// by recursive search from the root (O(panes), accepted).
pub fn close_leaf(node: &mut Node, target: PaneId) -> CloseOutcome {
    // The root itself is the target leaf: there is no parent to collapse into.
    if matches!(node, Node::Leaf(id) if *id == target) {
        return CloseOutcome::WasLast;
    }
    close_in(node, target)
}

fn close_in(node: &mut Node, target: PaneId) -> CloseOutcome {
    let Node::Split { a, b, .. } = node else {
        // A leaf that is not the target (the root-leaf case is handled in close_leaf).
        return CloseOutcome::NotFound;
    };
    let a_is_target = matches!(a.as_ref(), Node::Leaf(id) if *id == target);
    let b_is_target = matches!(b.as_ref(), Node::Leaf(id) if *id == target);
    if a_is_target || b_is_target {
        // Move the surviving sibling out, then promote it into this node's slot.
        let survivor = if a_is_target {
            std::mem::replace(b.as_mut(), Node::Leaf(target))
        } else {
            std::mem::replace(a.as_mut(), Node::Leaf(target))
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

/// Nudge the split around the focused leaf so positive `delta` grows it. Finds the
/// immediate parent `Split` of `target`, adjusts its ratio by `delta` (sign-flipped
/// when the leaf is child `b`, so +delta always grows the focused pane), and clamps
/// the result to `1..=99` so neither child collapses to zero area. Returns true if
/// a parent split was found.
pub fn nudge_ratio(node: &mut Node, target: PaneId, delta: i16) -> bool {
    let Node::Split { ratio, a, b, .. } = node else {
        return false;
    };
    let a_is_target = matches!(a.as_ref(), Node::Leaf(id) if *id == target);
    let b_is_target = matches!(b.as_ref(), Node::Leaf(id) if *id == target);
    if a_is_target || b_is_target {
        let signed = if a_is_target { delta } else { -delta };
        *ratio = (*ratio as i16 + signed).clamp(1, 99) as u16;
        return true;
    }
    nudge_ratio(a, target, delta) || nudge_ratio(b, target, delta)
}

/// A separator the pointer landed on: the path of `a`/`b` steps from the root to the
/// owning `Split` (`false` = child `a`, `true` = child `b`), the split's direction,
/// and the split's own area. `set_ratio_at` replays the path to retune that split,
/// and `area` lets a drag map an absolute cursor position back to a 1..=99 ratio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SepHit {
    pub path: Vec<bool>,
    pub dir: Direction,
    pub area: Rect,
}

/// True if `(x, y)` falls inside `r`. Local point-in-rect (ratatui's `contains` wants
/// a `Position`); `right`/`bottom` saturate, so no overflow at the u16 edge.
fn in_rect(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.right() && y >= r.y && y < r.bottom()
}

/// Find the split whose carved separator the point `(x, y)` lies on, descending the
/// same geometry `solve` uses so the hit-test can never drift from what's drawn. A
/// separator belongs to a split, not a child, so the parent separator is tested
/// before recursing — a point on it can't also be inside either child rect. Returns
/// `None` off every divider (or on a single leaf).
pub fn sep_hit(node: &Node, area: Rect, x: u16, y: u16) -> Option<SepHit> {
    let mut path = Vec::new();
    sep_hit_into(node, area, x, y, &mut path)
}

fn sep_hit_into(node: &Node, area: Rect, x: u16, y: u16, path: &mut Vec<bool>) -> Option<SepHit> {
    let Node::Split { dir, ratio, a, b } = node else {
        return None;
    };
    let (ra, sep, rb) = split_rects(*dir, *ratio, area);
    if in_rect(sep, x, y) {
        return Some(SepHit {
            path: path.clone(),
            dir: *dir,
            area,
        });
    }
    if in_rect(ra, x, y) {
        path.push(false);
        let hit = sep_hit_into(a, ra, x, y, path);
        path.pop();
        return hit;
    }
    if in_rect(rb, x, y) {
        path.push(true);
        let hit = sep_hit_into(b, rb, x, y, path);
        path.pop();
        return hit;
    }
    None
}

/// Set the ratio of the split named by `path` (the `a`/`b` steps `sep_hit` recorded),
/// clamped to `1..=99` so neither child collapses. A path that runs off a leaf is a
/// no-op (the tree changed under a stale drag).
pub fn set_ratio_at(node: &mut Node, path: &[bool], ratio: u16) {
    let mut cur = node;
    for &side in path {
        let Node::Split { a, b, .. } = cur else {
            return;
        };
        cur = if side { b } else { a };
    }
    if let Node::Split { ratio: r, .. } = cur {
        *r = ratio.clamp(1, 99);
    }
}

/// All leaf ids in layout order (in-order DFS: child `a` before child `b`) — the
/// order focus cycles through with next/prev.
pub fn leaves(node: &Node) -> Vec<PaneId> {
    let mut out = Vec::new();
    collect_leaves(node, &mut out);
    out
}

fn collect_leaves(node: &Node, out: &mut Vec<PaneId>) {
    match node {
        Node::Leaf(id) => out.push(*id),
        Node::Split { a, b, .. } => {
            collect_leaves(a, out);
            collect_leaves(b, out);
        }
    }
}

/// The first leaf reached descending the `a` (first-child) spine — the canonical
/// focus target when a subtree is promoted.
fn first_leaf(node: &Node) -> PaneId {
    match node {
        Node::Leaf(id) => *id,
        Node::Split { a, .. } => first_leaf(a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solve_single_leaf_fills_area() {
        // The behavior-preserving anchor: a single leaf occupies the whole region,
        // so the tracer renders pixel-identical to the pre-tree shell pane.
        let area = Rect::new(2, 3, 40, 20);
        assert_eq!(solve(&Node::Leaf(PaneId(0)), area), vec![(PaneId(0), area)]);
    }

    #[test]
    fn solve_vertical_split_carves_one_row_separator() {
        let area = Rect::new(0, 0, 80, 40);
        let tree = Node::Split {
            dir: Direction::Vertical,
            ratio: 50,
            a: Box::new(Node::Leaf(PaneId(0))),
            b: Box::new(Node::Leaf(PaneId(1))),
        };
        let out = solve(&tree, area);
        assert_eq!(out.len(), 2);
        let (_, top) = out[0];
        let (_, bot) = out[1];
        // Full width, stacked, with exactly one row carved out for the separator.
        assert_eq!((top.width, bot.width), (80, 80));
        assert_eq!((top.x, bot.x), (0, 0));
        assert_eq!(top.y, 0);
        assert_eq!(top.height + 1 + bot.height, 40);
        assert_eq!(bot.y, top.y + top.height + 1);
    }

    #[test]
    fn solve_horizontal_split_carves_one_col_separator() {
        let area = Rect::new(0, 0, 80, 40);
        let tree = Node::Split {
            dir: Direction::Horizontal,
            ratio: 50,
            a: Box::new(Node::Leaf(PaneId(0))),
            b: Box::new(Node::Leaf(PaneId(1))),
        };
        let out = solve(&tree, area);
        let (_, left) = out[0];
        let (_, right) = out[1];
        assert_eq!((left.height, right.height), (40, 40));
        assert_eq!(left.width + 1 + right.width, 80);
        assert_eq!(right.x, left.x + left.width + 1);
    }

    #[test]
    fn separators_emits_one_track_per_split() {
        let area = Rect::new(0, 0, 80, 40);
        // A single leaf has no separators.
        assert!(separators(&Node::Leaf(PaneId(0)), area).is_empty());
        // A vertical split carves one horizontal hairline spanning the full width.
        let seps = separators(&two_leaf(), area);
        assert_eq!(seps.len(), 1);
        assert!(seps[0].horizontal);
        assert_eq!((seps[0].rect.width, seps[0].rect.height), (80, 1));
    }

    // A two-leaf vertical split, used as the starting tree for the mutation tests.
    fn two_leaf() -> Node {
        Node::Split {
            dir: Direction::Vertical,
            ratio: 50,
            a: Box::new(Node::Leaf(PaneId(0))),
            b: Box::new(Node::Leaf(PaneId(1))),
        }
    }

    #[test]
    fn split_leaf_replaces_target_with_a_split() {
        let mut tree = Node::Leaf(PaneId(0));
        assert!(split_leaf(
            &mut tree,
            PaneId(0),
            PaneId(1),
            Direction::Horizontal
        ));
        assert_eq!(
            tree,
            Node::Split {
                dir: Direction::Horizontal,
                ratio: 50,
                a: Box::new(Node::Leaf(PaneId(0))),
                b: Box::new(Node::Leaf(PaneId(1))),
            }
        );
        // The original leaf stays first; both leaves are present in order.
        assert_eq!(leaves(&tree), vec![PaneId(0), PaneId(1)]);
    }

    #[test]
    fn split_leaf_finds_a_nested_target() {
        let mut tree = two_leaf();
        assert!(split_leaf(
            &mut tree,
            PaneId(1),
            PaneId(2),
            Direction::Vertical
        ));
        assert_eq!(leaves(&tree), vec![PaneId(0), PaneId(1), PaneId(2)]);
    }

    #[test]
    fn split_leaf_returns_false_when_target_absent() {
        let mut tree = two_leaf();
        assert!(!split_leaf(
            &mut tree,
            PaneId(9),
            PaneId(2),
            Direction::Vertical
        ));
        assert_eq!(leaves(&tree), vec![PaneId(0), PaneId(1)]);
    }

    #[test]
    fn close_leaf_promotes_surviving_sibling() {
        // Closing one of two siblings collapses the parent: the survivor becomes root.
        let mut tree = two_leaf();
        let outcome = close_leaf(&mut tree, PaneId(0));
        assert_eq!(outcome, CloseOutcome::Closed(PaneId(1)));
        assert_eq!(tree, Node::Leaf(PaneId(1)));
    }

    #[test]
    fn close_leaf_reassigns_focus_to_sibling_subtree_first_leaf() {
        // Sibling is itself a split; focus goes to that subtree's first leaf.
        let mut tree = Node::Split {
            dir: Direction::Vertical,
            ratio: 50,
            a: Box::new(Node::Leaf(PaneId(0))),
            b: Box::new(Node::Split {
                dir: Direction::Horizontal,
                ratio: 50,
                a: Box::new(Node::Leaf(PaneId(1))),
                b: Box::new(Node::Leaf(PaneId(2))),
            }),
        };
        let outcome = close_leaf(&mut tree, PaneId(0));
        assert_eq!(outcome, CloseOutcome::Closed(PaneId(1)));
        assert_eq!(leaves(&tree), vec![PaneId(1), PaneId(2)]);
    }

    #[test]
    fn close_leaf_on_last_leaf_is_was_last_and_leaves_tree_intact() {
        let mut tree = Node::Leaf(PaneId(0));
        assert_eq!(close_leaf(&mut tree, PaneId(0)), CloseOutcome::WasLast);
        assert_eq!(tree, Node::Leaf(PaneId(0)));
    }

    #[test]
    fn close_leaf_not_found_leaves_tree_intact() {
        let mut tree = two_leaf();
        assert_eq!(close_leaf(&mut tree, PaneId(9)), CloseOutcome::NotFound);
        assert_eq!(leaves(&tree), vec![PaneId(0), PaneId(1)]);
    }

    #[test]
    fn nudge_ratio_grows_focused_leaf_regardless_of_side() {
        // Leaf on side `a`: +delta raises the ratio (a grows).
        let mut tree = two_leaf();
        assert!(nudge_ratio(&mut tree, PaneId(0), 10));
        if let Node::Split { ratio, .. } = tree {
            assert_eq!(ratio, 60);
        } else {
            panic!("expected split");
        }
        // Leaf on side `b`: +delta lowers the ratio so the `b` leaf still grows.
        let mut tree = two_leaf();
        assert!(nudge_ratio(&mut tree, PaneId(1), 10));
        if let Node::Split { ratio, .. } = tree {
            assert_eq!(ratio, 40);
        } else {
            panic!("expected split");
        }
    }

    #[test]
    fn nudge_ratio_clamps_to_1_99() {
        // Cannot drive a child to zero area in either direction.
        let mut tree = two_leaf();
        assert!(nudge_ratio(&mut tree, PaneId(0), 100));
        if let Node::Split { ratio, .. } = tree {
            assert_eq!(ratio, 99);
        } else {
            panic!("expected split");
        }
        let mut tree = two_leaf();
        assert!(nudge_ratio(&mut tree, PaneId(0), -100));
        if let Node::Split { ratio, .. } = tree {
            assert_eq!(ratio, 1);
        } else {
            panic!("expected split");
        }
    }

    #[test]
    fn nudge_ratio_returns_false_when_target_absent() {
        let mut tree = two_leaf();
        assert!(!nudge_ratio(&mut tree, PaneId(9), 10));
    }

    #[test]
    fn leaves_lists_in_layout_order() {
        let tree = Node::Split {
            dir: Direction::Horizontal,
            ratio: 40,
            a: Box::new(Node::Leaf(PaneId(0))),
            b: Box::new(Node::Split {
                dir: Direction::Vertical,
                ratio: 50,
                a: Box::new(Node::Leaf(PaneId(1))),
                b: Box::new(Node::Leaf(PaneId(2))),
            }),
        };
        assert_eq!(leaves(&tree), vec![PaneId(0), PaneId(1), PaneId(2)]);
    }

    #[test]
    fn solve_nested_split_covers_area_exactly() {
        // A split whose second child is itself a split: leaves plus the carved
        // separators tile the area with no overlap and no gap (cell area conserved).
        let area = Rect::new(0, 0, 100, 30);
        let tree = Node::Split {
            dir: Direction::Horizontal,
            ratio: 40,
            a: Box::new(Node::Leaf(PaneId(0))),
            b: Box::new(Node::Split {
                dir: Direction::Vertical,
                ratio: 50,
                a: Box::new(Node::Leaf(PaneId(1))),
                b: Box::new(Node::Leaf(PaneId(2))),
            }),
        };
        let out = solve(&tree, area);
        assert_eq!(out.len(), 3);
        let leaf_area: u32 = out
            .iter()
            .map(|(_, r)| r.width as u32 * r.height as u32)
            .sum();
        let sep_area: u32 = separators(&tree, area)
            .iter()
            .map(|s| s.rect.width as u32 * s.rect.height as u32)
            .sum();
        assert_eq!(
            leaf_area + sep_area,
            100 * 30,
            "leaves plus separators tile the area exactly"
        );
    }

    #[test]
    fn sep_hit_finds_the_root_divider_and_misses_off_it() {
        // 80x40 vertical split at 50%: the carved separator is the single row that
        // `solve` leaves between top and bottom. Hitting it returns the root split.
        let area = Rect::new(0, 0, 80, 40);
        let tree = two_leaf();
        let sep = separators(&tree, area)[0].rect;
        let hit = sep_hit(&tree, area, sep.x, sep.y).expect("on the divider");
        assert_eq!(hit.path, Vec::<bool>::new(), "root split has an empty path");
        assert_eq!(hit.dir, Direction::Vertical);
        assert_eq!(hit.area, area);
        // A point in the top pane (not on the divider) misses.
        assert!(sep_hit(&tree, area, 10, 0).is_none());
    }

    #[test]
    fn sep_hit_descends_to_a_nested_divider() {
        // Outer horizontal split; its `b` child is a vertical split. A point on the
        // inner divider resolves to path [true] (descend into b), not the root.
        let area = Rect::new(0, 0, 100, 30);
        let tree = Node::Split {
            dir: Direction::Horizontal,
            ratio: 40,
            a: Box::new(Node::Leaf(PaneId(0))),
            b: Box::new(Node::Split {
                dir: Direction::Vertical,
                ratio: 50,
                a: Box::new(Node::Leaf(PaneId(1))),
                b: Box::new(Node::Leaf(PaneId(2))),
            }),
        };
        // The inner separator is the second track (root's is first).
        let inner = separators(&tree, area)[1].rect;
        let hit = sep_hit(&tree, area, inner.x, inner.y).expect("on the inner divider");
        assert_eq!(hit.path, vec![true]);
        assert_eq!(hit.dir, Direction::Vertical);
    }

    #[test]
    fn set_ratio_at_retunes_the_named_split_and_clamps() {
        let mut tree = Node::Split {
            dir: Direction::Horizontal,
            ratio: 40,
            a: Box::new(Node::Leaf(PaneId(0))),
            b: Box::new(Node::Split {
                dir: Direction::Vertical,
                ratio: 50,
                a: Box::new(Node::Leaf(PaneId(1))),
                b: Box::new(Node::Leaf(PaneId(2))),
            }),
        };
        // Empty path retunes the root; path [true] retunes the nested split.
        set_ratio_at(&mut tree, &[], 70);
        set_ratio_at(&mut tree, &[true], 200); // clamps to 99
        let Node::Split { ratio, b, .. } = &tree else {
            panic!("expected split");
        };
        assert_eq!(*ratio, 70);
        let Node::Split { ratio: inner, .. } = b.as_ref() else {
            panic!("expected nested split");
        };
        assert_eq!(*inner, 99);
    }

    #[test]
    fn set_ratio_at_is_a_noop_on_a_stale_path() {
        // A path that runs off a leaf (tree shrank under a drag) changes nothing.
        let mut tree = two_leaf();
        set_ratio_at(&mut tree, &[false, true], 10);
        if let Node::Split { ratio, .. } = tree {
            assert_eq!(ratio, 50, "untouched");
        } else {
            panic!("expected split");
        }
    }
}
