//! Aetherspace — a personal Ratatui command-center terminal.
//!
//! The SHELL region hosts a real `$SHELL` running in a PTY, rendered live via
//! tui-term. Focus the shell to type into it; `Tab` is a global key that always
//! cycles focus, so you're never trapped in the shell. The nav rail lists git
//! projects discovered from the config's `projects_root` (or a pinned list), and
//! the viewer shows the selected project's README/CLAUDE doc as cached markdown.

mod clipboard;
mod config;
mod layout;
mod log;
mod shell;
mod status;
mod theme;
mod ui;
mod workspace;
mod xdg;

use std::borrow::Cow;
use std::fs;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use config::Project;
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    crossterm::{
        event::{
            self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
            KeyModifiers,
        },
        execute,
    },
    layout::Direction,
    style::Style,
    text::{Line, Span, Text},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use shell::Shell;
use status::StatusMonitor;
use theme::{MarkdownTheme, Theme};
use ui::{body_rect, draw_label, draw_statusline, regions};
use workspace::Workspace;

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
pub(crate) enum Pane {
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

/// Input mode. `Normal` is the default; `Pane` is a one-shot prefix (entered with
/// ctrl+w) whose next key is a pane command — split, close, focus, resize, zoom —
/// after which it returns to `Normal`. `Resize` is a *sticky* sub-mode entered from
/// Pane (via `r`, or by the first `<`/`>`): its keys resize the focused pane and
/// stay, so a resize is one keystroke each instead of re-pressing the prefix, until
/// `Esc`/`Enter` returns to `Normal`. The tmux-style prefix keeps the multiplexer
/// verbs off the shell's keyspace without a full keymap abstraction.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Normal,
    Pane,
    Resize,
}

/// Load a project's primary doc for the viewer: README.md, else CLAUDE.md,
/// else a friendly placeholder so the pane is never blank.
fn load_doc(path: &Path) -> String {
    for candidate in ["README.md", "readme.md", "CLAUDE.md"] {
        if let Ok(text) = fs::read_to_string(path.join(candidate)) {
            return text;
        }
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    format!(
        "# {name}\n\nNo `README.md` or `CLAUDE.md` found in `{}`.",
        path.display()
    )
}

/// Parse markdown into an owned, render-ready `Text`, plus its line count for
/// scroll clamping. Parsing happens on nav selection, not per frame: the markdown
/// pass was the largest single per-frame allocation in the old 60fps loop. The
/// cached `Text` borrows nothing, so it lives on `App`.
fn render_markdown(raw: &str) -> (Text<'static>, usize) {
    let opts = tui_markdown::Options::new(MarkdownTheme);
    let text = into_static(tui_markdown::from_str_with_options(raw, &opts));
    let lines = text.lines.len();
    (text, lines)
}

/// Load a project's doc and render it. Convenience wrapper over `render_markdown`.
fn render_doc(path: &Path) -> (Text<'static>, usize) {
    render_markdown(&load_doc(path))
}

/// The viewer doc shown when no projects were discovered: tells the user where we
/// looked and how to fix it, instead of a blank pane.
fn empty_state_doc(root: &Path) -> (Text<'static>, usize) {
    render_markdown(&format!(
        "# No projects\n\nNo git repositories were found in `{}`.\n\n\
         Set `projects_root` (or pin a `projects` list) in your config:\n\n\
         `~/.config/aetherspace/config.toml`\n\n\
         See `config.example.toml` for the full format.",
        root.display()
    ))
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

pub(crate) struct App {
    running: bool,
    pub(crate) focus: Pane,
    projects: Vec<Project>,
    selected: usize,
    doc_text: Text<'static>, // parsed once on select(), not per frame
    doc_lines: usize,        // cached line count for scroll clamping
    scroll: u16,
    pub(crate) workspace: Workspace,
    pub(crate) status: StatusMonitor,
    /// Input mode: `Normal`, or the one-shot `Pane` prefix awaiting a pane command.
    pub(crate) mode: Mode,
}

impl App {
    /// Build the app from the discovered (or pinned) `projects`. With an empty
    /// list the viewer shows an empty-state doc naming `root` and the poller is
    /// pointed at no path; nav/select become no-ops (guarded by `selected_project`).
    fn new(
        shell: Shell,
        tx: Sender<Msg>,
        projects: Vec<Project>,
        root: &Path,
        scrollback: usize,
    ) -> Self {
        let (doc_text, doc_lines) = match projects.first() {
            Some(p) => render_doc(&p.path),
            None => empty_state_doc(root),
        };
        let initial = projects.first().map(|p| p.path.clone()).unwrap_or_default();
        let status = StatusMonitor::spawn(initial, tx.clone());
        Self {
            running: true,
            focus: Pane::Nav,
            projects,
            selected: 0,
            doc_text,
            doc_lines,
            scroll: 0,
            workspace: Workspace::new(shell, scrollback, tx),
            status,
            mode: Mode::Normal,
        }
    }

    /// The currently selected project, or `None` when the list is empty.
    pub(crate) fn selected_project(&self) -> Option<&Project> {
        self.projects.get(self.selected)
    }

    /// Move the nav selection, load that project's doc, and repoint the status
    /// poller so git/health reflect the newly selected project.
    fn select(&mut self, index: usize) {
        let Some(project) = self.projects.get(index) else {
            return; // empty list: nothing to select
        };
        self.selected = index;
        let (doc_text, doc_lines) = render_doc(&project.path);
        let path = project.path.clone();
        self.doc_text = doc_text;
        self.doc_lines = doc_lines;
        self.scroll = 0;
        self.status.set_selected(path);
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Resize mode is sticky: each key resizes the focused pane and stays, so a
        // resize is one keystroke per step. Esc/Enter (or ctrl+w) return to Normal.
        if self.mode == Mode::Resize {
            self.resize_command(key);
            return;
        }

        // Pane mode is a one-shot prefix: the next key is a pane command, then we
        // return to Normal whether or not it matched one (tmux-style).
        if self.mode == Mode::Pane {
            self.mode = Mode::Normal;
            self.pane_command(key);
            return;
        }

        // Tab is a global focus key: it always cycles panes, even out of the
        // shell, so you can never get trapped. Cost: the embedded shell never
        // receives Tab, so shell tab-completion is unavailable (see README).
        if key.code == KeyCode::Tab {
            // Leaving the shell drops any copy-mode scrollback back to live.
            self.workspace.exit_copy_mode();
            self.focus = self.focus.next();
            return;
        }

        // ctrl+w enters Pane mode (the multiplexer prefix) and focuses the shell, so
        // the pane command that follows acts on the workspace it controls. Cost
        // mirrors Tab/ctrl+z: the embedded shell never sees ctrl+w (its readline
        // delete-word), documented as a tradeoff in the README.
        if key.code == KeyCode::Char('w') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.mode = Mode::Pane;
            self.focus = Pane::Shell;
            return;
        }

        // ctrl+z is a global zoom toggle (like Tab): the focused leaf fills the
        // body. Global so it works from any pane; the cost mirrors Tab — the
        // embedded shell never receives ctrl+z, so job-control suspend is
        // unavailable inside it (see README).
        if key.code == KeyCode::Char('z') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.toggle_zoom();
            return;
        }

        // When the shell is focused, keys go to the PTY — unless copy-mode is
        // active, where they drive the scrollback view instead.
        if self.focus == Pane::Shell {
            self.workspace.on_key(key);
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

    /// Execute a Pane-mode command (the key after the ctrl+w prefix): split, close,
    /// focus-cycle, resize, or zoom the workspace. An unmapped key (Esc, anything
    /// else) is a no-op that just leaves Pane mode.
    fn pane_command(&mut self, key: KeyEvent) {
        const RESIZE_STEP: i16 = 5;
        match key.code {
            // s / - : split into stacked panes (a horizontal divider between them).
            KeyCode::Char('s') | KeyCode::Char('-') => {
                self.workspace.split_focused(Direction::Vertical)
            }
            // v / | / \ : split into side-by-side panes (a vertical divider).
            KeyCode::Char('v') | KeyCode::Char('|') | KeyCode::Char('\\') => {
                self.workspace.split_focused(Direction::Horizontal)
            }
            // x : close the focused pane; quit when it was the last one.
            KeyCode::Char('x') => {
                if !self.workspace.close_focused() {
                    self.running = false;
                }
            }
            KeyCode::Char('j') | KeyCode::Char('l') | KeyCode::Down | KeyCode::Right => {
                self.workspace.focus_next()
            }
            KeyCode::Char('k') | KeyCode::Char('h') | KeyCode::Up | KeyCode::Left => {
                self.workspace.focus_prev()
            }
            // r enters the sticky Resize sub-mode without moving anything yet.
            KeyCode::Char('r') => self.mode = Mode::Resize,
            // </> resize once, then drop into Resize so further presses repeat
            // without re-pressing the ctrl+w prefix.
            KeyCode::Char('<') | KeyCode::Char(',') => {
                self.workspace.nudge_focused(-RESIZE_STEP);
                self.mode = Mode::Resize;
            }
            KeyCode::Char('>') | KeyCode::Char('.') => {
                self.workspace.nudge_focused(RESIZE_STEP);
                self.mode = Mode::Resize;
            }
            KeyCode::Char('z') => self.toggle_zoom(),
            _ => {}
        }
    }

    /// Execute a key in the sticky Resize sub-mode: nudge the focused pane and stay,
    /// or return to Normal on Esc/Enter (or the ctrl+w prefix). `<`/`,`/`h`/`-` and
    /// Left/Down shrink it; `>`/`.`/`l`/`+`/`=` and Right/Up grow it. RESIZE_STEP
    /// percent per press. Stray keys are ignored (you stay in Resize until you exit).
    fn resize_command(&mut self, key: KeyEvent) {
        const RESIZE_STEP: i16 = 5;
        if key.code == KeyCode::Char('w') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.mode = Mode::Normal;
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Char('<')
            | KeyCode::Char(',')
            | KeyCode::Char('h')
            | KeyCode::Char('-')
            | KeyCode::Left
            | KeyCode::Down
            | KeyCode::Char('j') => self.workspace.nudge_focused(-RESIZE_STEP),
            KeyCode::Char('>')
            | KeyCode::Char('.')
            | KeyCode::Char('l')
            | KeyCode::Char('+')
            | KeyCode::Char('=')
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Char('k') => self.workspace.nudge_focused(RESIZE_STEP),
            _ => {}
        }
    }

    /// Toggle tree-zoom and, on zoom-in, focus the shell so keys reach the now
    /// full-screen leaf instead of the hidden nav/viewer.
    fn toggle_zoom(&mut self) {
        if self.workspace.toggle_zoom() {
            self.focus = Pane::Shell;
        }
    }

    /// Route a mouse event to the workspace: left-drag a divider to resize, or
    /// left-click a pane to focus it. Returns whether the frame should redraw. Other
    /// buttons and motion are ignored (mouse-to-PTY forwarding is a later phase).
    fn on_mouse(&mut self, m: event::MouseEvent) -> bool {
        use event::{MouseButton, MouseEventKind};
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // A divider grab wins; otherwise the click focuses the pane it hit.
                if self.workspace.begin_drag(m.column, m.row)
                    || self.workspace.focus_at(m.column, m.row)
                {
                    self.focus = Pane::Shell;
                    true
                } else {
                    false
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => self.workspace.update_drag(m.column, m.row),
            MouseEventKind::Up(MouseButton::Left) => self.workspace.end_drag(),
            _ => false,
        }
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    // No background fill: the app inherits the terminal's background everywhere.
    // The embedded shell always renders on the terminal bg (tui-term hard-codes
    // Color::Reset), so deferring to it is the only way to stay seamless — set the
    // terminal profile to jet black for the intended look. We paint fg + accents.
    let area = f.area();
    let r = regions(area);

    // Tree-zoom: the focused leaf fills the whole body; the nav/viewer chrome and
    // the inter-pane separators are skipped entirely. The statusline still shows.
    if app.workspace.zoomed() {
        app.workspace
            .render(f, body_rect(area), app.focus == Pane::Shell);
        draw_statusline(f, r.status, app);
        return;
    }

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
    let nav_lines: Vec<Line> = if app.projects.is_empty() {
        vec![Line::from(Span::styled(
            "  (none)",
            Style::default().fg(Theme::DIM),
        ))]
    } else {
        app.projects
            .iter()
            .enumerate()
            .map(|(i, p)| {
                if i == app.selected {
                    Line::from(Span::styled(
                        format!("› {}", p.name),
                        Style::default().fg(Theme::ACCENT),
                    ))
                } else {
                    Line::from(Span::styled(
                        format!("  {}", p.name),
                        Style::default().fg(Theme::DIM),
                    ))
                }
            })
            .collect()
    };
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
    // A thin scrollbar, only when the doc overflows a non-empty viewport. Track on
    // the hairline color, thumb dim — quiet, consistent with the borderless ethos.
    // The height guard also covers zoom, where the viewer collapses to 0 rows.
    if viewer_content.height > 0 && app.doc_lines > viewer_content.height as usize {
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

    // Embedded shell — real PTY rendered live. The workspace owns the resize+draw
    // loop internally (label with copy-mode marker, then the live screen), so this
    // mutable pane borrow never aliases the chrome reads above.
    app.workspace.render(f, r.shell, app.focus == Pane::Shell);

    draw_statusline(f, r.status, app);
}

/// Chain a panic hook *after* `ratatui::init()` has installed its restore hook:
/// log the payload first, then defer to the original (which restores the terminal
/// and prints the panic), so a crash leaves a trace and never wedges the terminal.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error(&format!("panic: {info}"));
        // Turn mouse reporting off before the terminal is restored, or a crash could
        // leave the host terminal emitting mouse escape sequences on every move.
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        original(info);
    }));
}

fn main() -> Result<()> {
    log::init();
    log::info("aetherspace starting");

    // Load config (defaults if absent/invalid) and resolve the project list:
    // pinned entries verbatim, else git discovery under projects_root.
    let config = config::Config::load();
    let projects = config.resolve_projects();
    log::info(&format!("resolved {} project(s)", projects.len()));

    // All sources funnel into one channel; create it before the producers.
    let (tx, rx) = mpsc::channel::<Msg>();

    // Do all fallible setup BEFORE entering the alternate screen. ratatui::init()
    // installs a panic hook that restores the terminal on panic, but an Err from
    // `?` is not a panic, so a failure between init() and restore() would skip
    // cleanup and leave the terminal in raw mode. Spawn with a reasonable default;
    // draw() resizes to the real pane next frame.
    let shell = Shell::spawn(24, 80, config.shell.scrollback, tx.clone())?;
    let mut app = App::new(
        shell,
        tx.clone(),
        projects,
        &config.projects_root,
        config.shell.scrollback,
    );

    // Clear the host terminal's main screen and scrollback before entering the
    // alternate screen. Terminal.app lets you scroll out of the alt screen into
    // the main buffer; without this, scrolling up at runtime reveals the
    // pre-launch `cargo run` output and a screenful of blank rows. CSI 2J clears
    // the screen, CSI 3J erases the scrollback, CSI H homes the cursor.
    {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[2J\x1b[3J\x1b[H");
        let _ = out.flush();
    }

    let mut terminal = ratatui::init();
    install_panic_hook();
    // Enable mouse reporting so dividers can be dragged to resize and a click can
    // focus the pane it lands on. Best-effort: a terminal that rejects it just loses
    // mouse resize. Tradeoff: with capture on, the terminal's own click-to-select is
    // suppressed — hold Option (Terminal.app/iTerm) for native selection. Turned off
    // again before restore (and in the panic hook) so the host terminal is left clean.
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    spawn_input_thread(tx);
    let result = run(&mut terminal, &mut app, rx);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
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
        Msg::Input(Event::Mouse(m)) => app.on_mouse(m),
        Msg::Input(_) => false, // non-press key kinds; paste lands in a later phase
        Msg::Pty => {
            app.workspace.process_pending();
            // A shell that hit EOF leaves a dead leaf; reap collapses it into its
            // sibling. When the last pane exits, the workspace is empty → quit.
            if !app.workspace.reap_dead() {
                app.running = false;
            }
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
        // Drain what's already queued so a burst becomes one redraw. PTY wakeups
        // are edge-triggered (see Shell), so this can't be starved by a flood;
        // bail on quit so q/Esc takes effect immediately.
        while let Ok(msg) = rx.try_recv() {
            dirty |= handle(app, msg);
            if !app.running {
                break;
            }
        }
        if dirty && app.running {
            // Throttle to at most one draw per frame. Bytes that arrive during the
            // sleep are parsed and shown on the next iteration (one frame later).
            let since = last_draw.elapsed();
            if since < FRAME {
                thread::sleep(FRAME - since);
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

    // `clamp_scroll` is a pure function of the scroll offset, line count, and
    // viewport height — the viewer's single clamp, tested directly. The layout and
    // statusline math moved to `ui.rs` with its own tests.

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
}
