//! Native macOS toast notifications, without a Swift/UserNotifications dep.
//!
//! `osascript -e 'display notification …'` runs in the caller's Aqua session
//! and renders a standard Notification Center banner. Invoked via
//! `Command::new("osascript")` with args (never a shell), so there is no
//! injection risk — but the AppleScript string literal itself still needs its
//! own quotes/backslashes escaped.

use std::process::Command;

use tracing::warn;

/// Show a macOS notification banner; logs a warning instead of failing the
/// caller if `osascript` is unavailable or the call errors (a missed toast is
/// not worth crashing the daemon over).
pub fn toast(title: &str, body: &str) {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        escape(body),
        escape(title)
    );
    match Command::new("osascript").arg("-e").arg(script).status() {
        Ok(status) if status.success() => {}
        Ok(status) => warn!(?status, %title, "osascript exited non-zero"),
        Err(err) => warn!(%err, %title, "failed to spawn osascript"),
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
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
