//! XDG config loading and dynamic project discovery (Phase 3).
//!
//! Config lives at `$XDG_CONFIG_HOME/aetherspace/config.toml` (else
//! `~/.config/aetherspace/config.toml`). Every field has a default, so a missing
//! or partial file works; an unparseable file logs a warning and falls back to
//! defaults rather than crashing the TUI. The nav list is either the pinned
//! `projects` (verbatim) or discovered from `projects_root`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::xdg;

/// Top-level config. `#[serde(default)]` fills any missing field from
/// `Config::default()`, so a partial TOML table still parses cleanly.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Directory scanned for git projects when `projects` is not pinned.
    pub projects_root: PathBuf,
    /// Pinned project list. When `Some`, it is used verbatim and discovery is
    /// skipped entirely.
    pub projects: Option<Vec<ProjectEntry>>,
    /// Health probes shown in the statusline. Consumed in Phase 5.
    #[allow(dead_code)] // wired into StatusMonitor in Phase 5
    pub probes: Vec<ProbeEntry>,
    /// Per-task poll cadences. Consumed in Phase 5.
    #[allow(dead_code)] // wired into the poll loop in Phase 5
    pub poll: PollCfg,
    /// Shell settings (scrollback depth, wired into the PTY parser).
    pub shell: ShellCfg,
    /// Input settings: command leader and future keymap surface.
    pub input: InputCfg,
    /// Workflow defaults for startup project and contextual viewer documents.
    pub workflow: WorkflowCfg,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            projects_root: default_projects_root(),
            projects: None,
            probes: Vec::new(),
            poll: PollCfg::default(),
            shell: ShellCfg::default(),
            input: InputCfg::default(),
            workflow: WorkflowCfg::default(),
        }
    }
}

/// A pinned project: a display name and its directory.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProjectEntry {
    pub name: String,
    pub path: PathBuf,
    pub viewer: Option<PathBuf>,
}

/// A statusline health probe. Consumed in Phase 5.
#[allow(dead_code)] // fields read once probes are wired in Phase 5
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProbeEntry {
    pub name: String,
    pub url: String,
}

/// Poll cadences in seconds, decoupling the full git walk from the health probe.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
#[allow(dead_code)] // fields read once cadences are wired in Phase 5
pub struct PollCfg {
    pub sys_secs: u64,
    pub git_secs: u64,
    pub health_secs: u64,
}

impl Default for PollCfg {
    fn default() -> Self {
        Self {
            sys_secs: 2,
            git_secs: 10,
            health_secs: 2,
        }
    }
}

/// Shell settings: the scrollback depth (rows) the embedded PTY parser retains.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct ShellCfg {
    pub scrollback: usize,
}

impl Default for ShellCfg {
    fn default() -> Self {
        Self { scrollback: 10_000 }
    }
}

/// Input settings. `leader` is parsed by `input.rs`; invalid strings fall back
/// to the default leader instead of making config loading fail.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct InputCfg {
    pub leader: String,
}

impl Default for InputCfg {
    fn default() -> Self {
        Self {
            leader: "ctrl-space".to_string(),
        }
    }
}

/// Workflow defaults that shape the initial command surface.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct WorkflowCfg {
    /// Preferred project name to select on startup. If absent or unmatched, the
    /// runtime chooses the current directory's project, then the first resolved
    /// project.
    pub startup_project: Option<String>,
    /// Relative path opened by the viewer for projects that do not override it.
    pub default_viewer: PathBuf,
}

impl Default for WorkflowCfg {
    fn default() -> Self {
        Self {
            startup_project: None,
            default_viewer: PathBuf::from("README.md"),
        }
    }
}

/// A project the nav can show: a display name and its directory. Owned (not a
/// `&'static str`) so it can come from discovery or a pinned config entry alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub viewer: Option<PathBuf>,
}

/// `$HOME/Projects` — the default discovery root. Computed (not a literal `~`) so
/// it resolves correctly; a literal tilde in the config file is NOT expanded.
fn default_projects_root() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_default()).join("Projects")
}

impl Config {
    /// Load from `$XDG_CONFIG_HOME/aetherspace/config.toml` (else
    /// `~/.config/...`). A missing file is the normal case and yields defaults;
    /// a present-but-unparseable file logs a warning and also yields defaults, so
    /// a typo never wedges startup.
    pub fn load() -> Self {
        let path = xdg::home("XDG_CONFIG_HOME", ".config").join("aetherspace/config.toml");
        match fs::read_to_string(&path) {
            Ok(text) => Self::from_toml_or_default(&text),
            Err(_) => Self::default(),
        }
    }

    /// Parse TOML, falling back to defaults (with a logged warning) on any parse
    /// error. Separated from the file IO in `load` so the fallback is unit-tested.
    fn from_toml_or_default(text: &str) -> Self {
        match toml::from_str(text) {
            Ok(cfg) => cfg,
            Err(e) => {
                crate::log::warn(&format!("config parse error, using defaults: {e}"));
                Self::default()
            }
        }
    }

    /// The nav project list: the pinned `projects` verbatim if configured, else
    /// discovery under `projects_root`.
    #[allow(dead_code)]
    pub fn resolve_projects(&self) -> Vec<Project> {
        match &self.projects {
            Some(pinned) => pinned
                .iter()
                .map(|e| Project {
                    name: e.name.clone(),
                    path: e.path.clone(),
                    viewer: e.viewer.clone(),
                })
                .collect(),
            None => discover_projects(&self.projects_root),
        }
    }
}

/// Discover git projects under `root`: immediate subdirectories containing a
/// `.git` entry, sorted by `.git/HEAD` mtime descending (most recently active
/// first) with a name tiebreak for determinism. An unreadable root yields an
/// empty list, which the caller renders as an empty-state placeholder.
#[allow(dead_code)]
pub fn discover_projects(root: &Path) -> Vec<Project> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<(Project, Option<SystemTime>)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join(".git").exists())
        .map(|path| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // `.git/HEAD` mtime tracks commits and branch switches; absent for a
            // gitfile worktree, which then sorts last (None < Some).
            let mtime = fs::metadata(path.join(".git/HEAD"))
                .and_then(|m| m.modified())
                .ok();
            (
                Project {
                    name,
                    path,
                    viewer: None,
                },
                mtime,
            )
        })
        .collect();
    // mtime descending (newest first), then name ascending as a stable tiebreak.
    found.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
    found.into_iter().map(|(p, _)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn parses_full_config() {
        let toml = r#"
            projects_root = "/srv/code"

            [[projects]]
            name = "alpha"
            path = "/srv/code/alpha"
            viewer = "docs/ops.md"

            [[probes]]
            name = "spark"
            url = "http://localhost:9292/health"

            [poll]
            sys_secs = 5
            git_secs = 30
            health_secs = 3

            [shell]
            scrollback = 50000

            [input]
            leader = "ctrl-g"

            [workflow]
            startup_project = "alpha"
            default_viewer = "docs/README.md"
        "#;
        let cfg = Config::from_toml_or_default(toml);
        assert_eq!(cfg.projects_root, PathBuf::from("/srv/code"));
        assert_eq!(
            cfg.projects,
            Some(vec![ProjectEntry {
                name: "alpha".into(),
                path: PathBuf::from("/srv/code/alpha"),
                viewer: Some(PathBuf::from("docs/ops.md")),
            }])
        );
        assert_eq!(
            cfg.probes,
            vec![ProbeEntry {
                name: "spark".into(),
                url: "http://localhost:9292/health".into(),
            }]
        );
        assert_eq!(cfg.poll.sys_secs, 5);
        assert_eq!(cfg.poll.git_secs, 30);
        assert_eq!(cfg.poll.health_secs, 3);
        assert_eq!(cfg.shell.scrollback, 50_000);
        assert_eq!(cfg.input.leader, "ctrl-g");
        assert_eq!(cfg.workflow.startup_project.as_deref(), Some("alpha"));
        assert_eq!(cfg.workflow.default_viewer, PathBuf::from("docs/README.md"));
    }

    #[test]
    fn partial_config_fills_defaults() {
        // Only projects_root set, and only one field of [poll].
        let toml = r#"
            projects_root = "/srv/code"
            [poll]
            sys_secs = 9
        "#;
        let cfg = Config::from_toml_or_default(toml);
        assert_eq!(cfg.projects_root, PathBuf::from("/srv/code"));
        assert_eq!(cfg.projects, None);
        assert_eq!(cfg.probes, Vec::new());
        // The set field wins; the rest of [poll] falls back to PollCfg::default().
        assert_eq!(cfg.poll.sys_secs, 9);
        assert_eq!(cfg.poll.git_secs, 10);
        assert_eq!(cfg.poll.health_secs, 2);
        assert_eq!(cfg.shell.scrollback, 10_000);
        assert_eq!(cfg.input, InputCfg::default());
        assert_eq!(cfg.workflow, WorkflowCfg::default());
    }

    #[test]
    fn garbage_config_falls_back_to_defaults() {
        let cfg = Config::from_toml_or_default("this is not = = valid toml [[[");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn discover_finds_only_git_dirs_recency_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Two git dirs (older `alpha`, newer `beta`), one plain dir, one file.
        for name in ["alpha", "beta"] {
            fs::create_dir_all(root.join(name).join(".git")).unwrap();
            let head = root.join(name).join(".git/HEAD");
            fs::write(&head, "ref: refs/heads/main\n").unwrap();
        }
        fs::create_dir(root.join("not-a-repo")).unwrap();
        fs::write(root.join("loose.txt"), "x").unwrap();

        // Set deterministic mtimes via std (File::set_modified, stable since 1.75)
        // so the recency order does not depend on wall-clock creation timing.
        let set_mtime = |name: &str, secs: u64| {
            let f = File::options()
                .write(true)
                .open(root.join(name).join(".git/HEAD"))
                .unwrap();
            f.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
                .unwrap();
        };
        set_mtime("alpha", 1_000);
        set_mtime("beta", 2_000); // newer

        let projects = discover_projects(root);
        let names: Vec<&str> = projects.iter().map(|p| p.name.as_str()).collect();
        // Only the two git dirs, newest (beta) first.
        assert_eq!(names, vec!["beta", "alpha"]);
        assert_eq!(projects[0].path, root.join("beta"));
    }

    #[test]
    fn discover_on_missing_root_is_empty() {
        let cfg = Config {
            projects_root: PathBuf::from("/nonexistent/aetherspace-xyz"),
            ..Config::default()
        };
        assert!(cfg.resolve_projects().is_empty());
    }

    #[test]
    fn pinned_projects_bypass_discovery() {
        // projects_root points at nothing, but a pinned list is returned verbatim.
        let cfg = Config {
            projects_root: PathBuf::from("/nonexistent/aetherspace-xyz"),
            projects: Some(vec![
                ProjectEntry {
                    name: "one".into(),
                    path: PathBuf::from("/p/one"),
                    viewer: None,
                },
                ProjectEntry {
                    name: "two".into(),
                    path: PathBuf::from("/p/two"),
                    viewer: Some(PathBuf::from("docs/two.md")),
                },
            ]),
            ..Config::default()
        };
        let projects = cfg.resolve_projects();
        assert_eq!(
            projects,
            vec![
                Project {
                    name: "one".into(),
                    path: PathBuf::from("/p/one"),
                    viewer: None,
                },
                Project {
                    name: "two".into(),
                    path: PathBuf::from("/p/two"),
                    viewer: Some(PathBuf::from("docs/two.md")),
                },
            ]
        );
    }
}
