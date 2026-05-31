//! Unified runtime loop for the Phase 3 layout surface.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use ratatui::crossterm::event::{self, MouseEvent};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Constraint, Direction, Layout as RatatuiLayout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::action::Action;
use crate::event::{PaneProcessId, RuntimeEvent};
use crate::input::{InputConfig, InputRouter};
use crate::layout::{self, FloatGeom, SolvedPane, SplitDir};
use crate::pane::{PaneRuntime, ensure_spec_matches_runtime};
use crate::session::{PaneId, Session};
use crate::terminal::TerminalGuard;
use crate::theme::Theme;

const FRAME: Duration = Duration::from_millis(16);
const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;

pub(crate) fn run(scrollback: usize) -> Result<()> {
    let (tx, rx) = mpsc::channel::<RuntimeEvent>();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let session = Session::single_shell(cwd);
    let mut app = RuntimeApp::new(
        session,
        InputRouter::new(InputConfig::default()),
        scrollback,
        tx.clone(),
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
    last_area: Rect,
    scrollback: usize,
    notify: Sender<RuntimeEvent>,
    last_error: Option<String>,
}

impl RuntimeApp {
    fn new(
        session: Session,
        input: InputRouter,
        scrollback: usize,
        notify: Sender<RuntimeEvent>,
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
            last_area: Rect::default(),
            scrollback,
            notify,
            last_error: None,
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
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let Some(spec) = self.session.split_focused_shell(cwd, dir) else {
            bail!("focused pane cannot be split");
        };
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
        if let Some(pane) = self.panes.get_mut(&id.pane) {
            pane.process_pending(id);
        }
    }

    fn mark_child_exit(&mut self, id: PaneProcessId) {
        if let Some(pane) = self.panes.get_mut(&id.pane) {
            pane.mark_child_exit(id);
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
        while let Ok(event) = rx.try_recv() {
            dirty |= handle(app, event);
            if !app.running {
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
        RuntimeEvent::Key(key) => {
            let action = app.input.route_key(key);
            apply_action(app, action)
        }
        RuntimeEvent::Paste(text) => {
            let action = app.input.route_paste(text);
            apply_action(app, action)
        }
        RuntimeEvent::Resize(cols, rows) => apply_action(app, Action::Resize { cols, rows }),
        RuntimeEvent::Mouse(mouse) => handle_mouse(mouse),
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

fn handle_mouse(_mouse: MouseEvent) -> bool {
    false
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

    let hint = if let Some(error) = &app.last_error {
        error.as_str()
    } else {
        "^Space |/- split  tab focus  </> size  z zoom  f float  r restart  x close  q quit"
    };
    let status_line = Line::from(vec![
        Span::styled(" Aetherspace ", Style::default().fg(Theme::FG)),
        Span::styled("│", Style::default().fg(Theme::HAIR)),
        Span::styled(" phase 3 layout runtime ", Style::default().fg(Theme::DIM)),
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
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{prefix}{}", pane.title(spec)),
            label_style,
        ))),
        label_rect(area),
    );

    let content = pane_content_rect(area);
    if content.width > 0 && content.height > 0 {
        let term = PseudoTerminal::new(pane.shell_screen())
            .cursor(Cursor::default().visibility(focused && pane.is_running()));
        frame.render_widget(term, content);
    }
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
}
