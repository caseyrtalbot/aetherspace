# Aetherspace

A personal command-center terminal built in Rust with [Ratatui](https://ratatui.rs).
One surface for your projects: a navigation rail, a markdown and script viewer, a
real embedded shell, and a clean statusline. Designed to be a workstation that gets
out of the way, in a borderless, terminal-native aesthetic.

## Aesthetic

Borderless and terminal-native. The app paints only foreground text plus one
retro-orange accent (reserved for selection and focus). It deliberately does not
paint a background, so the embedded shell, which always renders on the terminal's
own background, stays seamless with the rest of the surface. Set your terminal
profile background to `#0a0a0a` for the intended jet black, or run it in a terminal
like Ghostty. Panes are separated by thin hairline rules rather than boxes, and the
focused pane is marked by its label turning accent-orange. A glowy-retro secondary
palette carries live signals (the Spark health dot reads green when the endpoint is
up). All visual identity lives in `src/theme.rs`, so re-theming is a one-file edit.

## Keys

| Key | Action |
|-----|--------|
| `Tab` | Cycle focus: Projects -> Viewer -> Shell (global, works from anywhere) |
| `j` / `k` or arrows | Move selection (Projects) or scroll (Viewer) |
| `PageUp` / `PageDown` | Scroll the viewer faster |
| `q` / `Esc` | Quit (when the shell is not focused) |

When the shell is focused, keystrokes go to the real shell, with one exception:
`Tab` always cycles focus, so you can never get trapped in the shell. The
tradeoff is that the embedded shell does not receive `Tab`, so shell
tab-completion is unavailable. A future capture mode will restore it.

## Stack

| Crate | Version | Role |
|-------|---------|------|
| ratatui | 0.30 | TUI framework and render loop |
| tui-term | 0.3.4 | PseudoTerminal widget (renders the shell) |
| portable-pty | 0.9 | PTY creation and shell spawning |
| vt100 | 0.16 | Terminal state machine (via tui-term's re-export) |
| tui-markdown | 0.3 | Markdown rendering (foreground-only theme; `highlight-code` off, no syntect) |
| sysinfo | 0.39 | CPU and memory stats for the statusline |
| gix | 0.84 | Pure-Rust git: branch name and dirty state |
| ureq | 3.3 | Blocking HTTP for the Spark health probe (no TLS) |

Versions are coupled: `tui-term 0.3.4` pulls `vt100 0.16.2`, which requires
`ratatui 0.30` (0.29 hard-pins an older `unicode-width` and will not resolve). The
resolver is the source of truth here.

## Build and run

```sh
cargo run
```

## Status

- [x] Phase 2a: four-region layout, hairline theme, focus cycling
- [x] Phase 2b: real embedded shell via PTY, live render, key routing
- [x] Phase 2c: viewer renders the selected project's markdown, scrollable
- [x] Phase 2d: live statusline data (CPU, memory, per-project git state, Spark health probe)
- [x] Borderless redesign: terminal-native background, hairline separators, focus-by-label, foreground-only markdown
- [ ] Dynamic project discovery, scrollback, mouse, copy/paste
