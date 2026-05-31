# Aetherspace

Aetherspace is a personal Rust/Ratatui command-center terminal. It combines real
shell panes, tiled and floating layouts, project selection, a lightweight document
viewer, and a quiet status surface inside one recoverable terminal session.

The current build is intentionally terminal-first:

- Ratatui is the renderer.
- `portable-pty -> vt100 -> tui-term` owns embedded shells.
- The runtime uses a single event queue for input, PTY output, resize, and child
  exit notifications.
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
