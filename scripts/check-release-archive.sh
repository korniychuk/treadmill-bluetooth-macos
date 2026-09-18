#!/usr/bin/env bash
# Validate the consumer-facing archive before publishing it. No installation,
# Bluetooth, code signing, or LaunchAgent changes are performed.
set -euo pipefail
[[ $# -eq 1 ]] || { echo 'usage: check-release-archive.sh <archive>' >&2; exit 1; }
listing="$(tar -tzf "$1")"
root="${listing%%/*}"
for file in treadmill-bluetooth-macos scripts/install-prebuilt.sh \
  scripts/uninstall-daemon.sh scripts/register-notification-identity.sh \
  macos/AppIcon.icns README.md LICENSE; do
  if ! grep -Fxq "${root}/${file}" <<< "$listing"; then
    echo "release archive missing: ${file}" >&2
    exit 1
  fi
done
tar -xOf "$1" "${root}/scripts/install-prebuilt.sh" | bash -n
echo 'release archive: required runtime and prebuilt installer files present'
