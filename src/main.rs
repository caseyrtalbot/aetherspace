//! Aetherspace — a personal Ratatui command-center terminal.
//!
//! The SHELL region hosts a real `$SHELL` running in a PTY, rendered live via
//! tui-term. Focus the shell to type into it; `Tab` is a global key that always
//! cycles focus, so you're never trapped in the shell. The nav rail lists git
//! projects discovered from the config's `projects_root` (or a pinned list), and
//! the viewer shows the selected project's README/CLAUDE doc as cached markdown.

mod clipboard;
mod config;
mod log;
mod shell;
mod status;
mod theme;
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
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Alignment, Constraint, Direction, Layout, Rect, Spacing},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use shell::{Shell, encode_key};
use status::{GitState, Health, Snapshot, StatusMonitor, TreeState};
use theme::{MarkdownTheme, Theme};
use tui_term::widget::PseudoTerminal;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

struct App {
    running: bool,
    focus: Pane,
    projects: Vec<Project>,
    selected: usize,
    doc_text: Text<'static>, // parsed once on select(), not per frame
    doc_lines: usize,        // cached line count for scroll clamping
    scroll: u16,
    shell: Shell,
    status: StatusMonitor,
    /// Tmux-style zoom: when true the shell fills the body and the nav/viewer
    /// collapse. A throwaway lever (HANDOFF direction A) that Phase 3's tree-zoom
    /// subsumes and deletes.
    zoomed: bool,
    /// Copy-mode (scrollback view): true while the shell is scrolled back from the
    /// live bottom. Scroll-only for now; text selection/yank lands with Phase 6.
    copy_mode: bool,
    /// The shell pane's content height from the last frame, used as the copy-mode
    /// page size. Cached in `draw` because `on_key` has no `Frame`.
    shell_rows: u16,
}

impl App {
    /// Build the app from the discovered (or pinned) `projects`. With an empty
    /// list the viewer shows an empty-state doc naming `root` and the poller is
    /// pointed at no path; nav/select become no-ops (guarded by `selected_project`).
    fn new(shell: Shell, tx: Sender<Msg>, projects: Vec<Project>, root: &Path) -> Self {
        let (doc_text, doc_lines) = match projects.first() {
            Some(p) => render_doc(&p.path),
            None => empty_state_doc(root),
        };
        let initial = projects.first().map(|p| p.path.clone()).unwrap_or_default();
        let status = StatusMonitor::spawn(initial, tx);
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
            zoomed: false,
            copy_mode: false,
            shell_rows: 24,
        }
    }

    /// The currently selected project, or `None` when the list is empty.
    fn selected_project(&self) -> Option<&Project> {
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
        // Tab is a global focus key: it always cycles panes, even out of the
        // shell, so you can never get trapped. Cost: the embedded shell never
        // receives Tab, so shell tab-completion is unavailable (see README).
        if key.code == KeyCode::Tab {
            // Leaving the shell drops any copy-mode scrollback back to live.
            if self.copy_mode {
                self.shell.scroll_to_bottom();
                self.copy_mode = false;
            }
            self.focus = self.focus.next();
            return;
        }

        // ctrl+z is a global zoom toggle (like Tab): the shell body fills the
        // frame. Global so it works from any pane; the cost mirrors Tab — the
        // embedded shell never receives ctrl+z, so job-control suspend is
        // unavailable inside it (see README).
        if key.code == KeyCode::Char('z') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.zoomed = !self.zoomed;
            // The zoomed body is shell-only, so focus the shell — otherwise keys
            // would route to the now-hidden nav/viewer and the full-screen shell
            // would silently ignore typing.
            if self.zoomed {
                self.focus = Pane::Shell;
            }
            return;
        }

        // When the shell is focused, keys go to the PTY — unless copy-mode is
        // active, where they drive the scrollback view instead.
        if self.focus == Pane::Shell {
            self.shell_key(key);
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

    /// Key handling while the shell is focused. Normally every key passes through
    /// to the PTY. PageUp opens copy-mode (the scrollback view) when there is
    /// history to show; in copy-mode the arrows and Page keys move the view, Esc
    /// snaps back to live, and any other key snaps to live AND passes through, so
    /// you just start typing to resume.
    fn shell_key(&mut self, key: KeyEvent) {
        let page = self.shell_rows.max(1) as i64;
        if self.copy_mode {
            match key.code {
                KeyCode::PageUp => self.shell.scroll_by(page),
                KeyCode::Up => self.shell.scroll_by(1),
                KeyCode::PageDown => {
                    self.shell.scroll_by(-page);
                    self.copy_mode = self.shell.scrollback_offset() > 0;
                }
                KeyCode::Down => {
                    self.shell.scroll_by(-1);
                    self.copy_mode = self.shell.scrollback_offset() > 0;
                }
                KeyCode::Esc => {
                    self.shell.scroll_to_bottom();
                    self.copy_mode = false;
                }
                _ => {
                    self.shell.scroll_to_bottom();
                    self.copy_mode = false;
                    self.pty_send(key);
                }
            }
            return;
        }
        // Not in copy-mode: PageUp tries to open the scrollback view. If the offset
        // actually moves, there is history (primary buffer) and we enter copy-mode.
        // If it stays at 0 we are on the alternate screen — vt100 pins scrollback
        // there — so the key belongs to the app (vim/less paging) and falls through.
        if key.code == KeyCode::PageUp {
            self.shell.scroll_by(page);
            if self.shell.scrollback_offset() > 0 {
                self.copy_mode = true;
                return;
            }
        }
        self.pty_send(key);
    }

    /// Encode a key and write it to the PTY (no-op for keys that don't map).
    fn pty_send(&mut self, key: KeyEvent) {
        let bytes = encode_key(key);
        if !bytes.is_empty() {
            self.shell.send_input(&bytes);
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

fn regions(area: Rect, zoomed: bool) -> Regions {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    // Zoom: the shell fills the whole body; nav, viewer, and the separators
    // collapse to empty rects (which render nothing). A throwaway lever that
    // Phase 3's tree-zoom subsumes.
    if zoomed {
        let empty = Rect::new(outer[0].x, outer[0].y, 0, 0);
        return Regions {
            nav: empty,
            vsep: empty,
            viewer: empty,
            hsep: empty,
            shell: outer[0],
            status: outer[1],
        };
    }
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
    let r = regions(f.area(), app.zoomed);

    // Keep the PTY sized to the shell's content area (region minus its label row),
    // and cache that height as the copy-mode page size (on_key has no Frame).
    // Pending PTY bytes are drained in the event loop (on Msg::Pty), not here.
    let shell_content = content_area(r.shell);
    app.shell.resize(shell_content.height, shell_content.width);
    app.shell_rows = shell_content.height;

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

    // Embedded shell — real PTY rendered live. In copy-mode the label carries the
    // scrollback position marker (rows above the live bottom).
    let shell_title = if app.copy_mode {
        format!("SHELL  ↑{}", app.shell.scrollback_offset())
    } else {
        "SHELL".to_string()
    };
    draw_label(f, r.shell, &shell_title, app.focus == Pane::Shell);
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

/// Drop priority for a statusline segment. Higher survives a narrow row longer.
/// Title and project are non-negotiable; git is informative; the host gauges are
/// the first to go when columns run short.
const PRIO_TITLE: u8 = 3;
const PRIO_PROJECT: u8 = 3;
const PRIO_GIT: u8 = 2;
const PRIO_GAUGE: u8 = 1;

/// Cap a branch name at this many display columns before it enters the budget, so
/// one long branch can't shove the host gauges off on its own.
const BRANCH_MAX_W: u16 = 24;

/// Which left-to-right segments fit `budget` columns, given each segment's
/// `(priority, display_width)`. Drops lowest priority first; among equal priority
/// drops the rightmost (highest index) first, so the row collapses toward the
/// high-priority left edge. Pure, so the drop policy is unit-tested without a
/// `Frame`. Returns a keep-mask aligned with the input.
fn fit_segments(segs: &[(u8, u16)], budget: u16) -> Vec<bool> {
    let mut keep = vec![true; segs.len()];
    let mut total: u32 = segs.iter().map(|&(_, w)| w as u32).sum();
    let budget = budget as u32;
    if total <= budget {
        return keep;
    }
    // Drop order: priority ascending (lowest first), then index descending
    // (rightmost first) to break ties.
    let mut order: Vec<usize> = (0..segs.len()).collect();
    order.sort_by(|&a, &b| segs[a].0.cmp(&segs[b].0).then(b.cmp(&a)));
    for i in order {
        if total <= budget {
            break;
        }
        keep[i] = false;
        total -= segs[i].1 as u32;
    }
    keep
}

/// Truncate `s` to at most `max` display columns, appending `…` when cut. Uses
/// Unicode display width (not byte length) so multi-byte branch names budget
/// correctly. `max == 0` yields an empty string.
fn truncate_width(s: &str, max: u16) -> String {
    let max = max as usize;
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    // Reserve one column for the ellipsis (none available → empty).
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    if max > 0 {
        out.push('…');
    }
    out
}

/// Total display width of a segment's spans, in columns.
fn segment_width(spans: &[Span]) -> u16 {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()) as u16)
        .sum()
}

/// Single quiet row of segments divided by thin vertical rules — not a powerline.
/// Accent is reserved for selection/focus; the live spark dot reads glow-green.
///
/// Segments are assembled with explicit drop priorities and fit to the column
/// budget left of the hint: when the row is too narrow the host gauges drop
/// first, then git, while the title and project always survive. Each segment
/// carries its own leading separator, so a dropped segment takes its rule with
/// it and the row stays clean. This replaces the renderer's silent `Min(0)`
/// truncation, which let a wide branch collide with the gauges.
fn draw_statusline(f: &mut Frame, area: Rect, app: &App) {
    let s = app.status.snapshot();
    let dim = Style::default().fg(Theme::DIM);
    let sep = || Span::styled(" │ ", Style::default().fg(Theme::HAIR));
    let hint = if app.copy_mode {
        "↑↓/pgup scroll   esc: live"
    } else if app.focus == Pane::Shell {
        "tab: release   ^z: zoom"
    } else {
        "tab: focus   q: quit"
    };

    // Reserve the right edge for the hint (by display width) so it always
    // survives; the prioritized segments fit into the remaining columns.
    let hint_w = UnicodeWidthStr::width(hint) as u16;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(hint_w + 1)])
        .split(area);

    let selected = app.selected_project();
    let project_name = selected.map(|p| p.name.as_str()).unwrap_or("(no project)");

    // (priority, spans) in left-to-right order. Each gauge and the project carry
    // a leading rule; git attaches to the project with two spaces (no rule).
    let mut segs: Vec<(u8, Vec<Span>)> = vec![
        (
            PRIO_TITLE,
            vec![Span::styled(
                " Aetherspace ",
                Style::default().fg(Theme::FG).add_modifier(Modifier::BOLD),
            )],
        ),
        (
            PRIO_PROJECT,
            vec![
                sep(),
                Span::styled(project_name, Style::default().fg(Theme::FG)),
            ],
        ),
    ];

    // Git: only when the snapshot describes the *currently selected* project, so
    // switching never shows the previous project's branch. The branch is bounded
    // to a max display width so one long name can't crowd out the gauges. Clean,
    // dirty, and an errored check are three distinct, affirmative marks.
    if let Some((branch, dirty)) = selected.and_then(|p| should_show_git(&s, &p.path)) {
        let branch = truncate_width(branch, BRANCH_MAX_W);
        let mark = match dirty {
            TreeState::Dirty => Span::styled(" ●", Style::default().fg(Theme::GLOW_AMBER)),
            TreeState::Clean => Span::styled(" ✓", dim),
            TreeState::Unknown => Span::styled(" ?", Style::default().fg(Theme::GLOW_MAGENTA)),
        };
        segs.push((
            PRIO_GIT,
            vec![Span::styled(format!("  {branch}"), dim), mark],
        ));
    }

    segs.push((
        PRIO_GAUGE,
        vec![sep(), Span::styled(format!("cpu {:.0}%", s.cpu), dim)],
    ));
    segs.push((
        PRIO_GAUGE,
        vec![sep(), Span::styled(fmt_mem(s.mem_used, s.mem_total), dim)],
    ));

    // Spark health: green = reachable, magenta = down (clearly bad), dim = not
    // yet probed (genuinely no data, distinct from down).
    let spark = match s.spark {
        Health::Up => Style::default().fg(Theme::GLOW_GREEN),
        Health::Down => Style::default().fg(Theme::GLOW_MAGENTA),
        Health::Unknown => dim,
    };
    segs.push((
        PRIO_GAUGE,
        vec![sep(), Span::styled("●", spark), Span::styled(" spark", dim)],
    ));

    let widths: Vec<(u8, u16)> = segs.iter().map(|(p, sp)| (*p, segment_width(sp))).collect();
    let keep = fit_segments(&widths, cols[0].width);
    let spans: Vec<Span> = segs
        .into_iter()
        .zip(keep)
        .filter_map(|((_, sp), keep)| keep.then_some(sp))
        .flatten()
        .collect();

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
    let mut app = App::new(shell, tx.clone(), projects, &config.projects_root);

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
    use std::path::PathBuf;

    use super::*;

    // `regions` and `content_area` are pure functions of a `Rect`, so we test the
    // layout math directly rather than through `TestBackend` — same coverage, no
    // live terminal. The `Backend` seam exists for the native path and for future
    // `draw()` render tests, which would need a `TestBackend` Frame.

    #[test]
    fn regions_layout_120x40() {
        let r = regions(Rect::new(0, 0, 120, 40), false);
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
    fn regions_zoomed_shell_fills_body() {
        let area = Rect::new(0, 0, 120, 40);
        let r = regions(area, true);
        // Shell takes the full width and the whole body above the statusline.
        assert_eq!(r.shell.width, area.width, "zoomed shell spans full width");
        assert_eq!(r.shell.height, area.height - 1, "zoomed shell fills body");
        assert_eq!(r.status.height, 1, "statusline survives zoom");
        // Nav, viewer, and the separators collapse so they render nothing.
        assert_eq!(r.nav.width, 0);
        assert_eq!(r.viewer.height, 0);
        assert_eq!(r.vsep.width, 0);
        assert_eq!(r.hsep.height, 0);
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
    fn fit_segments_keeps_all_when_budget_ample() {
        // title=11, project=10, gauge=8 → 29 cols, budget 80.
        let segs = [(PRIO_TITLE, 11), (PRIO_PROJECT, 10), (PRIO_GAUGE, 8)];
        assert_eq!(fit_segments(&segs, 80), vec![true, true, true]);
    }

    #[test]
    fn fit_segments_drops_low_priority_first() {
        // Total 29, budget 22 → must shed 7+. The gauge (prio 1) drops; the
        // high-priority title/project survive.
        let segs = [(PRIO_TITLE, 11), (PRIO_PROJECT, 10), (PRIO_GAUGE, 8)];
        assert_eq!(fit_segments(&segs, 22), vec![true, true, false]);
    }

    #[test]
    fn fit_segments_drops_rightmost_among_equal_priority() {
        // Three equal-priority gauges (width 8 each = 24); budget 17 forces one
        // drop. The rightmost goes first, collapsing toward the left.
        let segs = [(PRIO_GAUGE, 8), (PRIO_GAUGE, 8), (PRIO_GAUGE, 8)];
        assert_eq!(fit_segments(&segs, 17), vec![true, true, false]);
    }

    #[test]
    fn fit_segments_high_priority_survives_narrow_budget() {
        // Budget fits only the two high-priority segments; git (mid) and both
        // gauges (low) drop, low-first.
        let segs = [
            (PRIO_TITLE, 11),
            (PRIO_PROJECT, 10),
            (PRIO_GIT, 9),
            (PRIO_GAUGE, 8),
            (PRIO_GAUGE, 8),
        ];
        assert_eq!(
            fit_segments(&segs, 21),
            vec![true, true, false, false, false]
        );
    }

    #[test]
    fn truncate_width_leaves_short_unchanged() {
        assert_eq!(truncate_width("main", 24), "main");
        assert_eq!(truncate_width("main", 4), "main"); // exact fit, no ellipsis
    }

    #[test]
    fn truncate_width_truncates_wide_with_ellipsis() {
        // 12 ASCII cols capped at 8 → 7 chars + '…' = 8 display cols.
        let out = truncate_width("feature/very-long-branch", 8);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 8);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_width_respects_display_width_of_multibyte() {
        // Each CJK char is 2 display cols. Cap 5 → 2 chars (4 cols) + '…' = 5.
        let out = truncate_width("日本語テスト", 5);
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 5);
        assert_eq!(out, "日本…");
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
