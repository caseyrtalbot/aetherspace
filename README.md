# Aetherspace

Aetherspace is a greenfield Rust/Ratatui terminal build.

The current source tree is prototype evidence only. Future agents should start
from the greenfield architecture spec:

`docs/working/GREENFIELD-TERMINAL-SPEC.md`

Build gate:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
