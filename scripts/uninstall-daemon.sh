#!/usr/bin/env bash
#
# Stop and remove the LaunchAgent installed by install-daemon.sh. Does not
# touch the SQLite store or workout logs under
# ~/Library/Application Support/treadmill-bluetooth-macos — those are your data.
set -euo pipefail

readonly LABEL="com.korniychuk.treadmill-bluetooth-macos.daemon"
readonly BIN_NAME="treadmill-bluetooth-macos"
# Must match install-daemon.sh.
LINK_DIR="${LINK_DIR:-$HOME/.bin}"
LINK_NAME="${LINK_NAME:-tm}"

plist="$HOME/Library/LaunchAgents/${LABEL}.plist"

if [[ -f "$plist" ]]; then
  launchctl unload "$plist" 2>/dev/null || true
  rm "$plist"
  echo "removed: $plist"
else
  echo "not installed: $plist"
fi

# Remove the `tm` alias only if it is our symlink — never touch a real file or
# someone else's `tm` that happens to sit at the same path.
link="$LINK_DIR/$LINK_NAME"
if [[ -L "$link" && "$(readlink "$link")" == *"/${BIN_NAME}" ]]; then
  rm "$link"
  echo "removed: $link"
fi
