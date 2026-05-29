//! Live system status for the statusline: CPU, memory, the selected project's
//! git state, and Spark inference-endpoint health. One background poller thread
//! refreshes everything on a timer and publishes the latest snapshot; the 16ms
//! render loop only ever reads it, so a slow or dead network probe can never
//! stall a frame. Transport is std + pure-Rust throughout (sysinfo, gix, ureq).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use sysinfo::System;
use ureq::Agent;

/// How often the poller refreshes. The network probe dominates; 2s keeps the
/// health dot responsive without hammering the endpoint.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Spark liveness endpoint — plain HTTP over the localhost SSH tunnel.
const SPARK_HEALTH_URL: &str = "http://localhost:9292/health";

/// A single coherent reading published to the render loop.
#[derive(Clone)]
pub struct Snapshot {
    pub cpu: f32,        // global CPU usage, 0..=100
    pub mem_used: u64,   // bytes
    pub mem_total: u64,  // bytes
    pub git: GitState,
    pub git_path: PathBuf, // the project dir `git` was computed for (stale guard)
    pub spark: Health,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            cpu: 0.0,
            mem_used: 0,
            mem_total: 0,
            git: GitState::None,
            git_path: PathBuf::new(),
            spark: Health::Unknown,
        }
    }
}

/// Git state of the selected project. `None` = the directory isn't a git repo.
#[derive(Clone)]
pub enum GitState {
    None,
    Repo { branch: String, dirty: TreeState },
}

/// Working-tree state. `Unknown` means the dirty check itself errored, which is
/// distinct from a confirmed-clean tree.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TreeState {
    Clean,
    Dirty,
    Unknown,
}

/// Reachability of the Spark inference endpoint. `Unknown` until the first probe.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Unknown,
    Up,
    Down,
}

/// Owns the poller thread and the two shared slots it reads from / writes to.
pub struct StatusMonitor {
    snapshot: Arc<Mutex<Snapshot>>,
    selected: Arc<Mutex<PathBuf>>,
}

impl StatusMonitor {
    /// Spawn the poller, pointed at `initial`. The thread is detached and dies
    /// with the process; a TUI needs no graceful join.
    pub fn spawn(initial: PathBuf) -> Self {
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let selected = Arc::new(Mutex::new(initial));
        let (snap, sel) = (Arc::clone(&snapshot), Arc::clone(&selected));
        thread::spawn(move || poll_loop(snap, sel));
        Self { snapshot, selected }
    }

    /// Repoint the poller when the nav selection moves. Latest write wins.
    pub fn set_selected(&self, path: PathBuf) {
        if let Ok(mut slot) = self.selected.lock() {
            *slot = path;
        }
    }

    /// The most recent snapshot. The lock is held only long enough to clone.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }
}

fn poll_loop(snapshot: Arc<Mutex<Snapshot>>, selected: Arc<Mutex<PathBuf>>) {
    let mut sys = System::new();
    let agent: Agent = Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .timeout_connect(Some(Duration::from_millis(800)))
        .build()
        .into();

    // Prime CPU, then let the platform's minimum sampling interval elapse before
    // the first real refresh. sysinfo computes usage as a delta between refreshes
    // and skips the recompute if less than MINIMUM_CPU_UPDATE_INTERVAL has passed,
    // so without this sleep the first published reading would be a cold 0%.
    sys.refresh_cpu_usage();
    thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);

    loop {
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let path = selected.lock().map(|p| p.clone()).unwrap_or_default();
        let git = read_git(&path);

        let next = Snapshot {
            cpu: sys.global_cpu_usage(),
            mem_used: sys.used_memory(),
            mem_total: sys.total_memory(),
            git,
            git_path: path,
            spark: probe(&agent),
        };

        if let Ok(mut slot) = snapshot.lock() {
            *slot = next;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Branch name + dirty flag for a project directory. gix's `is_dirty` counts
/// index and working-tree changes to tracked files; untracked-only changes do
/// not flip it, which is the right granularity for an at-a-glance dot.
///
/// Note (v0.1): `is_dirty` walks the full worktree on every poll. It runs on
/// this background thread so it never stalls a frame, but on a very large tree
/// it churns a core. Acceptable for now; a later pass can cache by path/mtime or
/// poll git on a slower cadence than the health probe.
fn read_git(path: &Path) -> GitState {
    let Ok(repo) = gix::open(path) else {
        return GitState::None;
    };
    let branch = match repo.head_name() {
        Ok(Some(name)) => name.shorten().to_string(),
        _ => "detached".to_string(), // detached HEAD or no commits yet
    };
    let dirty = match repo.is_dirty() {
        Ok(true) => TreeState::Dirty,
        Ok(false) => TreeState::Clean,
        Err(_) => TreeState::Unknown, // surface the failure rather than faking "clean"
    };
    GitState::Repo { branch, dirty }
}

/// A short, bounded GET against the Spark liveness route. Any non-success or
/// transport error (tunnel down, endpoint cold) reads as `Down`.
fn probe(agent: &Agent) -> Health {
    match agent.get(SPARK_HEALTH_URL).call() {
        Ok(resp) if resp.status().is_success() => Health::Up,
        _ => Health::Down,
    }
}
