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
- [x] **Check B (persistence):** confirmed re-prompting on every daemon
      rebuild during 005's development (operator noticed live). Fixed
      2026-07-05: generated a local self-signed code-signing certificate,
      CN "AnKor Treadmill BLE Dev" (org "AnKor"), via `openssl req -x509`
      (RSA 2048, `extendedKeyUsage=codeSigning`) + `security import ...
      -T /usr/bin/codesign` into the login keychain — operator approved this
      keychain write explicitly (the auto-mode classifier blocked the first,
      unauthorized attempt). `openssl pkcs12 -export` needed `-legacy`
      (OpenSSL 3.x's default MAC/cipher isn't what macOS's `security import`
      expects). Verified: `codesign -d -r-` now shows
      `designated => identifier "com.korniychuk.treadmill-bluetooth-macos"
      and certificate root = H"<cert hash>"` — pinned to the certificate, not
      a per-build cdhash. `scripts/install-daemon.sh` and `scripts/run.sh`
      default `IDENTITY` to this cert name now.

## Notes

- Ad-hoc signature's cdhash changes each rebuild → TCC re-prompts every time
  (confirmed, not just theoretical — see Check B). The self-signed cert fixes
  this because its designated requirement anchors on the certificate itself.
- `security find-identity -v -p codesigning` reports the self-signed identity
  as `0 valid identities` / `CSSMERR_TP_NOT_TRUSTED` (expected — no trust
  chain to a root). `codesign --sign "<name>"` still works: producing a
  signature does not require the signing cert to be trusted, only present
  (cert + private key) in the keychain.
- To re-arm the prompt while testing:
  `tccutil reset Bluetooth com.korniychuk.treadmill-bluetooth-macos`.
- Plain `cargo run` still works but is unsigned → attributed to the terminal;
  use `scripts/run.sh` to get the per-app grant.
