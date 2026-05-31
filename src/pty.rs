//! Low-level portable-pty process integration.
//!
//! This module owns the OS-facing handles, reader thread, and child waiter. It
//! has no vt100 parser and no rendering policy.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use portable_pty::{
    Child, ChildKiller, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system,
};

use crate::event::{PaneProcessId, RuntimeEvent};
use crate::session::ShellSpec;

const PTY_CHANNEL_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PtyDrainBudget {
    max_bytes: usize,
    max_chunks: usize,
    max_time: Duration,
}

impl PtyDrainBudget {
    pub(crate) fn interactive() -> Self {
        Self {
            max_bytes: 64 * 1024,
            max_chunks: 32,
            max_time: Duration::from_millis(4),
        }
    }
}

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

    pub(crate) fn process_pending(
        &mut self,
        budget: PtyDrainBudget,
        mut process: impl FnMut(&[u8]),
    ) -> bool {
        self.pty_pending.store(false, Ordering::Release);
        let start = Instant::now();
        let mut bytes = 0usize;
        let mut chunks = 0usize;

        while let Ok(chunk) = self.rx.try_recv() {
            bytes += chunk.len();
            chunks += 1;
            process(&chunk);
            if drain_budget_exhausted(start, bytes, chunks, budget) {
                self.pty_pending.store(true, Ordering::Release);
                return true;
            }
        }
        false
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

fn drain_budget_exhausted(
    start: Instant,
    bytes: usize,
    chunks: usize,
    budget: PtyDrainBudget,
) -> bool {
    bytes >= budget.max_bytes || chunks >= budget.max_chunks || start.elapsed() >= budget.max_time
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_size_never_reaches_zero() {
        assert_eq!(normalize_size(0, 0), (1, 1));
        assert_eq!(normalize_size(24, 80), (24, 80));
    }

    #[test]
    fn drain_budget_yields_on_byte_or_chunk_limits() {
        let budget = PtyDrainBudget {
            max_bytes: 10,
            max_chunks: 3,
            max_time: Duration::from_secs(60),
        };
        let start = Instant::now();
        assert!(!drain_budget_exhausted(start, 9, 2, budget));
        assert!(drain_budget_exhausted(start, 10, 2, budget));
        assert!(drain_budget_exhausted(start, 9, 3, budget));
    }
}
