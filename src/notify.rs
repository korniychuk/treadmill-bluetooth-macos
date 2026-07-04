//! Native macOS toast notifications via `NSUserNotificationCenter`.
//!
//! Uses `mac-notification-sys` (pure Rust, FFI straight to the Objective-C
//! runtime — no Swift, no shelling out to `osascript`). Posting a notification
//! from an unbundled process requires impersonating a bundle identifier that
//! LaunchServices can resolve (`set_application`); `scripts/register-notification-identity.sh`
//! registers a small, never-executed "Treadmill.app" purely so notifications
//! show that name and our custom icon instead of falling back to Finder's.
//! This is independent of the daemon's own TCC/Bluetooth identity — it never
//! runs, it just needs to exist on disk for LaunchServices to resolve.

use std::path::PathBuf;

use mac_notification_sys::error::{ApplicationError, Error as NotifyError};
use mac_notification_sys::{Notification, set_application};
use tracing::warn;

const BUNDLE_ID: &str = "com.korniychuk.treadmill-bluetooth-macos";

/// Set once per process; later calls return `ApplicationError::AlreadySet`,
/// which is expected (not a real failure) once the first call succeeds.
fn ensure_identity() {
    if let Err(err) = set_application(BUNDLE_ID)
        && !matches!(err, NotifyError::Application(ApplicationError::AlreadySet(_)))
    {
        warn!(%err, "could not set notification identity — icon/name may fall back to default");
    }
}

fn icon_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home)
        .join("Library/Application Support/treadmill-bluetooth-macos/Treadmill.app/Contents/Resources/AppIcon.icns");
    path.exists().then_some(path)
}

/// Show a macOS notification banner; logs a warning instead of failing the
/// caller if delivery errors (a missed toast is not worth crashing the daemon
/// over).
pub fn toast(title: &str, body: &str) {
    ensure_identity();

    let icon = icon_path();
    let icon_str = icon.as_ref().map(|p| p.to_string_lossy());
    let mut notification = Notification::new();
    notification.title(title).message(body).asynchronous(true);
    if let Some(icon_str) = &icon_str {
        notification.app_icon(icon_str);
    }

    if let Err(err) = notification.send() {
        warn!(%err, %title, "failed to deliver notification");
    }
}

pub fn treadmill_found() {
    toast("Treadmill", "Discovered and connected");
}

pub fn treadmill_lost() {
    toast("Treadmill", "Connection lost (powered off?)");
}

pub fn walker_away() {
    toast("Treadmill", "Belt is running but steps aren't counting — did you step off?");
}

pub fn walker_resumed() {
    toast("Treadmill", "Steps are counting again");
}

pub fn treadmill_paused() {
    toast("Treadmill", "Paused");
}

pub fn treadmill_resumed() {
    toast("Treadmill", "Resumed");
}
