//! Presentation layer: the command-center layout (`regions`/`body_rect`), the
//! borderless pane primitives (`content_area`/`draw_label`), and the statusline
//! (`draw_statusline` and its pure fit/truncate helpers).
//!
//! Split out of `main.rs` so the renderer stays under the file-size ceiling. The
//! layout math is pure (a function of a `Rect`); only `draw_statusline` reaches into
//! `App`, for the project name, git snapshot, and the mode-dependent hint. The
//! per-pane shell drawing lives in `workspace.rs`; the `draw` orchestrator that ties
//! these together stays in `main.rs`.

use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect, Spacing};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::status::{GitState, Health, Snapshot, TreeState};
use crate::theme::Theme;
use crate::{App, Mode, Pane};

/// Nav-rail width and the air on each side of a hairline separator.
const NAV_WIDTH: u16 = 24;
const PANE_GAP: u16 = 2;

/// The command-center layout: nav rail | hairline | (viewer / hairline / shell),
/// over a one-row statusline. `vsep`/`hsep` are 1-cell separator tracks. Returned
/// so rendering and PTY-resize share one source of truth.
pub(crate) struct Regions {
    pub(crate) nav: Rect,
    pub(crate) vsep: Rect,
    pub(crate) viewer: Rect,
    pub(crate) hsep: Rect,
    pub(crate) shell: Rect,
    pub(crate) status: Rect,
}

pub(crate) fn regions(area: Rect) -> Regions {
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

/// The body region: everything above the one-row statusline. The focused leaf fills
/// this when tree-zoom is active; otherwise `regions` subdivides it into nav/viewer/
/// shell. Shares the outer split with `regions`, so the statusline row always agrees.
pub(crate) fn body_rect(area: Rect) -> Rect {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area)[0]
}

/// The content sub-area of a borderless pane: the region minus its top label row.
/// Shared by rendering and PTY-resize so the shell is always sized to what's drawn.
pub(crate) fn content_area(area: Rect) -> Rect {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area)[1]
}

/// Render a borderless pane's label on its top row — accent when focused, dim
/// otherwise — and return the content area below it. The label is the only focus
/// cue; there is no box.
pub(crate) fn draw_label(f: &mut Frame, area: Rect, title: &str, focused: bool) -> Rect {
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
pub(crate) fn draw_statusline(f: &mut Frame, area: Rect, app: &App) {
    let s = app.status.snapshot();
    let dim = Style::default().fg(Theme::DIM);
    let sep = || Span::styled(" │ ", Style::default().fg(Theme::HAIR));
    let hint = if app.mode == Mode::Pane {
        "s/v:split  x:close  hjkl:move  <>:size  z:zoom"
    } else if app.workspace.copy_mode() {
        "↑↓/pgup scroll   esc: live"
    } else if app.focus == Pane::Shell {
        "^w: pane   tab: release   ^z: zoom"
    } else {
        "^w: pane   tab: focus   q: quit"
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    // `regions` and `content_area` are pure functions of a `Rect`, so we test the
    // layout math directly rather than through `TestBackend` — same coverage, no
    // live terminal.

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
    fn body_rect_fills_full_width_above_statusline() {
        // Tree-zoom hands the focused leaf this body rect: full width, the whole
        // height above the one-row statusline, anchored at the origin.
        let area = Rect::new(0, 0, 120, 40);
        let body = body_rect(area);
        assert_eq!(body.width, area.width, "body spans full width");
        assert_eq!(
            body.height,
            area.height - 1,
            "body fills above the statusline"
        );
        assert_eq!((body.x, body.y), (0, 0));
        // It agrees with the statusline row regions() reserves.
        assert_eq!(body.height, regions(area).status.y);
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
