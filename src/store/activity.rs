//! Daily stats, activity segments, workouts, baseline, and goal celebrations.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Local, Utc};
use rusqlite::{OptionalExtension, params};

use super::Store;

/// Aggregated totals for a single calendar day (local time).
#[derive(Debug, Clone, Default)]
pub struct DailyStats {
    pub date: String,
    pub distance_m: i64,
    pub steps: i64,
    pub walking_time_s: i64,
}

/// One continuous credited-walking spell (задача 014). A segment is OPEN while
/// the operator is walking and CLOSED the moment presence leaves `Walking` (a
/// pause `speed=0` or a step-away `AwayWhileRunning`); the daemon tracks the
/// open segment's id in memory and closes it on the presence transition. This
/// is the threshold-*independent* storage grain — displayed workouts are
/// derived from segments at read time by [`merge_segments`], so the workout-gap
/// can change retroactively without any rewrite.
///
/// `date` is fixed to the calendar date (local time) of `started_at` and never
/// changes, even if a credited burst extends `ended_at` past local midnight.
#[derive(Debug, Clone, Default)]
pub struct Segment {
    pub id: i64,
    pub date: String,
    pub started_at: String,
    pub ended_at: String,
    pub distance_m: i64,
    pub steps: i64,
    pub walking_time_s: i64,
}

/// One displayed workout: the merge of adjacent [`Segment`]s whose inter-segment
/// gap is ≤ the configured `workout_gap_minutes` (see [`merge_segments`]). This
/// is a read-time projection, never stored.
///
/// `date` is the start day of the first segment (workout attribution stays by
/// start-date), so on a midnight-crossing workout the day's `daily_stats` total
/// need not equal the sum of the workouts shown under it — that is intentional;
/// `daily_stats` is split strictly by calendar day, independent of workouts.
///
/// `id` is the id of the workout's first segment — stable and unique, so the
/// `status` "in progress" marker and the `#N` label stay meaningful.
#[derive(Debug, Clone, Default)]
pub struct Workout {
    pub id: i64,
    pub date: String,
    pub started_at: String,
    pub ended_at: String,
    pub distance_m: i64,
    pub steps: i64,
    pub walking_time_s: i64,
}

/// Per-sample deltas against the persisted device baseline. Not yet a
/// decision about whether they represent real walking — see module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawDeltas {
    pub steps: i64,
    pub distance_m: i64,
    pub elapsed_s: i64,
}

impl Store {
    /// Advance the persisted raw-counter baseline by one telemetry sample and
    /// return the deltas since the previous sample. Always safe to call —
    /// this does not touch `daily_stats`; the caller (daemon) decides whether
    /// the returned deltas represent confirmed walking.
    ///
    /// `raw_steps` / `raw_distance_m` / `raw_elapsed_s` are the *cumulative*
    /// device counters straight off `0x2ACD`.
    pub fn advance_baseline(
        &mut self,
        raw_steps: Option<u32>,
        raw_distance_m: Option<u32>,
        raw_elapsed_s: Option<u16>,
    ) -> Result<RawDeltas> {
        let tx = self
            .conn
            .transaction()
            .context("begin baseline transaction")?;

        let (last_steps, last_distance, last_elapsed): (Option<i64>, Option<i64>, Option<i64>) = tx
            .query_row(
                "SELECT last_steps, last_distance_m, last_elapsed_s FROM device_baseline WHERE id = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .context("read device_baseline")?
            .unwrap_or((None, None, None));

        let deltas = RawDeltas {
            steps: raw_steps
                .map(|v| delta_since(v as i64, last_steps))
                .unwrap_or(0),
            distance_m: raw_distance_m
                .map(|v| delta_since(v as i64, last_distance))
                .unwrap_or(0),
            elapsed_s: raw_elapsed_s
                .map(|v| delta_since(v as i64, last_elapsed))
                .unwrap_or(0),
        };

        tx.execute(
            "INSERT INTO device_baseline (id, last_steps, last_distance_m, last_elapsed_s)
             VALUES (0, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                last_steps = excluded.last_steps,
                last_distance_m = excluded.last_distance_m,
                last_elapsed_s = excluded.last_elapsed_s",
            params![
                raw_steps.map(|v| v as i64).or(last_steps),
                raw_distance_m.map(|v| v as i64).or(last_distance),
                raw_elapsed_s.map(|v| v as i64).or(last_elapsed),
            ],
        )
        .context("update device_baseline")?;

        tx.commit().context("commit baseline transaction")?;
        Ok(deltas)
    }

    /// Credit an already-decided amount of walking to both today's totals
    /// (`daily_stats`, unchanged calendar-split behavior) and the currently-open
    /// activity segment (`activity_segments`), in one transaction so a crash
    /// mid-credit cannot leave them disagreeing.
    ///
    /// `open_segment` is the open handle the daemon holds in memory (id +
    /// `started_at`, задача 044): `Some` extends only when both match a row,
    /// `None` opens a new segment. Matching id alone is insufficient —
    /// `recompute-segments` renumbers ids from 1, so a live daemon could
    /// otherwise UPDATE a different historical segment. Returns `(id, started_at)`
    /// for the segment credited (caller stores that as its open handle).
    ///
    /// There is no `open` flag in the DB — "open" lives in daemon memory, so a
    /// daemon restart simply starts a new segment and read-time
    /// [`merge_segments`] re-joins it to the pre-restart one if the gap is under
    /// the configured threshold (same restart reasoning as `device_baseline`).
    ///
    /// `now` is threaded through explicitly (rather than calling `Utc::now()`
    /// internally, like the rest of this module) so the segment start/extend is
    /// deterministic in tests.
    ///
    /// Midnight-crossing edge case: a segment's `date` is fixed to the calendar
    /// date of `started_at` and never changes while it is extended, even past
    /// local midnight. `daily_stats`, in contrast, gets the correct split by
    /// calendar day of each individual credit — it knows nothing about segments.
    pub fn credit_activity(
        &mut self,
        steps: i64,
        distance_m: i64,
        walking_time_s: i64,
        now: DateTime<Utc>,
        open_segment: Option<(i64, String)>,
    ) -> Result<(i64, String)> {
        let today = now.with_timezone(&Local).format("%Y-%m-%d").to_string();
        let tx = self
            .conn
            .transaction()
            .context("begin credit_activity transaction")?;

        tx.execute(
            "INSERT INTO daily_stats (date, distance_m, steps, walking_time_s)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(date) DO UPDATE SET
                distance_m = daily_stats.distance_m + excluded.distance_m,
                steps = daily_stats.steps + excluded.steps,
                walking_time_s = daily_stats.walking_time_s + excluded.walking_time_s",
            params![today, distance_m, steps, walking_time_s],
        )
        .context("credit daily_stats")?;

        // Extend only when id *and* started_at match (задача 044); a renumbered
        // table after recompute keeps the historical row out of the live cache.
        let extended = match open_segment {
            Some((id, ref started_at)) => {
                let rows = tx
                    .execute(
                        "UPDATE activity_segments SET
                            ended_at = ?1,
                            distance_m = distance_m + ?2,
                            steps = steps + ?3,
                            walking_time_s = walking_time_s + ?4
                         WHERE id = ?5 AND started_at = ?6",
                        params![
                            now.to_rfc3339(),
                            distance_m,
                            steps,
                            walking_time_s,
                            id,
                            started_at
                        ],
                    )
                    .context("extend activity segment")?;
                if rows == 0 {
                    tracing::warn!(
                        id,
                        started_at = %started_at,
                        "cached segment id no longer matches — reopened"
                    );
                    None
                } else {
                    Some((id, started_at.clone()))
                }
            }
            None => None,
        };

        let segment = match extended {
            Some(handle) => handle,
            None => {
                let started = now.to_rfc3339();
                tx.execute(
                    "INSERT INTO activity_segments (started_at, ended_at, date, distance_m, steps, walking_time_s)
                     VALUES (?1, ?1, ?2, ?3, ?4, ?5)",
                    params![&started, today, distance_m, steps, walking_time_s],
                )
                .context("insert activity segment")?;
                (tx.last_insert_rowid(), started)
            }
        };

        tx.commit().context("commit credit_activity transaction")?;
        Ok(segment)
    }

    /// Totals for today (local calendar date), zeroed if nothing recorded yet.
    pub fn today_stats(&self) -> Result<DailyStats> {
        let today = Local::now().format("%Y-%m-%d").to_string();
        self.stats_for(&today).map(|opt| {
            opt.unwrap_or(DailyStats {
                date: today,
                ..Default::default()
            })
        })
    }

    /// Totals for an arbitrary `YYYY-MM-DD` date, or `None` if nothing recorded.
    pub fn stats_for(&self, date: &str) -> Result<Option<DailyStats>> {
        self.conn
            .query_row(
                "SELECT date, distance_m, steps, walking_time_s FROM daily_stats WHERE date = ?1",
                params![date],
                |row| {
                    Ok(DailyStats {
                        date: row.get(0)?,
                        distance_m: row.get(1)?,
                        steps: row.get(2)?,
                        walking_time_s: row.get(3)?,
                    })
                },
            )
            .optional()
            .context("query daily_stats")
    }

    /// All recorded days, most recent first.
    pub fn all_stats(&self) -> Result<Vec<DailyStats>> {
        let mut stmt = self
            .conn
            .prepare("SELECT date, distance_m, steps, walking_time_s FROM daily_stats ORDER BY date DESC")
            .context("prepare all_stats query")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(DailyStats {
                    date: row.get(0)?,
                    distance_m: row.get(1)?,
                    steps: row.get(2)?,
                    walking_time_s: row.get(3)?,
                })
            })
            .context("run all_stats query")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("collect all_stats rows")
    }

    /// All recorded activity segments, oldest first (by `started_at`). The
    /// storage grain behind every displayed workout — few rows per day, so
    /// loading all of them and merging in memory is cheap (see the merge
    /// call sites). Ordered ascending so [`merge_segments`] sees them
    /// chronologically.
    pub(crate) fn all_segments_asc(&self) -> Result<Vec<Segment>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, date, started_at, ended_at, distance_m, steps, walking_time_s
                 FROM activity_segments ORDER BY started_at ASC, id ASC",
            )
            .context("prepare all_segments query")?;
        let rows = stmt
            .query_map([], segment_from_row)
            .context("run all_segments query")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("collect all_segments rows")
    }

    /// Workouts whose start day is the given `YYYY-MM-DD`, oldest first, derived
    /// by merging segments at the configured `gap_minutes` (задача 014). Merging
    /// spans the whole segment history (not just this day's) so a workout that
    /// began just before local midnight correctly absorbs the after-midnight
    /// segments before the day filter is applied.
    pub fn workouts_for(&self, date: &str, gap_minutes: i64) -> Result<Vec<Workout>> {
        let merged = merge_segments(&self.all_segments_asc()?, gap_minutes);
        Ok(merged.into_iter().filter(|w| w.date == date).collect())
    }

    /// The most recently started (derived) workout, or `None` if none recorded.
    /// Backs the `widget` command, which surfaces the current session's live
    /// metrics; its `walking_time_s`/`steps`/`distance_m` are the sum over the
    /// segments merged into that newest workout. Merges the whole (tiny) segment
    /// history each call — fine at this scale, and simpler than a partial merge.
    pub fn latest_workout(&self, gap_minutes: i64) -> Result<Option<Workout>> {
        Ok(merge_segments(&self.all_segments_asc()?, gap_minutes).pop())
    }

    /// Atomically replace the entire `activity_segments` table with `segments`
    /// (задача 015 recompute): clear it, then re-insert the given rows with
    /// their explicit ids, all in one transaction so a crash mid-rebuild leaves
    /// the old set intact. Idempotent when fed a deterministically-computed set
    /// (the replay produces identical ids/columns each run). Touches nothing
    /// else — `daily_stats`, `raw_samples`, and `workouts` are out of scope.
    pub fn replace_activity_segments(&mut self, segments: &[Segment]) -> Result<()> {
        let tx = self
            .conn
            .transaction()
            .context("begin replace_activity_segments transaction")?;
        tx.execute("DELETE FROM activity_segments", [])
            .context("clear activity_segments")?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO activity_segments (id, started_at, ended_at, date, distance_m, steps, walking_time_s)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .context("prepare activity_segments insert")?;
            for segment in segments {
                stmt.execute(params![
                    segment.id,
                    segment.started_at,
                    segment.ended_at,
                    segment.date,
                    segment.distance_m,
                    segment.steps,
                    segment.walking_time_s,
                ])
                .context("insert rebuilt activity segment")?;
            }
        }
        tx.commit()
            .context("commit replace_activity_segments transaction")?;
        Ok(())
    }

    /// Step-goal thresholds already celebrated on the given `YYYY-MM-DD`
    /// (local calendar date). Read before deciding what to fire so a daemon
    /// restart mid-day never re-celebrates a goal (задача 011).
    pub fn celebrated_thresholds(&self, date: &str) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT threshold FROM goal_celebrations WHERE date = ?1")
            .context("prepare celebrated_thresholds query")?;
        let rows = stmt
            .query_map(params![date], |row| row.get(0))
            .context("run celebrated_thresholds query")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("collect celebrated_thresholds rows")
    }

    /// Mark a goal threshold celebrated for `date`. `INSERT OR IGNORE` keeps
    /// it idempotent — a re-fire attempt (e.g. a race across a restart) is a
    /// no-op rather than a duplicate row or an error.
    pub fn mark_goal_celebrated(&self, date: &str, threshold: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO goal_celebrations (date, threshold, celebrated_at) VALUES (?1, ?2, ?3)",
                params![date, threshold, Utc::now().to_rfc3339()],
            )
            .context("insert goal_celebrations row")?;
        Ok(())
    }
}

fn segment_from_row(row: &rusqlite::Row) -> rusqlite::Result<Segment> {
    Ok(Segment {
        id: row.get(0)?,
        date: row.get(1)?,
        started_at: row.get(2)?,
        ended_at: row.get(3)?,
        distance_m: row.get(4)?,
        steps: row.get(5)?,
        walking_time_s: row.get(6)?,
    })
}

/// Merge chronologically-ordered activity segments into displayed workouts
/// (задача 014): adjacent segments whose inter-segment gap (`next.started_at −
/// prev.ended_at`) is ≤ `gap_minutes` are joined into one [`Workout`]; a larger
/// gap starts a new workout. Pure and unit-tested — the read-time projection
/// that makes the workout-gap retroactively configurable.
///
/// The boundary is inclusive (`<=`): a gap exactly at `gap_minutes` continues
/// the workout, matching the legacy split so seeded history re-merges identically
/// at the default 15-minute gap. `segments` must be sorted ascending by
/// `started_at`. A segment with an unparseable timestamp starts a new workout
/// (logged WARN) rather than corrupting the merge. Returned oldest-first.
pub fn merge_segments(segments: &[Segment], gap_minutes: i64) -> Vec<Workout> {
    let gap = Duration::minutes(gap_minutes.max(0));
    let mut workouts: Vec<Workout> = Vec::new();
    let mut prev_end: Option<DateTime<Utc>> = None;

    for segment in segments {
        let started = match DateTime::parse_from_rfc3339(&segment.started_at) {
            Ok(dt) => dt.with_timezone(&Utc),
            Err(err) => {
                tracing::warn!(%err, started_at = %segment.started_at, id = segment.id, "segment started_at not RFC3339 — starting a new workout");
                push_or_extend_new(&mut workouts, segment);
                prev_end = parse_end(segment);
                continue;
            }
        };

        let continues = match prev_end {
            Some(end) => started.signed_duration_since(end) <= gap,
            None => false,
        };

        if continues && let Some(current) = workouts.last_mut() {
            current.ended_at = segment.ended_at.clone();
            current.distance_m += segment.distance_m;
            current.steps += segment.steps;
            current.walking_time_s += segment.walking_time_s;
        } else {
            push_or_extend_new(&mut workouts, segment);
        }
        // A merged segment's own end anchors the gap to the next segment.
        prev_end = parse_end(segment).or(prev_end);
    }

    workouts
}

/// Start a fresh workout from a single segment (its start day, its endpoints,
/// its totals). The workout `id` is the seeding segment's id — stable/unique.
fn push_or_extend_new(workouts: &mut Vec<Workout>, segment: &Segment) {
    workouts.push(Workout {
        id: segment.id,
        date: segment.date.clone(),
        started_at: segment.started_at.clone(),
        ended_at: segment.ended_at.clone(),
        distance_m: segment.distance_m,
        steps: segment.steps,
        walking_time_s: segment.walking_time_s,
    });
}

/// Parse a segment's `ended_at`; `None` (logged WARN) makes the next segment
/// start a fresh workout rather than merge against a bogus anchor.
fn parse_end(segment: &Segment) -> Option<DateTime<Utc>> {
    match DateTime::parse_from_rfc3339(&segment.ended_at) {
        Ok(dt) => Some(dt.with_timezone(&Utc)),
        Err(err) => {
            tracing::warn!(%err, ended_at = %segment.ended_at, id = segment.id, "segment ended_at not RFC3339 — next segment will not merge into this one");
            None
        }
    }
}

/// Delta of a monotonically-increasing device counter since the last sample.
///
/// Two special cases besides the normal `new - last`:
/// - `last` is `None` (first-ever observation of this device, e.g. fresh
///   install or the daemon's first contact mid-workout): credit *zero*, not
///   `new`. We have no idea how much of that pre-existing counter value was
///   accrued while genuinely walking vs. paused vs. away, so crediting it
///   would silently front-load an unverified lump sum into today's totals.
///   The baseline is still captured so subsequent samples delta correctly.
/// - `new < last` (device power-cycled, counter reset to a small value):
///   treat `new` itself as the delta — rebaseline from zero instead of
///   going negative.
fn delta_since(new: i64, last: Option<i64>) -> i64 {
    match last {
        Some(last) if new >= last => new - last,
        Some(_) => new,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};

    use super::super::memory_store;
    use super::*;

    #[test]
    fn first_observation_credits_nothing() {
        assert_eq!(delta_since(587, None), 0);
    }

    #[test]
    fn normal_progress_credits_the_difference() {
        assert_eq!(delta_since(15, Some(10)), 5);
    }

    #[test]
    fn device_reset_rebaselines_from_zero() {
        assert_eq!(delta_since(3, Some(500)), 3);
    }

    #[test]
    fn unchanged_counter_credits_zero() {
        assert_eq!(delta_since(10, Some(10)), 0);
    }

    /// Credit one walking burst, threading the daemon's open-segment handle.
    fn credit(
        store: &mut Store,
        steps: i64,
        dist: i64,
        time: i64,
        now: DateTime<Utc>,
        open: Option<(i64, String)>,
    ) -> (i64, String) {
        store.credit_activity(steps, dist, time, now, open).unwrap()
    }

    /// A segment for the pure `merge_segments` tests. `date` is the UTC start
    /// day (tests fix `TZ=UTC`), distance is `2×steps` to keep sums distinct.
    fn seg(id: i64, start: DateTime<Utc>, end: DateTime<Utc>, steps: i64) -> Segment {
        Segment {
            id,
            date: start.format("%Y-%m-%d").to_string(),
            started_at: start.to_rfc3339(),
            ended_at: end.to_rfc3339(),
            distance_m: steps * 2,
            steps,
            walking_time_s: steps,
        }
    }

    #[test]
    fn credit_activity_extends_open_segment_when_id_threaded() {
        let mut store = memory_store();
        let t0 = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();
        let t1 = t0 + Duration::minutes(10);

        // Same open segment across both bursts (daemon never left Walking).
        let handle = credit(&mut store, 5, 10, 30, t0, None);
        let handle2 = credit(&mut store, 5, 10, 30, t1, Some(handle.clone()));

        assert_eq!(handle.0, handle2.0, "extending, not opening a new segment");
        assert_eq!(handle.1, handle2.1);
        let segs = store.all_segments_asc().unwrap();
        assert_eq!(segs.len(), 1, "single continuous segment");
        assert_eq!(segs[0].steps, 10);
        assert_eq!(segs[0].distance_m, 20);
        assert_eq!(segs[0].walking_time_s, 60);
        assert_eq!(segs[0].started_at, t0.to_rfc3339());
        assert_eq!(segs[0].ended_at, t1.to_rfc3339());
    }

    #[test]
    fn credit_activity_none_open_starts_a_new_segment() {
        let mut store = memory_store();
        let t0 = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();
        let t1 = t0 + Duration::minutes(20);

        // The daemon closed the first segment (presence left Walking), so the
        // second burst is credited with `None` → a brand-new segment.
        let handle = credit(&mut store, 5, 10, 30, t0, None);
        let handle2 = credit(&mut store, 5, 10, 30, t1, None);

        assert_ne!(handle.0, handle2.0);
        assert_eq!(
            store.all_segments_asc().unwrap().len(),
            2,
            "close-and-new yields two segments"
        );
    }

    #[test]
    fn credit_activity_stale_open_id_opens_new_segment_without_losing_credit() {
        let mut store = memory_store();
        let t0 = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();
        let t1 = t0 + Duration::minutes(5);
        credit(&mut store, 5, 10, 30, t0, None);

        // A bogus open id (e.g. DB wiped under a long-lived daemon) must open a
        // new segment rather than silently no-op the UPDATE and drop the credit.
        let handle2 = credit(&mut store, 7, 14, 42, t1, Some((999_999, t0.to_rfc3339())));
        assert_ne!(handle2.0, 999_999);
        let segs = store.all_segments_asc().unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(
            segs.iter().map(|s| s.steps).sum::<i64>(),
            12,
            "both credits landed"
        );
    }

    /// After `replace_activity_segments` renumbers ids, a live cache holding
    /// `(id, started_at)` must not extend a different historical row (задача 044).
    #[test]
    fn credit_activity_rejects_id_reuse_with_different_started_at() {
        let mut store = memory_store();
        let t0 = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();
        let t1 = t0 + Duration::minutes(30);
        let t2 = t1 + Duration::minutes(5);
        let live = credit(&mut store, 10, 20, 60, t0, None);
        // Simulate recompute: wipe and reinsert with deterministic ids from 1
        // but a different started_at on id=1 (historical closed segment).
        let historical = Segment {
            id: 1,
            date: "2026-07-01".into(),
            started_at: (t0 - Duration::days(4)).to_rfc3339(),
            ended_at: (t0 - Duration::days(4) + Duration::minutes(20)).to_rfc3339(),
            distance_m: 100,
            steps: 50,
            walking_time_s: 1200,
        };
        let historical_started = historical.started_at.clone();
        store.replace_activity_segments(&[historical]).unwrap();
        // Cached handle still has live's started_at under id 1 — must open new.
        let after = credit(&mut store, 5, 10, 30, t2, Some(live));
        assert_ne!(after.1, historical_started);
        let segs = store.all_segments_asc().unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].steps, 50, "historical row untouched");
        assert_eq!(segs[1].steps, 5, "live credit opened a new segment");
    }

    #[test]
    fn credit_activity_first_of_day_sets_segment_start_date() {
        let mut store = memory_store();
        let t0 = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();
        credit(&mut store, 5, 10, 30, t0, None);
        let segs = store.all_segments_asc().unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].date, "2026-07-05");
        assert_eq!(segs[0].steps, 5);
    }

    #[test]
    fn credit_activity_across_midnight_keeps_start_date_but_splits_daily_stats() {
        let mut store = memory_store();
        // One continuous open segment that starts before local midnight and is
        // extended past it (daemon never left Walking).
        let before_midnight = Utc.with_ymd_and_hms(2026, 7, 5, 23, 50, 0).unwrap();
        let after_midnight = Utc.with_ymd_and_hms(2026, 7, 6, 0, 0, 0).unwrap();

        let id = credit(&mut store, 10, 20, 60, before_midnight, None);
        credit(&mut store, 20, 40, 120, after_midnight, Some(id));

        let segs = store.all_segments_asc().unwrap();
        assert_eq!(segs.len(), 1, "single segment spanning midnight");
        assert_eq!(segs[0].date, "2026-07-05", "date fixed to the start day");
        assert_eq!(segs[0].steps, 30);
        assert_eq!(segs[0].ended_at, after_midnight.to_rfc3339());

        // The derived workout is attributed to the start day only.
        let day5 = store.workouts_for("2026-07-05", 15).unwrap();
        assert_eq!(day5.len(), 1);
        assert_eq!(day5[0].steps, 30);
        assert!(
            store.workouts_for("2026-07-06", 15).unwrap().is_empty(),
            "attributed to start day, not end day"
        );

        // daily_stats, in contrast, is genuinely split by calendar day.
        let day1 = store
            .stats_for("2026-07-05")
            .unwrap()
            .expect("day1 stats present");
        assert_eq!(day1.steps, 10);
        assert_eq!(day1.distance_m, 20);
        let day2 = store
            .stats_for("2026-07-06")
            .unwrap()
            .expect("day2 stats present");
        assert_eq!(day2.steps, 20);
        assert_eq!(day2.distance_m, 40);
    }

    #[test]
    fn merge_segments_empty_is_empty() {
        assert!(merge_segments(&[], 15).is_empty());
    }

    #[test]
    fn merge_segments_joins_within_gap_and_sums() {
        let a_start = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();
        let a_end = a_start + Duration::minutes(5);
        let b_start = a_end + Duration::minutes(10); // 10 min gap ≤ 15
        let b_end = b_start + Duration::minutes(5);
        let segs = [seg(1, a_start, a_end, 100), seg(2, b_start, b_end, 200)];

        let workouts = merge_segments(&segs, 15);
        assert_eq!(workouts.len(), 1, "gap under threshold merges");
        assert_eq!(workouts[0].id, 1, "workout id is its first segment's id");
        assert_eq!(workouts[0].steps, 300);
        assert_eq!(workouts[0].distance_m, 600);
        assert_eq!(workouts[0].walking_time_s, 300);
        assert_eq!(workouts[0].started_at, a_start.to_rfc3339());
        assert_eq!(workouts[0].ended_at, b_end.to_rfc3339());
    }

    #[test]
    fn merge_segments_splits_beyond_gap() {
        let a_start = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();
        let a_end = a_start + Duration::minutes(5);
        let b_start = a_end + Duration::minutes(20); // 20 min gap > 15
        let b_end = b_start + Duration::minutes(5);
        let workouts = merge_segments(
            &[seg(1, a_start, a_end, 100), seg(2, b_start, b_end, 200)],
            15,
        );
        assert_eq!(workouts.len(), 2);
        assert_eq!(workouts[0].id, 1);
        assert_eq!(workouts[1].id, 2);
    }

    #[test]
    fn merge_segments_boundary_is_inclusive() {
        let a_start = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();
        let a_end = a_start + Duration::minutes(5);
        // Gap exactly at the threshold merges; one second over splits.
        let at_threshold = a_end + Duration::minutes(15);
        let over = a_end + Duration::minutes(15) + Duration::seconds(1);

        let merged = merge_segments(
            &[
                seg(1, a_start, a_end, 1),
                seg(2, at_threshold, at_threshold, 1),
            ],
            15,
        );
        assert_eq!(merged.len(), 1, "gap == threshold continues the workout");

        let split = merge_segments(&[seg(1, a_start, a_end, 1), seg(2, over, over, 1)], 15);
        assert_eq!(split.len(), 2, "one second past the threshold splits");
    }

    #[test]
    fn merge_segments_regroups_retroactively_with_the_gap() {
        // Same three segments, spaced 10 min apart, regroup purely by the gap.
        let s0 = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();
        let segs = [
            seg(1, s0, s0 + Duration::minutes(1), 10),
            seg(
                2,
                s0 + Duration::minutes(11),
                s0 + Duration::minutes(12),
                10,
            ),
            seg(
                3,
                s0 + Duration::minutes(22),
                s0 + Duration::minutes(23),
                10,
            ),
        ];
        assert_eq!(
            merge_segments(&segs, 5).len(),
            3,
            "tight gap → three separate workouts"
        );
        assert_eq!(
            merge_segments(&segs, 15).len(),
            1,
            "loose gap → one merged workout"
        );
    }

    #[test]
    fn merge_segments_joins_across_midnight_under_start_date() {
        let s1 = Utc.with_ymd_and_hms(2026, 7, 5, 23, 50, 0).unwrap();
        let e1 = Utc.with_ymd_and_hms(2026, 7, 5, 23, 55, 0).unwrap();
        let s2 = Utc.with_ymd_and_hms(2026, 7, 6, 0, 0, 0).unwrap(); // 5 min gap
        let e2 = Utc.with_ymd_and_hms(2026, 7, 6, 0, 5, 0).unwrap();

        let workouts = merge_segments(&[seg(1, s1, e1, 10), seg(2, s2, e2, 20)], 15);
        assert_eq!(
            workouts.len(),
            1,
            "cross-midnight, gap under threshold, merges"
        );
        assert_eq!(
            workouts[0].date, "2026-07-05",
            "attributed to the start day"
        );
        assert_eq!(workouts[0].steps, 30);
        assert_eq!(workouts[0].ended_at, e2.to_rfc3339());
    }

    #[test]
    fn goal_celebrations_are_idempotent_and_scoped_to_date() {
        let store = memory_store();
        assert!(
            store
                .celebrated_thresholds("2026-07-05")
                .unwrap()
                .is_empty()
        );

        store.mark_goal_celebrated("2026-07-05", 8000).unwrap();
        store.mark_goal_celebrated("2026-07-05", 10000).unwrap();
        // Re-marking the same (date, threshold) must not duplicate or error.
        store.mark_goal_celebrated("2026-07-05", 8000).unwrap();

        let mut today = store.celebrated_thresholds("2026-07-05").unwrap();
        today.sort_unstable();
        assert_eq!(today, vec![8000, 10000]);

        // A different day starts fresh.
        assert!(
            store
                .celebrated_thresholds("2026-07-06")
                .unwrap()
                .is_empty()
        );
    }
}
