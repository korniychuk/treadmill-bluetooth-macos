#!/usr/bin/env bash
#
# Build, code-sign, and install the presence-aware daemon as a macOS
# LaunchAgent (not a LaunchDaemon — toast notifications and the Bluetooth
# permission prompt only work in the user's Aqua session, which a
# LaunchDaemon does not have).
#
# WorkingDirectory is pinned to the same "Application Support" directory the
# daemon's SQLite store already uses (see src/store.rs) so the relative
# `workouts/` JSONL logger (see src/logger.rs) lands next to it instead of
# under launchd's default cwd ("/"), which is not writable.
#
# Signs with the local self-signed "AnKor Treadmill BLE Dev" code-signing
# identity by default (see `docs/tasks/002-macos-bluetooth-permission.md` for
# how it was created) — its designated requirement pins to the certificate,
# not the binary's cdhash, so rebuilds keep the same TCC identity and the
# Bluetooth permission prompt does not reappear. Override with IDENTITY=- for
# a plain ad-hoc signature, or IDENTITY="Other Cert Name" for a different one.
#
#   scripts/install-daemon.sh
#   IDENTITY="-" scripts/install-daemon.sh   # ad-hoc — re-prompts every rebuild
set -euo pipefail

readonly BUNDLE_ID="com.korniychuk.treadmill-bluetooth-macos"
readonly LABEL="${BUNDLE_ID}.daemon"
readonly BIN_NAME="treadmill-bluetooth-macos"
IDENTITY="${IDENTITY:-AnKor Treadmill BLE Dev}"
# Short CLI alias symlinked into a PATH dir so `tm stats`/`tm status` work from
# anywhere. Points at the release artifact, so it tracks every rebuild. Set
# LINK_NAME="" to skip. Keep in sync with uninstall-daemon.sh.
LINK_DIR="${LINK_DIR:-$HOME/.bin}"
LINK_NAME="${LINK_NAME:-tm}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo build --release
bin="$repo_root/target/release/${BIN_NAME}"
if [[ ! -x "$bin" ]]; then
  echo "error: built binary not found at $bin" >&2
  exit 1
fi

codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$bin"
if [[ "$IDENTITY" == "-" ]]; then
  echo "note: ad-hoc signed — macOS may re-prompt for Bluetooth after a rebuild." >&2
  echo "      set IDENTITY=<cert name> for a rebuild-stable grant." >&2
fi

# Install/refresh the `tm` alias. `ln -sfn` replaces our own stale symlink in
# place, but we refuse to clobber a real file that happens to share the name.
link=""
if [[ -n "$LINK_NAME" ]]; then
  link="$LINK_DIR/$LINK_NAME"
  mkdir -p "$LINK_DIR"
  if [[ -e "$link" && ! -L "$link" ]]; then
    echo "warning: $link exists and is not a symlink — leaving it alone" >&2
    link=""
  else
    ln -sfn "$bin" "$link"
    case ":$PATH:" in
      *":$LINK_DIR:"*) ;;
      *) echo "note: $LINK_DIR is not in your PATH — add it to call '$LINK_NAME' directly." >&2 ;;
    esac
  fi
fi

app_support="$HOME/Library/Application Support/treadmill-bluetooth-macos"
log_dir="$HOME/Library/Logs/treadmill-bluetooth-macos"
mkdir -p "$app_support" "$log_dir"

# Registers the nominal "Treadmill.app" that gives toast notifications their
# name + icon (src/notify.rs) — see that script for why this is not the
# daemon binary itself.
bash "$repo_root/scripts/register-notification-identity.sh"

plist="$HOME/Library/LaunchAgents/${LABEL}.plist"
mkdir -p "$(dirname "$plist")"
cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${bin}</string>
        <string>daemon</string>
    </array>
    <key>WorkingDirectory</key>
    <string>${app_support}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>${log_dir}/daemon.log</string>
    <key>StandardErrorPath</key>
    <string>${log_dir}/daemon.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>treadmill_bluetooth_macos=info,warn</string>
        <!-- Config is read from a per-user file under $HOME
             (~/.config/treadmill-bluetooth-macos/config.json); no env needed.
             Set TREADMILL_CONFIG here only to override that path. -->
    </dict>
</dict>
</plist>
EOF

launchctl unload "$plist" 2>/dev/null || true
launchctl load "$plist"

echo "installed: $plist"
[[ -n "$link" ]] && echo "alias:     $link -> $bin"
echo "logs:      $log_dir/daemon.log"
echo "db:        $app_support/treadmill.db"
echo "uninstall: scripts/uninstall-daemon.sh"
