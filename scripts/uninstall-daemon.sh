#!/usr/bin/env bash
#
# Stop and remove the LaunchAgent installed by install-daemon.sh. Does not
# touch the SQLite store or workout logs under
# ~/Library/Application Support/treadmill-bluetooth-macos — those are your data.
set -euo pipefail

readonly LABEL="com.korniychuk.treadmill-bluetooth-macos.daemon"
plist="$HOME/Library/LaunchAgents/${LABEL}.plist"

if [[ -f "$plist" ]]; then
  launchctl unload "$plist" 2>/dev/null || true
  rm "$plist"
  echo "removed: $plist"
else
  echo "not installed: $plist"
fi
