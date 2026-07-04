#!/usr/bin/env bash
#
# Build, code-sign, and run the treadmill connector so macOS attributes the
# Bluetooth (TCC) permission to *this app* — not the terminal — and shows the
# usage prompt exactly once. Any args are passed through (e.g. `run.sh connect`).
#
# Why a wrapper instead of plain `cargo run`: an unsigned binary has no
# code-signing identity, so macOS pins the Bluetooth grant to the terminal.
# Signing (even ad-hoc) plus the embedded Info.plist (see build.rs) gives the
# binary its own identity, so the grant sticks to the app.
#
#   scripts/run.sh                # scan (default)
#   scripts/run.sh connect        # connect + stream
#   PROFILE=release scripts/run.sh connect
#   IDENTITY="Treadmill BLE" scripts/run.sh    # sign with a stable self-signed cert
#
# IDENTITY defaults to ad-hoc ("-"): enough to test that attribution works, but
# the signature's cdhash changes each rebuild, so macOS re-prompts after a
# rebuild. Create a self-signed *code-signing* cert in Keychain Access once and
# pass its name via IDENTITY to get a rebuild-stable identity (prompt only once).
set -euo pipefail

readonly BUNDLE_ID="com.korniychuk.treadmill-bluetooth-macos"
readonly BIN_NAME="treadmill-bluetooth-macos"
PROFILE="${PROFILE:-debug}"
IDENTITY="${IDENTITY:--}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

build_flags=()
[[ "$PROFILE" == "release" ]] && build_flags+=(--release)
cargo build "${build_flags[@]}"

bin="target/${PROFILE}/${BIN_NAME}"
if [[ ! -x "$bin" ]]; then
  echo "error: built binary not found at $bin" >&2
  exit 1
fi

# --force: replace any previous signature. -i pins the signing identifier to the
# same bundle id the embedded Info.plist declares (keeps TCC's key consistent).
codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$bin"

if [[ "$IDENTITY" == "-" ]]; then
  echo "note: ad-hoc signed — macOS may re-prompt after a rebuild." >&2
  echo "      set IDENTITY=<cert name> for a rebuild-stable grant." >&2
fi

exec "$bin" "$@"
