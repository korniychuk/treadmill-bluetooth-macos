# 003 — Yesoul W2 Pro: BLE protocol reverse-engineering & controller

**Status:** in progress — Phases 1–2 done, core control (Phase 4/5 subset) done & hardware-verified; Phase 3 (vendor capture) and logger/CLI polish remain
**Source:** external agent prompt, recorded verbatim-in-intent on 2026-07-04.
**Depends on:** [001](001-verify-treadmill-on-hardware.md) (does the device speak
FTMS?), [002](002-macos-bluetooth-permission.md) (Bluetooth permission).
**Feeds:** `docs/research/001-yesoul-ble-protocol.md`.

## Goal

A macOS CLI (Rust, `btleplug` + Tokio) that:
1. Auto-discovers and connects to a **Yesoul W2 Pro** treadmill over BLE.
2. Logs telemetry in real time to JSON or SQLite: **steps, speed (km/h or mph),
   incline (0–10 %), timestamp per sample**.
3. Sends control commands: speed (up/down/set), incline (up/down/set), disable
   the LED backlight, stop.
4. Is driven by CLI flags and/or an interactive menu.

## Phase 1 — Protocol analysis & documentation (reference reading)

Study these open projects and capture what maps to W2 Pro:
- **qdomyos-zwift** (`cagnulein/qdomyos-zwift`) — has Yesoul support incl.
  treadmills; FTMS impl; GATT characteristic layout for Yesoul.
- **Track My Indoor Workout** — Yesoul S3 (bike); BLE logging, FIT export —
  logic may transfer.
- **Yesoul_BLE** (`Raelx/Yesoul_BLE`) — ESP32 relay of Yesoul S3 BLE; shows how
  Yesoul frames data.
- **Yves Debeer treadmill-hacking blog** — methodology: Wireshark +
  `btsnoop_hci`, capturing/analyzing BLE ATT packets.

Deliverable: notes into `docs/research/001-yesoul-ble-protocol.md`.

## Phase 2 — Discover services & characteristics

1. Scan W2 Pro, collect: BLE name, (opaque macOS) id, all GATT service UUIDs,
   all characteristics (UUID + props: read/write/notify), descriptors (esp. CCCD).
2. Document discovered UUIDs as structured JSON **and** Rust consts, annotate the
   purpose of each characteristic.
3. Candidate characteristics (from competitor analysis) — confirm or refute:
   - Fitness-data service UUID (standard FTMS `0x1826` **or** proprietary Yesoul).
   - Steps/distance (read + notify), speed (read + notify), incline (read + notify).
   - Control: speed (write), incline (write), LED (write).

## Phase 3 — Reverse-engineer commands & data format

Methodology:
1. Drive the device with the official **Yesoul Fitness** app (Android
   emulator or real phone).
2. Capture BLE traffic via `btsnoop_hci` (Android) or PacketLogger (macOS).
3. Analyze in Wireshark.
4. Document the hex payload for each op: power on, speed +/- / set, incline
   +/- / set, LED off, stop.
5. Decode inbound notifications: encoding of steps (uint16/uint32 LE?), speed
   (float vs fixed-point?), incline (byte 0–10 vs percentage?), notify interval.

## Phase 4 — Rust implementation (target structure)

> Adapt to this repo's existing layout (`src/main.rs`, `src/scan.rs`,
> `src/ftms.rs`) rather than importing the tree wholesale — decide during
> planning of the impl sub-task.

Proposed modules (from the source prompt):
```
src/
  main.rs              # CLI entry
  ble/{mod,scanner,connection,protocol}.rs
  data/{mod,models,logger}.rs
  cli/{mod,commands}.rs
examples/basic_usage.rs
```

Crates proposed by the source prompt — **to be reconciled** with this repo's
already-pinned versions and the global `~`-only version policy before adopting:
`btleplug`, `tokio`, `uuid` (+serde), `serde`/`serde_json`, `rusqlite`
(bundled), `chrono` (+serde), `clap` (derive), `anyhow`, `futures`,
`log`/`env_logger`.

> ⚠️ Reconciliation notes (do at impl time, not now):
> - Repo already uses `tracing`, not `log`/`env_logger` — keep `tracing`.
> - Repo pins `btleplug = "0.12"`; the source prompt's `"0.10"` is stale — keep 0.12.
> - Global policy: `~` version ranges only, deps in repo root, `pnpm`/`npx` N/A (Rust).

## Phase 5 — Functional requirements

- **Scanner:** `find_yesoul_w2_pro()` — scan ~5 s, match name containing
  "Yesoul" (or known id), return first match.
- **Controller:** `YesoulController { peripheral, <characteristic handles> }`
  with `connect`, `start_notifications`, `set_speed(f32)`,
  `set_incline(u8)`, `toggle_led(bool)`, `get_current_stats()`.
- **Protocol:** raw-bytes → structured data, and command builders (inverse).
- **Logger:** JSON file with timestamps **or** SQLite table `workout_records`
  (timestamp, steps, speed, incline).
- **CLI:** `scan` · `connect` (connect + log) · `speed <v>` · `incline <v>` ·
  `led off` · `stats`.

## Phase 6 — Testing

- Mock peripheral for device-less testing.
- Unit tests for protocol parsing (extends existing `ftms` tests).
- Integration tests against a real W2 Pro when available.

## Constraints & expectations

- macOS only (CoreBluetooth via `btleplug`); needs Bluetooth permission (→ 002).
- Async / non-blocking via Tokio; graceful Ctrl-C shutdown.
- Memory safety via Rust.

## Outcome

1. Full Yesoul W2 Pro BLE protocol documentation.
2. Working Rust logging + control app.
3. Foundation for later integration into the larger **ReQuant** / LifeOS effort.

## Open questions (resolve before impl)

- ~~FTMS vs proprietary Yesoul service?~~ → **RESOLVED 2026-07-05: standard
  FTMS confirmed on hardware** (see research/001 hardware-verified section).
- ~~Incline control?~~ → **RESOLVED: no motorized incline over FTMS** —
  op 0x03 rejected with 0x04, no 0x2AD5, no feature bit.
- Steps: does W2 Pro even expose step count over BLE, or only speed/distance?
  (Telemetry so far: speed + total distance only → likely vendor notify.)
- LED-off command: standard FTMS has no LED control → almost certainly a
  vendor-specific write (`d18d2c10-…` or `0xFAB0`/`0xFFF0`); needs Phase 3 capture.
- Units (km/h vs mph) source of truth: device setting vs app-side conversion?

## Progress log

- 2026-07-05: Phase 1 research synthesized (research/001). Phase 2 GATT dump
  captured (research/gatt-snapshot.json). Implemented & hardware-verified:
  `scan`, `connect` (telemetry stream), `discover`, `start`, `stop`,
  `speed <kmh>`, `incline <pct>` (probe; device rejects). Speed change
  round-trip confirmed via telemetry (2.5 → 2.8 → 2.5 km/h).
- ⚠️ Standing constraint: **never touch firmware/OTA/DFU** without explicit
  operator approval.
