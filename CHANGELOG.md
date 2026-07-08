# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Heart-rate support via a chest-strap BLE sensor (e.g. Polar H10, задача
  025): the daemon holds a second, independent BLE link and records
  continuous bpm samples alongside steps. `tm stats` shows a compact `♥
  avg/max` summary per day/workout (trimmed-mean average, p95 peak); `tm
  widget` gains a field for the live bpm (empty unless a sensor is worn
  and fresh) — see the updated tmux widget contract in `scripts/tmux/README.md`;
  `tm status` shows whether a sensor is connected. `tm hr` is a new
  diagnostic command (bring-up/troubleshooting only). No sensor worn is the
  normal case throughout — every surface degrades silently (empty/omitted),
  never an error.
- HR sensor battery level (задача 026): read on connect and re-read
  adaptively (every 60 min, or every 30 min once at/below 20%). `tm status`
  shows the exact percentage; `tm widget` gains a raw `hr_battery_pct` field
  (10 fields total now) — the reference tmux script turns it into a small
  warning glyph only once it's low, no number in the status bar itself.
- **Zone Hold** (задача 027): closed-loop mode that auto-adjusts belt speed
  off live bpm (задача 025) to hold a target heart-rate zone (default Zone 2,
  60-70% of Tanaka HRmax) during desk walking, instead of a fixed speed —
  cardiac drift then naturally eases the speed down over a long session.
  5-minute HR-blind warm-up ramp, then a `band` (hold the whole zone, default)
  or `center` (hold the midpoint, more corrections) closed-loop corrector
  every 20s, bounded ±0.3 km/h per step. Freezes on stepping off the belt and
  runs a 45s no-acceleration grace window on return; force-reduces (and, at
  min speed, stops the belt) above 80%/85% of HRmax as a safety cap. New `tm
  zone` CLI (`on`/`off`/`setup`/`limits`/`target`/`list`/`mode`, no-arg =
  status) — `on` runs an interactive onboarding prompt (age, optional resting
  HR) the first time; `list` prints every configured zone (id, bpm range,
  effective max speed). Zones can be custom-named: `target_zone` accepts a
  1-based number, an explicit/derived zone `id`, or a name substring. `tm
  status` gains a Zone Hold line; `tm widget` gains an `hr_zone` field
  (`below`/`in`/`above`/empty, 11 fields total now) — the reference tmux
  script weights the whole `♥ NNN` token by it (plain/bold/bold-italic, no
  colour change) while Zone Hold is actively correcting. Off by default;
  every surface degrades to no-op when disabled or the sensor isn't worn.

## [0.2.1] — 2026-07-08

### Changed

- tmux widget: the day's total steps — the daily-goal metric — is now rendered
  **bold** in a fixed near-black (`#181818`) that stays high-contrast on every
  state background (emerald / yellow / orange / muted), so the most important
  number is the visual anchor. Presentation-only change to the reference
  `scripts/tmux/treadmill-widget.sh`; the binary is unchanged.

## [0.2.0] — 2026-07-07

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
