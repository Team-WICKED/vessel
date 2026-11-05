# Working on Vessel

Read `README.md` and `docs/ARCHITECTURE.md` first.

## Rules

- Rust 2024, toolchain pinned in `rust-toolchain.toml`.
- Before claiming anything builds, run all three:
  `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`. CI runs exactly these on three OSes.
- Lints are strict and deliberate. Do not add `#[allow]`; if a lint truly does
  not apply, use `#[expect(..., reason = "...")]`.
- No `unwrap`/`expect`/`panic`/indexing/raw arithmetic outside tests. Return
  errors; the node must survive misbehaving sessions.
- Logging goes through `tracing`. The CLI writes to a locked stdout handle,
  never `println!`.
- Wire types live only in `vessel-core`. If a client needs it, it goes there.
- Harnesses are wrapped, never re-implemented.

## Layout

- `crates/vessel-core`: types and protocol, no I/O.
- `crates/vessel-node`: daemon library + `vesseld` binary.
- `crates/vessel-cli`: `vessel` binary.
- `docs/`: vision, architecture, roadmap. Update them when behaviour changes.
