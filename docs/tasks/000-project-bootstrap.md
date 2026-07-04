# 000 — Project bootstrap

**Status:** done
**Date:** 2026-07-04

## Goal

Stand up the repository and a compiling Rust scaffold for a macOS BLE connector
to a Yesoul treadmill.

## Done

- `cargo init` binary crate, edition 2024.
- Dependencies: `btleplug`, `tokio`, `tracing`, `tracing-subscriber`, `anyhow`,
  `futures`, `uuid`.
- FTMS constants + Treadmill Data (`0x2ACD`) parser with unit tests (`src/ftms.rs`).
- BLE scan / connect / notification streaming over CoreBluetooth (`src/scan.rs`).
- CLI with `scan` (default) and `connect` modes (`src/main.rs`).
- `CLAUDE.md`, `README.md`, MIT `LICENSE`, `docs/` structure.
- Private GitHub repo `korniychuk/treadmill-bluetooth-macos`.

## Next

See `001-*` and `docs/research/001-yesoul-ble-protocol.md`.
