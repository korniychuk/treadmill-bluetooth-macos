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
#   IDENTITY="-" scripts/run.sh    # ad-hoc — re-prompts every rebuild
#
# IDENTITY defaults to the local self-signed "AnKor Treadmill BLE Dev"
# code-signing cert (see docs/tasks/002) — its designated requirement pins to
# the certificate, not the binary's cdhash, so rebuilds keep the same TCC
# identity and macOS does not re-prompt for Bluetooth. Ad-hoc ("-") is enough
# to test that attribution works, but re-prompts on every rebuild.
set -euo pipefail

readonly BUNDLE_ID="com.korniychuk.treadmill-bluetooth-macos"
readonly BIN_NAME="treadmill-bluetooth-macos"
PROFILE="${PROFILE:-debug}"
IDENTITY="${IDENTITY:-AnKor Treadmill BLE Dev}"

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
