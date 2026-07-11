//! Status-bar widget TSV contract (`tm widget`) and `speed-widget` toggle.

use anyhow::Result;
use chrono::{DateTime, Local, Utc};

use crate::SpeedWidgetAction;
use crate::commands::common::{
    HR_STALE_THRESHOLD_S, WATCHDOG_STALE_THRESHOLD_S, highlight_config, zone_hold_config_path,
};
use crate::goals;
use crate::store;

/// Dispatch a `tm speed-widget` sub-action (задача 029). Read/write config
/// only — no BLE, same constraint as `zone`/`status`/`widget`.
pub(crate) fn run_speed_widget(action: Option<SpeedWidgetAction>) -> Result<()> {
    match action {
        None => {
            let enabled = goals::load_show_speed();
            // `on`/`off` mirrors the config `show_speed` flag → cyan (задача 057).
            println!(
                "Speed widget: {}",
                highlight_config(if enabled { "on" } else { "off" })
            );
            Ok(())
        }
        Some(SpeedWidgetAction::On) => set_show_speed(true),
        Some(SpeedWidgetAction::Off) => set_show_speed(false),
    }
}

pub(crate) fn set_show_speed(enabled: bool) -> Result<()> {
    let path = zone_hold_config_path()?;
    goals::upsert_top_level_key(&path, "show_speed", if enabled { "true" } else { "false" })?;
    println!(
        "Speed widget {}.",
        highlight_config(if enabled { "enabled" } else { "disabled" })
    );
    Ok(())
}
/// Emit one TSV line for the status-bar widget, or nothing at all when the
/// treadmill is not on/connected (so the widget hides). Read-only, no BLE —
/// mirrors `run_status`'s constraint. See docs/tasks/009.
///
/// The line is tab-separated with 12 fields (задача 029 extension):
/// `state \t workout_count \t cur_walking_s \t cur_steps \t cur_distance_m \t
/// day_walking_s \t day_steps \t day_distance_m \t hr_bpm \t hr_battery_pct \t
/// hr_zone \t speed_kmh`.
/// - `state` — `walking | away | paused | unknown`.
/// - `workout_count` — number of TODAY's *merged* workouts (reflects the
///   configured `workout_gap_minutes`), so the widget can pick a single- vs
///   multi-workout layout.
/// - `cur_*` — the current (latest) workout's aggregates (sum of its segments).
/// - `day_*` — today's `daily_stats` totals (credited walking only, so already
///   free of step-away/pauses). `cur_* ≤ day_*` by construction.
/// - `hr_bpm` — live bpm from `daemon_status`, or **empty** when no sensor is
///   worn or its reading has gone stale (same freshness gate as the rest of
///   this snapshot). The field is always present (stable field count); an
///   empty value is the signal to hide the heart glyph.
/// - `hr_battery_pct` — the sensor's last-read battery level (задача 026), or
///   **empty** when not (yet) read or no sensor connected. Always the raw
///   percentage — presentation (e.g. only showing a low-battery glyph below a
///   threshold) is the consumer's job, same split as everything else here.
/// - `hr_zone` — `below | in | above` (задача 027), or **empty** unless Zone
///   Hold is actually engaged (`zone_hold_active` + `Hold` phase) in the
///   current `walking` state — see docs/tasks/027 §Индикация зоны. Empty is
///   the signal for the consumer to colour the heart glyph neutrally.
/// - `speed_kmh` — live belt speed (задача 029), formatted as e.g. `3.1kmh`/
///   `3kmh`, or **empty** when the `show_speed` config toggle (`tm
///   speed-widget on/off`) is off, the reading is stale, or the belt is
///   stopped (`0`).
pub(crate) fn run_widget() -> Result<()> {
    let store = store::Store::open()?;

    // Visibility gate: a `daemon_status` row that is `connected` and whose
    // heartbeat (`updated_at`) is fresh. The daemon touches `updated_at` every
    // idle tick (≤30s) and every telemetry sample (~1s), so a stale row means
    // the daemon is gone or hung — hide rather than show frozen data. This is
    // why no `launchctl`/pid probe is needed on the hot 2s poll path.
    let status = match store.daemon_status()? {
        Some(status) if status.connected && !widget_status_stale(&status) => status,
        _ => return Ok(()),
    };

    let state = widget_state(status.presence_state.as_deref());
    let gap_minutes = goals::load_workout_gap_minutes();

    // Current (latest) workout: `walking_time_s` is the *credited* walking time —
    // the presence filter has already excluded step-away and paused stretches
    // (the `36m27s`, not the `raw 41m42s`, that `stats` prints). It auto-freezes
    // when not walking, since nothing is credited then.
    //
    // Freshness gate: only treat the newest workout as the *current* one while a
    // step now would still merge into it — i.e. its last activity ended ≤
    // `gap_minutes` ago (`workout_is_live`). Otherwise `latest` is a *finished*
    // workout from before the gap; showing it as "current" is how reconnecting
    // after a long pause surfaced a stale 9-step workout as if in progress. When
    // filtered out, `cur_* = 0` (no live workout) and the day context falls back
    // to today.
    let latest = store
        .latest_workout(gap_minutes)?
        .filter(|w| workout_is_live(w, gap_minutes, Utc::now()));
    let (cur_walking_s, cur_steps, cur_distance_m) = match &latest {
        Some(workout) => (workout.walking_time_s, workout.steps, workout.distance_m),
        None => (0, 0, 0),
    };

    // The widget's "day" context follows the CURRENT workout's START date, not
    // the wall-clock calendar day — so a workout that crosses midnight keeps its
    // start-day context (count + totals) instead of the widget resetting to zero
    // at 00:00 mid-walk. Falls back to today when there is no workout yet. Day
    // totals are the sum of that day's workouts (by start-date), so the crossing
    // workout is counted whole; on a normal (non-midnight) day this equals the
    // calendar `daily_stats`. `cur_* ≤ day_*` still holds (the current workout is
    // one of the reference day's workouts). `tm stats` daily lines stay strictly
    // calendar — this start-date view is widget-only, for live-workout continuity.
    let reference_day = latest
        .as_ref()
        .map(|w| w.date.clone())
        .unwrap_or_else(|| Local::now().format("%Y-%m-%d").to_string());
    let workouts = store.workouts_for(&reference_day, gap_minutes)?;
    let workout_count = workouts.len();
    let day_walking_s: i64 = workouts.iter().map(|w| w.walking_time_s).sum();
    let day_steps: i64 = workouts.iter().map(|w| w.steps).sum();
    let day_distance_m: i64 = workouts.iter().map(|w| w.distance_m).sum();

    let hr_bpm = widget_hr_field(&status);
    let hr_battery_pct = status
        .hr_battery_pct
        .map(|pct| pct.to_string())
        .unwrap_or_default();
    let hr_zone = widget_hr_zone_field(&status, state);
    let speed_kmh = widget_speed_field(&status);

    println!(
        "{}",
        WidgetLine {
            state,
            workout_count,
            cur_walking_s,
            cur_steps,
            cur_distance_m,
            day_walking_s,
            day_steps,
            day_distance_m,
            hr_bpm: &hr_bpm,
            hr_battery_pct: &hr_battery_pct,
            hr_zone: &hr_zone,
            speed_kmh: &speed_kmh,
        }
        .to_tsv()
    );
    Ok(())
}

/// Pure TSV payload for the widget contract (12 fields). Used by [`run_widget`]
/// and golden-tested so the field count/order cannot drift silently.
struct WidgetLine<'a> {
    state: &'a str,
    workout_count: usize,
    cur_walking_s: i64,
    cur_steps: i64,
    cur_distance_m: i64,
    day_walking_s: i64,
    day_steps: i64,
    day_distance_m: i64,
    hr_bpm: &'a str,
    hr_battery_pct: &'a str,
    hr_zone: &'a str,
    speed_kmh: &'a str,
}

impl WidgetLine<'_> {
    /// Tab-separated line matching the historical `println!` field order bit-for-bit.
    fn to_tsv(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.state,
            self.workout_count,
            self.cur_walking_s,
            self.cur_steps,
            self.cur_distance_m,
            self.day_walking_s,
            self.day_steps,
            self.day_distance_m,
            self.hr_bpm,
            self.hr_battery_pct,
            self.hr_zone,
            self.speed_kmh,
        )
    }
}

/// The widget's 9th field: live bpm as a plain string, or empty when the
/// sensor isn't worn (`hr_connected` is cleared on both link loss and skin
/// contact loss — задачи 025/033) or its last reading is stale, so a hung
/// daemon can't leave a frozen bpm showing forever.
pub(crate) fn widget_hr_field(status: &store::DaemonStatus) -> String {
    if !status.hr_connected {
        return String::new();
    }
    match (status.last_bpm, status.last_bpm_ts) {
        (Some(bpm), Some(ts_ms)) => {
            let age_s = (Utc::now().timestamp_millis() - ts_ms) / 1000;
            if age_s <= HR_STALE_THRESHOLD_S {
                bpm.to_string()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// The widget's 11th field: `below | in | above`, or empty (задача 027). Per
/// the task doc's operator decision (§Индикация зоны), the heart glyph is only
/// coloured by zone while Zone Hold is *actually* driving corrections in the
/// current `walking` state — everywhere else (paused/away/unknown, disabled,
/// or a `ramp`/`frozen`/`grace` phase that isn't classifying live bpm yet)
/// this stays empty and the consumer keeps the neutral colour.
pub(crate) fn widget_hr_zone_field(status: &store::DaemonStatus, widget_state: &str) -> String {
    if widget_state != "walking" || !status.zone_hold_active {
        return String::new();
    }
    status.zone_hold_position.clone().unwrap_or_default()
}

/// The widget's 12th field: live belt speed as a formatted string (задача
/// 029), or empty when the `show_speed` config toggle is off, the reading is
/// stale (same freshness threshold as [`widget_hr_field`] — задача 043), or
/// the belt is stopped (`0` km/h — not worth showing, that's the common idle
/// state).
pub(crate) fn widget_speed_field(status: &store::DaemonStatus) -> String {
    if !goals::load_show_speed() {
        return String::new();
    }
    widget_speed_value(status, Utc::now().timestamp_millis())
}

/// Pure half of [`widget_speed_field`] (config toggle already applied): age vs
/// [`HR_STALE_THRESHOLD_S`] and zero-speed blanking. `now_ms` is injected so
/// unit tests do not need a live config or wall clock.
pub(crate) fn widget_speed_value(status: &store::DaemonStatus, now_ms: i64) -> String {
    match (status.last_speed_kmh, status.last_speed_ts) {
        (Some(kmh), Some(ts_ms)) => {
            let age_s = (now_ms - ts_ms) / 1000;
            if age_s <= HR_STALE_THRESHOLD_S && kmh > 0.0 {
                format_speed_kmh(kmh)
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// Format a belt speed for display (задача 029): rounded half-up to one
/// decimal place, dropping the `.0` when the rounded value is a whole number
/// (`3kmh`, not `3.0kmh`).
pub(crate) fn format_speed_kmh(kmh: f64) -> String {
    let rounded = (kmh * 10.0).round() / 10.0;
    if (rounded.fract()).abs() < f64::EPSILON {
        format!("{}kmh", rounded as i64)
    } else {
        format!("{rounded:.1}kmh")
    }
}

/// Is the newest merged workout still the *current* (live) one at `now`? True
/// only while a step now would merge into it — its last activity ended no more
/// than `gap_minutes` ago. Mirrors `merge_segments`' inclusive gap boundary, so
/// the widget stops showing a finished workout as "current" exactly when a fresh
/// step would open a new one (e.g. after reconnecting past a long pause). An
/// unparseable `ended_at` is an anomaly (we always write RFC3339) → treat as not
/// live, so a corrupt row never masquerades as an in-progress workout. `now` is
/// injected so the boundary is unit-testable.
pub(crate) fn workout_is_live(
    workout: &store::Workout,
    gap_minutes: i64,
    now: DateTime<Utc>,
) -> bool {
    match DateTime::parse_from_rfc3339(&workout.ended_at) {
        Ok(ended_at) => {
            now - ended_at.with_timezone(&Utc) <= chrono::Duration::minutes(gap_minutes)
        }
        Err(err) => {
            tracing::warn!(%err, ended_at = %workout.ended_at, "widget: unparseable workout ended_at, treating as not live");
            false
        }
    }
}

/// Is the daemon heartbeat too old to trust? An unparseable timestamp counts as
/// stale (hide) — a malformed row is not evidence the treadmill is on.
pub(crate) fn widget_status_stale(status: &store::DaemonStatus) -> bool {
    match DateTime::parse_from_rfc3339(&status.updated_at) {
        Ok(updated_at) => {
            (Utc::now() - updated_at.with_timezone(&Utc)).num_seconds() > WATCHDOG_STALE_THRESHOLD_S
        }
        Err(err) => {
            tracing::warn!(%err, updated_at = %status.updated_at, "widget: unparseable updated_at, hiding widget");
            true
        }
    }
}

/// Map the persisted presence label to the widget's compact state token. The
/// shell presentation layer keys its icon/colour off this string, so the set is
/// a stable contract: `walking | away | paused | unknown`.
pub(crate) fn widget_state(presence_state: Option<&str>) -> &'static str {
    match presence_state {
        Some("Walking") => "walking",
        Some("AwayWhileRunning") => "away",
        Some("Paused") => "paused",
        Some("Unknown") | None => "unknown",
        Some(other) => {
            // Edge case: schema drift or a writer that skipped `PresenceState::wire`
            // (задача 047). Log once per call path is fine — widget polls every 2s.
            tracing::warn!(
                value = other,
                "widget: unrecognised presence_state — treating as unknown"
            );
            "unknown"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::common::{HR_STALE_THRESHOLD_S, WATCHDOG_STALE_THRESHOLD_S};

    #[test]
    fn widget_state_maps_every_presence_label() {
        assert_eq!(widget_state(Some("Walking")), "walking");
        assert_eq!(widget_state(Some("AwayWhileRunning")), "away");
        assert_eq!(widget_state(Some("Paused")), "paused");
        assert_eq!(widget_state(Some("Unknown")), "unknown");
        assert_eq!(widget_state(None), "unknown");
        // An unrecognised label degrades to `unknown` rather than leaking through.
        assert_eq!(widget_state(Some("Bogus")), "unknown");
    }

    #[test]
    fn format_speed_kmh_rounds_half_up_and_drops_trailing_zero() {
        assert_eq!(format_speed_kmh(3.12), "3.1kmh");
        assert_eq!(format_speed_kmh(3.16), "3.2kmh");
        assert_eq!(format_speed_kmh(3.0), "3kmh");
        assert_eq!(format_speed_kmh(2.96), "3kmh");
        assert_eq!(format_speed_kmh(0.04), "0kmh");
    }

    /// A status with a bpm reading `age_s` seconds old (задача 033).
    fn status_with_bpm(hr_connected: bool, age_s: i64) -> store::DaemonStatus {
        store::DaemonStatus {
            hr_connected,
            last_bpm: Some(111),
            last_bpm_ts: Some(Utc::now().timestamp_millis() - age_s * 1000),
            ..Default::default()
        }
    }

    #[test]
    fn widget_hr_field_shows_a_fresh_bpm_from_a_worn_sensor() {
        assert_eq!(widget_hr_field(&status_with_bpm(true, 1)), "111");
        assert_eq!(
            widget_hr_field(&status_with_bpm(true, HR_STALE_THRESHOLD_S)),
            "111"
        );
    }

    /// Contact loss clears `hr_connected` in the daemon, and a hung daemon is
    /// caught by the age check — both must blank the heart glyph.
    #[test]
    fn widget_hr_field_is_empty_when_disconnected_or_stale() {
        assert_eq!(widget_hr_field(&status_with_bpm(false, 1)), "");
        assert_eq!(
            widget_hr_field(&status_with_bpm(true, HR_STALE_THRESHOLD_S + 1)),
            ""
        );
        // A bpm older than the HR threshold but younger than the watchdog
        // one is exactly the regression задача 033 fixes.
        const { assert!(HR_STALE_THRESHOLD_S < WATCHDOG_STALE_THRESHOLD_S) };
        assert_eq!(widget_hr_field(&status_with_bpm(true, 60)), "");
    }

    fn status_with_speed(kmh: f64, age_s: i64) -> store::DaemonStatus {
        let now = Utc::now().timestamp_millis();
        store::DaemonStatus {
            last_speed_kmh: Some(kmh),
            last_speed_ts: Some(now - age_s * 1000),
            ..Default::default()
        }
    }

    /// Belt speed uses the same 15s freshness as HR (задача 043), not the
    /// 120s watchdog threshold that previously left frozen kmh on screen.
    #[test]
    fn widget_speed_value_uses_hr_stale_threshold() {
        let now = Utc::now().timestamp_millis();
        assert_eq!(
            widget_speed_value(&status_with_speed(3.2, 1), now),
            "3.2kmh"
        );
        assert_eq!(
            widget_speed_value(&status_with_speed(3.0, HR_STALE_THRESHOLD_S), now),
            "3kmh"
        );
        assert_eq!(
            widget_speed_value(&status_with_speed(3.2, HR_STALE_THRESHOLD_S + 1), now),
            ""
        );
        // Zero / stopped belt is blank even when fresh.
        assert_eq!(widget_speed_value(&status_with_speed(0.0, 1), now), "");
    }

    fn workout_ending_at(ended_at: &str) -> store::Workout {
        store::Workout {
            ended_at: ended_at.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn workout_is_live_tracks_the_merge_gap_boundary() {
        let now = DateTime::parse_from_rfc3339("2026-07-05T18:40:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let gap = 15;

        // Ended just now / within the gap → still the current workout.
        assert!(workout_is_live(
            &workout_ending_at("2026-07-05T18:40:00Z"),
            gap,
            now
        ));
        assert!(workout_is_live(
            &workout_ending_at("2026-07-05T18:30:00Z"),
            gap,
            now
        )); // 10m ago
        // Exactly on the (inclusive) boundary → still live, mirroring merge_segments.
        assert!(workout_is_live(
            &workout_ending_at("2026-07-05T18:25:00Z"),
            gap,
            now
        )); // 15m ago
        // Past the gap → finished; the widget must not show it as current. This is
        // the reconnect-after-long-pause case that surfaced the stale workout.
        assert!(!workout_is_live(
            &workout_ending_at("2026-07-05T18:24:59Z"),
            gap,
            now
        ));
        assert!(!workout_is_live(
            &workout_ending_at("2026-07-05T18:00:00Z"),
            gap,
            now
        )); // 40m ago
        // A corrupt timestamp is never treated as a live workout.
        assert!(!workout_is_live(
            &workout_ending_at("not-a-timestamp"),
            gap,
            now
        ));
    }

    #[test]
    fn widget_line_has_twelve_tsv_fields_in_documented_order() {
        // Field order contract for tmux consumers (задача 029):
        // state, workout_count, cur_walking_s, cur_steps, cur_distance_m,
        // day_walking_s, day_steps, day_distance_m, hr_bpm, hr_battery_pct,
        // hr_zone, speed_kmh
        let line = WidgetLine {
            state: "walking",
            workout_count: 2,
            cur_walking_s: 600,
            cur_steps: 1200,
            cur_distance_m: 800,
            day_walking_s: 3600,
            day_steps: 8000,
            day_distance_m: 5000,
            hr_bpm: "142",
            hr_battery_pct: "87",
            hr_zone: "in",
            speed_kmh: "3.2kmh",
        }
        .to_tsv();
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 12);
        assert_eq!(fields[0], "walking");
        assert_eq!(fields[1], "2");
        assert_eq!(fields[2], "600");
        assert_eq!(fields[3], "1200");
        assert_eq!(fields[4], "800");
        assert_eq!(fields[5], "3600");
        assert_eq!(fields[6], "8000");
        assert_eq!(fields[7], "5000");
        assert_eq!(fields[8], "142");
        assert_eq!(fields[9], "87");
        assert_eq!(fields[10], "in");
        assert_eq!(fields[11], "3.2kmh");
    }

    #[test]
    fn widget_line_keeps_empty_optional_fields_for_stable_count() {
        let line = WidgetLine {
            state: "paused",
            workout_count: 1,
            cur_walking_s: 0,
            cur_steps: 0,
            cur_distance_m: 0,
            day_walking_s: 100,
            day_steps: 200,
            day_distance_m: 50,
            hr_bpm: "",
            hr_battery_pct: "",
            hr_zone: "",
            speed_kmh: "",
        }
        .to_tsv();
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 12);
        assert_eq!(fields[8], "");
        assert_eq!(fields[9], "");
        assert_eq!(fields[10], "");
        assert_eq!(fields[11], "");
    }
}
