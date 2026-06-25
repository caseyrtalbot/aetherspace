//! XDG config loading and dynamic project discovery.
//!
//! Config lives at `$XDG_CONFIG_HOME/aetherspace/config.toml` (else
//! `~/.config/aetherspace/config.toml`). Every field has a default, so a missing
//! or partial file works; an unparseable file logs a warning and falls back to
//! defaults rather than crashing the TUI. The nav list is either the pinned
//! `projects` (verbatim) or discovered from `projects_root`.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

use crate::keymap::{self, Keymap};
use crate::layout::SplitDir;
use crate::xdg;

/// The largest split ratio a layout step may request (the first child's percentage
/// of the parent split). `split_rects` clamps to 1..=99, so anything outside that
/// is a config mistake worth naming rather than silently coercing.
const MAX_LAYOUT_RATIO: u16 = 99;

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
    /// Health probes shown in the status surface.
    pub probes: Vec<ProbeEntry>,
    /// Per-task poll cadences for the status surface.
    pub poll: PollCfg,
    /// Shell settings (scrollback depth, wired into the PTY parser).
    pub shell: ShellCfg,
    /// Input settings: command leader and future keymap surface.
    pub input: InputCfg,
    /// Sparse keymap overlay: per-table `chord = action` maps that override the
    /// built-in defaults. Resolved post-parse into [`Config::keymap`].
    pub keymap: KeymapCfg,
    /// Workflow defaults for startup project and contextual viewer documents.
    pub workflow: WorkflowCfg,
    /// Raw `[[layouts]]` tables, kept as untyped values so a malformed layout
    /// names itself in a warning instead of failing the whole config parse. The
    /// resolved, validated form lives in [`Config::layouts`] (filled post-parse).
    #[serde(rename = "layouts")]
    raw_layouts: Vec<toml::Value>,
    /// Validated named layouts the runtime replays. Populated by
    /// [`from_toml_with_warning`] from [`raw_layouts`]; never set by serde.
    #[serde(skip)]
    pub layouts: Vec<NamedLayout>,
    /// Resolved keymap (default table + sparse overlay). Populated by
    /// [`from_toml_with_warning`] via [`resolve_keymap`]; never set by serde.
    #[serde(skip)]
    pub resolved_keymap: Keymap,
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
            keymap: KeymapCfg::default(),
            workflow: WorkflowCfg::default(),
            raw_layouts: Vec::new(),
            layouts: Vec::new(),
            resolved_keymap: Keymap::default(),
        }
    }
}

/// Sparse keymap overlay. Two `chord = action` maps mirror the two physical
/// dispatch tables. Unspecified chords keep the built-in default; a user entry
/// for a chord replaces that chord's binding (intended override, not a warning).
/// `BTreeMap<String,String>` is valid here because `String: Ord`; the resolved
/// keymap itself stores `Vec<(Chord, Binding)>` (crossterm types lack `Ord`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct KeymapCfg {
    pub global: BTreeMap<String, String>,
    pub leader: BTreeMap<String, String>,
}

/// A named, ordered SCRIPT of split/focus steps applied at runtime. Replayed
/// through the existing actions, so each step reuses proven PTY-spawn machinery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedLayout {
    pub name: String,
    pub steps: Vec<LayoutStep>,
}

/// One step in a layout script. Linear grammar: split the focused leaf (ratio
/// rides the split, applied at creation) or move relative focus. No new action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutStep {
    Split { dir: SplitDir, ratio: Option<u16> },
    Focus { dir: FocusDir },
}

/// Relative focus direction for a `Focus` step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusDir {
    Next,
    Prev,
}

/// Config-facing split direction. Separate from `layout::SplitDir` only to accept
/// lowercase strings (`"horizontal"`): `SplitDir`'s own Deserialize is PascalCase
/// because it drives the session.toml wire format, which B1 must not change.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConfigSplitDir {
    Horizontal,
    Vertical,
}

impl From<ConfigSplitDir> for SplitDir {
    fn from(dir: ConfigSplitDir) -> Self {
        match dir {
            ConfigSplitDir::Horizontal => SplitDir::Horizontal,
            ConfigSplitDir::Vertical => SplitDir::Vertical,
        }
    }
}

/// Untyped raw step keyed on the present field. `deny_unknown_fields` makes a
/// typo'd key ("splt") name itself in the error rather than silently parsing as a
/// no-op. Not `#[serde(untagged)]`: untagged collapses every key error into a
/// useless "did not match any variant", defeating the warning's DevEx.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLayoutStep {
    #[serde(default)]
    split: Option<ConfigSplitDir>,
    #[serde(default)]
    ratio: Option<u16>,
    #[serde(default)]
    focus: Option<FocusDir>,
}

/// Untyped raw layout. Parsed separately from the main config so a single bad
/// layout yields a warning instead of failing the whole file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNamedLayout {
    #[serde(default)]
    name: String,
    #[serde(default)]
    steps: Vec<RawLayoutStep>,
}

/// Validate one raw `[[layouts]]` value into a [`NamedLayout`], or a one-line
/// warning that names the offending layout. The name is read best-effort from the
/// raw value FIRST (falling back to a 1-based index) so even a structural/serde
/// error — a typo'd key or a bad direction string — still names which layout it
/// came from. Semantic errors (empty name/steps, ratio out of range, neither-or-
/// both of split/focus) are named here.
fn resolve_layout(index: usize, value: &toml::Value) -> Result<NamedLayout, String> {
    // Best-effort label for messages: the declared name if present, else the index.
    let label = value
        .get("name")
        .and_then(toml::Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .map(|name| format!("layout '{name}'"))
        .unwrap_or_else(|| format!("layout #{}", index + 1));

    let raw: RawNamedLayout = value
        .clone()
        .try_into()
        .map_err(|e| format!("{label}: {e}"))?;
    if raw.name.trim().is_empty() {
        return Err(format!("{label}: empty name"));
    }
    if raw.steps.is_empty() {
        return Err(format!("{label}: no steps"));
    }
    let mut steps = Vec::with_capacity(raw.steps.len());
    for (step_index, raw_step) in raw.steps.iter().enumerate() {
        let step_pos = step_index + 1;
        let step = match (raw_step.split, raw_step.focus) {
            (Some(dir), None) => {
                if let Some(ratio) = raw_step.ratio
                    && !(1..=MAX_LAYOUT_RATIO).contains(&ratio)
                {
                    return Err(format!(
                        "{label} step {step_pos}: ratio {ratio} out of range 1..={MAX_LAYOUT_RATIO}"
                    ));
                }
                LayoutStep::Split {
                    dir: dir.into(),
                    ratio: raw_step.ratio,
                }
            }
            (None, Some(dir)) => {
                if raw_step.ratio.is_some() {
                    return Err(format!(
                        "{label} step {step_pos}: ratio is only valid on a split"
                    ));
                }
                LayoutStep::Focus { dir }
            }
            (None, None) => {
                return Err(format!(
                    "{label} step {step_pos}: needs a split or focus key"
                ));
            }
            (Some(_), Some(_)) => {
                return Err(format!(
                    "{label} step {step_pos}: cannot set both split and focus"
                ));
            }
        };
        steps.push(step);
    }
    Ok(NamedLayout {
        name: raw.name,
        steps,
    })
}

/// A pinned project: a display name and its directory.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProjectEntry {
    pub name: String,
    pub path: PathBuf,
    pub viewer: Option<PathBuf>,
}

/// A statusline health probe.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProbeEntry {
    pub name: String,
    pub url: String,
}

/// Poll cadences in seconds, decoupling the full git walk from the health probe.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
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
    /// Load from `$XDG_CONFIG_HOME/aetherspace/config.toml` (else `~/.config/...`),
    /// also returning a one-line warning when a config file is present yet fails
    /// to parse. A missing file is the normal case and yields defaults with `None`;
    /// a present-but-unparseable file logs a warning and falls back to defaults so
    /// a typo never wedges startup, returning a banner (including the path) the
    /// runtime surfaces until dismissed.
    pub fn load_with_warning() -> (Self, Option<String>) {
        let path = xdg::home("XDG_CONFIG_HOME", ".config").join("aetherspace/config.toml");
        let Ok(text) = fs::read_to_string(&path) else {
            // Missing file is the normal case: defaults, no warning.
            return (Self::default(), None);
        };
        // File present: a parse failure still falls back to defaults, but here we
        // also surface a one-line banner that names the path the user must fix.
        Self::from_toml_with_warning(&path, &text)
    }

    /// Parse `text` from `path`, returning defaults plus a one-line warning when
    /// the file is present but unparseable. Split from the env-dependent file IO
    /// in [`load_with_warning`] so the warning decision is unit-testable without
    /// mutating process environment (which would race across test threads).
    fn from_toml_with_warning(path: &Path, text: &str) -> (Self, Option<String>) {
        match toml::from_str::<Self>(text) {
            Ok(mut cfg) => {
                let layout_warning = cfg.resolve_layouts();
                let keymap_warning = cfg.resolve_keymap();
                // Local ' | ' join: config is lower-level than runtime, so we do
                // NOT reach up into runtime::merge_warnings to combine these two.
                let warning = match (layout_warning, keymap_warning) {
                    (Some(a), Some(b)) => Some(format!("{a} | {b}")),
                    (Some(a), None) => Some(a),
                    (None, b) => b,
                };
                (cfg, warning)
            }
            Err(e) => {
                crate::log::warn(&format!("config parse error, using defaults: {e}"));
                let warning = format!("config {} parse error, using defaults", path.display());
                (Self::default(), Some(warning))
            }
        }
    }

    /// Validate `raw_layouts` into `layouts`, dropping any malformed layout and
    /// returning a one-line warning that names the first offender. The rest of the
    /// config (and the valid layouts) survive: a typo never wedges the whole file.
    fn resolve_layouts(&mut self) -> Option<String> {
        let mut resolved = Vec::with_capacity(self.raw_layouts.len());
        let mut first_warning = None;
        for (index, value) in self.raw_layouts.iter().enumerate() {
            match resolve_layout(index, value) {
                Ok(layout) => resolved.push(layout),
                Err(reason) => {
                    // serde errors can span lines ("...\nin `steps`"); flatten to a
                    // single line so the banner honors the one-line warning contract.
                    let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
                    crate::log::warn(&format!("layout config: {reason}"));
                    if first_warning.is_none() {
                        first_warning = Some(format!("config layout error: {reason}"));
                    }
                }
            }
        }
        self.layouts = resolved;
        first_warning
    }

    /// Build [`resolved_keymap`] from the built-in defaults plus the sparse
    /// `[keymap]` overlay, returning a one-line warning naming the first offending
    /// entry (unknown key, unknown action, or a duplicate within one table). The
    /// rest of the bindings survive: a single typo never wedges the keymap.
    fn resolve_keymap(&mut self) -> Option<String> {
        let mut map = keymap::default_keymap();
        let warning = map.merge_overlay(&self.keymap.global, &self.keymap.leader);
        self.resolved_keymap = map;
        warning
    }

    /// Parse TOML, falling back to defaults (with a logged warning) on any parse
    /// error. The unit-tested fallback seam; production loads go through
    /// [`from_toml_with_warning`], so this is only referenced by tests.
    #[cfg(test)]
    fn from_toml_or_default(text: &str) -> Self {
        Self::from_toml_with_warning(Path::new(""), text).0
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
    fn present_unparseable_config_yields_warning_with_path() {
        let path = Path::new("/cfg/aetherspace/config.toml");
        let (cfg, warning) = Config::from_toml_with_warning(path, "not = = valid [[[");
        // Still falls back to defaults so a typo never wedges startup.
        assert_eq!(cfg, Config::default());
        // One-line banner that names the offending path.
        let warning = warning.expect("unparseable present file must warn");
        assert!(
            warning.contains("/cfg/aetherspace/config.toml"),
            "{warning}"
        );
        assert!(
            !warning.contains('\n'),
            "warning must be one line: {warning}"
        );
    }

    #[test]
    fn valid_config_yields_no_warning() {
        let (cfg, warning) =
            Config::from_toml_with_warning(Path::new("/cfg/config.toml"), "projects_root = \"/x\"");
        assert_eq!(cfg.projects_root, PathBuf::from("/x"));
        assert_eq!(warning, None);
    }

    #[test]
    fn parses_layouts_into_step_sequence() {
        let toml = r#"
            [[layouts]]
            name = "dev"
            steps = [
              { split = "horizontal", ratio = 65 },
              { split = "vertical" },
            ]
        "#;
        let cfg = Config::from_toml_or_default(toml);
        assert_eq!(cfg.layouts.len(), 1);
        let dev = &cfg.layouts[0];
        assert_eq!(dev.name, "dev");
        assert_eq!(
            dev.steps,
            vec![
                LayoutStep::Split {
                    dir: SplitDir::Horizontal,
                    ratio: Some(65),
                },
                LayoutStep::Split {
                    dir: SplitDir::Vertical,
                    ratio: None,
                },
            ]
        );
    }

    #[test]
    fn parses_focus_step() {
        let toml = r#"
            [[layouts]]
            name = "scan"
            steps = [
              { split = "horizontal" },
              { focus = "prev" },
            ]
        "#;
        let cfg = Config::from_toml_or_default(toml);
        assert_eq!(
            cfg.layouts[0].steps,
            vec![
                LayoutStep::Split {
                    dir: SplitDir::Horizontal,
                    ratio: None,
                },
                LayoutStep::Focus {
                    dir: FocusDir::Prev
                },
            ]
        );
    }

    #[test]
    fn absent_layouts_yield_empty_vec_no_warning() {
        let (cfg, warning) =
            Config::from_toml_with_warning(Path::new("/cfg/config.toml"), "projects_root = \"/x\"");
        assert!(cfg.layouts.is_empty());
        assert_eq!(warning, None);
    }

    #[test]
    fn typoed_step_key_warns_and_names_layout_rest_defaults() {
        // `splt` is a typo of `split`: deny_unknown_fields names the bad key, and
        // the layout is dropped while the rest of the config still parses.
        let toml = r#"
            projects_root = "/srv/code"

            [[layouts]]
            name = "dev"
            steps = [
              { splt = "horizontal" },
            ]
        "#;
        let (cfg, warning) = Config::from_toml_with_warning(Path::new("/cfg/config.toml"), toml);
        let warning = warning.expect("typo'd key must warn");
        assert!(
            warning.contains("dev"),
            "warning must name the layout: {warning}"
        );
        assert!(
            warning.contains("splt"),
            "warning must name the bad key: {warning}"
        );
        assert!(!warning.contains('\n'), "one line: {warning}");
        // Rest of config survives; the malformed layout is dropped.
        assert_eq!(cfg.projects_root, PathBuf::from("/srv/code"));
        assert!(cfg.layouts.is_empty());
    }

    #[test]
    fn bad_split_direction_warns_and_names_layout() {
        let toml = r#"
            [[layouts]]
            name = "dev"
            steps = [
              { split = "sideways" },
            ]
        "#;
        let (cfg, warning) = Config::from_toml_with_warning(Path::new("/cfg/config.toml"), toml);
        let warning = warning.expect("bad split direction must warn");
        assert!(
            warning.contains("dev"),
            "warning must name the layout: {warning}"
        );
        assert!(cfg.layouts.is_empty());
    }

    #[test]
    fn empty_name_or_steps_warns() {
        let empty_name = r#"
            [[layouts]]
            name = ""
            steps = [ { split = "horizontal" } ]
        "#;
        let (cfg, warning) =
            Config::from_toml_with_warning(Path::new("/cfg/config.toml"), empty_name);
        assert!(warning.expect("empty name warns").contains("empty name"));
        assert!(cfg.layouts.is_empty());

        let empty_steps = r#"
            [[layouts]]
            name = "dev"
            steps = []
        "#;
        let (cfg, warning) =
            Config::from_toml_with_warning(Path::new("/cfg/config.toml"), empty_steps);
        let warning = warning.expect("empty steps warns");
        assert!(warning.contains("dev"), "{warning}");
        assert!(warning.contains("no steps"), "{warning}");
        assert!(cfg.layouts.is_empty());
    }

    #[test]
    fn out_of_range_ratio_warns_and_names_layout() {
        let toml = r#"
            [[layouts]]
            name = "dev"
            steps = [ { split = "horizontal", ratio = 150 } ]
        "#;
        let (cfg, warning) = Config::from_toml_with_warning(Path::new("/cfg/config.toml"), toml);
        let warning = warning.expect("out-of-range ratio must warn");
        assert!(warning.contains("dev"), "{warning}");
        assert!(warning.contains("ratio"), "{warning}");
        assert!(cfg.layouts.is_empty());
    }

    #[test]
    fn valid_layout_alongside_invalid_keeps_the_valid_one() {
        // The first layout is valid; the second is malformed. The valid one
        // survives and the warning names the offender.
        let toml = r#"
            [[layouts]]
            name = "good"
            steps = [ { split = "horizontal" } ]

            [[layouts]]
            name = "bad"
            steps = [ { split = "horizontal", ratio = 0 } ]
        "#;
        let (cfg, warning) = Config::from_toml_with_warning(Path::new("/cfg/config.toml"), toml);
        assert_eq!(cfg.layouts.len(), 1);
        assert_eq!(cfg.layouts[0].name, "good");
        let warning = warning.expect("the bad layout must warn");
        assert!(warning.contains("bad"), "{warning}");
    }

    #[test]
    fn keymap_override_replaces_one_binding_keeps_rest() {
        use crate::keymap::BindAction;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let toml = r#"
            [keymap.leader]
            x = "quit"
        "#;
        let (cfg, warning) = Config::from_toml_with_warning(Path::new("/cfg/config.toml"), toml);
        assert_eq!(warning, None);
        let ev = |code| KeyEvent::new(code, KeyModifiers::NONE);
        // 'x' now quits; 'q' still quits; the rest of the table is intact.
        assert_eq!(
            cfg.resolved_keymap.lookup_leader(ev(KeyCode::Char('x'))),
            Some(BindAction::Quit)
        );
        assert_eq!(
            cfg.resolved_keymap.lookup_leader(ev(KeyCode::Char('z'))),
            Some(BindAction::Zoom)
        );
    }

    #[test]
    fn unknown_action_name_in_keymap_drops_entry_and_warns() {
        let toml = r#"
            [keymap.leader]
            x = "frobnicate"
        "#;
        let (_cfg, warning) = Config::from_toml_with_warning(Path::new("/cfg/config.toml"), toml);
        let warning = warning.expect("unknown action must warn");
        assert!(warning.contains("frobnicate"), "{warning}");
        assert!(!warning.contains('\n'), "one line: {warning}");
    }

    #[test]
    fn unknown_key_name_in_keymap_drops_entry_and_warns() {
        let toml = r#"
            [keymap.leader]
            notakey = "quit"
        "#;
        let (_cfg, warning) = Config::from_toml_with_warning(Path::new("/cfg/config.toml"), toml);
        let warning = warning.expect("unknown key must warn");
        assert!(warning.contains("notakey"), "{warning}");
    }

    #[test]
    fn default_keymap_has_no_warning() {
        // No [keymap] block at all: the resolved keymap is the built-in default and
        // no warning is produced.
        let (cfg, warning) =
            Config::from_toml_with_warning(Path::new("/cfg/config.toml"), "projects_root = \"/x\"");
        assert_eq!(warning, None);
        assert_eq!(cfg.resolved_keymap, keymap::default_keymap());
    }

    #[test]
    fn layout_and_keymap_warnings_join_into_one_line() {
        // A bad layout AND a bad keymap entry: both warnings join with ' | ' in one
        // line (the local config-side join, not runtime::merge_warnings).
        let toml = r#"
            [[layouts]]
            name = "bad"
            steps = [ { split = "horizontal", ratio = 0 } ]

            [keymap.leader]
            x = "frobnicate"
        "#;
        let (_cfg, warning) = Config::from_toml_with_warning(Path::new("/cfg/config.toml"), toml);
        let warning = warning.expect("both warnings present");
        assert!(warning.contains("bad"), "{warning}");
        assert!(warning.contains("frobnicate"), "{warning}");
        assert!(warning.contains(" | "), "joined: {warning}");
        assert!(!warning.contains('\n'), "one line: {warning}");
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
