//! The shell workspace: the tiled pane region of the layout, its split-tree, and
//! its input.
//!
//! Holds a `PaneId`-keyed collection of shells laid out by a recursive split-tree
//! (`layout::Node`). `render` solves the tree into `(PaneId, Rect)` pairs and draws
//! each pane; because the whole resize+draw loop lives here behind one `&mut self`
//! call, the panes' mutable borrows never alias `App`'s chrome fields (projects,
//! viewer scroll) read in the same frame.
//!
//! For the Phase 2b tracer the tree is a single leaf, so this renders pixel-identical
//! to the pre-tree single shell. Splits, close, focus traversal, and tree-zoom build
//! on the same collection in Phase 3. Storage is the concrete `Shell` (not an `enum
//! Pane`/`dyn Pane`) because Shell is the only pane type through the Phase 4 STOP
//! line — the floating scratch pane is also a Shell. Promoting to a `Pane` enum or
//! trait is a localized change at the first non-Shell pane.

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use tui_term::widget::PseudoTerminal;

use crate::layout::{Node, PaneId, solve};
use crate::shell::{Shell, encode_key};
use crate::{content_area, draw_label};

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
}

impl Workspace {
    pub fn new(shell: Shell) -> Self {
        let id = PaneId(0);
        let mut panes = BTreeMap::new();
        panes.insert(id, shell);
        Self {
            panes,
            tree: Node::Leaf(id),
            focus: id,
            shell_rows: 24,
            copy_mode: false,
        }
    }

    /// Whether the focused pane is currently in copy-mode (drives the statusline hint).
    pub fn copy_mode(&self) -> bool {
        self.copy_mode
    }

    /// Render the tiled panes into `area`: solve the tree, then for each leaf size
    /// its PTY to the content region, draw the label (with the copy-mode marker on
    /// the focused pane), and paint its live screen. `active` is whether the shell
    /// region holds app focus, so the focus highlight only shows when it does.
    pub fn render(&mut self, f: &mut Frame, area: Rect, active: bool) {
        for (id, rect) in solve(&self.tree, area) {
            let Some(shell) = self.panes.get_mut(&id) else {
                continue;
            };
            let content = content_area(rect);
            shell.resize(content.height, content.width);
            let is_focus = id == self.focus;
            if is_focus {
                self.shell_rows = content.height;
            }
            // The copy-mode position marker (rows above the live bottom) shows only
            // on the focused pane, which is the one copy-mode scrolls.
            let title = if self.copy_mode && is_focus {
                format!("SHELL  ↑{}", shell.scrollback_offset())
            } else {
                "SHELL".to_string()
            };
            draw_label(f, rect, &title, active && is_focus);
            f.render_widget(PseudoTerminal::new(shell.screen()), content);
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
