# Aetherspace

A personal command-center terminal built in Rust with [Ratatui](https://ratatui.rs).
One surface for your projects: a navigation rail, a markdown and script viewer, a
real embedded shell, and a clean statusline. Designed to be a workstation that gets
out of the way, in a strict hairline aesthetic.

## Aesthetic

Jet black and white, one retro-orange accent reserved for selection and focus, and a
glowy-retro secondary palette for live signals (the Spark health dot reads green).
Hairline borders, monospace cell grid, no pills or powerline chrome. All visual
identity lives in `src/theme.rs`, so re-theming is a one-file edit.

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
| tui-markdown | 0.3 | Markdown rendering with syntect code highlighting |

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
- [ ] Phase 2d: live statusline data (system stats, git state, Spark health probe)
- [ ] Dynamic project discovery, scrollback, mouse, copy/paste
