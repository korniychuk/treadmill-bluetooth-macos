#!/usr/bin/env bash
#
# Register a nominal, never-executed .app bundle with LaunchServices purely so
# NSUserNotificationCenter has a real bundle to impersonate (see src/notify.rs
# and mac_notification_sys::set_application). This is NOT the daemon binary —
# the daemon keeps running as a loose signed executable, unrelated to this
# bundle's TCC identity. This nominal app exists only to give notifications a
# proper display name ("Treadmill") and our custom icon, instead of falling
# back to Finder's.
set -euo pipefail

readonly BUNDLE_ID="com.korniychuk.treadmill-bluetooth-macos"
readonly APP_NAME="Treadmill"
readonly LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
app_support="$HOME/Library/Application Support/treadmill-bluetooth-macos"
app_bundle="$app_support/${APP_NAME}.app"

mkdir -p "$app_bundle/Contents/MacOS" "$app_bundle/Contents/Resources"

cat > "$app_bundle/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID}</string>
    <key>CFBundleName</key>
    <string>${APP_NAME}</string>
    <key>CFBundleDisplayName</key>
    <string>${APP_NAME}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleExecutable</key>
    <string>${APP_NAME}</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
</dict>
</plist>
EOF

cat > "$app_bundle/Contents/MacOS/${APP_NAME}" <<'EOF'
#!/bin/sh
# Never actually run — this bundle exists only so LaunchServices can resolve
# its name and icon for notifications. See scripts/register-notification-identity.sh.
exit 0
EOF
chmod +x "$app_bundle/Contents/MacOS/${APP_NAME}"

cp "$repo_root/macos/AppIcon.icns" "$app_bundle/Contents/Resources/AppIcon.icns"

"$LSREGISTER" -f "$app_bundle"

echo "registered: $app_bundle"
