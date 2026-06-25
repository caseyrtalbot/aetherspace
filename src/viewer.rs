//! Lightweight document viewer pane runtime.
//!
//! Viewer panes are durable pane specs plus runtime-only loaded text and scroll
//! position. They deliberately do not own terminal state or participate in PTY
//! lifecycle events.

use std::fs;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::session::ViewerSpec;
use crate::theme::Theme;

const MAX_VIEWER_BYTES: usize = 512 * 1024;

/// One match of the active find query: original-line BYTE offsets so slicing the
/// line for highlight always lands on a char boundary (never lowercase-mapped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Match {
    line: usize,
    start: usize,
    end: usize,
}

/// Active incremental-find state. Held as a single `Option` so "no find" is one
/// `None` and impossible combinations (e.g. a current index with no matches) are
/// unrepresentable.
#[derive(Debug, Clone)]
struct FindState {
    query: String,
    matches: Vec<Match>,
    current: Option<usize>,
    /// Scroll position when find began, restored on cancel.
    origin: usize,
}

pub(crate) struct Viewer {
    lines: Vec<String>,
    scroll: usize,
    loaded_bytes: usize,
    truncated: bool,
    find: Option<FindState>,
}

impl Viewer {
    pub(crate) fn open(spec: &ViewerSpec) -> Self {
        match fs::read(&spec.path) {
            Ok(mut bytes) => {
                let truncated = bytes.len() > MAX_VIEWER_BYTES;
                if truncated {
                    bytes.truncate(MAX_VIEWER_BYTES);
                }
                let loaded_bytes = bytes.len();
                let text = String::from_utf8_lossy(&bytes);
                let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
                if lines.is_empty() {
                    lines.push("(empty file)".to_string());
                }
                if truncated {
                    lines.push(String::new());
                    lines.push(format!("truncated after {MAX_VIEWER_BYTES} bytes"));
                }
                Self {
                    lines,
                    scroll: 0,
                    loaded_bytes,
                    truncated,
                    find: None,
                }
            }
            Err(e) => Self {
                lines: vec![
                    "viewer file unavailable".to_string(),
                    spec.path.display().to_string(),
                    e.to_string(),
                ],
                scroll: 0,
                loaded_bytes: 0,
                truncated: false,
                find: None,
            },
        }
    }

    pub(crate) fn reload(&mut self, spec: &ViewerSpec) {
        *self = Self::open(spec);
    }

    pub(crate) fn scroll_by(&mut self, delta: isize, viewport_rows: u16) {
        let max_scroll = self.max_scroll(viewport_rows);
        let next = if delta.is_negative() {
            self.scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.scroll.saturating_add(delta as usize)
        };
        self.scroll = next.min(max_scroll);
    }

    pub(crate) fn scroll_home(&mut self) {
        self.scroll = 0;
    }

    pub(crate) fn scroll_end(&mut self, viewport_rows: u16) {
        self.scroll = self.max_scroll(viewport_rows);
    }

    /// Begin an incremental find, saving the current scroll so a cancel restores it.
    pub(crate) fn begin_find(&mut self) {
        self.find = Some(FindState {
            query: String::new(),
            matches: Vec::new(),
            current: None,
            origin: self.scroll,
        });
    }

    /// Update the live query, recompute matches (case-insensitive substring), and
    /// scroll the first match at/after the current position into view. Empty query
    /// clears the matches. No-op when not in a find.
    pub(crate) fn set_query(&mut self, query: &str, viewport_rows: u16) {
        if self.find.is_none() {
            return;
        }
        let needle: Vec<char> = query.chars().map(char_lower).collect();
        let mut matches = Vec::new();
        if !needle.is_empty() {
            for (line_idx, line) in self.lines.iter().enumerate() {
                for (start, end) in find_in_line(line, &needle) {
                    matches.push(Match {
                        line: line_idx,
                        start,
                        end,
                    });
                }
            }
        }
        let current = if matches.is_empty() {
            None
        } else {
            Some(
                matches
                    .iter()
                    .position(|m| m.line >= self.scroll)
                    .unwrap_or(0),
            )
        };
        let target_line = current.map(|ci| matches[ci].line);
        if let Some(find) = self.find.as_mut() {
            find.query = query.to_string();
            find.matches = matches;
            find.current = current;
        }
        if let Some(line) = target_line {
            self.scroll_to_line(line, viewport_rows);
        }
    }

    /// Advance to the next match (wrapping); scroll it into view. No-op if no matches.
    pub(crate) fn next_match(&mut self, viewport_rows: u16) {
        self.step_match(1, viewport_rows);
    }

    /// Step to the previous match (wrapping); scroll it into view.
    pub(crate) fn prev_match(&mut self, viewport_rows: u16) {
        self.step_match(-1, viewport_rows);
    }

    fn step_match(&mut self, dir: isize, viewport_rows: u16) {
        let target = {
            let Some(find) = self.find.as_mut() else {
                return;
            };
            let len = find.matches.len();
            if len == 0 {
                return;
            }
            let next = match find.current {
                Some(c) if dir < 0 => (c + len - 1) % len,
                Some(c) => (c + 1) % len,
                None => 0,
            };
            find.current = Some(next);
            find.matches[next].line
        };
        self.scroll_to_line(target, viewport_rows);
    }

    /// Cancel the find and restore the scroll position from when it began.
    pub(crate) fn cancel_find(&mut self) {
        if let Some(find) = self.find.take() {
            self.scroll = find.origin;
        }
    }

    /// Drop the find entirely (normal-mode Esc): removes highlights and match state.
    pub(crate) fn clear_find(&mut self) {
        self.find = None;
    }

    /// `i/n` while matches exist, "no matches" for a non-empty miss, `None` otherwise.
    pub(crate) fn find_status(&self) -> Option<String> {
        let find = self.find.as_ref()?;
        if find.query.is_empty() {
            return None;
        }
        if find.matches.is_empty() {
            return Some("no matches".to_string());
        }
        let i = find.current.map_or(0, |c| c + 1);
        Some(format!("{i}/{}", find.matches.len()))
    }

    /// The live find query, for the runtime's `/query` prompt while typing.
    pub(crate) fn find_query(&self) -> Option<&str> {
        self.find.as_ref().map(|f| f.query.as_str())
    }

    /// Scroll so `line` is on screen: bring it to the top only when it is currently
    /// outside the viewport, otherwise leave the scroll where it is (so incremental
    /// typing does not jitter the view when the match is already visible).
    fn scroll_to_line(&mut self, line: usize, viewport_rows: u16) {
        let rows = viewport_rows.max(1) as usize;
        let max = self.max_scroll(viewport_rows);
        if line < self.scroll || line >= self.scroll + rows {
            self.scroll = line.min(max);
        }
    }

    pub(crate) fn visible_lines(&self, height: u16) -> Vec<Line<'static>> {
        let rows = height as usize;
        if rows == 0 {
            return Vec::new();
        }
        let current = self
            .find
            .as_ref()
            .and_then(|f| f.current.map(|c| f.matches[c]));
        let mut out: Vec<Line<'static>> = Vec::new();
        for (offset, line) in self.lines.iter().skip(self.scroll).take(rows).enumerate() {
            let abs = self.scroll + offset;
            let line_matches: Vec<Match> = self
                .find
                .as_ref()
                .map(|f| {
                    f.matches
                        .iter()
                        .filter(|m| m.line == abs)
                        .copied()
                        .collect()
                })
                .unwrap_or_default();
            if line_matches.is_empty() {
                out.push(Line::from(Span::styled(line.clone(), style_for_line(line))));
            } else {
                out.push(highlight_line(line, &line_matches, current));
            }
        }
        if out.is_empty() {
            out.push(Line::from(Span::styled("(no viewer content)", dim())));
        }
        out
    }

    pub(crate) fn status(&self) -> String {
        let suffix = if self.truncated { " truncated" } else { "" };
        format!(
            "{} lines  {} bytes{}",
            self.lines.len(),
            self.loaded_bytes,
            suffix
        )
    }

    fn max_scroll(&self, viewport_rows: u16) -> usize {
        self.lines
            .len()
            .saturating_sub(viewport_rows.max(1) as usize)
    }
}

/// Lowercase a char to a single representative char for case-insensitive matching.
/// Takes the first char of the Unicode lowercase mapping (covers ASCII and the
/// overwhelming majority of scripts); a one-to-many lowercase (e.g. `İ`) collapses
/// to its first char, which is fine for find.
fn char_lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Find every non-overlapping case-insensitive occurrence of `needle` (already
/// lowercased to chars) in `line`, returning ORIGINAL-line byte ranges. Offsets come
/// from `char_indices`, so every (start, end) is a valid char boundary even for
/// multi-byte lines — slicing the original `line` at them never panics.
fn find_in_line(line: &str, needle: &[char]) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let idx: Vec<(usize, char)> = line.char_indices().collect();
    let n = needle.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i + n <= idx.len() {
        if (0..n).all(|k| char_lower(idx[i + k].1) == needle[k]) {
            let start = idx[i].0;
            let last = idx[i + n - 1];
            out.push((start, last.0 + last.1.len_utf8()));
            i += n;
        } else {
            i += 1;
        }
    }
    out
}

/// Build a styled line that highlights its matches: the current match in ACCENT+BOLD,
/// other matches on a SELECT_BG fill, and the non-match runs in the line's base style.
/// `matches` are byte ranges into `line` in ascending, non-overlapping order.
fn highlight_line(line: &str, matches: &[Match], current: Option<Match>) -> Line<'static> {
    let base = style_for_line(line);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize;
    for m in matches {
        if m.start > cursor {
            spans.push(Span::styled(line[cursor..m.start].to_string(), base));
        }
        let is_current = current.is_some_and(|c| c.line == m.line && c.start == m.start);
        let style = if is_current {
            Style::default()
                .fg(Theme::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(Theme::SELECT_BG)
        };
        spans.push(Span::styled(line[m.start..m.end].to_string(), style));
        cursor = m.end;
    }
    if cursor < line.len() {
        spans.push(Span::styled(line[cursor..].to_string(), base));
    }
    Line::from(spans)
}

fn style_for_line(line: &str) -> Style {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        Style::default()
            .fg(Theme::GLOW_CYAN)
            .add_modifier(Modifier::BOLD)
    } else if trimmed.starts_with("```") || trimmed.starts_with("    ") {
        Style::default().fg(Theme::GLOW_AMBER)
    } else if trimmed.starts_with('-') || trimmed.starts_with('*') {
        Style::default().fg(Theme::FG)
    } else if trimmed.is_empty() {
        dim()
    } else {
        Style::default().fg(Theme::DIM)
    }
}

fn dim() -> Style {
    Style::default().fg(Theme::DIM)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn missing_file_renders_error_lines() {
        let viewer = Viewer::open(&ViewerSpec {
            path: PathBuf::from("/no/such/aetherspace-viewer-file.md"),
        });
        assert!(
            viewer.visible_lines(4)[0].spans[0]
                .content
                .contains("viewer file unavailable")
        );
    }

    #[test]
    fn scroll_clamps_to_content() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        fs::write(tmp.path(), "one\ntwo\nthree\nfour\n").unwrap();
        let mut viewer = Viewer::open(&ViewerSpec {
            path: tmp.path().to_path_buf(),
        });
        viewer.scroll_by(10, 2);
        let lines = viewer.visible_lines(2);
        assert_eq!(lines[0].spans[0].content.as_ref(), "three");
        viewer.scroll_by(-10, 2);
        let lines = viewer.visible_lines(2);
        assert_eq!(lines[0].spans[0].content.as_ref(), "one");
    }

    fn viewer_with(text: &str) -> Viewer {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        fs::write(tmp.path(), text).unwrap();
        Viewer::open(&ViewerSpec {
            path: tmp.path().to_path_buf(),
        })
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn set_query_finds_and_scrolls_to_first_match() {
        let mut v = viewer_with("a\nb\nc\nd\ne\nf\nNEEDLE here\n");
        v.begin_find();
        v.set_query("needle", 3); // case-insensitive; match is on line 6, below the viewport
        assert_eq!(v.find_status().as_deref(), Some("1/1"));
        // The match was off-screen, so the view scrolled to bring it in.
        let visible: Vec<String> = v.visible_lines(3).iter().map(line_text).collect();
        assert!(
            visible.iter().any(|t| t.contains("NEEDLE here")),
            "match scrolled into view: {visible:?}"
        );
    }

    #[test]
    fn find_is_case_insensitive() {
        let mut v = viewer_with("Hello WORLD\n");
        v.begin_find();
        v.set_query("world", 4);
        assert_eq!(v.find_status().as_deref(), Some("1/1"));
    }

    #[test]
    fn next_and_prev_match_wrap() {
        let mut v = viewer_with("beta one\nbeta two\nbeta three\n");
        v.begin_find();
        v.set_query("beta", 4);
        assert_eq!(v.find_status().as_deref(), Some("1/3"));
        v.next_match(4);
        assert_eq!(v.find_status().as_deref(), Some("2/3"));
        v.next_match(4);
        assert_eq!(v.find_status().as_deref(), Some("3/3"));
        v.next_match(4); // wrap forward
        assert_eq!(v.find_status().as_deref(), Some("1/3"));
        v.prev_match(4); // wrap backward
        assert_eq!(v.find_status().as_deref(), Some("3/3"));
    }

    #[test]
    fn empty_query_clears_matches_and_no_match_reports() {
        let mut v = viewer_with("alpha\nbeta\n");
        v.begin_find();
        v.set_query("beta", 4);
        assert_eq!(v.find_status().as_deref(), Some("1/1"));
        v.set_query("", 4); // empty query: no status, no matches
        assert_eq!(v.find_status(), None);
        v.set_query("zzz", 4); // present-but-missing
        assert_eq!(v.find_status().as_deref(), Some("no matches"));
    }

    #[test]
    fn cancel_restores_origin_scroll() {
        let mut v = viewer_with("a\nb\nc\nd\ne\nf\ng\nMATCH\n");
        v.scroll_by(1, 3); // move off the top before find
        let before = v.scroll;
        v.begin_find();
        v.set_query("match", 3); // scrolls away to the match
        assert_ne!(v.scroll, before, "find moved the view");
        v.cancel_find();
        assert_eq!(v.scroll, before, "cancel restored the original scroll");
        assert_eq!(v.find_status(), None);
    }

    #[test]
    fn clear_find_drops_matches() {
        let mut v = viewer_with("beta\n");
        v.begin_find();
        v.set_query("beta", 4);
        assert!(v.find_status().is_some());
        v.clear_find();
        assert_eq!(v.find_status(), None);
        // After clear, a no-match line renders as a single span (no stale highlight).
        assert_eq!(v.visible_lines(1)[0].spans.len(), 1);
    }

    #[test]
    fn find_matches_record_original_byte_offsets_for_non_ascii() {
        // A multi-byte char before the match means lowercase-byte-mapping would
        // desync; char-index offsets keep the highlight slice on a char boundary.
        let mut v = viewer_with("café beta done\n");
        v.begin_find();
        v.set_query("beta", 8); // must not panic slicing the original line
        let line = &v.visible_lines(1)[0];
        // Spans: "café " (base), "beta" (highlight), " done" (base).
        let highlighted: Vec<&str> = line
            .spans
            .iter()
            .filter(|s| s.style.fg == Some(Theme::ACCENT) || s.style.bg == Some(Theme::SELECT_BG))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(highlighted, vec!["beta"]);
    }

    #[test]
    fn visible_lines_highlight_current_match_distinctly() {
        let mut v = viewer_with("beta and beta\n");
        v.begin_find();
        v.set_query("beta", 4); // two matches on one visible line; current = the first
        let line = &v.visible_lines(1)[0];
        let current: Vec<&str> = line
            .spans
            .iter()
            .filter(|s| s.style.fg == Some(Theme::ACCENT))
            .map(|s| s.content.as_ref())
            .collect();
        let other: Vec<&str> = line
            .spans
            .iter()
            .filter(|s| s.style.bg == Some(Theme::SELECT_BG) && s.style.fg != Some(Theme::ACCENT))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(current, vec!["beta"], "current match is ACCENT");
        assert_eq!(other, vec!["beta"], "other match is SELECT_BG, not ACCENT");
    }
}
