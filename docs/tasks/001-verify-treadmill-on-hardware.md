# 001 — Verify against real Yesoul hardware

**Status:** todo

## Goal

Confirm what the physical Yesoul treadmill actually advertises and whether the
standard FTMS path works, before building control on top.

## Steps

1. `cargo run` (scan) with the treadmill powered on & in Bluetooth pairing mode.
   Record: local name, advertised service UUIDs. Does `0x1826` (FTMS) appear?
2. If FTMS present → `cargo run -- connect`, walk the treadmill through
   idle → running → speed change → incline change. Capture decoded frames.
3. If FTMS absent → dump all services/characteristics; capture raw notifications
   from any vendor-specific service. Feed findings into
   `docs/research/001-yesoul-ble-protocol.md`.

## Acceptance

- Documented, reproducible answer to "does this treadmill speak FTMS?"
- At least speed telemetry decoded and logged from the real device.
