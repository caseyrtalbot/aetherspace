//! Embedded real shell: a `$SHELL` child running inside a PTY, its output
//! parsed by vt100 into a terminal screen that tui-term renders into a Ratatui
//! pane. Stack: portable-pty (PTY + spawn) → vt100 (state machine) → tui-term
//! (PseudoTerminal widget). vt100 comes from tui-term's own re-export so the
//! Screen type always matches the widget's expected version.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, sync_channel};
use std::thread;

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_term::vt100;

use crate::Msg;

/// Bound on the PTY byte channel: backpressure so a flood (`cat bigfile`, `yes`)
/// can't grow memory unbounded. The reader blocks once 64 chunks are unread; the
/// event loop drains them each wake. 64 * 8 KiB ≈ 512 KiB worst case in flight.
const PTY_CHANNEL_DEPTH: usize = 64;

pub struct Shell {
    parser: vt100::Parser,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    rx: Receiver<Vec<u8>>,
    // Edge-trigger for the wakeup: true while a Msg::Pty is outstanding. Lets a
    // byte flood coalesce to one wakeup instead of one per chunk (see spawn).
    pty_pending: Arc<AtomicBool>,
    rows: u16,
    cols: u16,
    _child: Box<dyn Child + Send + Sync>, // kept alive so the shell process lives
}

impl Shell {
    /// Open a PTY, spawn the user's default shell, and start draining its output
    /// on a background thread into a bounded channel. `notify` wakes the event loop
    /// (Msg::Pty) when bytes land, so the loop can sleep at idle instead of polling.
    ///
    /// The wakeup is *edge-triggered*: the reader sets `pty_pending` and sends
    /// Msg::Pty only on the false→true transition, so a saturating stream (`yes`,
    /// `cat bigfile`) produces one outstanding wakeup, not one per chunk. Without
    /// this, the loop's coalesce-drain could be re-fed faster than it drains and
    /// never reach the draw, stalling output and the quit check. The flag is
    /// cleared in `process_pending` before the byte channel is drained, so bytes
    /// arriving mid-drain re-arm a fresh wakeup.
    pub fn spawn(rows: u16, cols: u16, scrollback: usize, notify: Sender<Msg>) -> Result<Self> {
        let (rows, cols) = (rows.max(1), cols.max(1));
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let child = pair
            .slave
            .spawn_command(CommandBuilder::new_default_prog())?;
        // Drop the slave so the child receives EOF cleanly when it exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let master = pair.master;

        let (tx, rx) = sync_channel::<Vec<u8>>(PTY_CHANNEL_DEPTH);
        let pty_pending = Arc::new(AtomicBool::new(false));
        let pending = Arc::clone(&pty_pending);
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break, // EOF or read error → shell ended
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break; // receiver gone (app shutting down)
                        }
                        // Edge-triggered wakeup: notify only when none is pending.
                        // Ignore send errors (gone receiver = shutting down).
                        if !pending.swap(true, Ordering::AcqRel) {
                            let _ = notify.send(Msg::Pty);
                        }
                    }
                }
            }
        });

        Ok(Self {
            parser: vt100::Parser::new(rows, cols, scrollback),
            writer,
            master,
            rx,
            pty_pending,
            rows,
            cols,
            _child: child,
        })
    }

    /// Feed any bytes the shell has emitted since the last frame into the parser.
    /// Clears the wakeup flag *before* draining, so a chunk that arrives mid-drain
    /// re-arms a fresh Msg::Pty and is never lost.
    pub fn process_pending(&mut self) {
        self.pty_pending.store(false, Ordering::Release);
        while let Ok(chunk) = self.rx.try_recv() {
            self.parser.process(&chunk);
        }
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Keep the PTY and the parser's screen sized to the visible pane.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.parser.screen_mut().set_size(rows, cols);
    }

    pub fn send_input(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Move the scrollback view by `delta` rows: positive scrolls up into history,
    /// negative scrolls back toward the live bottom. The lower bound (0 = live) is
    /// ours; the upper bound is vt100's, clamped inside `set_scrollback` to the
    /// rows actually stored. On the alternate screen vt100 pins scrollback at 0
    /// (screen.rs), so this is a no-op there — the caller uses that to tell a
    /// scrollable primary buffer from an alt-screen app.
    pub fn scroll_by(&mut self, delta: i64) {
        let target = scroll_delta(self.parser.screen().scrollback(), delta);
        self.parser.screen_mut().set_scrollback(target);
    }

    /// Snap the view back to the live bottom (scrollback offset 0).
    pub fn scroll_to_bottom(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    /// Current scrollback offset in rows: 0 at the live bottom, higher further
    /// back. Drives the copy-mode position marker and the alt-screen detection.
    pub fn scrollback_offset(&self) -> usize {
        self.parser.screen().scrollback()
    }
}

/// New scrollback offset after moving `delta` rows (positive = further into
/// history). Clamped at 0, the live bottom; the upper bound belongs to vt100 and
/// is applied by `set_scrollback`. Pure, so the copy-mode scroll math is tested
/// without a live PTY.
fn scroll_delta(current: usize, delta: i64) -> usize {
    (current as i64 + delta).max(0) as usize
}

/// Translate a crossterm key event into the byte sequence a PTY expects.
/// Covers printable input, Ctrl-combos, and the common control/navigation keys.
pub fn encode_key(key: KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Ctrl-A..Ctrl-Z → 0x01..0x1a, etc.
                vec![(c.to_ascii_uppercase() as u8).wrapping_sub(0x40) & 0x1f]
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_letters_map_to_control_bytes() {
        assert_eq!(encode_key(ctrl(KeyCode::Char('a'))), vec![0x01]);
        assert_eq!(encode_key(ctrl(KeyCode::Char('z'))), vec![0x1a]);
        // case-insensitive: Ctrl-C is 0x03 regardless of shift.
        assert_eq!(encode_key(ctrl(KeyCode::Char('C'))), vec![0x03]);
    }

    #[test]
    fn enter_and_backspace() {
        assert_eq!(encode_key(key(KeyCode::Enter)), vec![b'\r']);
        assert_eq!(encode_key(key(KeyCode::Backspace)), vec![0x7f]);
    }

    #[test]
    fn arrows_emit_csi() {
        assert_eq!(encode_key(key(KeyCode::Up)), b"\x1b[A".to_vec());
        assert_eq!(encode_key(key(KeyCode::Down)), b"\x1b[B".to_vec());
        assert_eq!(encode_key(key(KeyCode::Right)), b"\x1b[C".to_vec());
        assert_eq!(encode_key(key(KeyCode::Left)), b"\x1b[D".to_vec());
    }

    #[test]
    fn plain_char_is_its_byte() {
        assert_eq!(encode_key(key(KeyCode::Char('a'))), vec![b'a']);
    }

    #[test]
    fn multibyte_char_is_utf8() {
        assert_eq!(encode_key(key(KeyCode::Char('é'))), "é".as_bytes().to_vec());
    }

    #[test]
    fn unmapped_key_is_empty() {
        assert!(encode_key(key(KeyCode::F(5))).is_empty());
    }

    #[test]
    fn scroll_delta_moves_into_history() {
        assert_eq!(scroll_delta(0, 10), 10);
        assert_eq!(scroll_delta(5, 3), 8);
    }

    #[test]
    fn scroll_delta_clamps_at_live_bottom() {
        // Scrolling down past the live bottom never goes negative.
        assert_eq!(scroll_delta(3, -10), 0);
        assert_eq!(scroll_delta(0, -1), 0);
    }
}
