# Contributing

Thanks for your interest! This is a small, single-maintainer project, but
contributions are welcome.

## Getting started

```bash
git clone https://github.com/korniychuk/treadmill-bluetooth-macos.git
cd treadmill-bluetooth-macos
cargo build
cargo test
```

You need **macOS** and **Rust 1.95+** (edition 2024). Most work does not require
a physical treadmill — the parser, presence/activity engine, stats and segment
logic are unit-tested and run without hardware. Hardware-specific changes are
called out in the relevant `docs/tasks/*` entry.

## Before you open a PR

CI (`macos-latest`) must pass. Run the same checks locally:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo build
cargo test
```

## Conventions

- **Docs-first.** Non-trivial work starts as a Markdown note under
  [`docs/tasks/`](./docs/tasks) (`NNN-name.md`); architecture decisions go to
  [`docs/adr/`](./docs/adr). Update the docs a change touches.
- **Code comments in English only.**
- **Log anomalies, not the happy path** — caught errors, fallbacks, empty/failed
  external responses, unexpected-but-handled states. Keep success logging to
  coarse lifecycle milestones.
- **Small, single-purpose files;** keep protocol parsing separate from transport.
- Commit messages follow the existing `type(scope): summary` style
  (e.g. `feat(017): hot-reload goals config`).

## Integration boundary (please read)

This repository is intentionally **self-contained and free of outward
integrations** with any personal or external system. If you want to plug the
treadmill data into a dashboard, home automation, or your own tooling, consume
its **public contracts from the outside** — the `tm widget` / `tm stats` /
`tm status` CLI output, or the local SQLite store — rather than adding an
outbound client here. See
[ADR 0002](./docs/adr/0002-life-os-integration-boundary.md).

Concretely: **do not** add HTTP clients, webhooks, telemetry export, or
references to private services/repos to this codebase.

## Scope & hardware

Only the Yesoul W2 Pro is verified. Reports and fixes for other FTMS treadmills
are very welcome — please include the device model, firmware string, and a GATT
snapshot (`tm connect` with `RUST_LOG=debug`) so the behavior can be reproduced.
