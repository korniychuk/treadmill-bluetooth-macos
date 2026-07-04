# 002 — macOS Bluetooth permission (TCC), requested once for this app

**Status:** implemented — awaiting on-hardware confirmation

## Goal

Trigger the macOS Bluetooth permission prompt for **this app** (not the
terminal), approve it once, and never be re-prompted. On macOS the Bluetooth
(TCC) grant is keyed on the calling process's **code-signing identity** + bundle
id, so a bare `cargo run` binary (unsigned, no bundle) gets attributed to the
terminal instead.

## What was done

- `macos/Info.plist` — declares `CFBundleIdentifier`
  (`com.korniychuk.treadmill-bluetooth-macos`), `CFBundleName`, and
  `NSBluetoothAlwaysUsageDescription` (the prompt text). Without the usage key,
  creating a `CBCentralManager` terminates the process.
- `build.rs` — embeds `macos/Info.plist` into the `__TEXT,__info_plist` Mach-O
  section (`-Wl,-sectcreate,...`), macOS target only. So even the plain
  `target/debug` binary carries the bundle id + usage string.
- `scripts/run.sh` — `cargo build` → `codesign` → `exec`. Signing gives the
  binary its own identity so the grant sticks to the app. Ad-hoc by default;
  `IDENTITY=<cert>` for a stable self-signed cert.

## How to run

```bash
scripts/run.sh            # scan (default)
scripts/run.sh connect    # connect + stream
```

First run → macOS shows the one-time Bluetooth prompt → approve.

## Verification (empirical, cheapest-first)

- [x] Builds with embedded plist; `otool -P` shows `CFBundleIdentifier` +
      `NSBluetoothAlwaysUsageDescription`.
- [x] `codesign -dv` → `Identifier=com.korniychuk.treadmill-bluetooth-macos`.
- [ ] **Check A (attribution):** run once, approve. System Settings → Privacy &
      Security → Bluetooth lists **treadmill-bluetooth-macos**, not the terminal.
- [ ] **Check B (persistence):** rebuild + run again.
  - Silent (no re-prompt) → **done, no cert needed.**
  - Re-prompts → create a self-signed **code-signing** cert in Keychain Access
    (Certificate Assistant → Create a Certificate → Self-Signed Root, Code
    Signing) and run with `IDENTITY="<cert name>"` — its DR is rebuild-stable.

## Notes

- Ad-hoc signature's cdhash changes each rebuild → TCC may re-prompt. That is
  the expected trigger for adding the self-signed cert (Check B).
- To re-arm the prompt while testing:
  `tccutil reset Bluetooth com.korniychuk.treadmill-bluetooth-macos`.
- Plain `cargo run` still works but is unsigned → attributed to the terminal;
  use `scripts/run.sh` to get the per-app grant.
