//! `stats` and `default-speed` CLI commands.

use std::io::IsTerminal;

use anyhow::Result;
use chrono::{DateTime, Local, TimeZone, Utc};

use crate::commands::common::{fmt_duration, format_local_time};
use crate::default_speed;
use crate::goals;
use crate::store;

/// Print today's accumulated stats, or every recorded day with `--all` —
/// each followed by its per-workout breakdown (see docs/tasks/006, задача C).
pub(crate) fn run_stats(all: bool) -> Result<()> {
    let store = store::Store::open()?;
    // Read-time workout grouping threshold (задача 014); daily totals below are
    // unaffected (strictly calendar, straight from `daily_stats`).
    let gap_minutes = goals::load_workout_gap_minutes();
    if all {
        for day in store.all_stats()? {
            print_day(&store, &day, gap_minutes)?;
        }
    } else {
        print_day(&store, &store.today_stats()?, gap_minutes)?;
    }
    Ok(())
}

pub(crate) fn print_day(
    store: &store::Store,
    day: &store::DailyStats,
    gap_minutes: i64,
) -> Result<()> {
    // The day header stays on filtered totals only: raw is shown per workout,
    // where the [started_at, ended_at] window makes reconstruction exact. A
    // day-level raw would have to sum workout spans, but `daily_stats` can
    // credit activity that never landed under this day's workouts (see the
    // midnight edge case in `store`), so that sum would silently understate.
    let hr = day_hr_summary(store, &day.date)
        .map(fmt_hr_summary)
        .unwrap_or_default();
    println!(
        "{}: {} steps, {:.2} km, {} walking{hr}",
        day.date,
        day.steps,
        day.distance_m as f64 / 1000.0,
        fmt_duration(day.walking_time_s),
    );
    for (i, workout) in store
        .workouts_for(&day.date, gap_minutes)?
        .iter()
        .enumerate()
    {
        print_workout_line(store, i + 1, workout, "");
    }
    Ok(())
}

/// One `workouts` row, indented under its day/status header. `marker` is
/// appended verbatim (e.g. `" [in progress]"`) — empty for `stats`, which has
/// no notion of "currently running".
///
/// The start→end range is spaced out with an arrow so the two clock times read
/// as distinct endpoints, not one run-on token. A dim `(raw …)` hint after the
/// distance and after the walking time shows the pre-filter figure — belt
/// distance/time including the moments the operator stepped off while it kept
/// spinning (see `store::raw_distance_m`); omitted when there's nothing extra.
pub(crate) fn print_workout_line(
    store: &store::Store,
    num: usize,
    workout: &store::Workout,
    marker: &str,
) {
    let (raw_dist, raw_time) = workout_raw(store, workout);
    let dist_hint = raw_hint(
        raw_dist.is_some_and(|d| d > workout.distance_m),
        &format!("{:.2}", raw_dist.unwrap_or(0) as f64 / 1000.0),
    );
    let time_hint = raw_hint(
        raw_time.is_some_and(|t| t > workout.walking_time_s),
        &fmt_duration(raw_time.unwrap_or(0)),
    );
    let hr = store
        .hr_summary_for(&workout.started_at, &workout.ended_at)
        .ok()
        .flatten()
        .map(fmt_hr_summary)
        .unwrap_or_default();
    // `num` is the workout's 1-based position within its day, not `workout.id`
    // (which is its first segment's id — not sequential after задача 014/015).
    println!(
        "  #{num}  {} \u{2192} {}   {} steps, {:.2} km{dist_hint}, {}{time_hint}{hr}{marker}",
        format_local_time(&workout.started_at),
        format_local_time(&workout.ended_at),
        workout.steps,
        workout.distance_m as f64 / 1000.0,
        fmt_duration(workout.walking_time_s),
        dist_hint = dist_hint,
        time_hint = time_hint,
    );
}

/// `♥ avg/max` suffix for a heart-rate summary (задача 025), agreed with the
/// operator as the default: trimmed-mean average, p95 as a spike-robust peak.
/// A leading three spaces separates it from the preceding field like the other
/// space-joined segments on these lines.
pub(crate) fn fmt_hr_summary(hr: store::HrSummary) -> String {
    format!("   \u{2665} {}/{}", hr.avg_bpm, hr.max_bpm)
}

/// Heart-rate summary for a whole calendar day (local time), or `None` when
/// the date can't be parsed or too few `hr_samples` fall in the window —
/// omitted from the day header rather than shown as a misleading zero.
pub(crate) fn day_hr_summary(store: &store::Store, date: &str) -> Option<store::HrSummary> {
    let (start, end) = day_bounds_rfc3339(date)?;
    store.hr_summary_for(&start, &end).ok().flatten()
}

/// `[local midnight, next local midnight)` for a `YYYY-MM-DD` date, as RFC3339
/// UTC bounds for `hr_summary_for`. `None` on an unparseable date or a
/// (practically impossible) nonexistent local midnight.
pub(crate) fn day_bounds_rfc3339(date: &str) -> Option<(String, String)> {
    let naive = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let midnight = naive.and_hms_opt(0, 0, 0)?;
    let start_local = match Local.from_local_datetime(&midnight) {
        chrono::LocalResult::Single(dt) => dt,
        chrono::LocalResult::Ambiguous(dt, _) => dt,
        chrono::LocalResult::None => return None,
    };
    let end_local = start_local + chrono::Duration::days(1);
    Some((
        start_local.with_timezone(&Utc).to_rfc3339(),
        end_local.with_timezone(&Utc).to_rfc3339(),
    ))
}

/// Raw (pre-filter) distance (meters) and time (seconds) for a workout, or
/// `None` for either when the reconstruction can't be trusted — no samples in
/// the window, or a figure that came back *below* the filtered total (a sign
/// of missing samples, since raw must be a superset of walking). The caller
/// then omits that hint rather than showing a misleading value.
pub(crate) fn workout_raw(
    store: &store::Store,
    workout: &store::Workout,
) -> (Option<i64>, Option<i64>) {
    let dist = store
        .raw_distance_m(&workout.started_at, &workout.ended_at)
        .ok()
        .flatten()
        .filter(|&d| d >= workout.distance_m);
    let time =
        raw_span_s(&workout.started_at, &workout.ended_at).filter(|&t| t >= workout.walking_time_s);
    (dist, time)
}

/// Wall-clock span of a workout in seconds — its raw time, before presence
/// filtering carves out the belt-spinning-but-not-walking gaps. `None` on an
/// unparseable or negative span.
pub(crate) fn raw_span_s(started_at: &str, ended_at: &str) -> Option<i64> {
    let start = DateTime::parse_from_rfc3339(started_at).ok()?;
    let end = DateTime::parse_from_rfc3339(ended_at).ok()?;
    let secs = (end - start).num_seconds();
    (secs >= 0).then_some(secs)
}
/// A dim `" (raw <value>)"` hint when `show` is true, else empty. Dimming uses
/// the ANSI faint code, but only on a TTY — piping `tm stats` into a file or
/// `grep` gets clean text with no escape sequences.
pub(crate) fn raw_hint(show: bool, value: &str) -> String {
    if !show {
        return String::new();
    }
    let hint = format!(" (raw {value})");
    if std::io::stdout().is_terminal() {
        format!("\x1b[2m{hint}\x1b[0m")
    } else {
        hint
    }
}
/// Print the computed default belt speed the daemon would apply at the next
/// workout start, and which workout it was derived from (задача 016). Read-only.
pub(crate) fn run_default_speed() -> Result<()> {
    let store = store::Store::open()?;
    let gap_minutes = goals::load_workout_gap_minutes();
    match default_speed::compute_default_speed(&store, gap_minutes)? {
        Some(default) => {
            println!("computed default speed: {:.1} km/h", default.kmh);
            println!(
                "  from workout on {} ({} → {}, {} walking)",
                default.source.date,
                format_local_time(&default.source.started_at),
                format_local_time(&default.source.ended_at),
                fmt_duration(default.source.walking_time_s),
            );
            println!(
                "  {} walking samples, {} kept after 15% top/bottom trim",
                default.walking_samples, default.kept_samples,
            );
        }
        None => println!(
            "no qualifying workout yet (need one with \u{2265}30m of credited walking) — \
             the belt would stay at its device default speed"
        ),
    }
    Ok(())
}
