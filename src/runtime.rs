//! Unified runtime loop for the Phase 2 shell surface.

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
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::action::Action;
use crate::event::{PaneProcessId, RuntimeEvent};
use crate::input::{InputConfig, InputRouter};
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
        let content = shell_content_rect(area);
        for pane in self.panes.values_mut() {
            pane.resize(content.height, content.width);
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
        Action::FocusNext | Action::FocusPrev => true,
        Action::Noop => dirty,
    }
}

fn draw(frame: &mut Frame, app: &RuntimeApp) {
    let area = frame.area();
    let shell_area = shell_area(area);
    let content = shell_content_rect(area);
    let status = status_rect(area);

    if let Some((title, pane)) = focused_pane(app) {
        let label_style = Theme::label_focused();
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {title}"), label_style))),
            label_rect(shell_area),
        );

        if content.width > 0 && content.height > 0 {
            let term = PseudoTerminal::new(pane.shell_screen())
                .cursor(Cursor::default().visibility(pane.is_running()));
            frame.render_widget(term, content);
        }
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " no pane",
                Style::default().fg(Theme::DIM),
            ))),
            label_rect(shell_area),
        );
    }

    let hint = if let Some(error) = &app.last_error {
        error.as_str()
    } else {
        "^Space q: quit  r: restart  x: close"
    };
    let status_line = Line::from(vec![
        Span::styled(" Aetherspace ", Style::default().fg(Theme::FG)),
        Span::styled("│", Style::default().fg(Theme::HAIR)),
        Span::styled(" phase 2 pane runtime ", Style::default().fg(Theme::DIM)),
        Span::styled("│", Style::default().fg(Theme::HAIR)),
        Span::styled(format!(" {hint}"), Style::default().fg(Theme::DIM)),
    ]);
    frame.render_widget(Paragraph::new(status_line), status);
}

fn focused_pane(app: &RuntimeApp) -> Option<(String, &PaneRuntime)> {
    let spec = app.session.focused_spec()?;
    let pane = app.panes.get(&spec.id)?;
    Some((pane.title(spec), pane))
}

fn shell_area(area: Rect) -> Rect {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area)[0]
}

fn label_rect(shell_area: Rect) -> Rect {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(shell_area)[0]
}

fn shell_content_rect(area: Rect) -> Rect {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area)[1]
}

fn status_rect(area: Rect) -> Rect {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area)[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_content_excludes_label_and_statusline() {
        let area = Rect::new(0, 0, 80, 24);
        let content = shell_content_rect(area);
        assert_eq!(content, Rect::new(0, 1, 80, 22));
    }

    #[test]
    fn tiny_terminal_still_has_valid_content_rect() {
        let area = Rect::new(0, 0, 12, 1);
        let content = shell_content_rect(area);
        assert_eq!(content.height, 0);
        assert_eq!(content.width, 12);
    }
}
