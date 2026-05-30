//! The shell workspace: the tiled pane region of the layout, its split-tree, and
//! its input.
//!
//! Holds a `PaneId`-keyed collection of shells laid out by a recursive split-tree
//! (`layout::Node`). `render` solves the tree into `(PaneId, Rect)` pairs and draws
//! each pane; because the whole resize+draw loop lives here behind one `&mut self`
//! call, the panes' mutable borrows never alias `App`'s chrome fields (projects,
//! viewer scroll) read in the same frame.
//!
//! Splits spawn a fresh shell into a new leaf, focus cycles in layout order, close
//! collapses a leaf into its sibling, and tree-zoom fills the region with the
//! focused leaf. Storage is the concrete `Shell` (not an `enum Pane`/`dyn Pane`)
//! because Shell is the only pane type through the Phase 4 STOP line — the floating
//! scratch pane is also a Shell. Promoting to a `Pane` enum or trait is a localized
//! change at the first non-Shell pane.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::Sender;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Direction, Rect};
use ratatui::style::Style;
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::Msg;
use crate::layout::{self, CloseOutcome, Node, PaneId, solve};
use crate::shell::{Shell, encode_key};
use crate::theme::Theme;
use crate::ui::{content_area, draw_label};

pub struct Workspace {
    panes: BTreeMap<PaneId, Shell>,
    tree: Node,
    /// Which pane in the tree currently holds focus (active when the shell region
    /// has app focus).
    focus: PaneId,
    /// The focused pane's content height from the last frame, used as the copy-mode
    /// page size (input handling has no `Frame`).
    shell_rows: u16,
    /// Copy-mode (scrollback view): true while the focused pane is scrolled back from
    /// the live bottom. Scroll-only for now; text selection/yank lands with Phase 6.
    copy_mode: bool,
    /// Tree-zoom: when true the focused leaf fills the whole workspace region and the
    /// tiled siblings (and their separators) are hidden until un-zoomed.
    zoomed: bool,
    /// Monotonic `PaneId` allocator. A closed pane's id is never reused in a session.
    next_id: u64,
    /// Per-pane scrollback depth for shells spawned by `split_focused`.
    scrollback: usize,
    /// A clone of the event-loop sender, so a split's new shell can wake the loop.
    notify: Sender<Msg>,
}

impl Workspace {
    pub fn new(shell: Shell, scrollback: usize, notify: Sender<Msg>) -> Self {
        let id = PaneId(0);
        let mut panes = BTreeMap::new();
        panes.insert(id, shell);
        Self {
            panes,
            tree: Node::Leaf(id),
            focus: id,
            shell_rows: 24,
            copy_mode: false,
            zoomed: false,
            next_id: 1,
            scrollback,
            notify,
        }
    }

    /// Whether the focused pane is currently in copy-mode (drives the statusline hint).
    pub fn copy_mode(&self) -> bool {
        self.copy_mode
    }

    /// Whether tree-zoom is active (drives the layout the workspace is given).
    pub fn zoomed(&self) -> bool {
        self.zoomed
    }

    /// Render the tiled panes into `area`: solve the tree, then draw each leaf and
    /// the hairline separators between them. `active` is whether the shell region
    /// holds app focus, so the focus highlight only shows when it does. When zoomed,
    /// only the focused leaf is drawn, filling the whole region.
    pub fn render(&mut self, f: &mut Frame, area: Rect, active: bool) {
        if self.zoomed {
            self.render_leaf(f, self.focus, area, active);
            return;
        }
        let solved = solve(&self.tree, area);
        for (id, rect) in &solved {
            self.render_leaf(f, *id, *rect, active);
        }
        self.draw_separators(f, area);
    }

    /// Draw one leaf: size its PTY to the content rect, draw its label (with the
    /// copy-mode marker when focused), and paint its live screen.
    fn render_leaf(&mut self, f: &mut Frame, id: PaneId, rect: Rect, active: bool) {
        let is_focus = id == self.focus;
        let Some(shell) = self.panes.get_mut(&id) else {
            // A leaf whose PaneId is not in the collection means the tree and the
            // pane map have desynced — make it loud rather than silently skipping.
            debug_assert!(false, "workspace desync: leaf {id:?} absent from pane map");
            crate::log::error(&format!(
                "workspace desync: leaf {id:?} absent from pane map"
            ));
            return;
        };
        let content = content_area(rect);
        shell.resize(content.height, content.width);
        if is_focus {
            self.shell_rows = content.height;
        }
        // The copy-mode position marker (rows above the live bottom) shows only on
        // the focused pane, which is the one copy-mode scrolls.
        let title = if self.copy_mode && is_focus {
            format!("SHELL  ↑{}", shell.scrollback_offset())
        } else {
            "SHELL".to_string()
        };
        draw_label(f, rect, &title, active && is_focus);
        // Cursor suppression: tui-term paints each terminal's cursor as a buffer
        // cell, so only the focused leaf should show one — otherwise every pane
        // sprouts a block cursor.
        let term = PseudoTerminal::new(shell.screen())
            .cursor(Cursor::default().visibility(active && is_focus));
        f.render_widget(term, content);
    }

    /// Paint the carved separators as hairlines, resolving the box-drawing junction
    /// glyph wherever perpendicular separators touch (so a nested split's divider
    /// joins its parent's with ┬/├/┼ instead of a broken cross).
    fn draw_separators(&self, f: &mut Frame, area: Rect) {
        let seps = layout::separators(&self.tree, area);
        if seps.is_empty() {
            return;
        }
        // Accumulate per-cell direction bits: a horizontal line contributes LEFT|RIGHT,
        // a vertical line UP|DOWN.
        let mut bits: HashMap<(u16, u16), u8> = HashMap::new();
        for s in &seps {
            let axis = if s.horizontal {
                LEFT | RIGHT
            } else {
                UP | DOWN
            };
            for y in s.rect.y..s.rect.y.saturating_add(s.rect.height) {
                for x in s.rect.x..s.rect.x.saturating_add(s.rect.width) {
                    *bits.entry((x, y)).or_insert(0) |= axis;
                }
            }
        }
        // Connect touching separators: where a cell's orthogonal neighbor is also a
        // separator, add the bit pointing at it, turning crossings into ┼/├/┬ glyphs.
        let cells: Vec<(u16, u16)> = bits.keys().copied().collect();
        for (x, y) in cells {
            let mut b = bits[&(x, y)];
            if x > 0 && bits.contains_key(&(x - 1, y)) {
                b |= LEFT;
            }
            if bits.contains_key(&(x + 1, y)) {
                b |= RIGHT;
            }
            if y > 0 && bits.contains_key(&(x, y - 1)) {
                b |= UP;
            }
            if bits.contains_key(&(x, y + 1)) {
                b |= DOWN;
            }
            bits.insert((x, y), b);
        }
        let hair = Style::default().fg(Theme::HAIR);
        for ((x, y), b) in bits {
            if let Some(cell) = f.buffer_mut().cell_mut((x, y)) {
                cell.set_symbol(junction(b)).set_style(hair);
            }
        }
    }

    /// Drain any bytes the shells emitted since the last frame (on `Msg::Pty`). Each
    /// Shell has its own edge-triggered wakeup, so draining every pane is correct and
    /// cheap — a pane with nothing pending just no-ops its channel drain.
    pub fn process_pending(&mut self) {
        for shell in self.panes.values_mut() {
            shell.process_pending();
        }
    }

    /// Reap panes whose shell process has exited (typed `exit`, killed). Each is
    /// collapsed into its sibling via the same close path. Returns whether any pane
    /// remains — false means the workspace is empty and the app should quit. Loops so
    /// several simultaneous exits are all cleared in one pass.
    pub fn reap_dead(&mut self) -> bool {
        loop {
            let dead: Vec<PaneId> = self
                .panes
                .iter()
                .filter(|(_, s)| !s.is_alive())
                .map(|(id, _)| *id)
                .collect();
            if dead.is_empty() {
                break;
            }
            for id in dead {
                self.close_pane(id);
            }
            if self.panes.is_empty() {
                break;
            }
        }
        !self.panes.is_empty()
    }

    /// Split the focused pane along `dir`, spawning a fresh shell into the new leaf
    /// and moving focus to it. A spawn failure is logged and leaves the layout intact.
    pub fn split_focused(&mut self, dir: Direction) {
        let new_id = PaneId(self.next_id);
        let shell = match Shell::spawn(24, 80, self.scrollback, self.notify.clone()) {
            Ok(s) => s,
            Err(e) => {
                crate::log::error(&format!("split: shell spawn failed: {e}"));
                return;
            }
        };
        if layout::split_leaf(&mut self.tree, self.focus, new_id, dir) {
            self.next_id += 1;
            self.panes.insert(new_id, shell);
            // Leaving the old focus context drops any copy-mode scrollback.
            self.exit_copy_mode();
            self.focus = new_id;
        } else {
            crate::log::error(&format!("split: focus {:?} is not a tree leaf", self.focus));
        }
    }

    /// Close the focused pane, collapsing it into its sibling. Returns whether any
    /// pane remains — false means the workspace is empty and the app should quit.
    pub fn close_focused(&mut self) -> bool {
        self.close_pane(self.focus)
    }

    fn close_pane(&mut self, id: PaneId) -> bool {
        match layout::close_leaf(&mut self.tree, id) {
            CloseOutcome::Closed(survivor) => {
                self.panes.remove(&id);
                if self.focus == id {
                    self.copy_mode = false;
                    self.focus = survivor;
                }
            }
            CloseOutcome::WasLast => {
                // The only pane: the tree keeps its Leaf(id), but the shell is gone,
                // so the map empties and the caller quits the app.
                self.panes.remove(&id);
            }
            CloseOutcome::NotFound => {
                crate::log::error(&format!("close: {id:?} not found in tree"));
            }
        }
        !self.panes.is_empty()
    }

    /// Cycle focus to the next pane in layout order (wrapping).
    pub fn focus_next(&mut self) {
        self.cycle_focus(1);
    }

    /// Cycle focus to the previous pane in layout order (wrapping).
    pub fn focus_prev(&mut self) {
        self.cycle_focus(-1);
    }

    fn cycle_focus(&mut self, step: isize) {
        let order = layout::leaves(&self.tree);
        if order.len() < 2 {
            return;
        }
        let Some(pos) = order.iter().position(|&id| id == self.focus) else {
            return;
        };
        let len = order.len() as isize;
        let next = (pos as isize + step).rem_euclid(len) as usize;
        // Changing focus drops the old pane's scrollback view back to live.
        self.exit_copy_mode();
        self.focus = order[next];
    }

    /// Grow (positive) or shrink (negative) the focused pane by nudging its parent
    /// split's ratio; the clamp to `1..=99` lives in `layout::nudge_ratio`.
    pub fn nudge_focused(&mut self, delta: i16) {
        layout::nudge_ratio(&mut self.tree, self.focus, delta);
    }

    /// Toggle tree-zoom on the focused pane; returns the new zoom state.
    pub fn toggle_zoom(&mut self) -> bool {
        self.zoomed = !self.zoomed;
        self.zoomed
    }

    /// Drop any scrollback view back to the live bottom and leave copy-mode. Called
    /// when focus leaves the shell so it never returns mid-scroll.
    pub fn exit_copy_mode(&mut self) {
        if self.copy_mode {
            if let Some(shell) = self.panes.get_mut(&self.focus) {
                shell.scroll_to_bottom();
            }
            self.copy_mode = false;
        }
    }

    /// Key handling while the shell is focused — routed to the focused pane. Normally
    /// every key passes through to the PTY. PageUp opens copy-mode (the scrollback
    /// view) when there is history to show; in copy-mode the arrows and Page keys
    /// move the view, Esc snaps back to live, and any other key snaps to live AND
    /// passes through, so you just start typing to resume.
    pub fn on_key(&mut self, key: KeyEvent) {
        let page = self.shell_rows.max(1) as i64;
        let Some(shell) = self.panes.get_mut(&self.focus) else {
            debug_assert!(
                false,
                "workspace desync: focus {:?} absent from map",
                self.focus
            );
            crate::log::error(&format!(
                "workspace desync: focus {:?} absent from pane map",
                self.focus
            ));
            return;
        };
        if self.copy_mode {
            match key.code {
                KeyCode::PageUp => shell.scroll_by(page),
                KeyCode::Up => shell.scroll_by(1),
                KeyCode::PageDown => {
                    shell.scroll_by(-page);
                    self.copy_mode = shell.scrollback_offset() > 0;
                }
                KeyCode::Down => {
                    shell.scroll_by(-1);
                    self.copy_mode = shell.scrollback_offset() > 0;
                }
                KeyCode::Esc => {
                    shell.scroll_to_bottom();
                    self.copy_mode = false;
                }
                _ => {
                    shell.scroll_to_bottom();
                    self.copy_mode = false;
                    pty_send(shell, key);
                }
            }
            return;
        }
        // Not in copy-mode: PageUp tries to open the scrollback view. If the offset
        // actually moves, there is history (primary buffer) and we enter copy-mode.
        // If it stays at 0 we are on the alternate screen — vt100 pins scrollback
        // there — so the key belongs to the app (vim/less paging) and falls through.
        if key.code == KeyCode::PageUp {
            shell.scroll_by(page);
            if shell.scrollback_offset() > 0 {
                self.copy_mode = true;
                return;
            }
        }
        pty_send(shell, key);
    }
}

/// Encode a key and write it to a shell's PTY (no-op for keys that don't map).
fn pty_send(shell: &mut Shell, key: KeyEvent) {
    let bytes = encode_key(key);
    if !bytes.is_empty() {
        shell.send_input(&bytes);
    }
}

// Direction bits for a separator cell, OR-ed to pick the box-drawing junction glyph.
const UP: u8 = 1;
const DOWN: u8 = 2;
const LEFT: u8 = 4;
const RIGHT: u8 = 8;

/// The box-drawing glyph for a cell's connected directions. A lone axis falls back
/// to a straight line so an unconnected separator still renders cleanly.
fn junction(bits: u8) -> &'static str {
    match bits {
        b if b == DOWN | RIGHT => "┌",
        b if b == DOWN | LEFT => "┐",
        b if b == UP | RIGHT => "└",
        b if b == UP | LEFT => "┘",
        b if b == UP | DOWN | RIGHT => "├",
        b if b == UP | DOWN | LEFT => "┤",
        b if b == DOWN | LEFT | RIGHT => "┬",
        b if b == UP | LEFT | RIGHT => "┴",
        b if b == UP | DOWN | LEFT | RIGHT => "┼",
        b if b & (UP | DOWN) != 0 => "│",
        _ => "─",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junction_resolves_crossings_and_tees() {
        assert_eq!(junction(LEFT | RIGHT), "─");
        assert_eq!(junction(UP | DOWN), "│");
        assert_eq!(junction(UP | DOWN | LEFT | RIGHT), "┼");
        assert_eq!(junction(DOWN | LEFT | RIGHT), "┬");
        assert_eq!(junction(UP | LEFT | RIGHT), "┴");
        assert_eq!(junction(UP | DOWN | RIGHT), "├");
        assert_eq!(junction(DOWN | RIGHT), "┌");
    }
}
