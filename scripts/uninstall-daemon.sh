#!/usr/bin/env bash
#
# Stop and remove the LaunchAgent installed by install-daemon.sh. Does not
# touch the SQLite store or workout logs under
# ~/Library/Application Support/treadmill-bluetooth-macos — those are your data.
set -euo pipefail

readonly LABEL="com.korniychuk.treadmill-bluetooth-macos.daemon"
readonly BIN_NAME="treadmill-bluetooth-macos"
# Must match install-daemon.sh. LINK_DIR unset → check every conventional dir.
LINK_NAME="${LINK_NAME:-tm}"
# shellcheck source=scripts/cli-link.sh
source "$(dirname "${BASH_SOURCE[0]}")/cli-link.sh"
if [[ -n "${LINK_DIR:-}" ]]; then
  link_dirs=("$LINK_DIR")
else
  link_dirs=("${TM_LINK_DIR_CANDIDATES[@]}")
fi

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
config_path="$HOME/.config/treadmill-bluetooth-macos/config.toml"
zoom_reset_done=""
for dir in "${link_dirs[@]}"; do
  link="$dir/$LINK_NAME"
  is_our_link "$link" "$BIN_NAME" || continue
  # Restore Alacritty fonts while the binary is still reachable (задача 062).
  if [[ -z "$zoom_reset_done" && -f "$config_path" ]] && grep -Eq '^[[:space:]]*alacritty_zoom[[:space:]]*=[[:space:]]*true([[:space:]]*(#.*)?)?$' "$config_path"; then
    "$link" alacritty-zoom reset || true
    zoom_reset_done=1
  fi
  rm "$link"
  echo "removed: $link"
done
