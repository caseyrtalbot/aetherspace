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
    // Constructed when Phase 3 wires split_leaf; solve already handles it and the
    // unit tests exercise it, so the tree gains splits as a localized change.
    #[allow(dead_code)]
    Split {
        dir: Direction,
        /// First child's share as a percentage (1..=99); the second child takes the
        /// remainder. Children tile the area exactly — separators are drawn over the
        /// shared edge (Phase 3), not carved out here.
        ratio: u16,
        a: Box<Node>,
        b: Box<Node>,
    },
}

/// Flatten the tree into `(pane, rect)` pairs, recursing through ratatui's `Layout`
/// at each split so cell rounding matches the rest of the UI. Children of a split
/// tile their parent exactly (no gaps, no overlap); a single leaf returns `area`
/// unchanged — the behavior-preserving anchor the tracer relies on.
pub fn solve(node: &Node, area: Rect) -> Vec<(PaneId, Rect)> {
    let mut out = Vec::new();
    solve_into(node, area, &mut out);
    out
}

fn solve_into(node: &Node, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
    match node {
        Node::Leaf(id) => out.push((*id, area)),
        Node::Split { dir, ratio, a, b } => {
            let parts = Layout::default()
                .direction(*dir)
                .constraints([
                    Constraint::Percentage(*ratio),
                    Constraint::Percentage(100u16.saturating_sub(*ratio)),
                ])
                .split(area);
            solve_into(a, parts[0], out);
            solve_into(b, parts[1], out);
        }
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
    fn solve_vertical_split_tiles_without_gaps() {
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
        // Full width, stacked, summing to the parent height with no gap or overlap.
        assert_eq!((top.width, bot.width), (80, 80));
        assert_eq!((top.x, bot.x), (0, 0));
        assert_eq!(top.y, 0);
        assert_eq!(top.height + bot.height, 40);
        assert_eq!(bot.y, top.y + top.height);
    }

    #[test]
    fn solve_horizontal_split_tiles_without_gaps() {
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
        assert_eq!(left.width + right.width, 80);
        assert_eq!(right.x, left.x + left.width);
    }

    #[test]
    fn solve_nested_split_covers_area_exactly() {
        // A split whose second child is itself a split: all leaves tile the area
        // with no overlap and no gap (total cell area conserved).
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
        let covered: u32 = out.iter().map(|(_, r)| r.width as u32 * r.height as u32).sum();
        assert_eq!(covered, 100 * 30, "leaves cover the area exactly");
    }
}
