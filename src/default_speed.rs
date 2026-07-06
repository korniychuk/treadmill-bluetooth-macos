//! Computed default belt speed for the start of a workout (задача 016).
//!
//! The treadmill resets the belt to its factory crawl (~0.5 km/h) on every
//! power-up/reset, so the operator re-dials their walking pace by hand each
//! time. Задача 012 already restores the pre-pause speed for a pause *within* a
//! live session; this module supplies the value the daemon applies at a fresh
//! *start*, where there is no pre-pause speed to restore.
//!
//! The value is not configured — it is the operator's own recent cruising pace,
//! derived from the most recent *qualifying* workout (≥ 30 min of credited
//! walking). Its belt-speed samples are floored (drop ramp/idle/crawl), sorted,
//! trimmed 15% off each end, and averaged — the stable middle 70%, free of the
//! warm-up ramp and any brief bursts. The trigger/guards live in `crate::daemon`.

use anyhow::Result;
use tracing::warn;

use crate::store::{Store, Workout, merge_segments};

/// Minimum credited walking time for a workout to qualify as the reference for
/// the computed default speed. Below this it is too short to represent the
/// operator's typical cruising pace. Credited (`walking_time_s`, presence-
/// filtered) time, not wall-clock — a mostly-paused span must not qualify.
const MIN_QUALIFYING_WALKING_S: i64 = 30 * 60;

/// Fraction trimmed from EACH end (top and bottom) of the sorted walking-speed
/// samples before averaging — 15% each, leaving the middle 70%.
const TRIM_FRACTION: f32 = 0.15;

/// Speeds at/below this (km/h) are ramp/idle/crawl, not real walking, and are
/// excluded before trimming. Mirrors `daemon::SPEED_CRUISE_FLOOR_KMH` (the belt
/// minimum sits around 0.5).
const WALKING_FLOOR_KMH: f32 = 0.8;

/// The computed default speed plus the workout it came from and how many
/// samples backed it — the daemon uses `kmh`, the `default-speed` CLI prints
/// the rest as a diagnostic.
#[derive(Debug, Clone)]
pub struct DefaultSpeed {
    pub kmh: f32,
    pub source: Workout,
    pub walking_samples: usize,
    pub kept_samples: usize,
}

/// Outcome of [`trimmed_mean_speed`]: the mean plus how many samples survived
/// the floor and the trim.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrimmedSpeed {
    pub mean_kmh: f32,
    pub walking_samples: usize,
    pub kept_samples: usize,
}

/// The computed default belt speed the daemon would apply at the next workout
/// start, or `None` when no qualifying workout (≥ 30 min credited walking with
/// usable speed samples) exists yet. Searches the whole history most-recent-
/// first, merging segments into workouts at `gap_minutes` (задача 014).
pub fn compute_default_speed(store: &Store, gap_minutes: i64) -> Result<Option<DefaultSpeed>> {
    let workouts = merge_segments(&store.all_segments_asc()?, gap_minutes);
    for workout in workouts.iter().rev() {
        if workout.walking_time_s < MIN_QUALIFYING_WALKING_S {
            continue;
        }
        let speeds = store.walking_speeds_in_window(&workout.started_at, &workout.ended_at)?;
        match trimmed_mean_speed(&speeds, WALKING_FLOOR_KMH, TRIM_FRACTION) {
            Some(trimmed) => {
                return Ok(Some(DefaultSpeed {
                    kmh: trimmed.mean_kmh,
                    source: workout.clone(),
                    walking_samples: trimmed.walking_samples,
                    kept_samples: trimmed.kept_samples,
                }));
            }
            // Long enough by time but no usable speed samples (e.g. a gap in
            // raw_samples) — anomalous; skip to an older qualifying workout.
            None => warn!(id = workout.id, "qualifying workout has no usable speed samples — trying an older one"),
        }
    }
    Ok(None)
}

/// Arithmetic mean of the middle `1 - 2·trim_fraction` of the walking speeds:
/// keep only samples `>= floor_kmh`, sort, drop `floor(n·trim_fraction)` from
/// each end, average the rest. `None` when nothing clears the floor. If the set
/// is too small for the trim to leave anything, averages the whole floored set
/// (fallback) rather than returning `None`. Rounded to 0.1 km/h. Pure and
/// unit-tested — the trigger/BLE write live in `crate::daemon`.
pub fn trimmed_mean_speed(speeds: &[f32], floor_kmh: f32, trim_fraction: f32) -> Option<TrimmedSpeed> {
    let mut walking: Vec<f32> = speeds.iter().copied().filter(|&s| s >= floor_kmh).collect();
    if walking.is_empty() {
        return None;
    }
    walking.sort_by(|a, b| a.partial_cmp(b).expect("belt speeds are never NaN"));

    let n = walking.len();
    let trim = (n as f32 * trim_fraction).floor() as usize;
    // Trimming both ends must leave at least one sample; otherwise average all.
    let kept = if 2 * trim < n { &walking[trim..n - trim] } else { &walking[..] };

    let mean = kept.iter().sum::<f32>() / kept.len() as f32;
    Some(TrimmedSpeed { mean_kmh: round_to_tenth(mean), walking_samples: n, kept_samples: kept.len() })
}

/// Round to one decimal place (0.1 km/h) — walking targets are naturally coarse
/// and the belt accepts 0.01 km/h steps, so a clean tenth avoids odd figures.
fn round_to_tenth(kmh: f32) -> f32 {
    (kmh * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use chrono::{Duration, TimeZone, Utc};

    use crate::ftms::TreadmillData;

    use super::*;

    fn memory_store() -> Store {
        unsafe {
            std::env::set_var("TZ", "UTC");
        }
        Store::open_at(Path::new(":memory:")).expect("open in-memory store")
    }

    #[test]
    fn trimmed_mean_ignores_floor_extremes_and_bursts() {
        // 90 steady 2.5 samples, 5 crawl (below floor) and 5 fast bursts. The
        // floor drops the crawl; the top 15% trim drops the bursts → 2.5.
        let mut speeds = vec![0.5; 5];
        speeds.extend(std::iter::repeat_n(2.5, 90));
        speeds.extend(std::iter::repeat_n(6.0, 5));

        let result = trimmed_mean_speed(&speeds, WALKING_FLOOR_KMH, TRIM_FRACTION).expect("some");
        assert!((result.mean_kmh - 2.5).abs() < 0.01, "got {}", result.mean_kmh);
        assert_eq!(result.walking_samples, 95, "5 crawl samples floored out");
    }

    #[test]
    fn trimmed_mean_none_when_all_below_floor() {
        assert_eq!(trimmed_mean_speed(&[0.3, 0.5, 0.7], WALKING_FLOOR_KMH, TRIM_FRACTION), None);
        assert_eq!(trimmed_mean_speed(&[], WALKING_FLOOR_KMH, TRIM_FRACTION), None);
    }

    #[test]
    fn trimmed_mean_small_set_falls_back_to_plain_mean() {
        // Two samples: 15% trim rounds to 0 per side, so both are kept.
        let result = trimmed_mean_speed(&[2.0, 3.0], WALKING_FLOOR_KMH, TRIM_FRACTION).expect("some");
        assert_eq!(result.mean_kmh, 2.5);
        assert_eq!(result.kept_samples, 2);
    }

    #[test]
    fn trimmed_mean_rounds_to_tenth() {
        // Mean 2.5333… → 2.5.
        let result = trimmed_mean_speed(&[2.4, 2.5, 2.7], WALKING_FLOOR_KMH, TRIM_FRACTION).expect("some");
        assert_eq!(result.mean_kmh, 2.5);
    }

    /// Insert a raw sample carrying just a belt speed at `ts_ms` (the compute
    /// path only reads `speed_centikmh`), via the public store API.
    fn insert_speed(store: &Store, ts_ms: i64, kmh: f32) {
        let sample = TreadmillData { speed_kmh: Some(kmh), ..Default::default() };
        store.insert_raw_sample(1, ts_ms, &sample, &[0]).unwrap();
    }

    #[test]
    fn compute_picks_recent_qualifying_workout() {
        let mut store = memory_store();
        store.start_session().unwrap(); // session id 1, satisfies raw_samples FK
        // A qualifying workout: 40 min of credited walking, one open segment.
        let start = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();
        let end = start + Duration::minutes(40);
        let id = store.credit_activity(100, 200, 1800, start, None).unwrap();
        store.credit_activity(100, 200, 1000, end, Some(id)).unwrap();

        // Speed samples across the window: mostly 2.5, a few crawl and burst.
        for i in 0..100 {
            let ts = (start + Duration::seconds(i * 20)).timestamp_millis();
            let kmh = if i < 5 { 0.5 } else if i >= 95 { 6.0 } else { 2.5 };
            insert_speed(&store, ts, kmh);
        }

        let default = compute_default_speed(&store, 15).unwrap().expect("a default");
        assert!((default.kmh - 2.5).abs() < 0.01, "got {}", default.kmh);
        assert_eq!(default.source.date, "2026-07-05");
    }

    #[test]
    fn compute_none_when_no_workout_reaches_thirty_minutes() {
        let mut store = memory_store();
        store.start_session().unwrap(); // session id 1, satisfies raw_samples FK
        let start = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();
        // Only 10 min of walking — under the 30-min bar.
        store.credit_activity(100, 200, 600, start, None).unwrap();
        insert_speed(&store, start.timestamp_millis(), 2.5);

        assert!(compute_default_speed(&store, 15).unwrap().is_none());
    }
}
