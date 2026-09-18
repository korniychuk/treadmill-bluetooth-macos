# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `alacritty_zoom` / `tm alacritty-zoom [on|off|pt <value>|preview|reset]` —
  grow the Alacritty font by `alacritty_zoom_pt` (default +0.625 pt) while
  walking and write the base size back when the belt stops or the treadmill
  session ends (задача 062). Font size only, verified by read-back with retries
  (Alacritty 0.17 IPC is lossy on macOS); recovery records in SQLite survive
  daemon and Alacritty restarts. A window zoomed by hand (⌘=/⌘-) ignores it
  until ⌘0.
- `tm toggle` and `tm speed up|down` (±0.1 km/h) — relative belt commands
  resolved by the daemon against live speed and a 5 s intent memory, so rapid
  key presses add up (задача 063). Refused on a stopped belt and while the
  daemon does not hold the link.
- `tm stats --json [--all]` and `tm samples --after-id N --limit N` — versioned,
  read-only JSON export of daily stats, workouts and raw readings; no Bluetooth
  (задача 068, contributed by @huertin03).
- Installers put the `tm` alias into `~/.bin` or `~/.local/bin`, whichever is
  already on `PATH` (default `~/.local/bin`), and print a ready-to-paste
  `export PATH=…` line when neither is (задача 067, contributed by @huertin03).

### Fixed

- Release archives now ship `scripts/install-prebuilt.sh` (the README's
  no-Rust install path) instead of the cargo-based installer, and CI checks the
  archive contents before publishing (задача 066, contributed by @huertin03).
- `tm led off` darkens the strip after a mains power-cycle: `Off` is primed
  with `On` first (задача 061).
- Piped CLI output (`tm stats | head`) exits quietly instead of panicking on a
  broken pipe (задача 060).

## [0.4.0] — 2026-08-28

### Added

- `led_on_connect` / `tm led default [off|on|none]` — persist a preferred
  ambient LED strip state and re-apply it on every daemon connect (задача
  059). The W2 Pro turns the strip back on after a mains power-cycle; the
  official app does the same. Absent or `"none"` keeps the leave-it-alone
  behaviour. Reload updates the in-memory value only (takes effect at the
  next connect); `tm status` shows the read-time value.
- `tm led on|off` — toggle the W2 Pro ambient LED strip over BLE (задача 058).
  Routed through the daemon queue when it holds the link, otherwise a
  direct connection. Does not persist a preference; the treadmill keeps
  its own state.

## [0.3.0] — 2026-07-09

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
  zone` CLI (`on`/`off`/`setup`/`limits`/`target`/`list`/`add`/`edit`/
  `remove`/`mode`, no-arg = status) — `on` runs an interactive onboarding
  prompt (age, optional resting HR) the first time; `list` prints every
  configured zone (id, bpm range, effective max speed); `add`/`edit`/`remove`
  interactively manage custom zones. Zones can be custom-named: `target_zone`
  accepts a 1-based number, an explicit/derived zone `id`, or a name
  substring. `tm status` gains a Zone Hold line; `tm widget` gains an
  `hr_zone` field (`below`/`in`/`above`/empty, 11 fields total now) — the
  reference tmux script weights the whole `♥ NNN` token by it
  (plain/bold/bold-italic, no colour change) while Zone Hold is actively
  correcting. Off by default;
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
