#!/usr/bin/env bash
#
# Build macos/AppIcon.icns from the SF Symbol rendered by generate-icon.swift.
# One-time asset generation, not part of the running daemon — re-run only if
# the icon design changes.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

swift "$repo_root/scripts/generate-icon.swift" "$work_dir/icon-1024.png"

iconset="$work_dir/AppIcon.iconset"
mkdir -p "$iconset"
# Standard macOS iconset sizes (1x and 2x for each point size).
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$work_dir/icon-1024.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$work_dir/icon-1024.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done

mkdir -p "$repo_root/macos"
iconutil -c icns "$iconset" -o "$repo_root/macos/AppIcon.icns"
cp "$work_dir/icon-1024.png" "$repo_root/macos/AppIcon.png"

echo "wrote $repo_root/macos/AppIcon.icns"
echo "wrote $repo_root/macos/AppIcon.png"
