//! Unified runtime loop for the Aetherspace TUI.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
use crate::config::{Config, Project};
use crate::event::{PaneProcessId, RuntimeEvent};
use crate::input::{InputConfig, InputRouter, encode_mouse};
use crate::layout::{self, FloatGeom, SolvedPane, SplitDir};
use crate::pane::{PaneRuntime, ensure_spec_matches_runtime};
use crate::session::{PaneId, PaneSpec, ProjectSelection, Session};
use crate::session_store;
use crate::status::{self, StatusSnapshot, StatusTarget};
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
    let (session, selected_project, restored_session) =
        initial_session(&projects, config.workflow.startup_project.as_deref(), &cwd);
    let status_target = StatusTarget::new(selected_project_path(&projects, selected_project));
    let scrollback = config.shell.scrollback;
    let mut app = match RuntimeApp::new(RuntimeAppInit {
        session,
        input: InputRouter::new(InputConfig::from_leader_name(&config.input.leader)),
        scrollback,
        notify: tx.clone(),
        projects: projects.clone(),
        selected_project,
        default_viewer: config.workflow.default_viewer.clone(),
        status_target: status_target.clone(),
        config_warning: config_warning.clone(),
        nest_depth,
    }) {
        Ok(app) => app,
        Err(e) if restored_session => {
            crate::log::warn(&format!("persisted session ignored: {e}"));
            let (session, selected_project) =
                fallback_session(&projects, config.workflow.startup_project.as_deref(), &cwd);
            status_target.set(selected_project_path(&projects, selected_project));
            RuntimeApp::new(RuntimeAppInit {
                session,
                input: InputRouter::new(InputConfig::from_leader_name(&config.input.leader)),
                scrollback,
                notify: tx.clone(),
                projects,
                selected_project,
                default_viewer: config.workflow.default_viewer,
                status_target: status_target.clone(),
                config_warning,
                nest_depth,
            })?
        }
        Err(e) => return Err(e),
    };

    let mut guard = TerminalGuard::enter();
    panic_after_terminal_for_smoke();
    spawn_input_thread(tx);
    status::spawn_status_thread(status_config, status_target, app.notify.clone());

    let result = run_loop(&mut guard, &mut app, rx);
    session_store::save(&app.session);
    app.shutdown();
    result
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
    projects: Vec<Project>,
    selected_project: Option<usize>,
    default_viewer: PathBuf,
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
    /// Set ONLY by the discrete session-mutating actions (split/close/restart/
    /// focus/resize/zoom/float, project select, open shell/viewer, reset). Drives
    /// the reboot-durable autosave in `run_loop`. NEVER set on SendBytes/Pty/
    /// Render/terminal-driven Resize: typing in a shell must not thrash the disk.
    session_dirty: bool,
    status: StatusSnapshot,
    status_target: StatusTarget,
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
    scrollback: usize,
    notify: Sender<RuntimeEvent>,
    projects: Vec<Project>,
    selected_project: Option<usize>,
    default_viewer: PathBuf,
    status_target: StatusTarget,
    config_warning: Option<String>,
    nest_depth: u32,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaletteCommand {
    ProjectPicker,
    OpenProjectViewer,
    OpenProjectShell,
    StatusDetails,
    ResetWorkspace,
    SplitHorizontal,
    SplitVertical,
    ToggleFloat,
    ToggleZoom,
    ToggleCompact,
    Restart,
    Close,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusTone {
    Normal,
    Notice,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GlobalShortcut {
    Help,
    Action(Action),
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
            projects: init.projects,
            selected_project: init.selected_project,
            default_viewer: init.default_viewer,
            palette: None,
            show_help: true,
            compact_chrome: false,
            last_area: Rect::default(),
            scrollback: init.scrollback,
            notify: init.notify,
            last_error: None,
            last_notice: None,
            config_warning: init.config_warning,
            session_dirty: false,
            status: StatusSnapshot::default(),
            status_target: init.status_target,
            nest_depth: init.nest_depth,
            pending_editor: None,
        })
    }

    fn shutdown(&mut self) {
        for pane in self.panes.values_mut() {
            pane.terminate();
        }
    }

    fn resize_to_area(&mut self, area: Rect) {
        self.last_area = area;
        let content_rects = pane_content_rects(&self.session, area, self.compact_chrome);
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
        pane_content_rects(&self.session, self.last_area, self.compact_chrome)
            .get(&id)
            .map(|rect| rect.height)
            .unwrap_or(INITIAL_ROWS)
    }

    fn spawn_runtime_for_spec(&mut self, spec: PaneSpec) -> Result<()> {
        let content_rects = pane_content_rects(&self.session, self.last_area, self.compact_chrome);
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

    fn open_help(&mut self) {
        self.palette = None;
        self.show_help = true;
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
        let geom = FloatGeom::centered(workspace_rect(self.last_area, self.compact_chrome));
        if self.session.toggle_float_focused(geom) {
            self.resize_to_area(self.last_area);
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
        let content =
            *pane_content_rects(&self.session, self.last_area, self.compact_chrome).get(&id)?;
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
        if let Some(id) = self.pane_at(column, row)
            && self.session.focus_pane(id)
        {
            self.resize_to_area(self.last_area);
            return true;
        }
        false
    }

    fn pane_at(&self, column: u16, row: u16) -> Option<PaneId> {
        let workspace = workspace_rect(self.last_area, self.compact_chrome);
        if self.session.zoomed().is_none() {
            for (id, geom) in self.session.floating().iter().rev() {
                let rect = layout::resolve_float(*geom, workspace);
                if rect_contains(rect, column, row) {
                    return Some(*id);
                }
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
            PaletteCommand::ToggleCompact => apply_action(self, Action::ToggleCompactChrome),
            PaletteCommand::Restart => apply_action(self, Action::RestartFocusedPane),
            PaletteCommand::Close => apply_action(self, Action::CloseFocusedPane),
            PaletteCommand::Quit => apply_action(self, Action::Quit),
        }
    }

    fn handle_viewer_key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        let rows = self.focused_content_height();
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
                session_store::save(&app.session);
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
    // First real keypress dismisses the config-warning banner. No timer exists,
    // so this is the only way it clears; force a redraw so it actually vanishes
    // (the blocked loop redraws only when the triggering key is dirty).
    let cleared_warning = key.kind != KeyEventKind::Release && app.config_warning.take().is_some();
    // OR the dismissal in so the banner vanishes even when the key was otherwise
    // a no-op for rendering.
    dispatch_key(app, key) || cleared_warning
}

fn dispatch_key(app: &mut RuntimeApp, key: KeyEvent) -> bool {
    if app.show_help {
        return handle_help_key(app, key);
    }
    if let Some(shortcut) = global_shortcut(app.focused_is_viewer(), key) {
        return match shortcut {
            GlobalShortcut::Help => {
                app.open_help();
                true
            }
            GlobalShortcut::Action(action) => apply_action(app, action),
        };
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

fn global_shortcut(focused_is_viewer: bool, key: KeyEvent) -> Option<GlobalShortcut> {
    if key.kind == KeyEventKind::Release {
        return None;
    }

    if key.modifiers == KeyModifiers::NONE {
        return match key.code {
            KeyCode::F(1) => Some(GlobalShortcut::Help),
            KeyCode::F(2) => Some(GlobalShortcut::Action(Action::OpenCommandPalette)),
            KeyCode::F(3) => Some(GlobalShortcut::Action(Action::OpenProjectPalette)),
            KeyCode::F(4) => Some(GlobalShortcut::Action(Action::OpenProjectViewer)),
            KeyCode::F(5) => Some(GlobalShortcut::Action(Action::OpenProjectShell)),
            KeyCode::F(6) => Some(GlobalShortcut::Action(Action::FocusNext)),
            KeyCode::F(7) => Some(GlobalShortcut::Action(Action::FocusPrev)),
            KeyCode::F(8) => Some(GlobalShortcut::Action(Action::ToggleZoomFocusedPane)),
            KeyCode::F(9) => Some(GlobalShortcut::Action(Action::CloseFocusedPane)),
            KeyCode::F(10) => Some(GlobalShortcut::Action(Action::Quit)),
            KeyCode::Tab if focused_is_viewer => Some(GlobalShortcut::Action(Action::FocusNext)),
            KeyCode::BackTab if focused_is_viewer => {
                Some(GlobalShortcut::Action(Action::FocusPrev))
            }
            _ => None,
        };
    }

    if key.modifiers == KeyModifiers::CONTROL {
        return match key.code {
            KeyCode::Enter => Some(GlobalShortcut::Action(Action::OpenCommandPalette)),
            KeyCode::Char('/') => Some(GlobalShortcut::Help),
            KeyCode::Tab => Some(GlobalShortcut::Action(Action::FocusNext)),
            KeyCode::BackTab => Some(GlobalShortcut::Action(Action::FocusPrev)),
            _ => None,
        };
    }

    if key.modifiers == KeyModifiers::ALT {
        return match key.code {
            KeyCode::Enter => Some(GlobalShortcut::Action(Action::OpenCommandPalette)),
            KeyCode::Char('/') => Some(GlobalShortcut::Help),
            KeyCode::Tab => Some(GlobalShortcut::Action(Action::FocusNext)),
            KeyCode::BackTab => Some(GlobalShortcut::Action(Action::FocusPrev)),
            _ => None,
        };
    }

    if key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT) {
        return match key.code {
            KeyCode::Tab | KeyCode::BackTab => Some(GlobalShortcut::Action(Action::FocusPrev)),
            _ => None,
        };
    }

    None
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
        Action::Noop => dirty,
    }
}

fn draw(frame: &mut Frame, app: &RuntimeApp) {
    let area = frame.area();
    let workspace = workspace_rect(area, app.compact_chrome);
    let status = status_rect(area, app.compact_chrome);

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
        draw_help(frame, workspace);
    }

    if !app.compact_chrome {
        draw_statusline(frame, app, status);
    }
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

fn draw_help(frame: &mut Frame, workspace: Rect) {
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

    let lines = vec![
        help_line("Ctrl+Enter", "commands", "open command palette"),
        help_line(
            "Alt+Enter",
            "commands",
            "fallback if Ctrl+Enter is not sent",
        ),
        help_line("Ctrl+/", "help", "open this guide"),
        help_line("click", "focus", "select a pane"),
        help_line("Ctrl+Tab", "focus", "next pane if your terminal sends it"),
        help_line("Shift+Tab", "focus", "previous pane in viewer panes"),
        help_line("palette", "project", "open project picker or a new shell"),
        help_line("palette", "reset", "collapse clutter to one project shell"),
        help_line("F keys", "fallback", "F1 help, F2 commands, F10 quit"),
        help_line("^Space", "leader", "legacy command prefix"),
        help_line("Esc/Enter", "close", "close this help panel"),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn help_line(key: &'static str, label: &'static str, detail: &'static str) -> Line<'static> {
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
        spans.push(Span::styled(truncate_width(message, budget), style));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn status_message(app: &RuntimeApp) -> (&str, StatusTone) {
    if let Some(warning) = &app.config_warning {
        // Highest priority and shown even over the startup help, since a present
        // config that failed to parse is a state the user should fix. Cleared on
        // the first keypress (see `handle_key`).
        (warning.as_str(), StatusTone::Error)
    } else if let Some(error) = &app.last_error {
        (error.as_str(), StatusTone::Error)
    } else if let Some(notice) = &app.last_notice {
        (notice.as_str(), StatusTone::Notice)
    } else if app.show_help {
        (
            "startup guide  Ctrl+Enter commands  Ctrl+/ help  Esc close",
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
            "status details  up/down inspect  enter/esc close",
            StatusTone::Normal,
        )
    } else if app.palette.is_some() {
        ("enter run  up/down select  esc close", StatusTone::Normal)
    } else if app.focused_is_viewer() {
        (
            "viewer  j/k scroll  Tab focus  Ctrl+Enter commands  Ctrl+/ help",
            StatusTone::Normal,
        )
    } else if app.input.mode_label() == "leader" {
        (
            "c commands  p projects  v viewer  s shell  h help  q quit",
            StatusTone::Normal,
        )
    } else if app.focused_child_mouse_enabled() {
        (
            "shell capture  mouse->child  Ctrl+Enter commands  Ctrl+/ help",
            StatusTone::Normal,
        )
    } else {
        (
            "click focus  Ctrl+Enter commands  Ctrl+/ help  ^Space leader",
            StatusTone::Normal,
        )
    }
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

fn pane_content_rects(session: &Session, area: Rect, compact: bool) -> BTreeMap<PaneId, Rect> {
    let workspace = workspace_rect(area, compact);
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

fn workspace_rect(area: Rect, compact: bool) -> Rect {
    if compact {
        return area;
    }
    RatatuiLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area)[0]
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

fn status_rect(area: Rect, compact: bool) -> Rect {
    if compact {
        return Rect::new(area.x, area.bottom(), area.width, 0);
    }
    RatatuiLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area)[1]
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

fn initial_session(
    projects: &[Project],
    startup_project: Option<&str>,
    cwd: &Path,
) -> (Session, Option<usize>, bool) {
    if let Some(mut session) = session_store::load() {
        let persisted_selected = selected_project_from_session(&session, projects);
        let selected =
            persisted_selected.or_else(|| select_start_project(projects, startup_project, cwd));
        if persisted_selected != selected
            && let Some(index) = selected
            && let Some(project) = projects.get(index)
        {
            session.select_project(project_selection(project));
        }
        return (session, selected, true);
    }

    let (session, selected) = fallback_session(projects, startup_project, cwd);
    (session, selected, false)
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
        assert_eq!(workspace_rect(area, false), Rect::new(0, 0, 80, 23));
        assert_eq!(status_rect(area, false), Rect::new(0, 23, 80, 1));
    }

    #[test]
    fn compact_chrome_reclaims_label_and_status_rows() {
        let area = Rect::new(0, 0, 80, 24);
        // Compact: workspace fills the frame and the statusline collapses to zero height.
        assert_eq!(workspace_rect(area, true), area);
        assert_eq!(status_rect(area, true).height, 0);
        // Compact: a pane keeps its whole rect as content (no label row reserved).
        let pane = Rect::new(2, 3, 40, 10);
        assert_eq!(pane_content_rect(pane, true), pane);
        // Normal mode still reserves exactly one row each, so the toggle is reversible.
        assert_eq!(pane_content_rect(pane, false), Rect::new(2, 4, 40, 9));
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

        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(
            global_shortcut(false, key(KeyCode::F(1))),
            Some(GlobalShortcut::Help)
        );
        assert_eq!(
            global_shortcut(false, key(KeyCode::F(2))),
            Some(GlobalShortcut::Action(Action::OpenCommandPalette))
        );
        assert_eq!(
            global_shortcut(false, key(KeyCode::F(6))),
            Some(GlobalShortcut::Action(Action::FocusNext))
        );
        assert_eq!(
            global_shortcut(false, key(KeyCode::F(10))),
            Some(GlobalShortcut::Action(Action::Quit))
        );
        assert_eq!(
            global_shortcut(false, KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
            Some(GlobalShortcut::Action(Action::OpenCommandPalette))
        );
        assert_eq!(
            global_shortcut(
                false,
                KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL)
            ),
            Some(GlobalShortcut::Help)
        );
        assert_eq!(
            global_shortcut(false, KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL)),
            Some(GlobalShortcut::Action(Action::FocusNext))
        );
        assert_eq!(global_shortcut(false, key(KeyCode::Char('?'))), None);
        assert_eq!(global_shortcut(false, key(KeyCode::Tab)), None);
        assert_eq!(
            global_shortcut(true, key(KeyCode::Tab)),
            Some(GlobalShortcut::Action(Action::FocusNext))
        );
        assert_eq!(
            global_shortcut(false, KeyEvent::new(KeyCode::F(2), KeyModifiers::SHIFT)),
            None
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
}
