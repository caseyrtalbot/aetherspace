# Aetherspace

Aetherspace is a personal Rust/Ratatui command-center terminal. It combines real
shell panes, tiled and floating layouts, project selection, a lightweight document
viewer, and a quiet status surface inside one recoverable terminal session.

The current build is intentionally terminal-first:

- Ratatui is the renderer.
- `portable-pty -> vt100 -> tui-term` owns embedded shells.
- The runtime uses a single event queue for input, PTY output, resize, and child
  exit notifications, and status snapshots.
- Session intent is separate from live process handles.
- Native rendering, broad persistence, and editor/file-browser scope are deferred.

## Install

```sh
cargo install --path .
aetherspace
```

For development:

```sh
cargo run
```

If this Mac's noninteractive shell cannot find `cargo`, use the stable toolchain
directly:

```sh
/usr/bin/env PATH=/Users/caseytalbot/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/bin:/bin:/usr/sbin:/sbin /Users/caseytalbot/.rustup/toolchains/stable-aarch64-apple-darwin/bin/cargo run
```

## Configure

```sh
mkdir -p ~/.config/aetherspace
cp config.example.toml ~/.config/aetherspace/config.toml
```

All config fields are optional. With no config file, Aetherspace discovers git
projects under `~/Projects`, selects the current project when possible, and opens
`README.md` in viewer panes.

The statusline polls system CPU/memory, selected-project git branch/tracked dirty
state, and any configured `[[probes]]` health URLs on the `[poll]` cadences.
Probe failures are reported in the health count; the command palette can show
the current probe names and states. Status samples stay runtime-only and do not
block startup.

Session layout is saved on clean exit to
`$XDG_STATE_HOME/aetherspace/session.toml`, falling back to
`~/.local/state/aetherspace/session.toml`.

## Controls

Default leader: `Ctrl+Space`.

| Key | Action |
| --- | --- |
| `leader c` or `leader :` | Command palette |
| `leader p` | Project picker |
| `leader v` | Open viewer for selected project |
| `leader s` | Open shell for selected project |
| `leader \|` / `leader -` | Split right / split down |
| `leader Tab` or arrows | Focus next / previous |
| `leader >` / `leader <` | Resize focused split |
| `leader f` | Float or dock focused pane |
| `leader z` | Zoom focused tiled pane |
| `leader r` | Restart shell or reload viewer |
| `leader x` | Close focused pane |
| `leader q` | Quit and restore terminal |

Viewer panes accept `j/k`, `PageUp/PageDown`, `Home`, and `End` while focused.
Shell panes stay in shell-capture mode, so readline keys and bracketed paste go
to the child process unless the leader is pressed.

The command palette includes status details for the current system, selected git
repo, and configured health probes.

Mouse events are forwarded to the focused shell only after the child app enables
xterm mouse mode. Aetherspace still owns pane chrome and palette mouse events.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
```

Release/package checks:

```sh
cargo build --release
cargo package --list
```

Operational details and smoke-test steps are in [docs/RUNBOOK.md](docs/RUNBOOK.md).
