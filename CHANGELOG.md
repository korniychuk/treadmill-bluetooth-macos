# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — First public release

First open-source release. macOS BLE connector for a Yesoul (FTMS) treadmill.

### Added

- BLE scan / connect / live FTMS telemetry streaming (speed, distance, steps).
- Background daemon (LaunchAgent): auto scan / connect / reconnect, watchdog for
  fast reconnect, AC-power awareness.
- Presence detection (belt moving vs. steps rising) and activity-based workout
  segmentation, with a read-time, retroactively-configurable merge gap.
- Daily stats (`tm stats`), restart-safe delta accumulation, SQLite store.
- Step-goal milestones with native macOS toasts, hot-reloaded from a per-user
  `goals.json`.
- FTMS control point: start / stop / set target speed; pause/resume speed restore;
  computed default start speed.
- `tm widget` status-bar output and a reference tmux renderer (`scripts/tmux/`).
- `recompute-segments` and `default-speed` offline (no-BLE) commands.
- Install tooling: `install-daemon.sh` (build from source), `install-prebuilt.sh`
  (prebuilt binary, no toolchain), `uninstall-daemon.sh`.
- GitHub Actions CI (fmt / clippy / build / test) and release workflow producing
  an unsigned macOS `.tar.gz`.

### Known limitations

See [README → Limitations](./README.md#limitations): macOS-only, verified on the
Yesoul W2 Pro only, no incline, release binaries not notarized.

[Unreleased]: https://github.com/korniychuk/treadmill-bluetooth-macos/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/korniychuk/treadmill-bluetooth-macos/releases/tag/v0.1.0
