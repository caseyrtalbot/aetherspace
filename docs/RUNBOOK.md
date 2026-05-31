# Aetherspace Runbook

## Gate

Run this before claiming a completed change:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
```

For release packaging:

```sh
cargo build --release
cargo package --list
```

## Manual Smoke

Run `target/debug/aetherspace` in a real terminal or a PTY harness.

Expected path:

1. The first shell pane renders and the statusline says `v0.1 tui`.
2. After the first status poll, the statusline includes compact `sys` and `git`
   status; configured probes appear as `health:ok/total`.
3. `Ctrl+Space c` opens the command palette.
4. `Esc` closes the palette.
5. `Ctrl+Space v` opens a viewer pane for the selected project.
6. `j` scrolls the focused viewer.
7. `Ctrl+Space p` opens the project picker.
8. `Enter` selects a project and opens a project shell.
9. `Ctrl+Space q` exits with status 0 and restores the terminal.

Optional mouse smoke:

1. In a shell pane, run a child app that enables xterm mouse mode.
2. Click inside the shell content area.
3. The child receives coordinates relative to its pane; palette and pane chrome
   clicks remain owned by Aetherspace.

Terminal recovery if a development build dies mid-frame:

```sh
reset
stty sane
tput cnorm
```

## Config

Default path:

```sh
~/.config/aetherspace/config.toml
```

Use `config.example.toml` as the complete reference. Paths are taken literally;
`~` is not expanded inside TOML.

Useful defaults:

- `projects_root`: git project discovery root.
- `[[projects]]`: pinned project list, bypassing discovery.
- `[[probes]]`: health URLs shown as `health:ok/total` in the statusline.
- `[poll]`: system, git, and health polling cadences in seconds.
- `[input].leader`: accepts `ctrl-space`, `ctrl-@`, `ctrl-g`, `alt-g`, and `esc`.
- `[workflow].startup_project`: exact project name to select first.
- `[workflow].default_viewer`: project-relative viewer document.

## Session State

Clean exits save durable pane intent here:

```sh
${XDG_STATE_HOME:-~/.local/state}/aetherspace/session.toml
```

The file stores selected project, pane specs, tiled layout, floating geometry,
focus, and zoom. It does not store PTY handles, process state, shell output, or
viewer scroll position. If the file is missing or fails to parse, startup falls
back to config-driven project selection.

## Boundaries

Current deliberate non-goals:

- No native renderer as the default runtime.
- No broad session persistence or storage layer.
- No file-browser/editor expansion.
- No mouse-driven Aetherspace chrome manipulation yet. Child mouse forwarding is
  limited to focused shell panes after the child enables an xterm mouse mode.
- Status polling is runtime-only; it does not add historical metrics storage or
  persist probe results.

Keep future work inside the runtime boundary: state changes before render, input
routed through `input.rs`, lifecycle through `pane.rs`/`shell.rs`/`pty.rs`, and
rendering kept in `runtime.rs`/`theme.rs`.
