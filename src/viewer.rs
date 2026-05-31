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

pub(crate) struct Viewer {
    lines: Vec<String>,
    scroll: usize,
    loaded_bytes: usize,
    truncated: bool,
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

    pub(crate) fn visible_lines(&self, height: u16) -> Vec<Line<'static>> {
        let rows = height as usize;
        if rows == 0 {
            return Vec::new();
        }
        let mut out: Vec<Line<'static>> = self
            .lines
            .iter()
            .skip(self.scroll)
            .take(rows)
            .map(|line| Line::from(Span::styled(line.clone(), style_for_line(line))))
            .collect();
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
}
