# 004 — LED backlight control (vendor command via HCI capture)

**Status:** backlog (deferred by operator, 2026-07-05)
**Depends on:** [003](../tasks/003-yesoul-w2-pro-controller.md) findings.

## Context

The official Yesoul app can toggle the W2 Pro's LED backlight over BLE, so a
vendor command exists — but it is documented nowhere public (not in the
FitShow protocol, treadspan, or qdomyos-zwift sources). The write target is
almost certainly one of: `d18d2c10-c44c-11e8-a355-529269fb1459` (write, inside
FTMS), `0xFAB1`/`0xFAB2`, or `0xFFF2`.

Note: the app exposes NO incline control (verified by operator, including
in-app workouts) — incline stays RF-remote-only; this task is LED only.

## Plan (when picked up)

1. Operator's Android (Xiaomi) phone: enable Developer options → **Bluetooth
   HCI snoop log** + USB debugging; toggle Bluetooth off/on to start a fresh log.
2. In the Yesoul app, connect to the treadmill and toggle the LED ~5 times
   with ~2 s pauses.
3. Pull the log over USB (`adb bugreport` → btsnoop_hci.log) and analyze in
   tshark, filtering ATT writes (same methodology as the Sperax pcap analysis).
4. Identify the LED frame(s) + target characteristic; implement `led on|off`
   in the CLI (likely via `src/fitshow.rs` or a new vendor module); verify live.

⚠️ Never write to `0xFF00`/`0xFF01` (suspected OTA channel) — firmware changes
are forbidden without explicit operator approval.
