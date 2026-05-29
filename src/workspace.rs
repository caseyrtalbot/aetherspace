//! The shell workspace: the embedded-PTY region of the layout and its input.
//!
//! Extracted from `App` so the pane's `&mut` render/resize loop borrows only the
//! workspace, never aliasing `App`'s chrome fields (projects, viewer scroll). That
//! separation is what lets a future pane *collection* be iterated mutably while the
//! statusline and nav read `&App` in the same frame (Phase 2b). For now it holds a
//! single `Shell`; the `Node` split-tree and a `PaneId` collection land next.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use tui_term::widget::PseudoTerminal;

use crate::shell::{Shell, encode_key};
use crate::{content_area, draw_label};

pub struct Workspace {
    shell: Shell,
    /// The shell pane's content height from the last frame, used as the copy-mode
    /// page size (input handling has no `Frame`).
    shell_rows: u16,
    /// Copy-mode (scrollback view): true while scrolled back from the live bottom.
    /// Scroll-only for now; text selection/yank lands with Phase 6.
    copy_mode: bool,
}

impl Workspace {
    pub fn new(shell: Shell) -> Self {
        Self {
            shell,
            shell_rows: 24,
            copy_mode: false,
        }
    }

    /// Whether the shell is currently in copy-mode (drives the statusline hint).
    pub fn copy_mode(&self) -> bool {
        self.copy_mode
    }

    /// Render the shell pane into `area`: size the PTY to the content region, draw
    /// the label (with the copy-mode position marker when scrolled back), and paint
    /// the live screen. Owns the resize+draw loop so callers never hold a pane
    /// borrow across the chrome reads in the same frame.
    pub fn render(&mut self, f: &mut Frame, area: Rect, focused: bool) {
        let content = content_area(area);
        self.shell.resize(content.height, content.width);
        self.shell_rows = content.height;
        // In copy-mode the label carries the scrollback position marker (rows
        // above the live bottom).
        let title = if self.copy_mode {
            format!("SHELL  ↑{}", self.shell.scrollback_offset())
        } else {
            "SHELL".to_string()
        };
        draw_label(f, area, &title, focused);
        f.render_widget(PseudoTerminal::new(self.shell.screen()), content);
    }

    /// Drain any bytes the shell emitted since the last frame (on `Msg::Pty`).
    pub fn process_pending(&mut self) {
        self.shell.process_pending();
    }

    /// Drop any scrollback view back to the live bottom and leave copy-mode. Called
    /// when focus leaves the shell so it never returns mid-scroll.
    pub fn exit_copy_mode(&mut self) {
        if self.copy_mode {
            self.shell.scroll_to_bottom();
            self.copy_mode = false;
        }
    }

    /// Key handling while the shell is focused. Normally every key passes through to
    /// the PTY. PageUp opens copy-mode (the scrollback view) when there is history
    /// to show; in copy-mode the arrows and Page keys move the view, Esc snaps back
    /// to live, and any other key snaps to live AND passes through, so you just
    /// start typing to resume.
    pub fn on_key(&mut self, key: KeyEvent) {
        let page = self.shell_rows.max(1) as i64;
        if self.copy_mode {
            match key.code {
                KeyCode::PageUp => self.shell.scroll_by(page),
                KeyCode::Up => self.shell.scroll_by(1),
                KeyCode::PageDown => {
                    self.shell.scroll_by(-page);
                    self.copy_mode = self.shell.scrollback_offset() > 0;
                }
                KeyCode::Down => {
                    self.shell.scroll_by(-1);
                    self.copy_mode = self.shell.scrollback_offset() > 0;
                }
                KeyCode::Esc => {
                    self.shell.scroll_to_bottom();
                    self.copy_mode = false;
                }
                _ => {
                    self.shell.scroll_to_bottom();
                    self.copy_mode = false;
                    self.pty_send(key);
                }
            }
            return;
        }
        // Not in copy-mode: PageUp tries to open the scrollback view. If the offset
        // actually moves, there is history (primary buffer) and we enter copy-mode.
        // If it stays at 0 we are on the alternate screen — vt100 pins scrollback
        // there — so the key belongs to the app (vim/less paging) and falls through.
        if key.code == KeyCode::PageUp {
            self.shell.scroll_by(page);
            if self.shell.scrollback_offset() > 0 {
                self.copy_mode = true;
                return;
            }
        }
        self.pty_send(key);
    }

    /// Encode a key and write it to the PTY (no-op for keys that don't map).
    fn pty_send(&mut self, key: KeyEvent) {
        let bytes = encode_key(key);
        if !bytes.is_empty() {
            self.shell.send_input(&bytes);
        }
    }
}
