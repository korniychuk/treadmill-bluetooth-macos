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
use std::time::Duration;

use mac_notification_sys::error::{ApplicationError, Error as NotifyError};
use mac_notification_sys::{Notification, set_application};
use tracing::warn;

const BUNDLE_ID: &str = "com.korniychuk.treadmill-bluetooth-macos";

/// Seconds per minute / hour — named to avoid magic numbers in the compact
/// duration formatter.
const SECS_PER_MIN: u64 = 60;
const SECS_PER_HOUR: u64 = 60 * 60;

/// Set once per process; later calls return `ApplicationError::AlreadySet`,
/// which is expected (not a real failure) once the first call succeeds.
fn ensure_identity() {
    if let Err(err) = set_application(BUNDLE_ID)
        && !matches!(
            err,
            NotifyError::Application(ApplicationError::AlreadySet(_))
        )
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
    toast_full(title, None, body, None);
}

/// Like [`toast`], but optionally renders a `subtitle` line (a second visible
/// line above the body — used for the multi-line resume toast, задача 012) and
/// plays a named macOS system sound (e.g. `"Glass"`, `"Hero"` — graduated
/// step-goal celebrations, задача 011). `None`/`None` matches a plain toast.
fn toast_full(title: &str, subtitle: Option<&str>, body: &str, sound: Option<&str>) {
    ensure_identity();

    let icon = icon_path();
    let icon_str = icon.as_ref().map(|p| p.to_string_lossy());
    let mut notification = Notification::new();
    notification
        .title(title)
        .maybe_subtitle(subtitle)
        .message(body)
        .asynchronous(true);
    if let Some(icon_str) = &icon_str {
        notification.app_icon(icon_str);
    }
    notification.maybe_sound(sound);

    if let Err(err) = notification.send() {
        warn!(%err, %title, "failed to deliver notification");
    }
}

/// Compact, human-friendly duration: `45s`, `2m37s`, `1h02m`. Lower units are
/// zero-padded so the string width stays stable within a bucket. Coarsens to
/// two significant units at most — good enough for a toast, unlike
/// `main::fmt_duration` (always `MmSSs`) or `main::humanize_ago` (single
/// coarse unit). Pure and unit-tested (задача 010).
pub fn humanize_short(d: Duration) -> String {
    let total = d.as_secs();
    if total < SECS_PER_MIN {
        return format!("{total}s");
    }
    if total < SECS_PER_HOUR {
        return format!("{}m{:02}s", total / SECS_PER_MIN, total % SECS_PER_MIN);
    }
    let hours = total / SECS_PER_HOUR;
    let minutes = (total % SECS_PER_HOUR) / SECS_PER_MIN;
    format!("{hours}h{minutes:02}m")
}

/// Group an integer with thousands separators: `12000` → `12,000`. Kept local
/// (no locale crate) since it is only ever applied to small step counts.
fn group_thousands(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let len = digits.len();
    let mut grouped = String::with_capacity(len + len / 3);
    for (i, ch) in digits.chars().enumerate() {
        // Insert a separator before every digit that starts a fresh group of
        // three counted from the right (but never before the first digit).
        if i != 0 && (len - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    if value < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

pub fn treadmill_found() {
    toast("Treadmill", "Discovered and connected");
}

pub fn treadmill_lost() {
    toast("Treadmill", "Connection lost (powered off?)");
}

pub fn walker_away() {
    toast(
        "Treadmill",
        "Belt is running but steps aren't counting — did you step off?",
    );
}

/// Fired when steps resume after an `AwayWhileRunning` spell. `away` is how
/// long the belt ran while the operator was NOT walking (задача 010); `None`
/// only if the daemon lost the start instant, in which case we omit the figure
/// rather than show a wrong one.
pub fn walker_resumed(away: Option<Duration>) {
    match away {
        Some(away) => toast(
            "Treadmill",
            &format!(
                "Steps are counting again — you were away {}",
                humanize_short(away)
            ),
        ),
        None => toast("Treadmill", "Steps are counting again"),
    }
}

pub fn treadmill_paused() {
    toast("Treadmill", "Paused");
}

/// Fired when the daemon auto-pauses an idle belt (задача 020): it ran
/// `AwayWhileRunning` for `away` (nobody walking) past the configured threshold,
/// so we sent a Control-Point pause. The machine's own built-in shutoff powers
/// it down from here. Replaces the generic "Paused" toast for this case (the
/// daemon suppresses that follow-up), so the operator sees why it stopped.
pub fn auto_paused(away: Duration) {
    toast(
        "Treadmill",
        &format!(
            "Auto-paused — belt ran idle {} after you stepped off",
            humanize_short(away)
        ),
    );
}

/// A belt-speed restore performed on resume (задача 012, Task D): the machine
/// reset to `from_kmh` on pause-resume and we re-sent `to_kmh` (the pre-pause
/// walking speed) over the FTMS Control Point.
#[derive(Debug, Clone, Copy)]
pub struct SpeedRestore {
    pub from_kmh: f32,
    pub to_kmh: f32,
}

/// Fired when the belt restarts after a pause (задачи 010/012). Multi-line:
/// the subtitle reports how long the pause lasted (when known), the body
/// reports the auto speed-restore (when one was applied) or a plain "Resumed".
pub fn treadmill_resumed(paused_for: Option<Duration>, restore: Option<SpeedRestore>) {
    let subtitle = paused_for.map(|d| format!("Paused for {}", humanize_short(d)));
    let body = match restore {
        Some(r) => format!("Speed restored {:.1} → {:.1} km/h", r.from_kmh, r.to_kmh),
        None => "Resumed".to_string(),
    };
    toast_full("Treadmill", subtitle.as_deref(), &body, None);
}

/// Fired when the daemon applies the computed default belt speed at a workout
/// start (задача 016): the device came up at its factory crawl `from_kmh` (~0.5)
/// and we set `to_kmh` — the operator's recent cruising pace. Distinct from the
/// pause-resume "Speed restored" toast (задача 012): this is a fresh start, not
/// a restore.
pub fn default_speed_applied(from_kmh: f32, to_kmh: f32) {
    toast(
        "Treadmill",
        &format!("Set your usual pace {from_kmh:.1} → {to_kmh:.1} km/h"),
    );
}

/// Celebrate crossing a daily step goal, graduated by `tier` (задача 011):
/// tier 1 is a quiet flourish, tier 2 adds heat and a sound, tier 3 is the
/// loudest with a distinct sound. `threshold` is the goal's step count.
pub fn goal_reached(threshold: i64, tier: u8) {
    let steps = group_thousands(threshold);
    let (body, sound) = match tier {
        1 => (format!("🎉 Goal reached: {steps} steps today"), None),
        2 => (
            format!("🔥🎉 {steps} steps — you're on fire today!"),
            Some("Glass"),
        ),
        // Tier 3 (and any unexpectedly higher tier) gets the loudest copy.
        _ => (
            format!("🏆🔥🎉 {steps} steps — crushing it! Absolute machine 💪"),
            Some("Hero"),
        ),
    };
    toast_full("Treadmill", None, &body, sound);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_short_seconds_bucket_has_no_minutes() {
        assert_eq!(humanize_short(Duration::from_secs(0)), "0s");
        assert_eq!(humanize_short(Duration::from_secs(45)), "45s");
        assert_eq!(humanize_short(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn humanize_short_minutes_bucket_zero_pads_seconds() {
        assert_eq!(humanize_short(Duration::from_secs(60)), "1m00s");
        assert_eq!(humanize_short(Duration::from_secs(157)), "2m37s");
        assert_eq!(humanize_short(Duration::from_secs(3599)), "59m59s");
    }

    #[test]
    fn humanize_short_hours_bucket_zero_pads_minutes() {
        assert_eq!(humanize_short(Duration::from_secs(3600)), "1h00m");
        assert_eq!(humanize_short(Duration::from_secs(3720)), "1h02m");
    }

    #[test]
    fn group_thousands_inserts_separators() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(500), "500");
        assert_eq!(group_thousands(8000), "8,000");
        assert_eq!(group_thousands(12000), "12,000");
        assert_eq!(group_thousands(1234567), "1,234,567");
    }
}
