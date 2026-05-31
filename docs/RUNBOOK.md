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
2. `Ctrl+Space c` opens the command palette.
3. `Esc` closes the palette.
4. `Ctrl+Space v` opens a viewer pane for the selected project.
5. `j` scrolls the focused viewer.
6. `Ctrl+Space p` opens the project picker.
7. `Enter` selects a project and opens a project shell.
8. `Ctrl+Space q` exits with status 0 and restores the terminal.

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
- `[input].leader`: accepts `ctrl-space`, `ctrl-@`, `ctrl-g`, `alt-g`, and `esc`.
- `[workflow].startup_project`: exact project name to select first.
- `[workflow].default_viewer`: project-relative viewer document.

## Boundaries

Current deliberate non-goals:

- No native renderer as the default runtime.
- No broad session persistence or storage layer.
- No file-browser/editor expansion.
- No child mouse forwarding yet; mouse activity is surfaced as no-forward policy.

Keep future work inside the runtime boundary: state changes before render, input
routed through `input.rs`, lifecycle through `pane.rs`/`shell.rs`/`pty.rs`, and
rendering kept in `runtime.rs`/`theme.rs`.
