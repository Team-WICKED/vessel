# Contributing

Vessel is source-available under [PolyForm Strict 1.0.0](LICENSE.md), and
Team-WICKED does not accept code or documentation contributions from outside
Team-WICKED. See [ADR 0014](docs/adr/0014-public-source-available.md) for the
project's decision. The license governs use of the source and software.

## Reports and questions

For non-sensitive bugs, feature requests and usage questions, use the
[issue tracker](https://github.com/Team-WICKED/vessel/issues/new/choose).
Include the version or commit, operating system, steps to reproduce, expected
and actual results, and relevant sanitized output. Never include tokens,
credentials, private paths, customer data or internal addresses in an issue.

Do not post suspected vulnerabilities publicly. See [SECURITY.md](SECURITY.md)
for the current status of private reporting.

## Working on Vessel

The following describes Team-WICKED's development workflow. The workspace
requires Rust 1.93 with `rustfmt` and `clippy` as specified in
[rust-toolchain.toml](rust-toolchain.toml). See [AGENTS.md](AGENTS.md) for the
project's implementation, testing and review rules, including the local gate:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Tests use workspace-built fake harnesses; they do not need real agent CLIs or
external network services. The first fake build can take time. CI runs the workspace checks
on Linux, macOS and Windows for pull requests and pushes to `main` and
`backend/**`; see [the workflow](.github/workflows/ci.yml) for its exact
commands. Follow the branch and milestone process in `AGENTS.md`.

See [architecture](docs/ARCHITECTURE.md) for the crate map,
[protocol](docs/PROTOCOL.md) for the wire contract, and
[roadmap](docs/ROADMAP.md) for project status.
