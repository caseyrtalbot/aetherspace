//! Unified runtime loop for the Phase 5 workflow surface.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use ratatui::crossterm::event::{
    self, KeyCode, KeyEvent, KeyEventKind, MouseEvent, MouseEventKind,
};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Constraint, Direction, Layout as RatatuiLayout, Rect},
    style::Style,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::action::Action;
use crate::config::{Config, Project};
use crate::event::{PaneProcessId, RuntimeEvent};
use crate::input::{InputConfig, InputRouter};
use crate::layout::{self, FloatGeom, SolvedPane, SplitDir};
use crate::pane::{PaneRuntime, ensure_spec_matches_runtime};
use crate::session::{PaneId, PaneSpec, ProjectSelection, Session};
use crate::terminal::TerminalGuard;
use crate::theme::Theme;

const FRAME: Duration = Duration::from_millis(16);
const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;
const MAX_EVENTS_PER_TURN: usize = 256;

pub(crate) fn run(config: Config) -> Result<()> {
    let (tx, rx) = mpsc::channel::<RuntimeEvent>();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let projects = config.resolve_projects();
    let selected_project =
        select_start_project(&projects, config.workflow.startup_project.as_deref(), &cwd);
    let shell_cwd = selected_project
        .and_then(|idx| projects.get(idx).map(|project| project.path.clone()))
        .unwrap_or_else(|| cwd.clone());
    let session = Session::single_shell_for_project(
        shell_cwd,
        selected_project
            .and_then(|idx| projects.get(idx))
            .map(project_selection),
    );
    let scrollback = config.shell.scrollback;
    let mut app = RuntimeApp::new(
        session,
        InputRouter::new(InputConfig::from_leader_name(&config.input.leader)),
        scrollback,
        tx.clone(),
        projects,
        selected_project,
        config.workflow.default_viewer,
    )?;

    let mut guard = TerminalGuard::enter();
    panic_after_terminal_for_smoke();
    spawn_input_thread(tx);

    let result = run_loop(guard.terminal_mut(), &mut app, rx);
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
    last_area: Rect,
    scrollback: usize,
    notify: Sender<RuntimeEvent>,
    last_error: Option<String>,
    last_notice: Option<String>,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaletteCommand {
    ProjectPicker,
    OpenProjectViewer,
    OpenProjectShell,
    SplitHorizontal,
    SplitVertical,
    ToggleFloat,
    ToggleZoom,
    Restart,
    Close,
    Quit,
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
    fn new(
        session: Session,
        input: InputRouter,
        scrollback: usize,
        notify: Sender<RuntimeEvent>,
        projects: Vec<Project>,
        selected_project: Option<usize>,
        default_viewer: PathBuf,
    ) -> Result<Self> {
        let mut panes = BTreeMap::new();
        for spec in session.pane_specs() {
            let pane =
                PaneRuntime::spawn(spec, INITIAL_ROWS, INITIAL_COLS, scrollback, notify.clone())?;
            panes.insert(spec.id, pane);
        }
        Ok(Self {
            running: true,
            session,
            panes,
            input,
            projects,
            selected_project,
            default_viewer,
            palette: None,
            last_area: Rect::default(),
            scrollback,
            notify,
            last_error: None,
            last_notice: None,
        })
    }

    fn shutdown(&mut self) {
        for pane in self.panes.values_mut() {
            pane.terminate();
        }
    }

    fn resize_to_area(&mut self, area: Rect) {
        self.last_area = area;
        let content_rects = pane_content_rects(&self.session, area);
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
        pane_content_rects(&self.session, self.last_area)
            .get(&id)
            .map(|rect| rect.height)
            .unwrap_or(INITIAL_ROWS)
    }

    fn spawn_runtime_for_spec(&mut self, spec: PaneSpec) -> Result<()> {
        let content_rects = pane_content_rects(&self.session, self.last_area);
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
        self.spawn_runtime_for_spec(spec)
    }

    fn open_project(&mut self, index: usize) -> Result<()> {
        self.select_project(index)?;
        self.open_shell_for_selected_project()
    }

    fn open_command_palette(&mut self) {
        self.palette = Some(Palette {
            kind: PaletteKind::Commands,
            selected: 0,
        });
    }

    fn open_project_palette(&mut self) {
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
        let geom = FloatGeom::centered(workspace_rect(self.last_area));
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
        if matches!(mouse.kind, MouseEventKind::Moved) {
            return false;
        }
        self.last_notice = Some("mouse policy: child forwarding off".to_string());
        true
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
        while let Ok(raw) = event::read() {
            let Some(event) = RuntimeEvent::from_crossterm(raw) else {
                continue;
            };
            if tx.send(event).is_err() {
                break;
            }
        }
    });
}

fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut RuntimeApp,
    rx: Receiver<RuntimeEvent>,
) -> Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let size = terminal.size()?;
    app.resize_to_area(Rect::new(0, 0, size.width, size.height));
    terminal.draw(|frame| draw(frame, app))?;
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

        if dirty && app.running {
            let since = last_draw.elapsed();
            if since < FRAME {
                thread::sleep(FRAME - since);
            }
            terminal.draw(|frame| draw(frame, app))?;
            last_draw = Instant::now();
        }
    }
    Ok(())
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
        RuntimeEvent::Status | RuntimeEvent::Tick => true,
    }
}

fn handle_key(app: &mut RuntimeApp, key: KeyEvent) -> bool {
    if let Some(dirty) = app.handle_palette_key(key) {
        return dirty;
    }
    if app.focused_is_viewer() && !app.input.is_leader_key(key) && app.handle_viewer_key(key) {
        return true;
    }
    let action = app.input.route_key(key);
    apply_action(app, action)
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
            true
        }
        Action::RestartFocusedPane => {
            if let Err(e) = app.restart_focused() {
                app.last_error = Some(format!("restart failed: {e}"));
            } else {
                app.last_error = None;
            }
            true
        }
        Action::CloseFocusedPane => {
            app.close_focused();
            true
        }
        Action::FocusNext => {
            app.focus_next();
            true
        }
        Action::FocusPrev => {
            app.focus_prev();
            true
        }
        Action::ResizeFocusedPane { delta } => {
            app.resize_focused(delta);
            true
        }
        Action::ToggleZoomFocusedPane => {
            app.toggle_zoom_focused();
            true
        }
        Action::ToggleFloatFocusedPane => {
            app.toggle_float_focused();
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
        Action::Noop => dirty,
    }
}

fn draw(frame: &mut Frame, app: &RuntimeApp) {
    let area = frame.area();
    let workspace = workspace_rect(area);
    let status = status_rect(area);

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

    let hint = if let Some(error) = &app.last_error {
        error.as_str()
    } else if let Some(notice) = &app.last_notice {
        notice.as_str()
    } else if app.palette.is_some() {
        "enter run  up/down select  esc close"
    } else if app.focused_is_viewer() {
        "viewer  j/k scroll  pgup/pgdn page  leader commands"
    } else if app.input.mode_label() == "leader" {
        "c palette  p projects  v viewer  s shell  |/- split  q quit"
    } else {
        "shell capture  leader opens workflow"
    };
    let leader = app.input.leader_label();
    let project = app
        .session
        .selected_project()
        .map(|project| project.name.as_str())
        .unwrap_or("no project");
    let status_line = Line::from(vec![
        Span::styled(" Aetherspace ", Style::default().fg(Theme::FG)),
        Span::styled("│", Style::default().fg(Theme::HAIR)),
        Span::styled(
            format!(" phase 5 workflow · {project} "),
            Style::default().fg(Theme::DIM),
        ),
        Span::styled("│", Style::default().fg(Theme::HAIR)),
        Span::styled(
            format!(" panes:{} ", app.panes.len()),
            Style::default().fg(Theme::DIM),
        ),
        Span::styled("│", Style::default().fg(Theme::HAIR)),
        Span::styled(
            format!(" {leader}:{} ", app.input.mode_label()),
            Style::default().fg(Theme::DIM),
        ),
        Span::styled("│", Style::default().fg(Theme::HAIR)),
        Span::styled(format!(" {hint}"), Style::default().fg(Theme::DIM)),
    ]);
    frame.render_widget(Paragraph::new(status_line), status);
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

    let content = pane_content_rect(area);
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
    };
    let rect = overlay_rect(workspace, 78, desired_height.max(5));
    if rect.width < 8 || rect.height < 3 {
        return;
    }

    frame.render_widget(Clear, rect);
    let title = match palette.kind {
        PaletteKind::Commands => " command palette ",
        PaletteKind::Projects => " projects ",
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
        palette_lines(app, palette, inner.height)
    };
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn palette_lines(app: &RuntimeApp, palette: Palette, height: u16) -> Vec<Line<'static>> {
    let len = app.palette_len(palette.kind);
    let rows = height.max(1) as usize;
    let selected = palette.selected.min(len.saturating_sub(1));
    let start = selected.saturating_sub(rows.saturating_sub(1));
    let end = len.min(start + rows);
    (start..end)
        .map(|idx| match palette.kind {
            PaletteKind::Commands => {
                let item = COMMAND_ITEMS[idx];
                palette_line(idx == selected, item.label, item.detail)
            }
            PaletteKind::Projects => {
                let project = &app.projects[idx];
                palette_line(
                    idx == selected,
                    project.name.as_str(),
                    &project.path.display().to_string(),
                )
            }
        })
        .collect()
}

fn palette_line(selected: bool, label: &str, detail: &str) -> Line<'static> {
    let label_style = if selected {
        Theme::label_focused()
    } else {
        Style::default().fg(Theme::FG)
    };
    Line::from(vec![
        Span::styled(
            if selected { "> " } else { "  " },
            Style::default().fg(Theme::ACCENT),
        ),
        Span::styled(label.to_string(), label_style),
        Span::styled(format!("  {detail}"), Style::default().fg(Theme::DIM)),
    ])
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

fn pane_content_rects(session: &Session, area: Rect) -> BTreeMap<PaneId, Rect> {
    let workspace = workspace_rect(area);
    let mut rects = BTreeMap::new();
    for pane in tiled_panes(session, workspace) {
        rects.insert(pane.id, pane_content_rect(pane.rect));
    }
    if session.zoomed().is_none() {
        for (id, geom) in session.floating() {
            let rect = layout::resolve_float(*geom, workspace);
            rects.insert(*id, pane_content_rect(rect));
        }
    }
    rects
}

fn workspace_rect(area: Rect) -> Rect {
    RatatuiLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area)[0]
}

fn label_rect(area: Rect) -> Rect {
    Rect::new(area.x, area.y, area.width, area.height.min(1))
}

fn pane_content_rect(area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    )
}

fn status_rect(area: Rect) -> Rect {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_excludes_statusline() {
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(workspace_rect(area), Rect::new(0, 0, 80, 23));
        assert_eq!(status_rect(area), Rect::new(0, 23, 80, 1));
    }

    #[test]
    fn pane_content_excludes_label() {
        let area = Rect::new(2, 3, 80, 24);
        let content = pane_content_rect(area);
        assert_eq!(content, Rect::new(2, 4, 80, 23));
    }

    #[test]
    fn tiny_pane_still_has_valid_content_rect() {
        let area = Rect::new(0, 0, 12, 1);
        let content = pane_content_rect(area);
        assert_eq!(content.height, 0);
        assert_eq!(content.width, 12);
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
}
