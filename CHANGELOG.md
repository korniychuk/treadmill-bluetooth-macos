# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Idle-belt auto-pause: when the belt keeps running with nobody walking (you
  stepped off), the daemon pauses it after `auto_pause_minutes` (default 5, `0`
  disables); the treadmill's own shutoff then powers it down.
- `tm status` now shows the config the daemon currently has loaded (goals,
  auto-pause) and when it last read the config file.

### Changed

- **Config is now TOML** at `~/.config/treadmill-bluetooth-macos/config.toml`
  (was JSON `goals.json`). TOML lets the example config document each key's
  default as a comment. The file already held more than goals (workout gap,
  auto-pause), so the name changed too.

### Removed

- JSON config support and the transitional `goals.json` filename /
  `TREADMILL_GOALS_CONFIG` env fallbacks — config is TOML-only.

### Migration

- Rename `~/.config/treadmill-bluetooth-macos/goals.json` → `config.toml` and
  convert it to TOML, e.g. `goals = [8000, 10000, 12000]` with optional
  `workout_gap_minutes` / `auto_pause_minutes`. See `config/config.example.toml`.

## [0.1.0] — 2026-07-06 — First public release

First open-source release. macOS BLE connector for a Yesoul (FTMS) treadmill.

### Added

- BLE scan / connect / live FTMS telemetry streaming (speed, distance, steps).
- Background daemon (LaunchAgent): auto scan / connect / reconnect, watchdog for
  fast reconnect, AC-power awareness.
- Presence detection (belt moving vs. steps rising) and activity-based workout
  segmentation, with a read-time, retroactively-configurable merge gap.
- Daily stats (`tm stats`), restart-safe delta accumulation, SQLite store.
- Step-goal milestones with native macOS toasts, hot-reloaded from a per-user
  `config.toml`.
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
