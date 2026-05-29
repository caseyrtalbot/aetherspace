//! Embedded real shell: a `$SHELL` child running inside a PTY, its output
//! parsed by vt100 into a terminal screen that tui-term renders into a Ratatui
//! pane. Stack: portable-pty (PTY + spawn) → vt100 (state machine) → tui-term
//! (PseudoTerminal widget). vt100 comes from tui-term's own re-export so the
//! Screen type always matches the widget's expected version.

use std::io::{Read, Write};
use std::sync::mpsc::{channel, Receiver};
use std::thread;

use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, Child, MasterPty, PtySize};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_term::vt100;

pub struct Shell {
    parser: vt100::Parser,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    rx: Receiver<Vec<u8>>,
    rows: u16,
    cols: u16,
    _child: Box<dyn Child + Send + Sync>, // kept alive so the shell process lives
}

impl Shell {
    /// Open a PTY, spawn the user's default shell, and start draining its
    /// output on a background thread into a channel.
    pub fn spawn(rows: u16, cols: u16) -> Result<Self> {
        let (rows, cols) = (rows.max(1), cols.max(1));
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let child = pair.slave.spawn_command(CommandBuilder::new_default_prog())?;
        // Drop the slave so the child receives EOF cleanly when it exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let master = pair.master;

        let (tx, rx) = channel::<Vec<u8>>();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break, // EOF or read error → shell ended
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break; // receiver gone (app shutting down)
                        }
                    }
                }
            }
        });

        Ok(Self {
            parser: vt100::Parser::new(rows, cols, 0),
            writer,
            master,
            rx,
            rows,
            cols,
            _child: child,
        })
    }

    /// Feed any bytes the shell has emitted since the last frame into the parser.
    pub fn process_pending(&mut self) {
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
