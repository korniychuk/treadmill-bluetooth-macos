# Yesoul BLE protocol — research

**Status:** open

## Working hypothesis

Yesoul treadmills likely implement the Bluetooth SIG **Fitness Machine Service**
(FTMS, `0x1826`), the same profile Zwift / Kinomap / most modern smart
treadmills use. If so, the standard path works out of the box:

- `0x2ACD` Treadmill Data (notify) — speed / incline / distance telemetry.
- `0x2AD9` Fitness Machine Control Point (write) — start/stop, target speed/incline.
- `0x2ADA` Fitness Machine Status (notify) — state changes.

**Risk:** some Yesoul models expose only a **vendor-specific** service and speak
a proprietary framing that only the Yesoul app understands. In that case we
reverse engineer from captures.

## How to capture (macOS)

- `cargo run` (this project) for a quick service dump.
- macOS **PacketLogger** (Additional Tools for Xcode) for a full HCI trace.
- Cross-check with a generic scanner app (nRF Connect / LightBlue) on a phone.

## Findings

_(empty — fill in after running task 001 against the hardware)_

## References

- Bluetooth SIG — Fitness Machine Service 1.0.
- Bluetooth SIG — GATT Specification Supplement (Treadmill Data 0x2ACD fields).
