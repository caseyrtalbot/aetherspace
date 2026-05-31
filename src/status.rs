//! Runtime-only status polling for the compact statusline.
//!
//! This module deliberately keeps status state out of the durable session model.
//! Probes, system load, and git state are sampled on a background thread and
//! sent through the runtime queue like any other external event.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc::Sender};
use std::thread;
use std::time::{Duration, Instant};

use sysinfo::System;

use crate::config::{Config, PollCfg, ProbeEntry};
use crate::event::RuntimeEvent;

const HEALTH_TIMEOUT: Duration = Duration::from_millis(800);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StatusSnapshot {
    pub(crate) system: Option<SystemSnapshot>,
    pub(crate) git: Option<GitSnapshot>,
    pub(crate) probes: Vec<ProbeSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SystemSnapshot {
    pub(crate) cpu_percent: u8,
    pub(crate) memory_percent: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitSnapshot {
    pub(crate) branch: String,
    pub(crate) dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeSnapshot {
    pub(crate) name: String,
    pub(crate) ok: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusDetailRow {
    pub(crate) label: String,
    pub(crate) detail: String,
}

#[derive(Debug, Clone)]
pub(crate) struct StatusConfig {
    poll: PollCfg,
    probes: Vec<ProbeEntry>,
}

impl StatusConfig {
    pub(crate) fn from_config(config: &Config) -> Self {
        Self {
            poll: config.poll.clone(),
            probes: config.probes.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct StatusTarget(Arc<Mutex<Option<PathBuf>>>);

impl StatusTarget {
    pub(crate) fn new(path: Option<PathBuf>) -> Self {
        Self(Arc::new(Mutex::new(path)))
    }

    pub(crate) fn set(&self, path: Option<PathBuf>) {
        if let Ok(mut target) = self.0.lock() {
            *target = path;
        }
    }

    fn get(&self) -> Option<PathBuf> {
        self.0.lock().ok().and_then(|target| target.clone())
    }
}

pub(crate) fn spawn_status_thread(
    config: StatusConfig,
    target: StatusTarget,
    notify: Sender<RuntimeEvent>,
) {
    thread::spawn(move || StatusPoller::new(config, target).run(notify));
}

struct StatusPoller {
    config: StatusConfig,
    target: StatusTarget,
    system: System,
    http: ureq::Agent,
    snapshot: StatusSnapshot,
    next_system: Instant,
    next_git: Instant,
    next_health: Instant,
}

impl StatusPoller {
    fn new(config: StatusConfig, target: StatusTarget) -> Self {
        let http_config = ureq::config::Config::builder()
            .timeout_global(Some(HEALTH_TIMEOUT))
            .build();
        let now = Instant::now();
        Self {
            config,
            target,
            system: System::new(),
            http: http_config.into(),
            snapshot: StatusSnapshot::default(),
            next_system: now,
            next_git: now,
            next_health: now,
        }
    }

    fn run(mut self, notify: Sender<RuntimeEvent>) {
        loop {
            let now = Instant::now();
            let mut changed = false;

            if now >= self.next_system {
                self.snapshot.system = Some(sample_system(&mut self.system));
                self.next_system = now + cadence(self.config.poll.sys_secs);
                changed = true;
            }

            if now >= self.next_git {
                self.snapshot.git = self.target.get().as_deref().and_then(sample_git);
                self.next_git = now + cadence(self.config.poll.git_secs);
                changed = true;
            }

            if now >= self.next_health {
                self.snapshot.probes = sample_probes(&self.http, &self.config.probes);
                self.next_health = now + cadence(self.config.poll.health_secs);
                changed = true;
            }

            if changed
                && notify
                    .send(RuntimeEvent::Status(self.snapshot.clone()))
                    .is_err()
            {
                break;
            }

            let next = self.next_system.min(self.next_git).min(self.next_health);
            let sleep_for = next.saturating_duration_since(Instant::now());
            thread::sleep(sleep_for.min(Duration::from_millis(250)));
        }
    }
}

fn cadence(secs: u64) -> Duration {
    Duration::from_secs(secs.max(1))
}

fn sample_system(system: &mut System) -> SystemSnapshot {
    system.refresh_cpu_usage();
    system.refresh_memory();
    SystemSnapshot {
        cpu_percent: system.global_cpu_usage().round().clamp(0.0, 100.0) as u8,
        memory_percent: percent(system.used_memory(), system.total_memory()),
    }
}

fn sample_git(path: &Path) -> Option<GitSnapshot> {
    let repo = gix::open(path).ok()?;
    let branch = repo
        .head_name()
        .ok()
        .flatten()
        .map(|name| String::from_utf8_lossy(name.shorten().as_ref()).into_owned())
        .or_else(|| repo.head_id().ok().map(|id| id.shorten_or_id().to_string()))?;
    Some(GitSnapshot {
        branch,
        dirty: repo.is_dirty().unwrap_or(false),
    })
}

fn sample_probes(http: &ureq::Agent, probes: &[ProbeEntry]) -> Vec<ProbeSnapshot> {
    probes
        .iter()
        .map(|probe| ProbeSnapshot {
            name: probe.name.clone(),
            ok: http.get(probe.url.as_str()).call().is_ok(),
        })
        .collect()
}

fn percent(used: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    ((used as f64 / total as f64) * 100.0)
        .round()
        .clamp(0.0, 100.0) as u8
}

pub(crate) fn status_segment(snapshot: &StatusSnapshot) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(system) = &snapshot.system {
        parts.push(format!(
            "sys:{}/{}",
            system.cpu_percent, system.memory_percent
        ));
    }
    if let Some(git) = &snapshot.git {
        parts.push(format!(
            "git:{}{}",
            git.branch,
            if git.dirty { "*" } else { "" }
        ));
    }
    if !snapshot.probes.is_empty() {
        let ok = snapshot.probes.iter().filter(|probe| probe.ok).count();
        parts.push(format!("health:{ok}/{}", snapshot.probes.len()));
    }

    (!parts.is_empty()).then(|| parts.join(" "))
}

pub(crate) fn status_detail_rows(snapshot: &StatusSnapshot) -> Vec<StatusDetailRow> {
    let mut rows = Vec::new();

    rows.push(match &snapshot.system {
        Some(system) => StatusDetailRow::new(
            "system",
            format!(
                "cpu {}%  memory {}%",
                system.cpu_percent, system.memory_percent
            ),
        ),
        None => StatusDetailRow::new("system", "waiting for first sample"),
    });

    rows.push(match &snapshot.git {
        Some(git) => StatusDetailRow::new(
            "git",
            format!(
                "{} {}",
                git.branch,
                if git.dirty { "dirty" } else { "clean" }
            ),
        ),
        None => StatusDetailRow::new("git", "no repository sample"),
    });

    if snapshot.probes.is_empty() {
        rows.push(StatusDetailRow::new("health", "no probes configured"));
    } else {
        let ok = snapshot.probes.iter().filter(|probe| probe.ok).count();
        rows.push(StatusDetailRow::new(
            "health",
            format!("{ok}/{} probes reachable", snapshot.probes.len()),
        ));
        rows.extend(snapshot.probes.iter().map(|probe| {
            StatusDetailRow::new(probe.name.as_str(), if probe.ok { "ok" } else { "failed" })
        }));
    }

    rows
}

impl StatusDetailRow {
    fn new(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_handles_zero_and_clamps() {
        assert_eq!(percent(0, 0), 0);
        assert_eq!(percent(25, 100), 25);
        assert_eq!(percent(150, 100), 100);
    }

    #[test]
    fn status_segment_includes_available_statuses() {
        let snapshot = StatusSnapshot {
            system: Some(SystemSnapshot {
                cpu_percent: 12,
                memory_percent: 61,
            }),
            git: Some(GitSnapshot {
                branch: "main".to_string(),
                dirty: true,
            }),
            probes: vec![
                ProbeSnapshot {
                    name: "one".to_string(),
                    ok: true,
                },
                ProbeSnapshot {
                    name: "two".to_string(),
                    ok: false,
                },
            ],
        };
        assert_eq!(
            status_segment(&snapshot),
            Some("sys:12/61 git:main* health:1/2".to_string())
        );
    }

    #[test]
    fn status_segment_hides_empty_snapshot() {
        assert_eq!(status_segment(&StatusSnapshot::default()), None);
    }

    #[test]
    fn status_details_include_probe_names_without_history() {
        let rows = status_detail_rows(&StatusSnapshot {
            system: Some(SystemSnapshot {
                cpu_percent: 12,
                memory_percent: 61,
            }),
            git: Some(GitSnapshot {
                branch: "main".to_string(),
                dirty: true,
            }),
            probes: vec![
                ProbeSnapshot {
                    name: "api".to_string(),
                    ok: true,
                },
                ProbeSnapshot {
                    name: "worker".to_string(),
                    ok: false,
                },
            ],
        });

        assert_eq!(
            rows,
            vec![
                StatusDetailRow::new("system", "cpu 12%  memory 61%"),
                StatusDetailRow::new("git", "main dirty"),
                StatusDetailRow::new("health", "1/2 probes reachable"),
                StatusDetailRow::new("api", "ok"),
                StatusDetailRow::new("worker", "failed"),
            ]
        );
    }

    #[test]
    fn git_sample_ignores_non_repos() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(sample_git(tmp.path()), None);
    }
}
