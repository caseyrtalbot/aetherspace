//! Unified runtime loop for the Phase 1 shell surface.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
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
use crate::event::RuntimeEvent;
use crate::input::{InputConfig, InputRouter};
use crate::shell::Shell;
use crate::terminal::TerminalGuard;
use crate::theme::Theme;

const FRAME: Duration = Duration::from_millis(16);

pub(crate) fn run(scrollback: usize) -> Result<()> {
    let (tx, rx) = mpsc::channel::<RuntimeEvent>();
    let shell = Shell::spawn(24, 80, scrollback, tx.clone())?;
    let mut app = RuntimeApp::new(shell, InputRouter::new(InputConfig::default()));

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
    shell: Shell,
    input: InputRouter,
    last_area: Rect,
}

impl RuntimeApp {
    fn new(shell: Shell, input: InputRouter) -> Self {
        Self {
            running: true,
            shell,
            input,
            last_area: Rect::default(),
        }
    }

    fn shutdown(&mut self) {
        self.shell.terminate();
    }

    fn resize_to_area(&mut self, area: Rect) {
        self.last_area = area;
        let content = shell_content_rect(area);
        self.shell.resize(content.height, content.width);
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
        RuntimeEvent::Pty => {
            app.shell.process_pending();
            if !app.shell.is_alive() {
                app.running = false;
            }
            true
        }
        RuntimeEvent::ChildExit | RuntimeEvent::QuitRequested => {
            app.shell.process_pending();
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
            app.shell.send_input(&bytes);
            dirty
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

    let label_style = Theme::label_focused();
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(" SHELL", label_style))),
        label_rect(shell_area),
    );

    if content.width > 0 && content.height > 0 {
        let term =
            PseudoTerminal::new(app.shell.screen()).cursor(Cursor::default().visibility(true));
        frame.render_widget(term, content);
    }

    let hint = "^Space q: quit";
    let status_line = Line::from(vec![
        Span::styled(" Aetherspace ", Style::default().fg(Theme::FG)),
        Span::styled("│", Style::default().fg(Theme::HAIR)),
        Span::styled(" phase 1 boundary ", Style::default().fg(Theme::DIM)),
        Span::styled("│", Style::default().fg(Theme::HAIR)),
        Span::styled(format!(" {hint}"), Style::default().fg(Theme::DIM)),
    ]);
    frame.render_widget(Paragraph::new(status_line), status);
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
