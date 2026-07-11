//! `recompute-segments` — rebuild `activity_segments` from `raw_samples`
//! (задача 015).
//!
//! `raw_samples` is the ground truth: every telemetry frame the daemon ever
//! saw, with the raw cumulative device counters. The displayed workout grain
//! (`activity_segments`) is *derived* from it by the live presence + credit
//! engine. The 014 migration seeded segments coarsely (one per aggregated
//! legacy `workouts` row), collapsing each workout's internal pause/away
//! structure. This command replays the **same** engine (`crate::activity`)
//! over the raw frames to reconstruct the true fine-grained segments, then
//! atomically replaces the table.
//!
//! Why the replay matches the live daemon exactly:
//! - It runs the identical [`ActivityAccumulator`] the daemon runs — presence,
//!   pending-credit buffering, and segment open/extend/close are one codebase.
//! - `raw_samples.ts_ms` was stamped with `Utc::now()` in the *same* daemon
//!   loop iteration that fired the live credit's `Utc::now()`, so feeding
//!   `ts_ms` back as the credit timestamp reproduces segment start/end and the
//!   local-date attribution within milliseconds.
//! - The scratch in-memory [`Store`] reuses `advance_baseline` verbatim, so the
//!   counter-reset handling (`delta_since`) is the very same code as live —
//!   nothing is forked. Its throwaway `daily_stats` writes are discarded; the
//!   real `daily_stats` is never touched (out of scope — already calendar-correct).
//!
//! Read-only over BLE (like `stats`/`status`): no adapter is opened.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{Local, TimeZone, Utc};
use tracing::{info, warn};

use crate::activity::ActivityAccumulator;
use crate::store::{RawSample, Segment, Store};

/// Rebuild `activity_segments` from `raw_samples` and print a summary.
pub fn run() -> Result<()> {
    let mut store = Store::open()?;
    let rows = store.raw_samples_ordered()?;
    if rows.is_empty() {
        println!("recompute-segments: no raw_samples recorded — nothing to rebuild.");
        return Ok(());
    }

    let outcome = replay(&rows)?;
    store.replace_activity_segments(&outcome.segments)?;

    let (first_date, last_date) = date_range(&rows);
    println!(
        "recompute-segments: replayed {frames} frames across {sessions} sessions ({first_date} → {last_date}), \
         rebuilt {segments} activity segments{resets}.",
        frames = rows.len(),
        sessions = outcome.sessions,
        segments = outcome.segments.len(),
        resets = if outcome.resets > 0 {
            format!(" (handled {} device counter resets)", outcome.resets)
        } else {
            String::new()
        },
    );
    Ok(())
}

/// The result of a replay pass over raw frames.
struct ReplayOutcome {
    segments: Vec<Segment>,
    sessions: u64,
    resets: u64,
}

/// Replay the presence + credit engine over `rows` (ordered by `ts_ms`) and
/// return the reconstructed segments. Deterministic: the scratch store is fresh
/// (ids restart at 1), so the same input yields byte-identical output — which is
/// what makes the whole command idempotent.
///
/// Session handling mirrors the live daemon reconnect exactly: a fresh
/// [`ActivityAccumulator`] per `session_id` (presence/pending/open-segment all
/// reset), while the device-counter baseline persists across sessions in the
/// scratch store's `device_baseline` — precisely as live's single persisted
/// baseline row does (never reset on reconnect).
fn replay(rows: &[RawSample]) -> Result<ReplayOutcome> {
    // Throwaway store: `advance_baseline` + `credit_activity` run against it so
    // the reset handling and segment-open/extend logic are the live code, not a
    // fork. Its `daily_stats` writes are collateral and never read.
    let mut scratch = Store::open_at(Path::new(":memory:")).context("open scratch replay store")?;

    let anchor = Instant::now();
    let mut accumulator = ActivityAccumulator::new();
    let mut current_session: Option<i64> = None;
    let mut session_first_ts = 0i64;
    let mut prev_steps: Option<i64> = None;
    let mut sessions = 0u64;
    let mut resets = 0u64;

    for row in rows {
        if current_session != Some(row.session_id) {
            // New device session: fresh presence/pending/open-segment, like the
            // live daemon on reconnect. The scratch baseline is *not* reset.
            accumulator = ActivityAccumulator::new();
            current_session = Some(row.session_id);
            session_first_ts = row.ts_ms;
            sessions += 1;
        }

        // Count counter resets for the summary/log — routine on every power
        // cycle, so a single aggregate line, not a per-reset warning. The
        // actual delta/reset math lives in `advance_baseline` below (reused).
        if let (Some(prev), Some(cur)) = (prev_steps, row.steps.map(|v| v as i64))
            && cur < prev
        {
            resets += 1;
        }
        prev_steps = row.steps.map(|v| v as i64);

        let deltas = scratch.advance_baseline(row.steps, row.distance_m, row.elapsed_s)?;

        // Presence away-threshold is measured in monotonic `Instant`; synthesize
        // one from the frame timestamp, offset within the session (presence
        // resets per session, so cross-session magnitude never matters and we
        // avoid `Instant + huge Duration`).
        let now = anchor + Duration::from_millis((row.ts_ms - session_first_ts).max(0) as u64);
        let credit_now = match Utc.timestamp_millis_opt(row.ts_ms).single() {
            Some(dt) => dt,
            None => {
                warn!(
                    ts_ms = row.ts_ms,
                    session_id = row.session_id,
                    "raw_sample ts_ms out of range — skipping frame"
                );
                continue;
            }
        };

        accumulator.observe(now, row.speed, row.steps);
        accumulator.credit(&mut scratch, credit_now, deltas)?;
    }

    if resets > 0 {
        info!(resets, "device counter resets handled during replay");
    }

    let segments = scratch.all_segments_asc()?;
    Ok(ReplayOutcome {
        segments,
        sessions,
        resets,
    })
}

/// Local start/end dates of the frame span, for the summary line.
fn date_range(rows: &[RawSample]) -> (String, String) {
    let fmt = |ts_ms: i64| {
        Utc.timestamp_millis_opt(ts_ms)
            .single()
            .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "?".to_string())
    };
    // Rows are ordered by ts_ms, so first/last bound the span.
    (
        fmt(rows.first().map(|r| r.ts_ms).unwrap_or(0)),
        fmt(rows.last().map(|r| r.ts_ms).unwrap_or(0)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a raw sample; cumulative counters as the device reports them.
    fn sample(
        session_id: i64,
        ts_ms: i64,
        speed_kmh: Option<f32>,
        steps: Option<u32>,
        distance_m: Option<u32>,
        elapsed_s: Option<u16>,
    ) -> RawSample {
        RawSample {
            session_id,
            ts_ms,
            speed: speed_kmh.and_then(crate::speed::CentiKmh::from_kmh_f32),
            distance_m,
            elapsed_s,
            steps,
        }
    }

    /// Segment step totals in chronological order — the shape most assertions want.
    fn segment_steps(segments: &[Segment]) -> Vec<i64> {
        segments.iter().map(|s| s.steps).collect()
    }

    #[test]
    fn replays_walk_pause_walk_away_walk_into_three_segments() {
        // One session, ~1 frame/s. Cumulative counters advance by 10 steps / 5 m
        // / 1 s per walking frame. The very first frame credits nothing (baseline
        // = None), matching live's first-ever contact.
        let mut rows = Vec::new();
        let base = 1_000_000_000_000i64;
        let push =
            |rows: &mut Vec<RawSample>, t: i64, speed: f32, steps: u32, dist: u32, el: u16| {
                rows.push(sample(
                    1,
                    base + t * 1000,
                    Some(speed),
                    Some(steps),
                    Some(dist),
                    Some(el),
                ));
            };

        // Walk: frames 0..=2 (t=0,1,2). Steps 10,20,30.
        push(&mut rows, 0, 2.5, 10, 5, 1);
        push(&mut rows, 1, 2.5, 20, 10, 2);
        push(&mut rows, 2, 2.5, 30, 15, 3);
        let mut t = 3i64;
        // Pause: speed 0 for a couple frames (segment 1 closes).
        push(&mut rows, t, 0.0, 30, 15, 3);
        push(&mut rows, t + 1, 0.0, 30, 15, 3);
        t += 2; // t=5
        // Walk again: frames t=5,6. Steps 40,50 (opens segment 2).
        push(&mut rows, t, 2.5, 40, 20, 4);
        push(&mut rows, t + 1, 2.5, 50, 25, 5);
        t += 2; // t=7
        // Step off: belt keeps running, steps frozen at 50. Distance/time keep
        // climbing. Away fires 10s after the last step change (t=6) → t=16.
        for i in 0..10 {
            let secs = t + i;
            push(&mut rows, secs, 2.5, 50, 30 + i as u32 * 5, (6 + i) as u16);
        }
        t += 10; // t=17
        // Back on: steps advance again (opens segment 3).
        push(&mut rows, t, 2.5, 60, 85, 17);
        push(&mut rows, t + 1, 2.5, 70, 90, 18);

        let outcome = replay(&rows).unwrap();

        // Three distinct credited-walking segments.
        assert_eq!(
            outcome.segments.len(),
            3,
            "walk / walk / walk split by pause and step-away"
        );
        // Each walking spell credited 20 steps of confirmed deltas (the first
        // frame of the whole history credited 0; the away frames credited 0).
        assert_eq!(segment_steps(&outcome.segments), vec![20, 20, 20]);
        assert_eq!(outcome.sessions, 1);

        // Discard-on-away: the belt distance accrued while stepped off (frozen
        // steps, t=7..16) must not land on any segment. Segment 2 got exactly
        // its two 5 m credited deltas, not the phantom away metres.
        assert_eq!(
            outcome.segments[1].distance_m, 10,
            "away-window distance is discarded, not credited"
        );
    }

    #[test]
    fn handles_midstream_counter_reset() {
        // A power-cycle mid-walk resets the device counters to a small value.
        // `delta_since` rebaselines from zero (delta = new when new < prev), so
        // credited steps are the sum of positive per-frame deltas, never negative.
        let base = 1_700_000_000_000i64;
        let rows = vec![
            sample(1, base, Some(2.5), Some(100), Some(50), Some(10)), // first-ever: credits 0
            sample(1, base + 1000, Some(2.5), Some(110), Some(55), Some(11)), // +10 steps
            sample(1, base + 2000, Some(2.5), Some(120), Some(60), Some(12)), // +10 steps
            // Reset: counters drop. delta = new (5 steps, 3 m, 1 s).
            sample(1, base + 3000, Some(2.5), Some(5), Some(3), Some(1)),
            sample(1, base + 4000, Some(2.5), Some(15), Some(8), Some(2)), // +10 steps
        ];

        let outcome = replay(&rows).unwrap();
        assert_eq!(outcome.resets, 1, "one reset detected");
        // Belt never stopped and steps kept changing → one continuous segment.
        assert_eq!(outcome.segments.len(), 1);
        // 10 + 10 + 5 (reset frame) + 10 = 35 credited steps.
        assert_eq!(
            outcome.segments[0].steps, 35,
            "reset rebaselines from zero, no negative/huge delta"
        );
    }

    #[test]
    fn resets_accumulator_between_sessions() {
        // Two sessions; the second reconnect gets a fresh accumulator, so its
        // walking opens a brand-new segment rather than extending the first.
        let s1 = 1_700_000_000_000i64;
        let s2 = s1 + 3_600_000; // an hour later
        let rows = vec![
            // Session 1: first frame credits 0, second credits +10 → segment A.
            sample(1, s1, Some(2.5), Some(10), Some(5), Some(1)),
            sample(1, s1 + 1000, Some(2.5), Some(20), Some(10), Some(2)),
            // Session 2: device reset to a low counter. Fresh accumulator; the
            // first frame here credits its delta (baseline carried = 20 → reset
            // → delta 5) and opens segment B.
            sample(2, s2, Some(2.5), Some(5), Some(3), Some(1)),
            sample(2, s2 + 1000, Some(2.5), Some(15), Some(8), Some(2)),
        ];

        let outcome = replay(&rows).unwrap();
        assert_eq!(outcome.sessions, 2);
        assert_eq!(
            outcome.segments.len(),
            2,
            "each session yields its own segment"
        );
    }

    #[test]
    fn replay_is_deterministic_for_idempotency() {
        // The same input replayed twice must produce byte-identical segments
        // (ids and all columns) — the foundation of the command's idempotency.
        let base = 1_700_000_000_000i64;
        let rows = vec![
            sample(1, base, Some(2.5), Some(10), Some(5), Some(1)),
            sample(1, base + 1000, Some(2.5), Some(20), Some(10), Some(2)),
            sample(1, base + 2000, Some(0.0), Some(20), Some(10), Some(2)),
            sample(1, base + 3000, Some(2.5), Some(30), Some(15), Some(3)),
        ];

        let first = replay(&rows).unwrap().segments;
        let second = replay(&rows).unwrap().segments;

        let key = |segs: &[Segment]| {
            segs.iter()
                .map(|s| {
                    (
                        s.id,
                        s.date.clone(),
                        s.started_at.clone(),
                        s.ended_at.clone(),
                        s.distance_m,
                        s.steps,
                        s.walking_time_s,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(key(&first), key(&second), "two replays are identical");
    }

    #[test]
    fn replace_activity_segments_is_idempotent() {
        // Replacing the table twice with a deterministic set leaves identical
        // rows — the store-level guarantee behind running the command twice.
        let mut store = Store::open_at(Path::new(":memory:")).unwrap();
        let base = 1_700_000_000_000i64;
        let rows = vec![
            sample(1, base, Some(2.5), Some(10), Some(5), Some(1)),
            sample(1, base + 1000, Some(2.5), Some(20), Some(10), Some(2)),
            sample(1, base + 2000, Some(2.5), Some(30), Some(15), Some(3)),
        ];
        let segments = replay(&rows).unwrap().segments;

        store.replace_activity_segments(&segments).unwrap();
        let after_first = store.all_segments_asc().unwrap();
        store.replace_activity_segments(&segments).unwrap();
        let after_second = store.all_segments_asc().unwrap();

        let key = |segs: &[Segment]| {
            segs.iter()
                .map(|s| (s.id, s.steps, s.distance_m))
                .collect::<Vec<_>>()
        };
        assert_eq!(key(&after_first), key(&after_second));
        assert_eq!(after_first.len(), 1);
    }
}
