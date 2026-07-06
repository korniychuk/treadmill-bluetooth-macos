#!/usr/bin/env bash
#
# Install a PRE-BUILT daemon binary as a macOS LaunchAgent — for users who
# downloaded a release tarball and do NOT have a Rust toolchain. Unlike
# install-daemon.sh, this never invokes cargo: it installs the binary shipped
# next to this script (or one passed via SOURCE_BIN) into a stable location and
# points the LaunchAgent at it.
#
# LaunchAgent (not LaunchDaemon): toast notifications and the Bluetooth
# permission prompt only work in the user's Aqua session.
#
# The binary is installed to a fixed path under "Application Support" (not the
# extracted tarball dir, which the user may delete) so the LaunchAgent keeps
# working after cleanup. WorkingDirectory is pinned to the same directory the
# daemon's SQLite store uses (see src/store.rs) so the relative `workouts/`
# JSONL logger lands next to it, not under launchd's non-writable cwd ("/").
#
# Signing: defaults to an ad-hoc signature (IDENTITY="-"). A downloaded binary
# also carries com.apple.quarantine, which we strip so Gatekeeper does not block
# the launchd-spawned process. With ad-hoc signing macOS may re-prompt for the
# Bluetooth permission if the binary ever changes; that is fine for a pinned
# release. Override IDENTITY="<cert name>" to sign with your own certificate for
# a rebuild-stable TCC grant (see docs/tasks/002-macos-bluetooth-permission.md).
#
#   scripts/install-prebuilt.sh
#   IDENTITY="My Cert" scripts/install-prebuilt.sh
#   SOURCE_BIN=/path/to/treadmill-bluetooth-macos scripts/install-prebuilt.sh
set -euo pipefail

readonly BUNDLE_ID="com.korniychuk.treadmill-bluetooth-macos"
readonly LABEL="${BUNDLE_ID}.daemon"
readonly BIN_NAME="treadmill-bluetooth-macos"
IDENTITY="${IDENTITY:--}"
# Short CLI alias symlinked into a PATH dir so `tm stats`/`tm status` work from
# anywhere. Set LINK_NAME="" to skip. Keep in sync with uninstall-daemon.sh.
LINK_DIR="${LINK_DIR:-$HOME/.bin}"
LINK_NAME="${LINK_NAME:-tm}"

bundle_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Locate the pre-built binary: SOURCE_BIN override, else next to the tarball
# root, else the conventional cargo output path (handy when run from a checkout).
source_bin="${SOURCE_BIN:-}"
if [[ -z "$source_bin" ]]; then
  for candidate in "$bundle_root/$BIN_NAME" "$bundle_root/target/release/$BIN_NAME"; do
    if [[ -x "$candidate" ]]; then
      source_bin="$candidate"
      break
    fi
  done
fi
if [[ -z "$source_bin" || ! -x "$source_bin" ]]; then
  echo "error: pre-built binary not found (looked for '$BIN_NAME' next to this" >&2
  echo "       script). Pass SOURCE_BIN=/path/to/$BIN_NAME explicitly." >&2
  exit 1
fi

app_support="$HOME/Library/Application Support/treadmill-bluetooth-macos"
log_dir="$HOME/Library/Logs/treadmill-bluetooth-macos"
bin_dir="$app_support/bin"
bin="$bin_dir/$BIN_NAME"
mkdir -p "$app_support" "$log_dir" "$bin_dir"

# Install the binary into its stable home, strip quarantine, then sign.
cp "$source_bin" "$bin"
chmod +x "$bin"
xattr -d com.apple.quarantine "$bin" 2>/dev/null || true
codesign --force --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$bin"
if [[ "$IDENTITY" == "-" ]]; then
  echo "note: ad-hoc signed — macOS may re-prompt for Bluetooth if the binary changes." >&2
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

# Registers the nominal "Treadmill.app" that gives toast notifications their
# name + icon (src/notify.rs). Uses macos/AppIcon.icns shipped in the tarball.
bash "$bundle_root/scripts/register-notification-identity.sh"

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
        <!-- Step goals are read from a per-user file under $HOME
             (~/.config/treadmill-bluetooth-macos/goals.json); no env needed.
             Set TREADMILL_GOALS_CONFIG here only to override that path. -->
    </dict>
</dict>
</plist>
EOF

launchctl unload "$plist" 2>/dev/null || true
launchctl load "$plist"

echo "installed: $plist"
echo "binary:    $bin"
[[ -n "$link" ]] && echo "alias:     $link -> $bin"
echo "logs:      $log_dir/daemon.log"
echo "db:        $app_support/treadmill.db"
echo "uninstall: scripts/uninstall-daemon.sh"
