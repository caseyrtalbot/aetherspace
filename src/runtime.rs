//! Unified runtime loop for the Aetherspace TUI.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use ratatui::crossterm::event::{
    self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout as RatatuiLayout, Rect},
    style::Style,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};
use tui_term::vt100::MouseProtocolMode;
use tui_term::widget::{Cursor, PseudoTerminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::action::Action;
use crate::config::{Config, LayoutStep, NamedLayout, Project};
use crate::event::{PaneProcessId, RuntimeEvent};
use crate::input::{InputConfig, InputRouter, encode_mouse};
use crate::keymap::{BindAction, Chord, Keymap, chord_label};
use crate::layout::{self, FloatGeom, SolvedPane, SplitDir};
use crate::pane::{PaneRuntime, ensure_spec_matches_runtime};
use crate::session::{PaneId, PaneSpec, ProjectSelection, Session};
use crate::session_store;
use crate::status::{self, StatusConfigHandle, StatusSnapshot, StatusTarget};
use crate::terminal::TerminalGuard;
use crate::theme::Theme;

const FRAME: Duration = Duration::from_millis(16);
const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;
const MAX_EVENTS_PER_TURN: usize = 256;
/// Poll cadence for the input thread. Long enough that idle CPU stays near zero
/// (one wakeup every 200 ms), short enough that the editor-suspend flag is
/// re-checked promptly. `event::poll` blocks efficiently until input or timeout.
const INPUT_POLL: Duration = Duration::from_millis(200);

/// Set while an `$EDITOR` owns the real terminal during EditScrollback. The input
/// thread must not call `event::read` while this is set, or it would consume bytes
/// meant for the editor. Module-level like `KEYBOARD_FLAGS_PUSHED` in `terminal`,
/// since the producer (the editor spawn) and consumer (the input thread) are in
/// different stack frames.
static INPUT_SUSPENDED: AtomicBool = AtomicBool::new(false);

pub(crate) fn run(config: Config, config_warning: Option<String>, nest_depth: u32) -> Result<()> {
    let (tx, rx) = mpsc::channel::<RuntimeEvent>();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let projects = config.resolve_projects();
    let status_config = status::StatusConfig::from_config(&config);
    // Shared handle so a live config reload can swap the poll cadence / probe set
    // into the detached status thread without restarting it (C3).
    let status_config_handle = StatusConfigHandle::new(status_config);
    let InitialSession {
        session,
        selected: selected_project,
        restored: restored_session,
        persist: session_persist,
        warning: session_warning,
    } = initial_session(
        &session_store::default_path(),
        &projects,
        config.workflow.startup_project.as_deref(),
        &cwd,
    );
    // Fold the Incompatible banner into the single config-warning sink so the
    // existing keypress-cleared banner surfaces it without a parallel channel.
    let config_warning = merge_warnings(config_warning, session_warning);
    let status_target = StatusTarget::new(selected_project_path(&projects, selected_project));
    let scrollback = config.shell.scrollback;
    // One resolved keymap, shared (cloned Arc) between the input router and the
    // runtime so both dispatch surfaces and the help/status renderers agree.
    let keymap = Arc::new(config.resolved_keymap.clone());
    let mut app = match RuntimeApp::new(RuntimeAppInit {
        session,
        input: InputRouter::with_keymap(
            InputConfig::from_leader_name(&config.input.leader),
            keymap.clone(),
        ),
        keymap: keymap.clone(),
        scrollback,
        notify: tx.clone(),
        projects: projects.clone(),
        selected_project,
        default_viewer: config.workflow.default_viewer.clone(),
        layouts: config.layouts.clone(),
        status_target: status_target.clone(),
        status_config_handle: status_config_handle.clone(),
        config_warning: config_warning.clone(),
        nest_depth,
        session_persist,
    }) {
        Ok(app) => app,
        Err(e) if restored_session => {
            crate::log::warn(&format!("persisted session ignored: {e}"));
            let (session, selected_project) =
                fallback_session(&projects, config.workflow.startup_project.as_deref(), &cwd);
            status_target.set(selected_project_path(&projects, selected_project));
            RuntimeApp::new(RuntimeAppInit {
                session,
                input: InputRouter::with_keymap(
                    InputConfig::from_leader_name(&config.input.leader),
                    keymap.clone(),
                ),
                keymap,
                scrollback,
                notify: tx.clone(),
                projects,
                selected_project,
                default_viewer: config.workflow.default_viewer,
                layouts: config.layouts,
                status_target: status_target.clone(),
                status_config_handle: status_config_handle.clone(),
                config_warning,
                nest_depth,
                session_persist,
            })?
        }
        Err(e) => return Err(e),
    };

    let mut guard = TerminalGuard::enter();
    panic_after_terminal_for_smoke();
    spawn_input_thread(tx);
    status::spawn_status_thread(status_config_handle, status_target, app.notify.clone());

    let result = run_loop(&mut guard, &mut app, rx);
    // Refuse-to-clobber: an Incompatible boot set session_persist=false, so the
    // final save is skipped entirely (never reaching save_to_path, which would
    // remove the file on an empty session).
    if app.session_persist {
        session_store::save(&app.session);
    }
    app.shutdown();
    result
}

/// Combine two optional one-line warnings into the single banner sink. Both
/// present → joined with `" | "`; otherwise whichever is `Some` (or `None`).
fn merge_warnings(a: Option<String>, b: Option<String>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("{a} | {b}")),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

fn panic_after_terminal_for_smoke() {
    #[cfg(debug_assertions)]
    if std::env::var_os("AETHERSPACE_PANIC_SMOKE").is_some() {
        panic!("AETHERSPACE_PANIC_SMOKE requested");
    }
}

struct RuntimeApp {
    running: bool,
    session: Session,
    panes: BTreeMap<PaneId, PaneRuntime>,
    input: InputRouter,
    /// Resolved keymap shared with the [`InputRouter`] (one `Arc<Keymap>` cloned
    /// into both at init). Drives `global_shortcut`, the help overlay, and the
    /// statusline hints so all three read one source of truth.
    keymap: Arc<Keymap>,
    projects: Vec<Project>,
    selected_project: Option<usize>,
    default_viewer: PathBuf,
    layouts: Vec<NamedLayout>,
    palette: Option<Palette>,
    show_help: bool,
    compact_chrome: bool,
    last_area: Rect,
    scrollback: usize,
    notify: Sender<RuntimeEvent>,
    last_error: Option<String>,
    last_notice: Option<String>,
    /// Shared error sink for config/reload/keybind failures, surfaced as an Error
    /// banner until the first keypress dismisses it. No timer exists, so it is
    /// keypress-cleared only (see `handle_key`).
    config_warning: Option<String>,
    /// Reserved-row banner painted at the top of the frame independent of the
    /// statusline, so a config/keybind warning survives compact mode. Reserves one
    /// row of the workspace (shrinking the PTY winsize) while `Some`. Set at boot
    /// alongside `config_warning` and cleared together on first keypress.
    banner: Option<Banner>,
    /// Set ONLY by the discrete session-mutating actions (split/close/restart/
    /// focus/resize/zoom/float, project select, open shell/viewer, reset). Drives
    /// the reboot-durable autosave in `run_loop`. NEVER set on SendBytes/Pty/
    /// Render/terminal-driven Resize: typing in a shell must not thrash the disk.
    session_dirty: bool,
    /// Gate for every save call site. Set false only by an Incompatible boot; the
    /// gate lives at the call sites in `run()`/`run_loop`, NEVER inside
    /// `save_to_path` (which removes the file on an empty session).
    session_persist: bool,
    status: StatusSnapshot,
    status_target: StatusTarget,
    /// Live handle into the detached status thread's config. A reload pushes a new
    /// poll cadence / probe set here without restarting the thread.
    status_config_handle: StatusConfigHandle,
    /// Nesting depth this process is AT (0 = top-level). Drives the `nested:N`
    /// statusline span; the child env var is already incremented at boot.
    nest_depth: u32,
    /// Set by `Action::EditScrollback` once the focused shell's screen has been
    /// written to a temp file. Drained in `run_loop`, which is the only place the
    /// `TerminalGuard` is reachable to restore/spawn-editor/reenter. Never set for
    /// viewers or dead shells (those get a notice instead).
    pending_editor: Option<PathBuf>,
}

struct RuntimeAppInit {
    session: Session,
    input: InputRouter,
    keymap: Arc<Keymap>,
    scrollback: usize,
    notify: Sender<RuntimeEvent>,
    projects: Vec<Project>,
    selected_project: Option<usize>,
    default_viewer: PathBuf,
    layouts: Vec<NamedLayout>,
    status_target: StatusTarget,
    status_config_handle: StatusConfigHandle,
    config_warning: Option<String>,
    nest_depth: u32,
    /// False after an Incompatible boot: every save call site is gated on this so
    /// the newer on-disk file is never overwritten (or removed on empty).
    session_persist: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Palette {
    kind: PaletteKind,
    selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaletteKind {
    Commands,
    Projects,
    StatusDetails,
    Layouts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaletteCommand {
    ProjectPicker,
    OpenProjectViewer,
    OpenProjectShell,
    StatusDetails,
    LayoutPicker,
    ResetWorkspace,
    SplitHorizontal,
    SplitVertical,
    ToggleFloat,
    ToggleZoom,
    ToggleStack,
    ToggleCompact,
    Restart,
    Close,
    ReloadConfig,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusTone {
    Normal,
    Notice,
    Error,
}

/// A reserved-row banner, painted at the top of the frame INDEPENDENT of the
/// statusline so a config/keybind warning stays visible in compact mode (where the
/// statusline collapses to zero height). The reserved row shrinks the PTY winsize,
/// so it must flow through the geometry callers, not just `draw`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Banner {
    text: String,
    tone: BannerTone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BannerTone {
    /// A keybind/config conflict surfaced at load or reload.
    Conflict,
    /// A hard error (e.g. a vanished config file on reload).
    Error,
}

#[derive(Debug, Clone, Copy)]
struct CommandItem {
    command: PaletteCommand,
    label: &'static str,
    detail: &'static str,
}

const COMMAND_ITEMS: &[CommandItem] = &[
    CommandItem {
        command: PaletteCommand::ProjectPicker,
        label: "projects",
        detail: "select project and open shell",
    },
    CommandItem {
        command: PaletteCommand::OpenProjectViewer,
        label: "viewer",
        detail: "open selected project document",
    },
    CommandItem {
        command: PaletteCommand::OpenProjectShell,
        label: "project shell",
        detail: "open shell at selected project",
    },
    CommandItem {
        command: PaletteCommand::StatusDetails,
        label: "status details",
        detail: "show current sys/git/probes",
    },
    CommandItem {
        command: PaletteCommand::LayoutPicker,
        label: "layouts",
        detail: "apply a named template layout",
    },
    CommandItem {
        command: PaletteCommand::ResetWorkspace,
        label: "reset workspace",
        detail: "collapse to one project shell",
    },
    CommandItem {
        command: PaletteCommand::SplitHorizontal,
        label: "split right",
        detail: "split focused shell horizontally",
    },
    CommandItem {
        command: PaletteCommand::SplitVertical,
        label: "split down",
        detail: "split focused shell vertically",
    },
    CommandItem {
        command: PaletteCommand::ToggleFloat,
        label: "float/dock",
        detail: "toggle focused pane",
    },
    CommandItem {
        command: PaletteCommand::ToggleZoom,
        label: "zoom",
        detail: "toggle focused tiled pane",
    },
    CommandItem {
        command: PaletteCommand::ToggleStack,
        label: "stack/unstack",
        detail: "collapse split group into a stack",
    },
    CommandItem {
        command: PaletteCommand::ToggleCompact,
        label: "compact ui",
        detail: "hide pane labels and statusline",
    },
    CommandItem {
        command: PaletteCommand::Restart,
        label: "reload/restart",
        detail: "restart shell or reload viewer",
    },
    CommandItem {
        command: PaletteCommand::Close,
        label: "close pane",
        detail: "close focused pane",
    },
    CommandItem {
        command: PaletteCommand::ReloadConfig,
        label: "reload config",
        detail: "re-read config.toml (keymap/layouts/probes)",
    },
    CommandItem {
        command: PaletteCommand::Quit,
        label: "quit",
        detail: "restore terminal and exit",
    },
];

impl RuntimeApp {
    fn new(init: RuntimeAppInit) -> Result<Self> {
        let mut panes = BTreeMap::new();
        for spec in init.session.pane_specs() {
            let pane = PaneRuntime::spawn(
                spec,
                INITIAL_ROWS,
                INITIAL_COLS,
                init.scrollback,
                init.notify.clone(),
            )?;
            panes.insert(spec.id, pane);
        }
        Ok(Self {
            running: true,
            session: init.session,
            panes,
            input: init.input,
            keymap: init.keymap,
            projects: init.projects,
            selected_project: init.selected_project,
            default_viewer: init.default_viewer,
            layouts: init.layouts,
            palette: None,
            show_help: true,
            compact_chrome: false,
            last_area: Rect::default(),
            scrollback: init.scrollback,
            notify: init.notify,
            last_error: None,
            last_notice: None,
            // Route the boot warning into BOTH sinks: config_warning preserves the
            // A2 statusline behavior; the banner makes it visible in compact too.
            // First keypress clears both together (one dismissal policy).
            banner: init.config_warning.as_ref().map(|text| Banner {
                text: text.clone(),
                tone: BannerTone::Conflict,
            }),
            config_warning: init.config_warning,
            session_dirty: false,
            session_persist: init.session_persist,
            status: StatusSnapshot::default(),
            status_target: init.status_target,
            status_config_handle: init.status_config_handle,
            nest_depth: init.nest_depth,
            pending_editor: None,
        })
    }

    fn shutdown(&mut self) {
        for pane in self.panes.values_mut() {
            pane.terminate();
        }
    }

    /// Rows the reserved-row banner currently occupies (1 while a banner is set, 0
    /// otherwise). Threaded into every geometry caller so the banner shrinks the
    /// workspace AND the PTY winsize, never just the painted frame.
    fn banner_rows(&self) -> u16 {
        self.banner.is_some() as u16
    }

    fn resize_to_area(&mut self, area: Rect) {
        self.last_area = area;
        let content_rects =
            pane_content_rects(&self.session, area, self.banner_rows(), self.compact_chrome);
        for (id, pane) in &mut self.panes {
            if let Some(content) = content_rects.get(id) {
                pane.resize(content.height, content.width);
            }
        }
    }

    fn focused_pane_mut(&mut self) -> Option<&mut PaneRuntime> {
        let id = self.session.focused()?;
        self.panes.get_mut(&id)
    }

    fn focused_pane(&self) -> Option<&PaneRuntime> {
        let id = self.session.focused()?;
        self.panes.get(&id)
    }

    fn selected_project(&self) -> Option<&Project> {
        self.selected_project.and_then(|idx| self.projects.get(idx))
    }

    fn select_project(&mut self, index: usize) -> Result<()> {
        let Some(project) = self.projects.get(index).cloned() else {
            bail!("project index {index} is not available");
        };
        self.selected_project = Some(index);
        self.session.select_project(project_selection(&project));
        self.session_dirty = true;
        self.status_target.set(Some(project.path.clone()));
        self.last_notice = Some(format!("project selected: {}", project.name));
        self.last_error = None;
        Ok(())
    }

    fn focused_is_viewer(&self) -> bool {
        self.session
            .focused()
            .and_then(|id| self.panes.get(&id))
            .map(PaneRuntime::is_viewer)
            .unwrap_or(false)
    }

    fn focused_content_height(&self) -> u16 {
        let Some(id) = self.session.focused() else {
            return INITIAL_ROWS;
        };
        pane_content_rects(
            &self.session,
            self.last_area,
            self.banner_rows(),
            self.compact_chrome,
        )
        .get(&id)
        .map(|rect| rect.height)
        .unwrap_or(INITIAL_ROWS)
    }

    fn spawn_runtime_for_spec(&mut self, spec: PaneSpec) -> Result<()> {
        let content_rects = pane_content_rects(
            &self.session,
            self.last_area,
            self.banner_rows(),
            self.compact_chrome,
        );
        let content = content_rects.get(&spec.id).copied().unwrap_or(Rect::new(
            0,
            0,
            INITIAL_COLS,
            INITIAL_ROWS,
        ));
        match PaneRuntime::spawn(
            &spec,
            content.height,
            content.width,
            self.scrollback,
            self.notify.clone(),
        ) {
            Ok(pane) => {
                self.panes.insert(spec.id, pane);
                self.resize_to_area(self.last_area);
                Ok(())
            }
            Err(e) => {
                self.session.close_pane(spec.id);
                Err(e)
            }
        }
    }

    fn open_shell_for_selected_project(&mut self) -> Result<()> {
        let Some(project) = self.selected_project().cloned() else {
            bail!("no selected project");
        };
        let spec = self
            .session
            .open_shell(project.path, format!("SHELL · {}", project.name));
        self.session_dirty = true;
        self.spawn_runtime_for_spec(spec)
    }

    fn open_viewer_for_selected_project(&mut self) -> Result<()> {
        let Some(project) = self.selected_project().cloned() else {
            bail!("no selected project");
        };
        let path = resolve_viewer_path(&project, &self.default_viewer);
        let spec = self
            .session
            .open_viewer(path, format!("VIEWER · {}", project.name));
        self.session_dirty = true;
        self.spawn_runtime_for_spec(spec)
    }

    fn open_project(&mut self, index: usize) -> Result<()> {
        self.select_project(index)?;
        self.open_shell_for_selected_project()
    }

    fn open_command_palette(&mut self) {
        self.show_help = false;
        self.palette = Some(Palette {
            kind: PaletteKind::Commands,
            selected: 0,
        });
    }

    fn open_project_palette(&mut self) {
        self.show_help = false;
        if self.projects.is_empty() {
            self.last_error = Some("no projects configured or discovered".to_string());
            self.palette = None;
            return;
        }
        self.palette = Some(Palette {
            kind: PaletteKind::Projects,
            selected: self
                .selected_project
                .unwrap_or(0)
                .min(self.projects.len() - 1),
        });
    }

    fn open_status_palette(&mut self) {
        self.show_help = false;
        self.palette = Some(Palette {
            kind: PaletteKind::StatusDetails,
            selected: 0,
        });
    }

    fn open_layout_palette(&mut self) {
        self.show_help = false;
        if self.layouts.is_empty() {
            self.last_error = Some("no layouts configured".to_string());
            self.palette = None;
            return;
        }
        self.palette = Some(Palette {
            kind: PaletteKind::Layouts,
            selected: 0,
        });
    }

    /// Apply the layout at `index`: tear the workspace down to one shell, then
    /// replay each step through the existing actions so PTY spawn/resize/autosave
    /// come for free. A first-shell spawn failure during the reset leaves an empty
    /// workspace — bail before replaying onto an empty tree. A mid-script spawn
    /// failure leaves the partial layout (no rollback) with `last_error` set.
    fn apply_layout(&mut self, index: usize) {
        let Some(layout) = self.layouts.get(index).cloned() else {
            self.last_error = Some(format!("layout index {index} is not available"));
            return;
        };
        if let Err(e) = self.reset_workspace() {
            // reset_workspace clears panes BEFORE its ?-propagating spawn loop, so a
            // first-shell failure leaves the workspace empty. Do not replay steps.
            self.last_error = Some(format!("layout '{}' reset failed: {e}", layout.name));
            return;
        }
        for step in &layout.steps {
            match *step {
                LayoutStep::Split { dir, ratio } => {
                    if !apply_action(self, Action::SplitFocusedPane { dir }) {
                        // apply_action returns false only on a non-render action; the
                        // split arm always returns true, so this is unreachable in
                        // practice. Surface anything unexpected rather than spin.
                        self.last_error =
                            Some(format!("layout '{}' split step failed", layout.name));
                        return;
                    }
                    if self.last_error.is_some() {
                        // split_focused set last_error (e.g. PTY spawn failed):
                        // leave the partial layout, stop replaying.
                        return;
                    }
                    // Ratio rides the split: focus is still on the just-created leaf,
                    // whose parent IS the split made above, so nudge_ratio can reach
                    // it — the one moment it can. From 50 toward `target` for the
                    // FIRST child (the original/left pane).
                    if let Some(target) = ratio {
                        apply_action(
                            self,
                            Action::ResizeFocusedPane {
                                delta: split_ratio_delta(target),
                            },
                        );
                    }
                }
                LayoutStep::Focus { dir } => {
                    let action = match dir {
                        crate::config::FocusDir::Next => Action::FocusNext,
                        crate::config::FocusDir::Prev => Action::FocusPrev,
                    };
                    apply_action(self, action);
                }
            }
        }
        self.palette = None;
        self.show_help = false;
        self.last_notice = Some(format!("layout applied: {}", layout.name));
    }

    fn open_help(&mut self) {
        self.palette = None;
        self.show_help = true;
    }

    /// Live config reload (C3): re-read config.toml without a restart. A vanished or
    /// unparseable file KEEPS the current in-memory state and shows an error banner —
    /// never a silent wipe to defaults. On success, rebuild the ONE shared keymap +
    /// input router (so they cannot diverge), swap in the new layouts, and push the
    /// new poll/probe config into the detached status thread. Theme is compile-time
    /// (not reloadable); reloaded layouts affect only FUTURE `apply_layout` calls,
    /// not the live tree.
    fn reload_config(&mut self) -> bool {
        let (config, present, warning) = Config::load_present_with_warning();
        match plan_reload(config, present, warning) {
            // Vanished file or a parse/keymap/layout/conflict warning: KEEP all
            // current in-memory state, surface the banner. Never a silent wipe.
            ReloadPlan::Keep(banner) => {
                self.set_banner(banner);
                self.last_notice = None;
            }
            // Clean reload: rebuild the ONE shared keymap (so the input router and
            // the renderers cannot diverge), swap layouts, push the new poll/probe
            // config into the status thread. Layouts affect only future apply_layout;
            // theme is compile-time and not reloaded.
            ReloadPlan::Apply(config) => {
                let keymap = Arc::new(config.resolved_keymap.clone());
                self.input = InputRouter::with_keymap(
                    InputConfig::from_leader_name(&config.input.leader),
                    keymap.clone(),
                );
                self.keymap = keymap;
                self.layouts = config.layouts.clone();
                self.status_config_handle
                    .set(status::StatusConfig::from_config(&config));
                self.clear_banner();
                self.last_error = None;
                self.last_notice = Some("config reloaded".to_string());
            }
        }
        true
    }

    /// Show a reserved-row banner and resize so the child PTY winsize tracks the
    /// newly reserved row (a banner set AFTER boot changes the geometry, unlike the
    /// boot banner which is in place before the first resize).
    fn set_banner(&mut self, banner: Banner) {
        self.banner = Some(banner);
        self.resize_to_area(self.last_area);
    }

    /// Clear the banner (if any) and resize so the freed row returns to the panes.
    fn clear_banner(&mut self) {
        if self.banner.take().is_some() {
            self.resize_to_area(self.last_area);
        }
    }

    fn reset_workspace(&mut self) -> Result<()> {
        let selected = self.selected_project().cloned();
        let cwd = selected
            .as_ref()
            .map(|project| project.path.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let selection = selected.as_ref().map(project_selection);

        self.shutdown();
        self.panes.clear();
        self.session = Session::single_shell_for_project(cwd, selection);
        let specs: Vec<PaneSpec> = self.session.pane_specs().to_vec();
        for spec in specs {
            self.spawn_runtime_for_spec(spec)?;
        }
        self.palette = None;
        self.show_help = false;
        self.last_error = None;
        self.last_notice = Some("workspace reset to one project shell".to_string());
        self.session_dirty = true;
        self.resize_to_area(self.last_area);
        Ok(())
    }

    /// Dump the focused shell's visible screen to a unique temp file and queue it
    /// for `$EDITOR`. Viewers and non-running shells are a no-op-with-notice (per
    /// the slice: never silently swallow the request). The actual editor spawn
    /// happens in `run_loop`, where the `TerminalGuard` is reachable.
    fn edit_scrollback(&mut self) {
        let Some(id) = self.session.focused() else {
            self.last_notice = Some("edit: no focused pane".to_string());
            return;
        };
        let Some(pane) = self.panes.get(&id) else {
            self.last_notice = Some("edit: no focused pane".to_string());
            return;
        };
        if !pane.is_running() {
            self.last_notice = Some("edit: focused pane is not a running shell".to_string());
            return;
        }
        let Some(text) = pane.shell_screen_text() else {
            self.last_notice = Some("edit: focused pane is not a running shell".to_string());
            return;
        };
        let path = scrollback_temp_path(id);
        if let Err(e) = std::fs::write(&path, text.as_bytes()) {
            self.last_error = Some(format!("edit: could not write scratch file: {e}"));
            return;
        }
        self.last_error = None;
        self.pending_editor = Some(path);
    }

    fn restart_focused(&mut self) -> Result<()> {
        let Some(id) = self.session.focused() else {
            bail!("no focused pane to restart");
        };
        let spec = ensure_spec_matches_runtime(id, self.session.spec(id))?.clone();
        let Some(pane) = self.panes.get_mut(&id) else {
            bail!("focused pane {id:?} has no runtime");
        };
        pane.restart(&spec, self.scrollback, self.notify.clone())
    }

    fn split_focused(&mut self, dir: SplitDir) -> Result<()> {
        let cwd = self
            .session
            .focused_shell_cwd()
            .or_else(|| self.selected_project().map(|project| project.path.clone()))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let Some(spec) = self.session.split_focused_shell(cwd, dir) else {
            bail!("focused pane cannot be split");
        };
        self.spawn_runtime_for_spec(spec)
    }

    fn close_focused(&mut self) {
        let Some(id) = self.session.focused() else {
            self.running = false;
            return;
        };
        if let Some(mut pane) = self.panes.remove(&id) {
            pane.terminate();
        }
        if !self.session.close_pane(id) {
            self.running = false;
        } else {
            self.resize_to_area(self.last_area);
        }
    }

    fn focus_next(&mut self) {
        self.session.focus_next();
        self.resize_to_area(self.last_area);
    }

    fn focus_prev(&mut self) {
        self.session.focus_prev();
        self.resize_to_area(self.last_area);
    }

    fn resize_focused(&mut self, delta: i16) {
        if self.session.resize_focused(delta) {
            self.resize_to_area(self.last_area);
        }
    }

    fn toggle_zoom_focused(&mut self) {
        if self.session.toggle_zoom_focused() {
            self.resize_to_area(self.last_area);
        }
    }

    fn toggle_float_focused(&mut self) {
        let geom = FloatGeom::centered(workspace_rect(
            self.last_area,
            self.banner_rows(),
            self.compact_chrome,
        ));
        if self.session.toggle_float_focused(geom) {
            self.resize_to_area(self.last_area);
        }
    }

    fn toggle_stack(&mut self) {
        if self.session.toggle_stack_focused() {
            self.resize_to_area(self.last_area);
            self.last_notice = Some("stack toggled".to_string());
        } else {
            self.last_notice = Some("stack: focus a tiled split first".to_string());
        }
    }

    fn process_pty(&mut self, id: PaneProcessId) {
        if let Some(pane) = self.panes.get_mut(&id.pane)
            && pane.process_pending(id)
        {
            let _ = self.notify.send(RuntimeEvent::Pty(id));
        }
    }

    fn mark_child_exit(&mut self, id: PaneProcessId) {
        if let Some(pane) = self.panes.get_mut(&id.pane) {
            pane.mark_child_exit(id);
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        if self.palette.is_none() && self.forward_mouse_to_child(mouse) {
            return false;
        }
        if self.palette.is_none()
            && !self.show_help
            && matches!(mouse.kind, MouseEventKind::Down(_))
            && self.focus_pane_at(mouse.column, mouse.row)
        {
            self.last_notice = Some("pane focused".to_string());
            return true;
        }
        if matches!(mouse.kind, MouseEventKind::Moved) {
            return false;
        }
        self.last_notice = Some(self.mouse_policy_notice(mouse).to_string());
        true
    }

    fn forward_mouse_to_child(&mut self, mouse: MouseEvent) -> bool {
        let Some((id, content)) = self.focused_mouse_content_rect(mouse) else {
            return false;
        };
        let Some((mode, encoding)) = self
            .panes
            .get(&id)
            .filter(|pane| pane.is_running())
            .and_then(PaneRuntime::child_mouse_protocol)
        else {
            return false;
        };
        if mode == MouseProtocolMode::None {
            return false;
        }

        let column = mouse.column.saturating_sub(content.x);
        let row = mouse.row.saturating_sub(content.y);
        let Some(bytes) = encode_mouse(mouse, column, row, mode, encoding) else {
            return false;
        };
        if let Some(pane) = self.panes.get_mut(&id) {
            pane.send_input(&bytes);
            self.last_notice = None;
            return true;
        }
        false
    }

    fn focused_mouse_content_rect(&self, mouse: MouseEvent) -> Option<(PaneId, Rect)> {
        let id = self.session.focused()?;
        let content = *pane_content_rects(
            &self.session,
            self.last_area,
            self.banner_rows(),
            self.compact_chrome,
        )
        .get(&id)?;
        if rect_contains(content, mouse.column, mouse.row) {
            Some((id, content))
        } else {
            None
        }
    }

    fn focused_child_mouse_enabled(&self) -> bool {
        self.session
            .focused()
            .and_then(|id| self.panes.get(&id))
            .filter(|pane| pane.is_running())
            .and_then(PaneRuntime::child_mouse_protocol)
            .map(|(mode, _)| mode != MouseProtocolMode::None)
            .unwrap_or(false)
    }

    fn focus_pane_at(&mut self, column: u16, row: u16) -> bool {
        let Some(id) = self.pane_at(column, row) else {
            return false;
        };
        let prev = self.session.focused();
        if !self.session.focus_pane(id) {
            return false;
        }
        // A mouse focus change to a DIFFERENT pane mid-find tears find down: the
        // keyboard force-exit in dispatch_key cannot fire for a mouse event, so clear
        // the router mode and the old viewer's highlights here (keyboard focus-switch
        // keys are inert during find, so the mouse is the only path that reaches this).
        if self.input.in_viewer_find() && prev != Some(id) {
            self.input.exit_viewer_find();
            if let Some(prev_id) = prev
                && let Some(pane) = self.panes.get_mut(&prev_id)
            {
                pane.clear_viewer_find();
            }
        }
        self.resize_to_area(self.last_area);
        true
    }

    fn pane_at(&self, column: u16, row: u16) -> Option<PaneId> {
        let workspace = workspace_rect(self.last_area, self.banner_rows(), self.compact_chrome);
        if self.session.zoomed().is_none() {
            for (id, geom) in self.session.floating().iter().rev() {
                let rect = layout::resolve_float(*geom, workspace);
                if rect_contains(rect, column, row) {
                    return Some(*id);
                }
            }
            // A click on a collapsed-member chip focuses+expands that member. Tested
            // before tiled bodies because a chip sits in the strip that the active
            // member's body rect already excludes. Hidden while zoomed (single pane).
            if let Some(tree) = self.session.tiled()
                && let Some(chip) = layout::stack_chips(tree, workspace)
                    .into_iter()
                    .find(|chip| rect_contains(chip.rect, column, row))
            {
                return Some(chip.id);
            }
        }
        tiled_panes(&self.session, workspace)
            .into_iter()
            .find(|pane| rect_contains(pane.rect, column, row))
            .map(|pane| pane.id)
    }

    fn mouse_policy_notice(&self, mouse: MouseEvent) -> &'static str {
        if self.show_help {
            "mouse: help owns pointer"
        } else if self.palette.is_some() {
            "mouse: palette owns pointer"
        } else if self.focused_child_mouse_enabled() {
            if self.focused_mouse_content_rect(mouse).is_some() {
                "mouse: child mode ignored this event"
            } else {
                "mouse: outside child surface"
            }
        } else {
            "mouse: child capture inactive"
        }
    }

    fn handle_palette_key(&mut self, key: KeyEvent) -> Option<bool> {
        if key.kind == KeyEventKind::Release {
            return Some(false);
        }
        self.palette?;
        match key.code {
            KeyCode::Esc => {
                self.palette = None;
                Some(true)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_palette(-1);
                Some(true)
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                self.move_palette(1);
                Some(true)
            }
            KeyCode::BackTab => {
                self.move_palette(-1);
                Some(true)
            }
            KeyCode::Enter => Some(self.accept_palette()),
            _ => Some(false),
        }
    }

    fn move_palette(&mut self, delta: isize) {
        let Some(palette) = self.palette else {
            return;
        };
        let len = self.palette_len(palette.kind);
        if len == 0 {
            return;
        }
        let current = palette.selected.min(len - 1);
        let next = (current as isize + delta).rem_euclid(len as isize) as usize;
        self.palette = Some(Palette {
            selected: next,
            ..palette
        });
    }

    fn palette_len(&self, kind: PaletteKind) -> usize {
        match kind {
            PaletteKind::Commands => COMMAND_ITEMS.len(),
            PaletteKind::Projects => self.projects.len(),
            PaletteKind::StatusDetails => status::status_detail_rows(&self.status).len(),
            PaletteKind::Layouts => self.layouts.len(),
        }
    }

    fn accept_palette(&mut self) -> bool {
        let Some(palette) = self.palette.take() else {
            return false;
        };
        match palette.kind {
            PaletteKind::Commands => {
                let Some(item) = COMMAND_ITEMS.get(palette.selected) else {
                    return true;
                };
                self.run_palette_command(item.command)
            }
            PaletteKind::Projects => {
                if let Err(e) = self.open_project(palette.selected) {
                    self.last_error = Some(format!("project open failed: {e}"));
                }
                true
            }
            PaletteKind::StatusDetails => true,
            PaletteKind::Layouts => {
                self.apply_layout(palette.selected);
                true
            }
        }
    }

    fn run_palette_command(&mut self, command: PaletteCommand) -> bool {
        match command {
            PaletteCommand::ProjectPicker => {
                self.open_project_palette();
                true
            }
            PaletteCommand::OpenProjectViewer => {
                if let Err(e) = self.open_viewer_for_selected_project() {
                    self.last_error = Some(format!("viewer failed: {e}"));
                } else {
                    self.last_error = None;
                }
                true
            }
            PaletteCommand::OpenProjectShell => {
                if let Err(e) = self.open_shell_for_selected_project() {
                    self.last_error = Some(format!("project shell failed: {e}"));
                } else {
                    self.last_error = None;
                }
                true
            }
            PaletteCommand::StatusDetails => {
                self.open_status_palette();
                true
            }
            PaletteCommand::LayoutPicker => {
                self.open_layout_palette();
                true
            }
            PaletteCommand::ResetWorkspace => {
                if let Err(e) = self.reset_workspace() {
                    self.last_error = Some(format!("workspace reset failed: {e}"));
                }
                true
            }
            PaletteCommand::SplitHorizontal => apply_action(
                self,
                Action::SplitFocusedPane {
                    dir: SplitDir::Horizontal,
                },
            ),
            PaletteCommand::SplitVertical => apply_action(
                self,
                Action::SplitFocusedPane {
                    dir: SplitDir::Vertical,
                },
            ),
            PaletteCommand::ToggleFloat => apply_action(self, Action::ToggleFloatFocusedPane),
            PaletteCommand::ToggleZoom => apply_action(self, Action::ToggleZoomFocusedPane),
            PaletteCommand::ToggleStack => apply_action(self, Action::ToggleStack),
            PaletteCommand::ToggleCompact => apply_action(self, Action::ToggleCompactChrome),
            PaletteCommand::Restart => apply_action(self, Action::RestartFocusedPane),
            PaletteCommand::Close => apply_action(self, Action::CloseFocusedPane),
            PaletteCommand::ReloadConfig => apply_action(self, Action::ReloadConfig),
            PaletteCommand::Quit => apply_action(self, Action::Quit),
        }
    }

    fn handle_viewer_key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        let rows = self.focused_content_height();
        // `/` enters incremental find. Flip the router into find mode ONLY if a viewer
        // accepted `begin_viewer_find` (atomic entry), so the mode and the viewer's
        // find state can never desync.
        if key.code == KeyCode::Char('/') {
            if self
                .focused_pane_mut()
                .is_some_and(PaneRuntime::begin_viewer_find)
            {
                self.input.enter_viewer_find();
                return true;
            }
            return false;
        }
        let Some(pane) = self.focused_pane_mut() else {
            return false;
        };
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => pane.scroll_viewer_by(1, rows),
            KeyCode::Up | KeyCode::Char('k') => pane.scroll_viewer_by(-1, rows),
            KeyCode::PageDown | KeyCode::Char(' ') => {
                pane.scroll_viewer_by(rows.saturating_sub(1).max(1) as isize, rows)
            }
            KeyCode::PageUp => {
                pane.scroll_viewer_by(-(rows.saturating_sub(1).max(1) as isize), rows)
            }
            KeyCode::Home => pane.scroll_viewer_home(),
            KeyCode::End => pane.scroll_viewer_end(rows),
            // Committed find navigation (n/N) and clear (Esc) — direct pane calls,
            // matching the scroll arms; no Action round-trip.
            KeyCode::Char('n') => pane.viewer_find_next(rows),
            KeyCode::Char('N') => pane.viewer_find_prev(rows),
            KeyCode::Esc => pane.clear_viewer_find(),
            _ => false,
        }
    }
}

fn spawn_input_thread(tx: Sender<RuntimeEvent>) {
    thread::spawn(move || {
        loop {
            // While an editor owns the terminal, do not touch stdin: poll/read
            // would steal the editor's keystrokes. Sleep instead of polling so a
            // ready-but-unconsumed stdin does not spin this loop.
            if INPUT_SUSPENDED.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            match event::poll(INPUT_POLL) {
                // Re-check suspension between poll and read: the editor may have
                // taken over in that window, and we must leave the byte for it.
                Ok(true) if !INPUT_SUSPENDED.load(Ordering::Acquire) => match event::read() {
                    Ok(raw) => {
                        if let Some(event) = RuntimeEvent::from_crossterm(raw)
                            && tx.send(event).is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(_) => continue,
                Err(_) => break,
            }
        }
    });
}

fn run_loop(
    guard: &mut TerminalGuard,
    app: &mut RuntimeApp,
    rx: Receiver<RuntimeEvent>,
) -> Result<()> {
    let size = guard.terminal_mut().size()?;
    app.resize_to_area(Rect::new(0, 0, size.width, size.height));
    guard.terminal_mut().draw(|frame| draw(frame, app))?;
    let mut last_draw = Instant::now();

    while app.running {
        let Ok(event) = rx.recv() else { break };
        let mut dirty = handle(app, event);
        let mut handled = 1usize;
        while let Ok(event) = rx.try_recv() {
            dirty |= handle(app, event);
            handled += 1;
            if !app.running {
                break;
            }
            if handled >= MAX_EVENTS_PER_TURN {
                break;
            }
        }

        // An EditScrollback action leaves a temp file queued. Run the editor here,
        // the one place the TerminalGuard is reachable, then force a redraw so the
        // TUI is repainted no matter how the editor exited.
        if app.pending_editor.is_some() {
            run_pending_editor(guard, app)?;
            dirty = true;
            last_draw = Instant::now();
        }

        if dirty && app.running {
            let since = last_draw.elapsed();
            if since < FRAME {
                thread::sleep(FRAME - since);
            }
            guard.terminal_mut().draw(|frame| draw(frame, app))?;
            last_draw = Instant::now();
            // Reboot-durable autosave: persist layout+cwd after a session-mutating
            // action so a hard kill (kill -9) keeps the same state the clean-exit
            // save in run() would have written. session_dirty is set only on the
            // discrete mutations, so shell typing/PTY bytes never reach here.
            if app.session_dirty {
                // Gate at the call site (not inside save_to_path): a no-persist boot
                // must never reach save, which removes the file on an empty session.
                if app.session_persist {
                    session_store::save(&app.session);
                }
                app.session_dirty = false;
            }
        }
    }
    Ok(())
}

/// Drop out of the alternate screen, run `$EDITOR` on the queued scratch file,
/// then re-enter and clear. The temp file is deleted inline once the editor
/// returns. Recoverability is the priority here: whatever the editor did to the
/// terminal (exited 0 after changing modes, or was SIGKILLed), `reenter` +
/// `clear` restores a usable TUI with mouse capture and bracketed paste back on.
fn run_pending_editor(guard: &mut TerminalGuard, app: &mut RuntimeApp) -> Result<()> {
    let Some(path) = app.pending_editor.take() else {
        return Ok(());
    };
    let editor = editor_command();

    // Suspend the input thread before leaving raw mode so it cannot read stdin
    // while the editor owns it; resume only after re-entry restores raw mode.
    INPUT_SUSPENDED.store(true, Ordering::Release);
    guard.restore();
    let status = std::process::Command::new(&editor).arg(&path).status();
    guard.reenter();
    // Resume input before the fallible clear() so an error on that line cannot
    // leave the input thread suspended (which would freeze all input).
    INPUT_SUSPENDED.store(false, Ordering::Release);
    guard.terminal_mut().clear()?;

    let _ = std::fs::remove_file(&path);
    match status {
        Ok(_) => app.last_notice = Some(format!("edited screen with {editor}")),
        Err(e) => app.last_error = Some(format!("editor {editor} failed: {e}")),
    }
    Ok(())
}

/// `$EDITOR`, then `$VISUAL`, then `vi`. Never panics on an unset variable; the
/// fallback guarantees a launchable command.
fn editor_command() -> String {
    pick_editor(std::env::var("EDITOR").ok(), std::env::var("VISUAL").ok())
}

/// Pure core of [`editor_command`], split out so the fallback chain is testable
/// without mutating process environment (which would race across test threads).
/// Blank/whitespace values are treated as unset.
fn pick_editor(editor: Option<String>, visual: Option<String>) -> String {
    let non_blank = |value: Option<String>| value.filter(|v| !v.trim().is_empty());
    non_blank(editor)
        .or_else(|| non_blank(visual))
        .unwrap_or_else(|| "vi".to_string())
}

/// Unique scratch path for a pane's screen dump: pid + pane id keeps concurrent
/// aetherspace instances and panes from colliding. Lives in the XDG runtime dir
/// (falling back to the system temp dir when `XDG_RUNTIME_DIR` is unset).
fn scrollback_temp_path(id: PaneId) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(std::env::temp_dir);
    dir.join(format!(
        "aetherspace-scrollback-{}-{}.txt",
        std::process::id(),
        id.0
    ))
}

fn handle(app: &mut RuntimeApp, event: RuntimeEvent) -> bool {
    match event {
        RuntimeEvent::Key(key) => handle_key(app, key),
        RuntimeEvent::Paste(text) => {
            let action = app.input.route_paste(text);
            apply_action(app, action)
        }
        RuntimeEvent::Resize(cols, rows) => apply_action(app, Action::Resize { cols, rows }),
        RuntimeEvent::Mouse(mouse) => app.handle_mouse(mouse),
        RuntimeEvent::Pty(id) => {
            app.process_pty(id);
            true
        }
        RuntimeEvent::ChildExit(id) => {
            app.process_pty(id);
            app.mark_child_exit(id);
            true
        }
        RuntimeEvent::QuitRequested => {
            app.running = false;
            true
        }
        RuntimeEvent::Status(snapshot) => {
            app.status = snapshot;
            true
        }
        RuntimeEvent::Tick => true,
    }
}

fn handle_key(app: &mut RuntimeApp, key: KeyEvent) -> bool {
    // First real keypress dismisses BOTH the config_warning (statusline sink) and the
    // reserved-row banner together — one dismissal policy, no split-brain. No timer
    // exists, so this is the only way they clear; force a redraw so they actually
    // vanish (the blocked loop redraws only when the triggering key is dirty).
    let real = key.kind != KeyEventKind::Release;
    let cleared_warning = real && app.config_warning.take().is_some();
    let cleared_banner = real && app.banner.take().is_some();
    // Clearing the banner frees its reserved row; resize so the child PTY winsize
    // reclaims it immediately (otherwise the child renders short until the next
    // terminal resize event).
    if cleared_banner {
        app.resize_to_area(app.last_area);
    }
    // OR the dismissal in so the banner vanishes even when the key was otherwise
    // a no-op for rendering.
    dispatch_key(app, key) || cleared_warning || cleared_banner
}

/// The modal viewer-find predicate: keystrokes route to the find buffer ONLY while a
/// viewer is focused. If focus leaves the viewer while find is active, the caller
/// force-exits find instead. Pulled out as a free fn so the rule is unit-testable
/// without constructing a `RuntimeApp` (which would spawn PTYs).
fn should_route_to_find(in_viewer_find: bool, focused_is_viewer: bool) -> bool {
    in_viewer_find && focused_is_viewer
}

fn dispatch_key(app: &mut RuntimeApp, key: KeyEvent) -> bool {
    if app.show_help {
        return handle_help_key(app, key);
    }
    // Viewer find is modal: while active and the viewer is still focused, EVERY key
    // edits the query (so F-keys/scroll/leader cannot steal keystrokes). If focus
    // left the viewer (e.g. an async layout change), tear find down and fall through.
    if app.input.in_viewer_find() {
        if should_route_to_find(true, app.focused_is_viewer()) {
            let action = app.input.route_key(key);
            return apply_action(app, action);
        }
        app.input.exit_viewer_find();
    }
    if let Some(action) = global_shortcut(&app.keymap, app.focused_is_viewer(), key) {
        // Help collapses into Action::OpenHelp; apply_action opens the overlay.
        return apply_action(app, action);
    }
    if let Some(dirty) = app.handle_palette_key(key) {
        return dirty;
    }
    if app.focused_is_viewer() && !app.input.is_leader_key(key) && app.handle_viewer_key(key) {
        return true;
    }
    let action = app.input.route_key(key);
    apply_action(app, action)
}

fn handle_help_key(app: &mut RuntimeApp, key: KeyEvent) -> bool {
    if key.kind == KeyEventKind::Release {
        return false;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::F(1) | KeyCode::Char('?') => {
            app.show_help = false;
            true
        }
        _ => false,
    }
}

fn global_shortcut(keymap: &Keymap, focused_is_viewer: bool, key: KeyEvent) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    keymap
        .lookup_global(focused_is_viewer, key)
        .map(BindAction::to_action)
}

fn apply_action(app: &mut RuntimeApp, action: Action) -> bool {
    let dirty = action.is_render_request();
    match action {
        Action::Quit => {
            app.running = false;
            true
        }
        Action::Render => true,
        Action::Resize { cols, rows } => {
            app.resize_to_area(Rect::new(0, 0, cols, rows));
            true
        }
        Action::SendBytes(bytes) => {
            if let Some(pane) = app.focused_pane_mut() {
                pane.send_input(&bytes);
            }
            app.last_notice = None;
            dirty
        }
        Action::SplitFocusedPane { dir } => {
            if let Err(e) = app.split_focused(dir) {
                app.last_error = Some(format!("split failed: {e}"));
            } else {
                app.last_error = None;
            }
            app.session_dirty = true;
            true
        }
        Action::RestartFocusedPane => {
            if let Err(e) = app.restart_focused() {
                app.last_error = Some(format!("restart failed: {e}"));
            } else {
                app.last_error = None;
            }
            app.session_dirty = true;
            true
        }
        Action::CloseFocusedPane => {
            app.close_focused();
            app.session_dirty = true;
            true
        }
        Action::FocusNext => {
            app.focus_next();
            app.session_dirty = true;
            true
        }
        Action::FocusPrev => {
            app.focus_prev();
            app.session_dirty = true;
            true
        }
        Action::ResizeFocusedPane { delta } => {
            app.resize_focused(delta);
            app.session_dirty = true;
            true
        }
        Action::ToggleZoomFocusedPane => {
            app.toggle_zoom_focused();
            app.session_dirty = true;
            true
        }
        Action::ToggleFloatFocusedPane => {
            app.toggle_float_focused();
            app.session_dirty = true;
            true
        }
        Action::ToggleStack => {
            app.toggle_stack();
            app.session_dirty = true;
            true
        }
        Action::ToggleCompactChrome => {
            app.compact_chrome = !app.compact_chrome;
            app.resize_to_area(app.last_area);
            app.last_notice = Some(if app.compact_chrome {
                "compact ui on".to_string()
            } else {
                "compact ui off".to_string()
            });
            true
        }
        Action::OpenHelp => {
            app.open_help();
            true
        }
        Action::OpenCommandPalette => {
            app.open_command_palette();
            true
        }
        Action::OpenProjectPalette => {
            app.open_project_palette();
            true
        }
        Action::OpenProjectViewer => {
            if let Err(e) = app.open_viewer_for_selected_project() {
                app.last_error = Some(format!("viewer failed: {e}"));
            } else {
                app.last_error = None;
            }
            true
        }
        Action::OpenProjectShell => {
            if let Err(e) = app.open_shell_for_selected_project() {
                app.last_error = Some(format!("project shell failed: {e}"));
            } else {
                app.last_error = None;
            }
            true
        }
        Action::EditScrollback => {
            app.edit_scrollback();
            true
        }
        Action::ReloadConfig => app.reload_config(),
        Action::ViewerFind(query) => {
            let rows = app.focused_content_height();
            if let Some(pane) = app.focused_pane_mut() {
                pane.set_viewer_query(&query, rows);
            }
            true
        }
        Action::ViewerFindCommit => {
            app.last_notice = app
                .focused_pane_mut()
                .and_then(|pane| pane.viewer_find_status());
            true
        }
        Action::ViewerFindCancel => {
            if let Some(pane) = app.focused_pane_mut() {
                pane.cancel_viewer_find();
            }
            true
        }
        Action::Noop => dirty,
    }
}

/// The decision half of a config reload, split from the state mutation in
/// [`RuntimeApp::reload_config`] so the keep-vs-apply SAFETY logic is unit-testable
/// without constructing a `RuntimeApp` (which would spawn PTYs). `present`/`warning`
/// come from [`Config::load_present_with_warning`]: a missing file or any warning
/// means KEEP current state (never apply a partial/default config); only a clean
/// load applies.
enum ReloadPlan {
    Keep(Banner),
    Apply(Box<Config>),
}

fn plan_reload(config: Config, present: bool, warning: Option<String>) -> ReloadPlan {
    if !present {
        return ReloadPlan::Keep(Banner {
            text: "config file missing — keeping current settings".to_string(),
            tone: BannerTone::Error,
        });
    }
    if let Some(reason) = warning {
        return ReloadPlan::Keep(Banner {
            text: reason,
            tone: BannerTone::Conflict,
        });
    }
    ReloadPlan::Apply(Box::new(config))
}

fn draw(frame: &mut Frame, app: &RuntimeApp) {
    let area = frame.area();
    let banner_rows = app.banner_rows();
    let banner = banner_rect(area, banner_rows, app.compact_chrome);
    let workspace = workspace_rect(area, banner_rows, app.compact_chrome);
    let status = status_rect(area, banner_rows, app.compact_chrome);

    let tiled = tiled_panes(&app.session, workspace);
    if tiled.is_empty() && app.session.floating().is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " no pane",
                Style::default().fg(Theme::DIM),
            ))),
            label_rect(workspace),
        );
    }

    if app.session.zoomed().is_none()
        && let Some(tree) = app.session.tiled()
    {
        for sep in layout::separators(tree, workspace) {
            draw_separator(frame, sep);
        }
    }

    for pane in tiled {
        draw_pane(frame, app, pane.id, pane.rect, false);
    }

    // Stack chip strips ride the tiled-pane pass (NOT a global overlay) so floats and
    // zoom paint over them correctly. Hidden while zoomed: a single pane fills the
    // workspace and has no strip.
    if app.session.zoomed().is_none()
        && !app.compact_chrome
        && let Some(tree) = app.session.tiled()
    {
        for chip in layout::stack_chips(tree, workspace) {
            draw_stack_chip(frame, app, chip);
        }
    }

    if app.session.zoomed().is_none() {
        for (id, geom) in app.session.floating() {
            let rect = layout::resolve_float(*geom, workspace);
            draw_pane(frame, app, *id, rect, true);
        }
    }

    if app.palette.is_some() {
        draw_palette(frame, app, workspace);
    }
    if app.show_help {
        draw_help(frame, &app.keymap, workspace);
    }

    if !app.compact_chrome {
        draw_statusline(frame, app, status);
    }

    // Paint the banner UNCONDITIONALLY (outside the compact guard) so a config /
    // keybind warning stays visible after the statusline collapses.
    if let Some(b) = &app.banner {
        draw_banner(frame, b, banner);
    }
}

/// Paint the reserved-row banner. Tone picks a legible Theme color: a conflict reads
/// as a warning, a hard error as an accent.
fn draw_banner(frame: &mut Frame, banner: &Banner, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let fg = match banner.tone {
        BannerTone::Conflict => Theme::GLOW_AMBER,
        BannerTone::Error => Theme::ACCENT,
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", banner.text),
            Style::default().fg(fg),
        )))
        .style(Style::default().bg(Theme::SELECT_BG)),
        area,
    );
}

fn draw_pane(frame: &mut Frame, app: &RuntimeApp, id: PaneId, area: Rect, floating: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(spec) = app.session.spec(id) else {
        return;
    };
    let Some(pane) = app.panes.get(&id) else {
        return;
    };

    if floating {
        frame.render_widget(Clear, area);
    }

    let focused = app.session.focused() == Some(id);
    if !app.compact_chrome {
        let label_style = if focused {
            Theme::label_focused()
        } else {
            Theme::label()
        };
        let prefix = if floating { " FLOAT " } else { " " };
        let mut title = pane.title(spec);
        if let Some(status) = pane.viewer_status() {
            title = format!("{title}  {status}");
        }
        // Surface incremental find on the focused viewer's label row: the live
        // `/query` while typing, plus the match count (i/n or "no matches").
        if focused {
            if app.input.in_viewer_find() {
                title = format!("{title}  /{}", pane.viewer_find_query().unwrap_or_default());
            }
            if let Some(find_status) = pane.viewer_find_status() {
                title = format!("{title}  {find_status}");
            }
        }
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{prefix}{title}"),
                label_style,
            ))),
            label_rect(area),
        );
    }

    let content = pane_content_rect(area, app.compact_chrome);
    if content.width > 0 && content.height > 0 {
        if let Some(screen) = pane.shell_screen() {
            let term = PseudoTerminal::new(screen)
                .cursor(Cursor::default().visibility(focused && pane.is_running()));
            frame.render_widget(term, content);
        } else if let Some(lines) = pane.viewer_lines(content.height) {
            frame.render_widget(Paragraph::new(Text::from(lines)), content);
        }
    }
}

fn draw_palette(frame: &mut Frame, app: &RuntimeApp, workspace: Rect) {
    let Some(palette) = app.palette else {
        return;
    };
    let len = app.palette_len(palette.kind);
    let desired_height = match palette.kind {
        PaletteKind::Commands => (COMMAND_ITEMS.len() + 2) as u16,
        PaletteKind::Projects => (app.projects.len().min(12) + 2) as u16,
        PaletteKind::StatusDetails => {
            (app.palette_len(PaletteKind::StatusDetails).min(14) + 2) as u16
        }
        PaletteKind::Layouts => (app.layouts.len().min(12) + 2) as u16,
    };
    let rect = overlay_rect(workspace, 78, desired_height.max(5));
    if rect.width < 8 || rect.height < 3 {
        return;
    }

    frame.render_widget(Clear, rect);
    let title = match palette.kind {
        PaletteKind::Commands => " command palette ",
        PaletteKind::Projects => " projects ",
        PaletteKind::StatusDetails => " status details ",
        PaletteKind::Layouts => " layouts ",
    };
    let block = Block::default()
        .title(Line::from(Span::styled(title, Theme::label_focused())))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::HAIR));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let lines = if len == 0 {
        vec![Line::from(Span::styled(
            " no entries",
            Style::default().fg(Theme::DIM),
        ))]
    } else {
        palette_lines(app, palette, inner.height, inner.width)
    };
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn draw_help(frame: &mut Frame, keymap: &Keymap, workspace: Rect) {
    let rect = overlay_rect(workspace, 86, 17);
    if rect.width < 20 || rect.height < 8 {
        return;
    }

    frame.render_widget(Clear, rect);
    let block = Block::default()
        .title(Line::from(Span::styled(" help ", Theme::label_focused())))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::HAIR));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    // Key labels derive from the keymap so they cannot drift from dispatch. The two
    // terminal-capability caveat rows stay hand-written so auto-generation never
    // claims a Ctrl chord works on terminals that cannot send it.
    let lines = vec![
        help_line(
            advertised_chord_label(keymap, BindAction::CommandPalette, false),
            "commands",
            "open command palette",
        ),
        help_line(
            "Alt+Enter".to_string(),
            "commands",
            "fallback if Ctrl+Enter is not sent",
        ),
        help_line(
            advertised_chord_label(keymap, BindAction::Help, false),
            "help",
            "open this guide",
        ),
        help_line("click".to_string(), "focus", "select a pane"),
        help_line(
            advertised_chord_label(keymap, BindAction::FocusNext, false),
            "focus",
            "next pane if your terminal sends it",
        ),
        help_line(
            "Shift+Tab".to_string(),
            "focus",
            "previous pane in viewer panes",
        ),
        help_line(
            "palette".to_string(),
            "project",
            "open project picker or a new shell",
        ),
        help_line(
            "palette".to_string(),
            "reset",
            "collapse clutter to one project shell",
        ),
        help_line(f_key_summary(keymap), "fallback", "function-key shortcuts"),
        help_line("^Space".to_string(), "leader", "legacy command prefix"),
        help_line("Esc/Enter".to_string(), "close", "close this help panel"),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// A compact summary of the bound function-key globals (e.g. "F1 help, F2
/// commands, F10 quit"), derived from the table so it tracks rebinds.
fn f_key_summary(keymap: &Keymap) -> String {
    let mut parts = Vec::new();
    for (chord, binding) in keymap.global_entries() {
        if let Chord::Global(KeyCode::F(_), KeyModifiers::NONE) = chord {
            parts.push(format!(
                "{} {}",
                chord_label(chord),
                bind_action_word(binding.action)
            ));
            if parts.len() == 3 {
                break;
            }
        }
    }
    parts.join(", ")
}

/// A one-word description of a bindable action for compact help summaries.
fn bind_action_word(action: BindAction) -> &'static str {
    match action {
        BindAction::Help => "help",
        BindAction::CommandPalette => "commands",
        BindAction::ProjectPalette => "projects",
        BindAction::Viewer => "viewer",
        BindAction::Shell => "shell",
        BindAction::FocusNext => "next",
        BindAction::FocusPrev => "prev",
        BindAction::Zoom => "zoom",
        BindAction::Close => "close",
        BindAction::Quit => "quit",
        BindAction::SplitHorizontal => "split",
        BindAction::SplitVertical => "split",
        BindAction::Restart => "restart",
        BindAction::ResizeGrow => "grow",
        BindAction::ResizeShrink => "shrink",
        BindAction::Float => "float",
        BindAction::ToggleStack => "stack",
        BindAction::ToggleCompact => "compact",
        BindAction::EditScrollback => "edit",
        BindAction::ReloadConfig => "reload",
    }
}

fn help_line(key: String, label: &'static str, detail: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(format!("{key:<8}"), Theme::selected_row()),
        Span::styled(format!(" {label:<10}"), Style::default().fg(Theme::FG)),
        Span::styled(detail, Style::default().fg(Theme::DIM)),
    ])
}

fn palette_lines(
    app: &RuntimeApp,
    palette: Palette,
    height: u16,
    width: u16,
) -> Vec<Line<'static>> {
    if palette.kind == PaletteKind::StatusDetails {
        let rows = status::status_detail_rows(&app.status);
        let len = rows.len();
        let view_rows = height.max(1) as usize;
        let selected = palette.selected.min(len.saturating_sub(1));
        let start = selected.saturating_sub(view_rows.saturating_sub(1));
        let end = len.min(start + view_rows);
        return (start..end)
            .map(|idx| {
                let row = &rows[idx];
                palette_line(
                    idx == selected,
                    row.label.as_str(),
                    row.detail.as_str(),
                    width,
                )
            })
            .collect();
    }

    let len = app.palette_len(palette.kind);
    let rows = height.max(1) as usize;
    let selected = palette.selected.min(len.saturating_sub(1));
    let start = selected.saturating_sub(rows.saturating_sub(1));
    let end = len.min(start + rows);
    (start..end)
        .map(|idx| match palette.kind {
            PaletteKind::Commands => {
                let item = COMMAND_ITEMS[idx];
                palette_line(idx == selected, item.label, item.detail, width)
            }
            PaletteKind::Projects => {
                let project = &app.projects[idx];
                palette_line(
                    idx == selected,
                    project.name.as_str(),
                    &project.path.display().to_string(),
                    width,
                )
            }
            PaletteKind::Layouts => {
                let layout = &app.layouts[idx];
                let steps = layout.steps.len();
                let detail = format!("{steps} step{}", if steps == 1 { "" } else { "s" });
                palette_line(idx == selected, layout.name.as_str(), &detail, width)
            }
            PaletteKind::StatusDetails => unreachable!("status palette handled above"),
        })
        .collect()
}

fn palette_line(selected: bool, label: &str, detail: &str, width: u16) -> Line<'static> {
    let label_style = if selected {
        Theme::selected_row()
    } else {
        Style::default().fg(Theme::FG)
    };
    let detail_style = if selected {
        Style::default().fg(Theme::FG).bg(Theme::SELECT_BG)
    } else {
        Style::default().fg(Theme::DIM)
    };
    if width <= 4 {
        return Line::from(Span::styled(truncate_width(label, width), label_style));
    }
    let label_budget = width.saturating_sub(4).min(24);
    let label = truncate_width(label, label_budget);
    let detail_budget = width
        .saturating_sub(UnicodeWidthStr::width(label.as_str()) as u16)
        .saturating_sub(4);
    let detail = truncate_width(detail, detail_budget);
    Line::from(vec![
        Span::styled(
            if selected { "> " } else { "  " },
            if selected {
                Theme::selected_row()
            } else {
                Style::default().fg(Theme::ACCENT)
            },
        ),
        Span::styled(label.to_string(), label_style),
        Span::styled(format!("  {detail}"), detail_style),
    ])
}

fn draw_statusline(frame: &mut Frame, app: &RuntimeApp, area: Rect) {
    let (message, tone) = status_message(app);
    let project = app
        .session
        .selected_project()
        .map(|project| project.name.as_str())
        .unwrap_or("no project");
    let project = truncate_width(project, status_project_budget(area.width));
    let pane_state = pane_state(app);
    let mode = if app.input.mode_label() == "leader" {
        "leader"
    } else {
        "capture"
    };
    let leader = app.input.leader_label();

    let sep = || Span::styled(" │ ", Style::default().fg(Theme::HAIR));
    let mut spans = vec![
        Span::styled(" Aetherspace ", Theme::status_title()),
        sep(),
        Span::styled("v0.1 tui ", Theme::status_meta()),
    ];
    if let Some(nested) = nest_segment(app.nest_depth) {
        spans.push(sep());
        spans.push(Span::styled(nested, Theme::status_notice()));
    }
    spans.extend([
        sep(),
        Span::styled(format!("project:{project} "), Theme::status_meta()),
        sep(),
        Span::styled(format!("{pane_state} "), Theme::status_meta()),
        sep(),
        Span::styled(format!("{leader}:{mode} "), Theme::status_meta()),
    ]);

    if let Some(status) = status::status_segment(&app.status) {
        spans.push(sep());
        spans.push(Span::styled(
            truncate_width(&status, status_poll_budget(area.width)),
            Theme::status_meta(),
        ));
    }

    let used = spans_width(&spans);
    let budget = area.width.saturating_sub(used).saturating_sub(3);
    if budget > 0 {
        let style = match tone {
            StatusTone::Normal => Theme::status_meta(),
            StatusTone::Notice => Theme::status_notice(),
            StatusTone::Error => Theme::status_error(),
        };
        spans.push(sep());
        spans.push(Span::styled(truncate_width(&message, budget), style));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The advertised primary "commands … help" hint fragment, derived from the
/// keymap. On macOS we advertise the Alt mirror as primary (Terminal.app cannot
/// send Ctrl+Enter); elsewhere the Ctrl chord. Display only — never routing.
fn primary_hint(keymap: &Keymap) -> String {
    let advertise_alt = cfg!(target_os = "macos");
    let commands = advertised_chord_label(keymap, BindAction::CommandPalette, advertise_alt);
    let help = advertised_chord_label(keymap, BindAction::Help, advertise_alt);
    format!("{commands} commands  {help} help")
}

/// Label for the chord advertised as the PRIMARY way to reach `action`: the
/// Alt-modified mirror when `advertise_alt` is set (macOS), else the Ctrl-modified
/// chord, falling back to the first bound global chord (e.g. an F-key) if neither
/// exists. Display only — every chord stays routable regardless of what we show.
fn advertised_chord_label(keymap: &Keymap, action: BindAction, advertise_alt: bool) -> String {
    let preferred = if advertise_alt {
        KeyModifiers::ALT
    } else {
        KeyModifiers::CONTROL
    };
    for (chord, binding) in keymap.global_entries() {
        if binding.action == action
            && let Chord::Global(_, modifiers) = chord
            && modifiers.contains(preferred)
        {
            return chord_label(chord);
        }
    }
    keymap
        .global_chord_for(action)
        .map(chord_label)
        .unwrap_or_else(|| "?".to_string())
}

fn status_message(app: &RuntimeApp) -> (String, StatusTone) {
    let hint = primary_hint(&app.keymap);
    if let Some(warning) = &app.config_warning {
        // Highest priority and shown even over the startup help, since a present
        // config that failed to parse is a state the user should fix. Cleared on
        // the first keypress (see `handle_key`).
        (warning.clone(), StatusTone::Error)
    } else if let Some(error) = &app.last_error {
        (error.clone(), StatusTone::Error)
    } else if let Some(notice) = &app.last_notice {
        (notice.clone(), StatusTone::Notice)
    } else if app.show_help {
        (
            format!("startup guide  {hint}  Esc close"),
            StatusTone::Normal,
        )
    } else if matches!(
        app.palette,
        Some(Palette {
            kind: PaletteKind::StatusDetails,
            ..
        })
    ) {
        (
            "status details  up/down inspect  enter/esc close".to_string(),
            StatusTone::Normal,
        )
    } else if app.palette.is_some() {
        (
            "enter run  up/down select  esc close".to_string(),
            StatusTone::Normal,
        )
    } else if app.input.in_viewer_find() {
        let query = app
            .focused_pane()
            .and_then(PaneRuntime::viewer_find_query)
            .unwrap_or_default();
        (
            format!("find  /{query}  enter accept  esc cancel"),
            StatusTone::Notice,
        )
    } else if app.focused_is_viewer() {
        (
            format!("viewer  j/k scroll  / find  n/N next  Tab focus  {hint}"),
            StatusTone::Normal,
        )
    } else if app.input.mode_label() == "leader" {
        (leader_hint(&app.keymap), StatusTone::Normal)
    } else if app.focused_child_mouse_enabled() {
        (
            format!("shell capture  mouse->child  {hint}"),
            StatusTone::Normal,
        )
    } else {
        (
            format!("click focus  {hint}  ^Space leader"),
            StatusTone::Normal,
        )
    }
}

/// The leader-mode hint ("c commands  p projects  …"), derived from the leader
/// table so a rebind is reflected here too.
fn leader_hint(keymap: &Keymap) -> String {
    let label = |action: BindAction| {
        keymap
            .leader_chord_for(action)
            .map(chord_label)
            .unwrap_or_else(|| "?".to_string())
            .to_ascii_lowercase()
    };
    format!(
        "{} commands  {} projects  {} viewer  {} shell  {} help  {} quit",
        label(BindAction::CommandPalette),
        label(BindAction::ProjectPalette),
        label(BindAction::Viewer),
        label(BindAction::Shell),
        label(BindAction::Help),
        label(BindAction::Quit),
    )
}

fn pane_state(app: &RuntimeApp) -> String {
    let focus_kind = if app.focused_is_viewer() {
        "viewer"
    } else {
        "shell"
    };
    let surface = if app.session.zoomed().is_some() {
        "zoom"
    } else if app
        .session
        .focused()
        .map(|id| app.session.is_floating(id))
        .unwrap_or(false)
    {
        "float"
    } else {
        "tile"
    };
    format!("panes:{} {surface}:{focus_kind}", app.panes.len())
}

/// `nested:N` indicator shown only when running inside another aetherspace
/// pane. Depth 0 (top-level) renders nothing.
fn nest_segment(depth: u32) -> Option<String> {
    (depth >= 1).then(|| format!("nested:{depth} "))
}

fn status_project_budget(width: u16) -> u16 {
    match width {
        0..=79 => 14,
        80..=119 => 24,
        _ => 36,
    }
}

fn status_poll_budget(width: u16) -> u16 {
    match width {
        0..=99 => 18,
        100..=139 => 28,
        _ => 42,
    }
}

fn spans_width(spans: &[Span<'_>]) -> u16 {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()) as u16)
        .sum()
}

fn truncate_width(s: &str, max: u16) -> String {
    let max = max as usize;
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut width = 0;
    for ch in s.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > budget {
            break;
        }
        out.push(ch);
        width += char_width;
    }
    if max > 0 {
        out.push('…');
    }
    out
}

fn overlay_rect(area: Rect, desired_width: u16, desired_height: u16) -> Rect {
    if area.width == 0 || area.height == 0 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    let width = desired_width.min(area.width.saturating_sub(2).max(1));
    let height = desired_height.min(area.height.saturating_sub(2).max(1));
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

/// Paint one collapsed-stack chip into the indicator strip: the active member in the
/// accent label style, collapsed members dim. The chip carries the member's title so
/// the strip reads as tabs.
fn draw_stack_chip(frame: &mut Frame, app: &RuntimeApp, chip: layout::StackChip) {
    if chip.rect.width == 0 || chip.rect.height == 0 {
        return;
    }
    let style = if chip.active {
        Theme::label_focused()
    } else {
        Theme::label()
    };
    let title = app
        .session
        .spec(chip.id)
        .map(|spec| {
            app.panes
                .get(&chip.id)
                .map(|pane| pane.title(spec))
                .unwrap_or_else(|| spec.title.clone())
        })
        .unwrap_or_else(|| format!("pane {}", chip.id.0));
    let marker = if chip.active { "▣ " } else { "▢ " };
    let label = truncate_width(
        &format!("{marker}{title}"),
        chip.rect.width.saturating_sub(1),
    );
    frame.render_widget(Clear, chip.rect);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {label}"), style))),
        chip.rect,
    );
}

fn draw_separator(frame: &mut Frame, sep: layout::SepLine) {
    let glyph = if sep.horizontal { "─" } else { "│" };
    let style = Style::default().fg(Theme::HAIR);
    let buffer = frame.buffer_mut();
    for y in sep.rect.y..sep.rect.bottom() {
        for x in sep.rect.x..sep.rect.right() {
            buffer[(x, y)].set_symbol(glyph).set_style(style);
        }
    }
}

fn tiled_panes(session: &Session, workspace: Rect) -> Vec<SolvedPane> {
    if let Some(id) = session.zoomed()
        && session.is_tiled(id)
    {
        return vec![SolvedPane {
            id,
            rect: workspace,
        }];
    }
    session
        .tiled()
        .map(|tree| layout::solve_tiled(tree, workspace))
        .unwrap_or_default()
}

fn pane_content_rects(
    session: &Session,
    area: Rect,
    banner_rows: u16,
    compact: bool,
) -> BTreeMap<PaneId, Rect> {
    let workspace = workspace_rect(area, banner_rows, compact);
    let mut rects = BTreeMap::new();
    for pane in tiled_panes(session, workspace) {
        rects.insert(pane.id, pane_content_rect(pane.rect, compact));
    }
    if session.zoomed().is_none() {
        for (id, geom) in session.floating() {
            let rect = layout::resolve_float(*geom, workspace);
            rects.insert(*id, pane_content_rect(rect, compact));
        }
    }
    rects
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
}

/// Split the frame into `[banner, workspace, status]` from ONE layout pass so the
/// three rects cannot overlap or gap. `banner_rows` reserves the top reserved-row
/// banner (0 when absent); `compact` collapses the statusline to zero height. This
/// is the single source of chrome geometry: `banner_rect`/`workspace_rect`/
/// `status_rect` are thin wrappers, and the banner row subtracted HERE flows into
/// `pane_content_rects`→`pane.resize`, so the PTY winsize shrinks (the A1 lesson).
fn chrome_split(area: Rect, banner_rows: u16, compact: bool) -> [Rect; 3] {
    let status_rows = if compact { 0 } else { 1 };
    let chunks = RatatuiLayout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(banner_rows),
            Constraint::Min(0),
            Constraint::Length(status_rows),
        ])
        .split(area);
    [chunks[0], chunks[1], chunks[2]]
}

fn banner_rect(area: Rect, banner_rows: u16, compact: bool) -> Rect {
    chrome_split(area, banner_rows, compact)[0]
}

fn workspace_rect(area: Rect, banner_rows: u16, compact: bool) -> Rect {
    chrome_split(area, banner_rows, compact)[1]
}

fn label_rect(area: Rect) -> Rect {
    Rect::new(area.x, area.y, area.width, area.height.min(1))
}

fn pane_content_rect(area: Rect, compact: bool) -> Rect {
    if compact {
        return area;
    }
    Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    )
}

fn status_rect(area: Rect, banner_rows: u16, compact: bool) -> Rect {
    chrome_split(area, banner_rows, compact)[2]
}

fn select_start_project(
    projects: &[Project],
    startup_project: Option<&str>,
    cwd: &Path,
) -> Option<usize> {
    startup_project
        .and_then(|name| projects.iter().position(|project| project.name == name))
        .or_else(|| {
            projects
                .iter()
                .position(|project| cwd.starts_with(&project.path))
        })
        .or_else(|| (!projects.is_empty()).then_some(0))
}

fn selected_project_path(projects: &[Project], selected: Option<usize>) -> Option<PathBuf> {
    selected.and_then(|index| projects.get(index).map(|project| project.path.clone()))
}

fn project_selection(project: &Project) -> ProjectSelection {
    ProjectSelection {
        name: project.name.clone(),
        path: project.path.clone(),
    }
}

fn resolve_viewer_path(project: &Project, default_viewer: &Path) -> PathBuf {
    let viewer = project.viewer.as_deref().unwrap_or(default_viewer);
    if viewer.is_absolute() {
        viewer.to_path_buf()
    } else {
        project.path.join(viewer)
    }
}

/// Resize delta that turns a freshly-created split (ratio 50, new leaf focused as
/// the SECOND child) into one whose FIRST child occupies `target` percent. Shared
/// by `apply_layout` and its test so the nudge direction cannot drift: `nudge_ratio`
/// on the focused second child applies `ratio + (-delta)`, so `50 + (-delta) =
/// target` ⇒ `delta = 50 - target`.
fn split_ratio_delta(target: u16) -> i16 {
    50i16 - target as i16
}

/// The boot session plus the flags that drive `run()`'s fallback and persistence.
/// `restored` gates the rebuild-on-spawn-failure arm; `persist` gates BOTH save
/// call sites so an Incompatible boot never overwrites the newer on-disk file.
struct InitialSession {
    session: Session,
    selected: Option<usize>,
    restored: bool,
    persist: bool,
    warning: Option<String>,
}

fn initial_session(
    session_path: &Path,
    projects: &[Project],
    startup_project: Option<&str>,
    cwd: &Path,
) -> InitialSession {
    match session_store::load_classified(session_path) {
        Ok(session_store::SessionLoad::Loaded(mut session)) => {
            let persisted_selected = selected_project_from_session(&session, projects);
            let selected =
                persisted_selected.or_else(|| select_start_project(projects, startup_project, cwd));
            if persisted_selected != selected
                && let Some(index) = selected
                && let Some(project) = projects.get(index)
            {
                session.select_project(project_selection(project));
            }
            InitialSession {
                session,
                selected,
                restored: true,
                persist: true,
                warning: None,
            }
        }
        Ok(session_store::SessionLoad::Incompatible) => {
            // A newer Aetherspace wrote this file. Start fresh, but DO NOT persist:
            // restored=false keeps a spawn failure off the rebuild arm (which would
            // drop this banner), and persist=false stops the first autosave from
            // clobbering — including the remove-on-empty path if the default session
            // is then emptied.
            let (session, selected) = fallback_session(projects, startup_project, cwd);
            InitialSession {
                session,
                selected,
                restored: false,
                persist: false,
                warning: Some(
                    "session.toml written by a newer Aetherspace; not overwriting — \
                     start fresh or upgrade"
                        .to_string(),
                ),
            }
        }
        Ok(session_store::SessionLoad::None) => {
            let (session, selected) = fallback_session(projects, startup_project, cwd);
            InitialSession {
                session,
                selected,
                restored: false,
                persist: true,
                warning: None,
            }
        }
        Err(e) => {
            crate::log::warn(&format!("session load skipped: {e}"));
            let (session, selected) = fallback_session(projects, startup_project, cwd);
            InitialSession {
                session,
                selected,
                restored: false,
                persist: true,
                warning: None,
            }
        }
    }
}

fn fallback_session(
    projects: &[Project],
    startup_project: Option<&str>,
    cwd: &Path,
) -> (Session, Option<usize>) {
    let selected = select_start_project(projects, startup_project, cwd);
    let shell_cwd = selected
        .and_then(|idx| projects.get(idx).map(|project| project.path.clone()))
        .unwrap_or_else(|| cwd.to_path_buf());
    let session = Session::single_shell_for_project(
        shell_cwd,
        selected
            .and_then(|idx| projects.get(idx))
            .map(project_selection),
    );
    (session, selected)
}

fn selected_project_from_session(session: &Session, projects: &[Project]) -> Option<usize> {
    let selection = session.selected_project()?;
    projects.iter().position(|project| {
        project.name == selection.name || same_project_path(&project.path, &selection.path)
    })
}

fn same_project_path(a: &Path, b: &Path) -> bool {
    let normalize = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    normalize(a) == normalize(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_falls_back_through_visual_to_vi() {
        assert_eq!(
            pick_editor(Some("nvim".into()), Some("emacs".into())),
            "nvim"
        );
        assert_eq!(pick_editor(None, Some("emacs".into())), "emacs");
        assert_eq!(
            pick_editor(Some("  ".into()), Some("emacs".into())),
            "emacs"
        );
        assert_eq!(pick_editor(None, None), "vi");
        assert_eq!(pick_editor(Some(String::new()), None), "vi");
    }

    #[test]
    fn scrollback_temp_paths_are_unique_per_pane() {
        let a = scrollback_temp_path(PaneId(0));
        let b = scrollback_temp_path(PaneId(1));
        assert_ne!(a, b);
        assert!(a.is_absolute());
        assert!(
            a.file_name()
                .and_then(|n| n.to_str())
                .unwrap()
                .ends_with("-0.txt")
        );
    }

    #[test]
    fn nest_segment_hidden_at_top_level_visible_when_nested() {
        assert_eq!(nest_segment(0), None);
        assert_eq!(nest_segment(1).as_deref(), Some("nested:1 "));
        assert_eq!(nest_segment(3).as_deref(), Some("nested:3 "));
    }

    #[test]
    fn workspace_excludes_statusline() {
        let area = Rect::new(0, 0, 80, 24);
        // No banner (banner_rows = 0): workspace excludes only the status row.
        assert_eq!(workspace_rect(area, 0, false), Rect::new(0, 0, 80, 23));
        assert_eq!(status_rect(area, 0, false), Rect::new(0, 23, 80, 1));
        assert_eq!(banner_rect(area, 0, false), Rect::new(0, 0, 80, 0));
    }

    #[test]
    fn compact_chrome_reclaims_label_and_status_rows() {
        let area = Rect::new(0, 0, 80, 24);
        // Compact, no banner: workspace fills the frame and the statusline collapses.
        assert_eq!(workspace_rect(area, 0, true), area);
        assert_eq!(status_rect(area, 0, true).height, 0);
        // Compact: a pane keeps its whole rect as content (no label row reserved).
        let pane = Rect::new(2, 3, 40, 10);
        assert_eq!(pane_content_rect(pane, true), pane);
        // Normal mode still reserves exactly one row each, so the toggle is reversible.
        assert_eq!(pane_content_rect(pane, false), Rect::new(2, 4, 40, 9));
    }

    #[test]
    fn workspace_rect_reserves_banner_row_when_present() {
        let area = Rect::new(0, 0, 80, 24);
        // NORMAL: banner steals the top row, status the bottom — workspace shrinks
        // by one more row than the no-banner case (23 → 22).
        assert_eq!(banner_rect(area, 1, false), Rect::new(0, 0, 80, 1));
        assert_eq!(workspace_rect(area, 1, false), Rect::new(0, 1, 80, 22));
        assert_eq!(status_rect(area, 1, false), Rect::new(0, 23, 80, 1));
        // COMPACT: statusline is gone, but the banner row STILL reserves (24 → 23),
        // which is the compact-proof guarantee.
        assert_eq!(banner_rect(area, 1, true), Rect::new(0, 0, 80, 1));
        assert_eq!(workspace_rect(area, 1, true), Rect::new(0, 1, 80, 23));
        assert_eq!(status_rect(area, 1, true).height, 0);
    }

    #[test]
    fn should_route_to_find_only_when_viewer_focused() {
        // Keystrokes go to the find buffer ONLY while find is active AND a viewer is
        // focused. If focus left the viewer, the dispatch force-exits find instead.
        assert!(should_route_to_find(true, true));
        assert!(!should_route_to_find(true, false));
        assert!(!should_route_to_find(false, true));
        assert!(!should_route_to_find(false, false));
    }

    #[test]
    fn reload_plan_keeps_state_on_vanished_or_warning_applies_on_clean() {
        // The state-safety guarantee: a vanished file or any warning must KEEP the
        // current config (a Banner, never Apply), so reload can never silently wipe
        // the live keymap/layouts/probes. Only a clean load applies.
        match plan_reload(Config::default(), false, None) {
            ReloadPlan::Keep(b) => assert_eq!(b.tone, BannerTone::Error),
            ReloadPlan::Apply(_) => panic!("a vanished file must KEEP state, not apply"),
        }
        match plan_reload(Config::default(), true, Some("dup chord".to_string())) {
            ReloadPlan::Keep(b) => {
                assert_eq!(b.tone, BannerTone::Conflict);
                assert_eq!(b.text, "dup chord");
            }
            ReloadPlan::Apply(_) => panic!("a warning must KEEP state, not apply"),
        }
        match plan_reload(Config::default(), true, None) {
            ReloadPlan::Apply(_) => {}
            ReloadPlan::Keep(_) => panic!("a clean load must apply"),
        }
    }

    #[test]
    fn chrome_split_chunks_tile_without_overlap() {
        let area = Rect::new(0, 0, 80, 24);
        for (banner_rows, compact) in [(0, false), (1, false), (0, true), (1, true)] {
            let [banner, workspace, status] = chrome_split(area, banner_rows, compact);
            // The three chunks tile the whole frame top-to-bottom with no gap/overlap.
            assert_eq!(banner.y, area.y);
            assert_eq!(workspace.y, banner.bottom());
            assert_eq!(status.y, workspace.bottom());
            assert_eq!(status.bottom(), area.bottom());
            assert_eq!(
                banner.height + workspace.height + status.height,
                area.height
            );
        }
    }

    #[test]
    fn pane_content_rects_shrink_when_banner_present() {
        // A single full-screen tiled pane: the content rect (the PTY winsize source)
        // loses one row when the banner reserves its row. Pure geometry, no PTY.
        let session = Session::single_shell(PathBuf::from("/work/x"));
        let area = Rect::new(0, 0, 80, 24);
        let id = session.focused().expect("a focused pane");
        let without = pane_content_rects(&session, area, 0, false)[&id].height;
        let with = pane_content_rects(&session, area, 1, false)[&id].height;
        assert_eq!(with, without - 1, "banner row must shrink the winsize");
    }

    #[test]
    fn stack_chip_click_resolves_to_member_through_banner_aware_workspace() {
        use crate::layout::{self, TileNode};
        // A root Stack of three members. Chip rects are computed THROUGH the same
        // banner-aware workspace rect the live hit-test uses, so a banner row offsets
        // the strip and a synthetic click still lands on the intended member's chip.
        let tree = TileNode::Stack {
            children: vec![
                TileNode::Leaf(PaneId(10)),
                TileNode::Leaf(PaneId(11)),
                TileNode::Leaf(PaneId(12)),
            ],
            active: 0,
        };
        let area = Rect::new(0, 0, 80, 24);
        // Banner present (1 row): the workspace — and thus the chip strip — shifts down.
        let workspace = workspace_rect(area, 1, false);
        assert_eq!(workspace.y, 1, "banner reserves the top row");
        let chips = layout::stack_chips(&tree, workspace);
        assert_eq!(chips.len(), 3);

        // Mirror pane_at's chip branch: a click inside a chip rect resolves to its id.
        let resolve = |column: u16, row: u16| -> Option<PaneId> {
            layout::stack_chips(&tree, workspace)
                .into_iter()
                .find(|chip| rect_contains(chip.rect, column, row))
                .map(|chip| chip.id)
        };

        for (i, want) in [PaneId(10), PaneId(11), PaneId(12)].into_iter().enumerate() {
            let chip = chips[i].rect;
            // Click the chip's center; the strip rides the banner-shifted workspace.
            let column = chip.x + chip.width / 2;
            let row = chip.y;
            assert!(row >= workspace.y, "chip strip sits inside the workspace");
            assert_eq!(resolve(column, row), Some(want), "chip {i}");
        }
    }

    #[test]
    fn pane_content_excludes_label() {
        let area = Rect::new(2, 3, 80, 24);
        let content = pane_content_rect(area, false);
        assert_eq!(content, Rect::new(2, 4, 80, 23));
    }

    #[test]
    fn tiny_pane_still_has_valid_content_rect() {
        let area = Rect::new(0, 0, 12, 1);
        let content = pane_content_rect(area, false);
        assert_eq!(content.height, 0);
        assert_eq!(content.width, 12);
    }

    #[test]
    fn rect_contains_uses_exclusive_bottom_right_edges() {
        let rect = Rect::new(3, 4, 10, 5);
        assert!(rect_contains(rect, 3, 4));
        assert!(rect_contains(rect, 12, 8));
        assert!(!rect_contains(rect, 13, 8));
        assert!(!rect_contains(rect, 12, 9));
    }

    #[test]
    fn global_shortcuts_keep_shell_typing_safe() {
        use ratatui::crossterm::event::KeyModifiers;

        // Thread the default keymap and assert post-collapse Action values
        // (GlobalShortcut::Help is now Action::OpenHelp).
        let map = Keymap::default();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(
            global_shortcut(&map, false, key(KeyCode::F(1))),
            Some(Action::OpenHelp)
        );
        assert_eq!(
            global_shortcut(&map, false, key(KeyCode::F(2))),
            Some(Action::OpenCommandPalette)
        );
        assert_eq!(
            global_shortcut(&map, false, key(KeyCode::F(6))),
            Some(Action::FocusNext)
        );
        assert_eq!(
            global_shortcut(&map, false, key(KeyCode::F(10))),
            Some(Action::Quit)
        );
        assert_eq!(
            global_shortcut(
                &map,
                false,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)
            ),
            Some(Action::OpenCommandPalette)
        );
        assert_eq!(
            global_shortcut(
                &map,
                false,
                KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL)
            ),
            Some(Action::OpenHelp)
        );
        assert_eq!(
            global_shortcut(
                &map,
                false,
                KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL)
            ),
            Some(Action::FocusNext)
        );
        // Shell-safety cases: plain '?', plain Tab, Shift+F2 fall through to typing.
        assert_eq!(global_shortcut(&map, false, key(KeyCode::Char('?'))), None);
        assert_eq!(global_shortcut(&map, false, key(KeyCode::Tab)), None);
        // Viewer-Tab gating: Tab only fires FocusNext when a viewer is focused.
        assert_eq!(
            global_shortcut(&map, true, key(KeyCode::Tab)),
            Some(Action::FocusNext)
        );
        assert_eq!(
            global_shortcut(
                &map,
                false,
                KeyEvent::new(KeyCode::F(2), KeyModifiers::SHIFT)
            ),
            None
        );
    }

    #[test]
    fn help_overlay_lists_only_bound_keys() {
        // Anti-drift guard: the help key labels come from the keymap, so rebinding
        // a global chord changes what the overlay advertises (never a stale string).
        // The help overlay advertises the Ctrl chords as primary on every platform
        // (the Alt row is a hand-written caveat).
        let map = Keymap::default();
        assert_eq!(
            advertised_chord_label(&map, BindAction::CommandPalette, false),
            "Ctrl+Enter"
        );
        assert_eq!(
            advertised_chord_label(&map, BindAction::Help, false),
            "Ctrl+/"
        );
        // FocusNext's Ctrl chord is Ctrl+Tab (the "if your terminal sends it" row).
        assert_eq!(
            advertised_chord_label(&map, BindAction::FocusNext, false),
            "Ctrl+Tab"
        );
        // The function-key summary is derived, listing the first three NONE F-keys.
        assert_eq!(f_key_summary(&map), "F1 help, F2 commands, F3 projects");
    }

    #[test]
    fn status_hints_derive_from_chord_label() {
        // Anti-drift guard for the statusline phrases: they are built from the
        // keymap's chord labels, not hardcoded.
        let map = Keymap::default();
        let leader = leader_hint(&map);
        assert!(leader.starts_with("c commands"), "{leader}");
        assert!(leader.contains("h help"), "{leader}");
        assert!(leader.contains("q quit"), "{leader}");
        // The primary hint names the command-palette and help chords.
        let hint = primary_hint(&map);
        assert!(hint.contains("commands"), "{hint}");
        assert!(hint.contains("help"), "{hint}");
    }

    #[test]
    fn macos_advertises_alt_as_primary_fallback() {
        let map = Keymap::default();
        // advertise_alt=true picks the Alt mirror; false picks the Ctrl chord.
        assert_eq!(
            advertised_chord_label(&map, BindAction::CommandPalette, true),
            "Alt+Enter"
        );
        assert_eq!(
            advertised_chord_label(&map, BindAction::CommandPalette, false),
            "Ctrl+Enter"
        );
        // The cfg!-driven primary hint matches the platform.
        let expected = if cfg!(target_os = "macos") {
            "Alt+Enter"
        } else {
            "Ctrl+Enter"
        };
        assert!(
            primary_hint(&map).starts_with(expected),
            "{}",
            primary_hint(&map)
        );
    }

    #[test]
    fn startup_project_prefers_config_then_cwd_then_first() {
        let projects = vec![
            Project {
                name: "one".into(),
                path: PathBuf::from("/work/one"),
                viewer: None,
            },
            Project {
                name: "two".into(),
                path: PathBuf::from("/work/two"),
                viewer: None,
            },
        ];
        assert_eq!(
            select_start_project(&projects, Some("two"), Path::new("/nowhere")),
            Some(1)
        );
        assert_eq!(
            select_start_project(&projects, Some("missing"), Path::new("/work/two/src")),
            Some(1)
        );
        assert_eq!(
            select_start_project(&projects, None, Path::new("/elsewhere")),
            Some(0)
        );
        assert_eq!(
            select_start_project(&[], None, Path::new("/elsewhere")),
            None
        );
    }

    #[test]
    fn viewer_path_uses_project_override_or_default_relative_to_project() {
        let project = Project {
            name: "one".into(),
            path: PathBuf::from("/work/one"),
            viewer: None,
        };
        assert_eq!(
            resolve_viewer_path(&project, Path::new("README.md")),
            PathBuf::from("/work/one/README.md")
        );

        let project = Project {
            name: "one".into(),
            path: PathBuf::from("/work/one"),
            viewer: Some(PathBuf::from("docs/start.md")),
        };
        assert_eq!(
            resolve_viewer_path(&project, Path::new("README.md")),
            PathBuf::from("/work/one/docs/start.md")
        );
    }

    #[test]
    fn fallback_session_uses_startup_project_context() {
        let projects = vec![Project {
            name: "one".into(),
            path: PathBuf::from("/work/one"),
            viewer: None,
        }];
        let (session, selected) =
            fallback_session(&projects, Some("one"), Path::new("/tmp/elsewhere"));
        assert_eq!(selected, Some(0));
        assert_eq!(
            session.selected_project(),
            Some(&ProjectSelection {
                name: "one".into(),
                path: PathBuf::from("/work/one"),
            })
        );
    }

    #[test]
    fn selected_project_can_be_rehydrated_from_session() {
        let projects = vec![Project {
            name: "one".into(),
            path: PathBuf::from("/work/one"),
            viewer: None,
        }];
        let session = Session::single_shell_for_project(
            PathBuf::from("/work/one"),
            Some(ProjectSelection {
                name: "one".into(),
                path: PathBuf::from("/work/one"),
            }),
        );
        assert_eq!(selected_project_from_session(&session, &projects), Some(0));
    }

    #[test]
    fn loaded_session_updates_stale_project_selection_to_fallback() {
        let projects = vec![Project {
            name: "fresh".into(),
            path: PathBuf::from("/work/fresh"),
            viewer: None,
        }];
        let mut session = Session::single_shell_for_project(
            PathBuf::from("/work/stale"),
            Some(ProjectSelection {
                name: "stale".into(),
                path: PathBuf::from("/work/stale"),
            }),
        );
        let persisted_selected = selected_project_from_session(&session, &projects);
        let selected =
            persisted_selected.or_else(|| select_start_project(&projects, None, Path::new("/tmp")));
        if persisted_selected != selected
            && let Some(index) = selected
            && let Some(project) = projects.get(index)
        {
            session.select_project(project_selection(project));
        }

        assert_eq!(selected, Some(0));
        assert_eq!(
            session.selected_project(),
            Some(&ProjectSelection {
                name: "fresh".into(),
                path: PathBuf::from("/work/fresh"),
            })
        );
    }

    #[test]
    fn initial_session_on_incompatible_file_refuses_to_restore_or_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.toml");
        // A file from a newer binary (format_version beyond CURRENT).
        std::fs::write(
            &path,
            "format_version = 999\nfocused = 0\nnext_pane_id = 1\n\n\
             [[panes]]\nid = 0\ntitle = \"SHELL\"\n\n\
             [panes.kind.Shell]\ncwd = \"/work/x\"\n\n\
             [tiled]\nLeaf = 0\n\n[floating]\n",
        )
        .unwrap();

        let projects = vec![Project {
            name: "x".into(),
            path: PathBuf::from("/work/x"),
            viewer: None,
        }];
        let init = initial_session(&path, &projects, None, Path::new("/work/x"));
        // Default session, NOT routed through the restore/rebuild arm, NOT persisted.
        assert!(!init.restored, "Incompatible must not mark restored");
        assert!(
            !init.persist,
            "Incompatible must not persist (refuse-to-clobber)"
        );
        let warning = init.warning.expect("Incompatible must carry a banner");
        assert!(warning.contains("newer Aetherspace"), "{warning}");
    }

    /// Replay a layout's steps on a Session exactly as `apply_layout` does at the
    /// session level (split_focused_shell per Split, resize_focused for the ratio,
    /// focus_* per Focus). Lets the tree SHAPE be asserted without spawning PTYs;
    /// the runtime wiring (apply_action → PTY spawn) is covered by the owner smoke.
    fn replay_layout_shape(layout: &NamedLayout) -> Session {
        let mut session = Session::single_shell(PathBuf::from("/work/x"));
        for step in &layout.steps {
            match *step {
                LayoutStep::Split { dir, ratio } => {
                    session
                        .split_focused_shell(PathBuf::from("/work/x"), dir)
                        .expect("split");
                    if let Some(target) = ratio {
                        assert!(session.resize_focused(split_ratio_delta(target)));
                    }
                }
                LayoutStep::Focus { dir } => match dir {
                    crate::config::FocusDir::Next => session.focus_next(),
                    crate::config::FocusDir::Prev => session.focus_prev(),
                },
            }
        }
        session
    }

    #[test]
    fn dev_layout_yields_big_left_split_right_tree() {
        use crate::layout::TileNode;
        let dev = NamedLayout {
            name: "dev".into(),
            steps: vec![
                LayoutStep::Split {
                    dir: SplitDir::Horizontal,
                    ratio: Some(65),
                },
                LayoutStep::Split {
                    dir: SplitDir::Vertical,
                    ratio: None,
                },
            ],
        };
        let session = replay_layout_shape(&dev);
        // Root: horizontal split, left 65%, right is a vertical split (top/bottom).
        let tree = session.tiled().expect("tiled");
        match tree {
            TileNode::Split { dir, ratio, a, b } => {
                assert_eq!(*dir, SplitDir::Horizontal, "root is the left/right split");
                assert_eq!(*ratio, 65, "left pane occupies 65%");
                assert!(
                    matches!(a.as_ref(), TileNode::Leaf(_)),
                    "left child is a single leaf"
                );
                match b.as_ref() {
                    TileNode::Split { dir, a, b, .. } => {
                        assert_eq!(*dir, SplitDir::Vertical, "right child is top/bottom");
                        assert!(matches!(a.as_ref(), TileNode::Leaf(_)));
                        assert!(matches!(b.as_ref(), TileNode::Leaf(_)));
                    }
                    other => panic!("right child must be a vertical split, got {other:?}"),
                }
            }
            other => panic!("root must be a horizontal split, got {other:?}"),
        }
    }

    #[test]
    fn two_horizontal_splits_yield_three_leaves() {
        let triple = NamedLayout {
            name: "triple".into(),
            steps: vec![
                LayoutStep::Split {
                    dir: SplitDir::Horizontal,
                    ratio: None,
                },
                LayoutStep::Split {
                    dir: SplitDir::Horizontal,
                    ratio: None,
                },
            ],
        };
        let session = replay_layout_shape(&triple);
        let leaves = session.tiled().map(crate::layout::leaves).expect("tiled");
        assert_eq!(leaves.len(), 3, "two splits on one shell yield three panes");
    }
}
