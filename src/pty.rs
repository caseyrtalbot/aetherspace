//! Low-level portable-pty process integration.
//!
//! This module owns the OS-facing handles, reader thread, and child waiter. It
//! has no vt100 parser and no rendering policy.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use portable_pty::{
    Child, ChildKiller, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system,
};

use crate::event::{PaneProcessId, RuntimeEvent};
use crate::session::ShellSpec;

const PTY_CHANNEL_DEPTH: usize = 64;

pub(crate) struct PtyProcess {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    rx: Receiver<Vec<u8>>,
    pty_pending: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    exit_status: Arc<Mutex<Option<ExitStatus>>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    rows: u16,
    cols: u16,
}

impl PtyProcess {
    pub(crate) fn spawn(
        id: PaneProcessId,
        spec: &ShellSpec,
        rows: u16,
        cols: u16,
        notify: Sender<RuntimeEvent>,
    ) -> Result<Self> {
        let (rows, cols) = normalize_size(rows, cols);
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut command = CommandBuilder::new_default_prog();
        command.cwd(spec.cwd.as_os_str());

        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let master = pair.master;

        let (tx, rx) = sync_channel::<Vec<u8>>(PTY_CHANNEL_DEPTH);
        let pty_pending = Arc::new(AtomicBool::new(false));
        spawn_reader(id, reader, tx, Arc::clone(&pty_pending), notify.clone());

        let alive = Arc::new(AtomicBool::new(true));
        let exit_status = Arc::new(Mutex::new(None));
        let killer = child.clone_killer();
        spawn_waiter(
            id,
            child,
            Arc::clone(&alive),
            Arc::clone(&exit_status),
            notify,
        );

        Ok(Self {
            writer,
            master,
            rx,
            pty_pending,
            alive,
            exit_status,
            killer,
            rows,
            cols,
        })
    }

    pub(crate) fn process_pending(&mut self, mut process: impl FnMut(&[u8])) {
        self.pty_pending.store(false, Ordering::Release);
        while let Ok(chunk) = self.rx.try_recv() {
            process(&chunk);
        }
    }

    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = normalize_size(rows, cols);
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
    }

    pub(crate) fn send_input(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub(crate) fn terminate(&mut self) {
        let _ = self.killer.kill();
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub(crate) fn exit_status_text(&self) -> Option<String> {
        self.exit_status
            .lock()
            .ok()
            .and_then(|status| status.as_ref().map(ToString::to_string))
    }
}

fn spawn_reader(
    id: PaneProcessId,
    mut reader: Box<dyn Read + Send>,
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    pending: Arc<AtomicBool>,
    notify: Sender<RuntimeEvent>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                    if !pending.swap(true, Ordering::AcqRel) {
                        let _ = notify.send(RuntimeEvent::Pty(id));
                    }
                }
            }
        }
    });
}

fn spawn_waiter(
    id: PaneProcessId,
    mut child: Box<dyn Child + Send + Sync>,
    alive: Arc<AtomicBool>,
    exit_status: Arc<Mutex<Option<ExitStatus>>>,
    notify: Sender<RuntimeEvent>,
) {
    thread::spawn(move || {
        let status = child.wait();
        if let Ok(status) = status
            && let Ok(mut slot) = exit_status.lock()
        {
            *slot = Some(status);
        }
        alive.store(false, Ordering::Release);
        let _ = notify.send(RuntimeEvent::ChildExit(id));
    });
}

fn normalize_size(rows: u16, cols: u16) -> (u16, u16) {
    (rows.max(1), cols.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_size_never_reaches_zero() {
        assert_eq!(normalize_size(0, 0), (1, 1));
        assert_eq!(normalize_size(24, 80), (24, 80));
    }
}
