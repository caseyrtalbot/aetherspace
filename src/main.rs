//! Aetherspace — a personal Ratatui command-center terminal.
//!
//! Phase 2b: the SHELL region now hosts a real `$SHELL` running in a PTY,
//! rendered live via tui-term. Focus the shell to type into it; `Tab` is a
//! global key that always cycles focus, so you're never trapped in the shell.
//! Nav rail and viewer remain placeholders for Phase 2c.

mod shell;
mod status;
mod theme;

use std::time::Duration;
use std::{env, fs, path::PathBuf};

use anyhow::Result;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};
use shell::{encode_key, Shell};
use status::{GitState, Health, StatusMonitor, TreeState};
use theme::Theme;
use tui_term::widget::PseudoTerminal;

/// Tick between frames; also the input poll timeout, so shell output renders
/// live even when no key is pressed.
const TICK: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Nav,
    Viewer,
    Shell,
}

impl Pane {
    fn next(self) -> Self {
        match self {
            Pane::Nav => Pane::Viewer,
            Pane::Viewer => Pane::Shell,
            Pane::Shell => Pane::Nav,
        }
    }
}

/// Resolve a project name to its directory under ~/Projects.
fn project_path(name: &str) -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_default())
        .join("Projects")
        .join(name)
}

/// Load a project's primary doc for the viewer: README.md, else CLAUDE.md,
/// else a friendly placeholder so the pane is never blank.
fn load_doc(name: &str) -> String {
    let base = project_path(name);
    for candidate in ["README.md", "readme.md", "CLAUDE.md"] {
        if let Ok(text) = fs::read_to_string(base.join(candidate)) {
            return text;
        }
    }
    format!(
        "# {name}\n\nNo `README.md` or `CLAUDE.md` found in `{}`.",
        base.display()
    )
}

struct App {
    running: bool,
    focus: Pane,
    projects: Vec<&'static str>,
    selected: usize,
    doc: String,
    scroll: u16,
    shell: Shell,
    status: StatusMonitor,
}

impl App {
    fn new(shell: Shell) -> Self {
        let projects = vec![
            "thought-engine",
            "antigrav-explore",
            "field-theory-cli",
            "aetherspace",
        ];
        let doc = load_doc(projects[0]);
        let status = StatusMonitor::spawn(project_path(projects[0]));
        Self {
            running: true,
            focus: Pane::Nav,
            projects,
            selected: 0,
            doc,
            scroll: 0,
            shell,
            status,
        }
    }

    /// Move the nav selection, load that project's doc, and repoint the status
    /// poller so git/health reflect the newly selected project.
    fn select(&mut self, index: usize) {
        self.selected = index;
        self.doc = load_doc(self.projects[index]);
        self.scroll = 0;
        self.status.set_selected(project_path(self.projects[index]));
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Tab is a global focus key: it always cycles panes, even out of the
        // shell, so you can never get trapped. Cost: the embedded shell never
        // receives Tab, so shell tab-completion is unavailable (see README).
        if key.code == KeyCode::Tab {
            self.focus = self.focus.next();
            return;
        }

        // When the shell is focused, every other key goes straight to the PTY.
        if self.focus == Pane::Shell {
            let bytes = encode_key(key);
            if !bytes.is_empty() {
                self.shell.send_input(&bytes);
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.running = false,
            KeyCode::Down | KeyCode::Char('j') if self.focus == Pane::Nav => {
                if self.selected + 1 < self.projects.len() {
                    self.select(self.selected + 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') if self.focus == Pane::Nav => {
                if self.selected > 0 {
                    self.select(self.selected - 1);
                }
            }
            // Viewer scrolling when it holds focus.
            KeyCode::Down | KeyCode::Char('j') if self.focus == Pane::Viewer => {
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyCode::Up | KeyCode::Char('k') if self.focus == Pane::Viewer => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::PageDown if self.focus == Pane::Viewer => {
                self.scroll = self.scroll.saturating_add(10);
            }
            KeyCode::PageUp if self.focus == Pane::Viewer => {
                self.scroll = self.scroll.saturating_sub(10);
            }
            _ => {}
        }
    }
}

/// The four-region command-center layout: nav rail + (viewer over shell) + a
/// one-row statusline. Returned so both rendering and PTY-resize use one source.
struct Regions {
    nav: Rect,
    viewer: Rect,
    shell: Rect,
    status: Rect,
}

fn regions(area: Rect) -> Regions {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(0)])
        .split(outer[0]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(body[1]);
    Regions {
        nav: body[0],
        viewer: right[0],
        shell: right[1],
        status: outer[1],
    }
}

/// A bordered region in the hairline aesthetic: 1px border (accent when focused,
/// hairline when idle) with a tiny uppercase label riding on the top edge.
fn block(title: &str, focused: bool) -> Block<'_> {
    let border = if focused {
        Theme::border_focused()
    } else {
        Theme::border_idle()
    };
    Block::bordered()
        .border_style(border)
        .title(Span::styled(format!(" {title} "), Theme::label()))
        .style(Style::default().bg(Theme::BG).fg(Theme::FG))
}

fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().bg(Theme::BG)),
        area,
    );
    let r = regions(area);

    // Keep the PTY sized to the shell pane's inner area, then absorb new output.
    let inner = block("", false).inner(r.shell);
    app.shell.resize(inner.height, inner.width);
    app.shell.process_pending();

    // Nav rail — project list, selected row in the one accent color.
    let nav_lines: Vec<Line> = app
        .projects
        .iter()
        .enumerate()
        .map(|(i, name)| {
            if i == app.selected {
                Line::from(Span::styled(
                    format!("› {name}"),
                    Style::default().fg(Theme::ACCENT),
                ))
            } else {
                Line::from(Span::styled(
                    format!("  {name}"),
                    Style::default().fg(Theme::DIM),
                ))
            }
        })
        .collect();
    f.render_widget(
        Paragraph::new(nav_lines).block(block("PROJECTS", app.focus == Pane::Nav)),
        r.nav,
    );

    // Content viewer — selected project's doc, rendered as markdown (with
    // syntect code-block highlighting) and scrollable when focused.
    let doc = tui_markdown::from_str(&app.doc);
    f.render_widget(
        Paragraph::new(doc)
            .block(block("VIEWER", app.focus == Pane::Viewer))
            .scroll((app.scroll, 0)),
        r.viewer,
    );

    // Embedded shell — real PTY rendered live.
    let shell_block = block("SHELL", app.focus == Pane::Shell);
    let shell_inner = shell_block.inner(r.shell);
    f.render_widget(shell_block, r.shell);
    f.render_widget(PseudoTerminal::new(app.shell.screen()), shell_inner);

    draw_statusline(f, r.status, app);
}

/// Single quiet row of segments divided by thin vertical rules — not a powerline.
/// Accent is reserved for selection/focus; the live spark dot reads glow-green.
fn draw_statusline(f: &mut Frame, area: Rect, app: &App) {
    let s = app.status.snapshot();
    let dim = Style::default().fg(Theme::DIM);
    let bg = Style::default().bg(Theme::BG);
    let sep = Span::styled(" │ ", Style::default().fg(Theme::HAIR));
    let hint = if app.focus == Pane::Shell {
        "tab: release shell"
    } else {
        "tab: focus   q: quit"
    };

    // Reserve the right edge for the hint so it always survives; the status
    // segments left-pack and clip before they can reach it.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(hint.len() as u16 + 1)])
        .split(area);

    let mut spans = vec![
        Span::styled(
            " Aetherspace ",
            Style::default().fg(Theme::FG).add_modifier(Modifier::BOLD),
        ),
        sep.clone(),
        Span::styled(app.projects[app.selected], Style::default().fg(Theme::FG)),
    ];

    // Git: only when the snapshot describes the *currently selected* project, so
    // switching never shows the previous project's branch. Clean, dirty, and an
    // errored check are three distinct, affirmative marks.
    if s.git_path == project_path(app.projects[app.selected])
        && let GitState::Repo { branch, dirty } = &s.git
    {
        spans.push(Span::styled(format!("  {branch}"), dim));
        spans.push(match dirty {
            TreeState::Dirty => Span::styled(" ●", Style::default().fg(Theme::GLOW_AMBER)),
            TreeState::Clean => Span::styled(" ✓", dim),
            TreeState::Unknown => Span::styled(" ?", Style::default().fg(Theme::GLOW_MAGENTA)),
        });
    }

    spans.push(sep.clone());
    spans.push(Span::styled(format!("cpu {:.0}%", s.cpu), dim));
    spans.push(sep.clone());
    spans.push(Span::styled(fmt_mem(s.mem_used, s.mem_total), dim));
    spans.push(sep);

    // Spark health: green = reachable, magenta = down (clearly bad), dim = not
    // yet probed (genuinely no data, distinct from down).
    let spark = match s.spark {
        Health::Up => Style::default().fg(Theme::GLOW_GREEN),
        Health::Down => Style::default().fg(Theme::GLOW_MAGENTA),
        Health::Unknown => dim,
    };
    spans.push(Span::styled("●", spark));
    spans.push(Span::styled(" spark", dim));

    f.render_widget(Paragraph::new(Line::from(spans)).style(bg), cols[0]);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, dim)))
            .alignment(Alignment::Right)
            .style(bg),
        cols[1],
    );
}

/// Used/total memory in GiB, compact for the statusline, e.g. "mem 4.2/16G".
fn fmt_mem(used: u64, total: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    format!("mem {:.1}/{:.0}G", used as f64 / GIB, total as f64 / GIB)
}

fn main() -> Result<()> {
    // Do all fallible setup BEFORE entering the alternate screen. ratatui::init()
    // installs a panic hook that restores the terminal on panic, but an Err from
    // `?` is not a panic, so a failure between init() and restore() would skip
    // cleanup and leave the terminal in raw mode. Spawn with a reasonable default;
    // draw() resizes to the real pane next frame.
    let shell = Shell::spawn(24, 80)?;
    let mut app = App::new(shell);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    while app.running {
        terminal.draw(|f| draw(f, app))?;
        if event::poll(TICK)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.on_key(key);
        }
    }
    Ok(())
}
