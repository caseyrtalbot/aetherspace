//! Aetherspace — a personal Ratatui command-center terminal.
//!
//! Phase 2b: the SHELL region now hosts a real `$SHELL` running in a PTY,
//! rendered live via tui-term. Focus the shell to type into it; `Tab` is a
//! global key that always cycles focus, so you're never trapped in the shell.
//! Nav rail and viewer remain placeholders for Phase 2c.

mod clipboard;
mod config;
mod log;
mod shell;
mod status;
mod theme;

use std::borrow::Cow;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    layout::{Alignment, Constraint, Direction, Layout, Rect, Spacing},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use shell::{Shell, encode_key};
use status::{GitState, Health, Snapshot, StatusMonitor, TreeState};
use theme::{MarkdownTheme, Theme};
use tui_term::widget::PseudoTerminal;

/// The render-rate cap: under heavy PTY output we coalesce bursts and draw at
/// most once per frame (~60fps). At idle the loop blocks on the channel and never
/// wakes, so this is a ceiling, not a clock.
const FRAME: Duration = Duration::from_millis(16);

/// A single multi-source event channel feeds the render loop. A dedicated input
/// thread blocks on `event::read()` and forwards `Input`; the PTY reader thread
/// sends `Pty` when bytes land; the status poller sends `Status` after it
/// publishes. The main loop blocks on the receiver (true sleep at idle), drains
/// queued messages to coalesce a burst, and draws once. No async runtime, in
/// keeping with the std/mpsc stance already in shell.rs and status.rs.
pub enum Msg {
    Input(Event),
    Pty,
    Status,
}

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

/// Load a project's doc and parse it into an owned, render-ready `Text`, plus its
/// line count for scroll clamping. Parsing happens here (on nav selection), not
/// per frame: the markdown pass was the largest single per-frame allocation in
/// the old 60fps loop. The cached `Text` borrows nothing, so it lives on `App`.
fn render_doc(name: &str) -> (Text<'static>, usize) {
    let raw = load_doc(name);
    let opts = tui_markdown::Options::new(MarkdownTheme);
    let text = into_static(tui_markdown::from_str_with_options(&raw, &opts));
    let lines = text.lines.len();
    (text, lines)
}

/// Map a borrowed `Text` (tui-markdown returns one tied to its input `&str`) into
/// a `'static` one by owning every span's content, so it can be cached without a
/// self-referential borrow of the source string.
fn into_static(text: Text<'_>) -> Text<'static> {
    let lines = text
        .lines
        .into_iter()
        .map(|line| Line {
            spans: line
                .spans
                .into_iter()
                .map(|s| Span {
                    style: s.style,
                    content: Cow::Owned(s.content.into_owned()),
                })
                .collect(),
            style: line.style,
            alignment: line.alignment,
        })
        .collect();
    Text {
        lines,
        style: text.style,
        alignment: text.alignment,
    }
}

/// Clamp a viewer scroll offset so it can never run past the document end (which
/// would blank the pane): the maximum offset leaves the last line at the viewport
/// bottom. Pure, so it's unit-tested and used as the single clamp in `draw`.
fn clamp_scroll(scroll: u16, total_lines: usize, viewport_h: u16) -> u16 {
    let total = total_lines.min(u16::MAX as usize) as u16;
    scroll.min(total.saturating_sub(viewport_h))
}

struct App {
    running: bool,
    focus: Pane,
    projects: Vec<&'static str>,
    selected: usize,
    doc_text: Text<'static>, // parsed once on select(), not per frame
    doc_lines: usize,        // cached line count for scroll clamping
    scroll: u16,
    shell: Shell,
    status: StatusMonitor,
}

impl App {
    fn new(shell: Shell, tx: Sender<Msg>) -> Self {
        let projects = vec![
            "thought-engine",
            "antigrav-explore",
            "field-theory-cli",
            "aetherspace",
        ];
        let (doc_text, doc_lines) = render_doc(projects[0]);
        let status = StatusMonitor::spawn(project_path(projects[0]), tx);
        Self {
            running: true,
            focus: Pane::Nav,
            projects,
            selected: 0,
            doc_text,
            doc_lines,
            scroll: 0,
            shell,
            status,
        }
    }

    /// Move the nav selection, load that project's doc, and repoint the status
    /// poller so git/health reflect the newly selected project.
    fn select(&mut self, index: usize) {
        self.selected = index;
        let (doc_text, doc_lines) = render_doc(self.projects[index]);
        self.doc_text = doc_text;
        self.doc_lines = doc_lines;
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

/// Nav-rail width and the air on each side of a hairline separator.
const NAV_WIDTH: u16 = 24;
const PANE_GAP: u16 = 2;

/// The command-center layout: nav rail | hairline | (viewer / hairline / shell),
/// over a one-row statusline. `vsep`/`hsep` are 1-cell separator tracks. Returned
/// so rendering and PTY-resize share one source of truth.
struct Regions {
    nav: Rect,
    vsep: Rect,
    viewer: Rect,
    hsep: Rect,
    shell: Rect,
    status: Rect,
}

fn regions(area: Rect) -> Regions {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    // Nav | 1-col hairline | content, with PANE_GAP cols of air around the line.
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(NAV_WIDTH),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .spacing(Spacing::Space(PANE_GAP))
        .split(outer[0]);
    // Viewer / 1-row hairline / shell, split at the halfway line so the shell
    // fills the bottom half of the terminal.
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(50),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(body[2]);
    Regions {
        nav: body[0],
        vsep: body[1],
        viewer: right[0],
        hsep: right[1],
        shell: right[2],
        status: outer[1],
    }
}

/// The content sub-area of a borderless pane: the region minus its top label row.
/// Shared by rendering and PTY-resize so the shell is always sized to what's drawn.
fn content_area(area: Rect) -> Rect {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area)[1]
}

/// Render a borderless pane's label on its top row — accent when focused, dim
/// otherwise — and return the content area below it. The label is the only focus
/// cue; there is no box.
fn draw_label(f: &mut Frame, area: Rect, title: &str, focused: bool) -> Rect {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    let style = if focused {
        Theme::label_focused()
    } else {
        Theme::label()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {title}"), style))),
        rows[0],
    );
    rows[1]
}

fn draw(f: &mut Frame, app: &mut App) {
    // No background fill: the app inherits the terminal's background everywhere.
    // The embedded shell always renders on the terminal bg (tui-term hard-codes
    // Color::Reset), so deferring to it is the only way to stay seamless — set the
    // terminal profile to jet black for the intended look. We paint fg + accents.
    let r = regions(f.area());

    // Keep the PTY sized to the shell's content area (region minus its label row).
    // Pending PTY bytes are drained in the event loop (on Msg::Pty), not here, so
    // draw() is a pure function of current state.
    let shell_content = content_area(r.shell);
    app.shell.resize(shell_content.height, shell_content.width);

    // Hairline separators between panes — single lines, no boxes. Vertical line
    // runs the full body height between nav and content; horizontal line splits
    // viewer from shell.
    let hair = Style::default().fg(Theme::HAIR);
    let vline: Vec<Line> = (0..r.vsep.height)
        .map(|_| Line::from(Span::styled("│", hair)))
        .collect();
    f.render_widget(Paragraph::new(vline), r.vsep);
    f.render_widget(
        Paragraph::new("─".repeat(r.hsep.width as usize)).style(hair),
        r.hsep,
    );

    // Nav rail — project list, selected row in the one accent color.
    let nav_content = draw_label(f, r.nav, "PROJECTS", app.focus == Pane::Nav);
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
    f.render_widget(Paragraph::new(nav_lines), nav_content);

    // Content viewer — selected project's doc as cached markdown, styled
    // foreground-only (no background patches), scrollable when focused. The parse
    // is cached on App (see render_doc); here we clamp the scroll against the
    // viewport and render. Clamping in draw (writing back to app.scroll) is the
    // single source of truth, so key handlers can't drift the offset past the end.
    let viewer_content = draw_label(f, r.viewer, "VIEWER", app.focus == Pane::Viewer);
    app.scroll = clamp_scroll(app.scroll, app.doc_lines, viewer_content.height);
    f.render_widget(
        Paragraph::new(app.doc_text.clone()).scroll((app.scroll, 0)),
        viewer_content,
    );
    // A thin scrollbar, only when the doc overflows the viewport. Track on the
    // hairline color, thumb dim — quiet, consistent with the borderless ethos.
    if app.doc_lines > viewer_content.height as usize {
        let mut sb = ScrollbarState::new(app.doc_lines).position(app.scroll as usize);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .thumb_style(Style::default().fg(Theme::DIM))
                .track_style(Style::default().fg(Theme::HAIR)),
            viewer_content,
            &mut sb,
        );
    }

    // Embedded shell — real PTY rendered live.
    draw_label(f, r.shell, "SHELL", app.focus == Pane::Shell);
    f.render_widget(PseudoTerminal::new(app.shell.screen()), shell_content);

    draw_statusline(f, r.status, app);
}

/// Whether the statusline should show git for the selected project, and what.
/// Returns `Some((branch, dirty))` only when the snapshot was computed for the
/// path currently selected — guarding against showing the previous project's
/// branch for the frame between a nav move and the poller catching up. Extracted
/// from `draw_statusline` so the staleness rule is unit-testable.
fn should_show_git<'a>(snap: &'a Snapshot, selected: &Path) -> Option<(&'a str, TreeState)> {
    if snap.git_path == selected
        && let GitState::Repo { branch, dirty } = &snap.git
    {
        Some((branch.as_str(), *dirty))
    } else {
        None
    }
}

/// Single quiet row of segments divided by thin vertical rules — not a powerline.
/// Accent is reserved for selection/focus; the live spark dot reads glow-green.
fn draw_statusline(f: &mut Frame, area: Rect, app: &App) {
    let s = app.status.snapshot();
    let dim = Style::default().fg(Theme::DIM);
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
        .constraints([
            Constraint::Min(0),
            Constraint::Length(hint.len() as u16 + 1),
        ])
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
    if let Some((branch, dirty)) = should_show_git(&s, &project_path(app.projects[app.selected])) {
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

    f.render_widget(Paragraph::new(Line::from(spans)), cols[0]);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, dim))).alignment(Alignment::Right),
        cols[1],
    );
}

/// Used/total memory in GiB, compact for the statusline, e.g. "mem 4.2/16G".
fn fmt_mem(used: u64, total: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    format!("mem {:.1}/{:.0}G", used as f64 / GIB, total as f64 / GIB)
}

/// Chain a panic hook *after* `ratatui::init()` has installed its restore hook:
/// log the payload first, then defer to the original (which restores the terminal
/// and prints the panic), so a crash leaves a trace and never wedges the terminal.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error(&format!("panic: {info}"));
        original(info);
    }));
}

fn main() -> Result<()> {
    log::init();
    log::info("aetherspace starting");

    // All sources funnel into one channel; create it before the producers.
    let (tx, rx) = mpsc::channel::<Msg>();

    // Do all fallible setup BEFORE entering the alternate screen. ratatui::init()
    // installs a panic hook that restores the terminal on panic, but an Err from
    // `?` is not a panic, so a failure between init() and restore() would skip
    // cleanup and leave the terminal in raw mode. Spawn with a reasonable default;
    // draw() resizes to the real pane next frame.
    let shell = Shell::spawn(24, 80, tx.clone())?;
    let mut app = App::new(shell, tx.clone());

    let mut terminal = ratatui::init();
    install_panic_hook();
    spawn_input_thread(tx);
    let result = run(&mut terminal, &mut app, rx);
    ratatui::restore();
    if let Err(e) = &result {
        log::error(&format!("exited with error: {e}"));
    }
    result
}

/// A dedicated thread that blocks on `event::read()` and forwards every terminal
/// event into the channel. crossterm's event API is synchronous and cannot wait
/// on our PTY/status channels at once, so reading on its own thread is what lets
/// the main loop sleep on a single receiver instead of polling at 60fps.
fn spawn_input_thread(tx: Sender<Msg>) {
    thread::spawn(move || {
        // Exits when event::read() errors or the main loop drops the receiver.
        while let Ok(ev) = event::read() {
            if tx.send(Msg::Input(ev)).is_err() {
                break; // main loop gone
            }
        }
    });
}

/// Apply one message to the app, returning whether it warrants a redraw.
/// Invariant: a `Msg::Pty` must always drain the PTY and request a redraw, or
/// live shell output would stall (covered by the `cat bigfile` smoke test).
fn handle(app: &mut App, msg: Msg) -> bool {
    match msg {
        Msg::Input(Event::Key(key)) if key.kind == KeyEventKind::Press => {
            app.on_key(key);
            true
        }
        Msg::Input(Event::Resize(_, _)) => true,
        Msg::Input(_) => false, // non-press key kinds; mouse/paste land in Phase 8
        Msg::Pty => {
            app.shell.process_pending();
            true
        }
        Msg::Status => true,
    }
}

/// Generic over the ratatui `Backend`: the renderer seam. `DefaultTerminal`'s
/// backend errors are `io::Error` (satisfying the bound), so `main()` is unchanged;
/// the payoff is the future native frontend and `TestBackend`-driven render tests.
///
/// The loop blocks on `rx.recv()` (a true sleep, ~0% CPU at idle), wakes on any
/// message, coalesces everything already queued into one redraw, and caps the
/// draw rate at `FRAME` under heavy output.
fn run<B: Backend>(terminal: &mut Terminal<B>, app: &mut App, rx: Receiver<Msg>) -> Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    terminal.draw(|f| draw(f, app))?; // initial paint
    let mut last_draw = Instant::now();

    while app.running {
        // Sleep until a message arrives.
        let Ok(msg) = rx.recv() else { break };
        let mut dirty = handle(app, msg);
        // Drain anything else already queued so a burst becomes a single redraw.
        while let Ok(msg) = rx.try_recv() {
            dirty |= handle(app, msg);
        }
        if dirty && app.running {
            // Throttle to at most one draw per frame; coalesce what lands meanwhile.
            let since = last_draw.elapsed();
            if since < FRAME {
                thread::sleep(FRAME - since);
                while let Ok(msg) = rx.try_recv() {
                    handle(app, msg);
                }
            }
            terminal.draw(|f| draw(f, app))?;
            last_draw = Instant::now();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // `regions` and `content_area` are pure functions of a `Rect`, so we test the
    // layout math directly rather than through `TestBackend` — same coverage, no
    // live terminal. The `Backend` seam exists for the native path and for future
    // `draw()` render tests, which would need a `TestBackend` Frame.

    #[test]
    fn regions_layout_120x40() {
        let r = regions(Rect::new(0, 0, 120, 40));
        assert_eq!(r.nav.width, NAV_WIDTH, "nav rail width");
        assert_eq!(r.status.height, 1, "statusline is one row");
        assert_eq!(r.vsep.width, 1, "vertical separator is one cell");
        assert_eq!(r.hsep.height, 1, "horizontal separator is one row");
        // nav rail, then PANE_GAP cols of air, then the 1-col separator.
        assert_eq!(r.nav.x + r.nav.width + PANE_GAP, r.vsep.x);
        // viewer / hsep / shell exactly fill the body (height minus the statusline).
        let body_h = 40 - r.status.height;
        assert_eq!(r.viewer.height + r.hsep.height + r.shell.height, body_h);
        // ~50/50: the viewer takes Percentage(50) of the whole body, the 1-row
        // separator comes out of the remainder, so the shell trails by up to the
        // separator height plus odd-parity rounding (2 rows at this geometry).
        assert!((r.viewer.height as i32 - r.shell.height as i32).abs() <= 2);
    }

    #[test]
    fn clamp_scroll_clamps_past_end() {
        // 100 lines, 40-row viewport → max offset 60.
        assert_eq!(clamp_scroll(500, 100, 40), 60);
        assert_eq!(clamp_scroll(60, 100, 40), 60);
    }

    #[test]
    fn clamp_scroll_leaves_in_range_untouched() {
        assert_eq!(clamp_scroll(10, 100, 40), 10);
    }

    #[test]
    fn clamp_scroll_zero_when_content_fits() {
        // Fewer lines than the viewport → no scrolling.
        assert_eq!(clamp_scroll(5, 30, 40), 0);
    }

    #[test]
    fn content_area_drops_exactly_one_top_row() {
        let area = Rect::new(2, 3, 40, 10);
        let c = content_area(area);
        assert_eq!(c.y, area.y + 1);
        assert_eq!(c.height, area.height - 1);
        assert_eq!(c.x, area.x);
        assert_eq!(c.width, area.width);
    }

    #[test]
    fn fmt_mem_formats_gib() {
        let gib = 1024u64 * 1024 * 1024;
        assert_eq!(fmt_mem(4 * gib + gib / 2, 16 * gib), "mem 4.5/16G");
    }

    #[test]
    fn fmt_mem_zero_total_does_not_panic() {
        let _ = fmt_mem(0, 0); // float division, so no div-by-zero
    }

    #[test]
    fn should_show_git_returns_branch_when_path_matches() {
        let p = PathBuf::from("/x/y");
        let snap = Snapshot {
            git: GitState::Repo {
                branch: "main".into(),
                dirty: TreeState::Clean,
            },
            git_path: p.clone(),
            ..Snapshot::default()
        };
        assert!(matches!(
            should_show_git(&snap, &p),
            Some(("main", TreeState::Clean))
        ));
    }

    #[test]
    fn should_show_git_is_none_when_path_mismatches() {
        let snap = Snapshot {
            git: GitState::Repo {
                branch: "main".into(),
                dirty: TreeState::Dirty,
            },
            git_path: PathBuf::from("/a"),
            ..Snapshot::default()
        };
        assert!(should_show_git(&snap, Path::new("/b")).is_none());
    }
}
