//! Sample inserts, HR aggregates, and raw telemetry reads.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::params;

use crate::default_speed::trimmed_mean_speed;
use crate::ftms::TreadmillData;
use crate::hr::HrMeasurement;
use crate::speed::CentiKmh;

use super::Store;

/// One decoded `raw_samples` row, in the shape the offline replay
/// (`crate::recompute`) needs to re-drive the presence + credit engine: the
/// device session it belongs to, its wall-clock timestamp, and the *cumulative*
/// device counters (already decoded back from their wire scale). See
/// `docs/tasks/015`.
#[derive(Debug, Clone)]
pub struct RawSample {
    pub session_id: i64,
    pub ts_ms: i64,
    pub speed: Option<CentiKmh>,
    pub distance_m: Option<u32>,
    pub elapsed_s: Option<u16>,
    pub steps: Option<u32>,
}

/// One stored HR sample as `recompute-hr` (задача 034) sees it: identity, time,
/// and the untouched wire frame it was decoded from.
#[derive(Debug, Clone)]
pub struct HrRow {
    pub id: i64,
    pub ts_ms: i64,
    pub raw_frame: Vec<u8>,
}

/// Below this many `hr_samples` in a window, [`Store::hr_summary_for`] returns
/// `None` rather than a summary computed from too little data to be meaningful.
const MIN_HR_SAMPLES_FOR_SUMMARY: usize = 10;

/// Trim fraction for the heart-rate average — mirrors
/// `default_speed::TRIM_FRACTION` (15% off each end of the sorted samples).
const HR_TRIM_FRACTION: f32 = 0.15;

/// Compact heart-rate summary over a time window (задача 025): `avg_bpm` is
/// the trimmed mean (drops brief contact-loss/noise artifacts), `max_bpm` is
/// the p95 (a "peak effort" figure, robust against a single spike).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HrSummary {
    pub avg_bpm: i64,
    pub max_bpm: i64,
}

impl Store {
    /// Record the start of a new BLE connection to the treadmill. Returns the
    /// new session id, used to tag `raw_samples`/`status_events` rows.
    pub fn start_session(&self) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "INSERT INTO sessions (started_at, ended_at) VALUES (?1, NULL)",
                params![now],
            )
            .context("insert session")?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Close the most recently opened session (on disconnect).
    pub fn end_session(&self) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "UPDATE sessions SET ended_at = ?1
                 WHERE id = (SELECT id FROM sessions WHERE ended_at IS NULL ORDER BY id DESC LIMIT 1)",
                params![now],
            )
            .context("close session")?;
        Ok(())
    }

    /// Persist one decoded Treadmill Data (`0x2ACD`) sample verbatim.
    ///
    /// Structured columns use the *wire* scale (e.g. `speed_centikmh`, the raw
    /// 0.01 km/h unit the device sends) rather than a decoded `f32` — integer
    /// affinity stores small values in 1-3 bytes, where SQLite's `REAL` always
    /// takes 8, and it sidesteps float round-tripping entirely. `raw_frame`
    /// keeps the exact bytes alongside the decode, so a future protocol
    /// discovery (e.g. this device starting to set a flag bit we don't parse
    /// yet) can be recovered from history instead of lost.
    pub fn insert_raw_sample(
        &self,
        session_id: i64,
        ts_ms: i64,
        sample: &TreadmillData,
        raw_frame: &[u8],
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO raw_samples
                    (session_id, ts_ms, speed_centikmh, avg_speed_centikmh, distance_m, energy_kcal, elapsed_s, steps, raw_frame)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    session_id,
                    ts_ms,
                    sample.speed.map(|s| i64::from(s.to_wire())),
                    sample.avg_speed.map(|s| i64::from(s.to_wire())),
                    sample.total_distance_m.map(|v| v as i64),
                    sample.total_energy_kcal.map(|v| v as i64),
                    sample.elapsed_s.map(|v| v as i64),
                    sample.steps.map(|v| v as i64),
                    raw_frame,
                ],
            )
            .context("insert raw_samples row")?;
        Ok(())
    }

    /// Persist one decoded Heart Rate Measurement (`0x2A37`) sample (задача
    /// 025). `rr_ms` is left `NULL` for now — the column is a deliberate
    /// forward-looking slot for a future HRV feature, not decoded/stored yet.
    pub fn insert_hr_sample(
        &self,
        session_id: i64,
        ts_ms: i64,
        sample: &HrMeasurement,
        raw_frame: &[u8],
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO hr_samples (session_id, ts_ms, bpm, rr_ms, raw_frame)
                 VALUES (?1, ?2, ?3, NULL, ?4)",
                params![session_id, ts_ms, sample.bpm as i64, raw_frame],
            )
            .context("insert hr_samples row")?;
        Ok(())
    }

    /// Compact heart-rate summary over a wall-clock window `[from_rfc3339,
    /// to_rfc3339]` — a trimmed-mean average and a p95 peak (задача 025), the
    /// same statistical shape `default_speed::trimmed_mean_speed` already uses
    /// for belt-speed cruising estimates: trimming kills the strap's own
    /// artifacts (brief contact-loss spikes, warm-up noise) without a manual
    /// floor. Returns `None` when fewer than [`MIN_HR_SAMPLES_FOR_SUMMARY`]
    /// samples fall in the window — too little to summarize meaningfully
    /// (e.g. a workout with no HR sensor worn, or a very short one).
    pub fn hr_summary_for(
        &self,
        from_rfc3339: &str,
        to_rfc3339: &str,
    ) -> Result<Option<HrSummary>> {
        let start_ms = DateTime::parse_from_rfc3339(from_rfc3339)
            .context("parse hr window start")?
            .timestamp_millis();
        let end_ms = DateTime::parse_from_rfc3339(to_rfc3339)
            .context("parse hr window end")?
            .timestamp_millis();

        let mut stmt = self
            .conn
            .prepare(
                "SELECT bpm FROM hr_samples
                 WHERE ts_ms BETWEEN ?1 AND ?2 AND bpm > 0
                 ORDER BY bpm ASC",
            )
            .context("prepare hr_summary_for query")?;
        let bpms: Vec<f32> = stmt
            .query_map(params![start_ms, end_ms], |row| {
                let bpm: i64 = row.get(0)?;
                Ok(bpm as f32)
            })
            .context("run hr_summary_for query")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("collect hr_summary_for rows")?;

        if bpms.len() < MIN_HR_SAMPLES_FOR_SUMMARY {
            return Ok(None);
        }

        // `bpms` is already sorted ascending (the query's `ORDER BY bpm`), so
        // the trim + percentile below need no re-sort.
        let trimmed = trimmed_mean_speed(&bpms, 0.0, HR_TRIM_FRACTION)
            .expect("non-empty bpms always yields a trimmed mean");
        Ok(Some(HrSummary {
            avg_bpm: trimmed.mean_kmh.round() as i64,
            max_bpm: percentile_95(&bpms).round() as i64,
        }))
    }

    /// Persist one Fitness Machine Status (`0x2ADA`) event verbatim.
    ///
    /// `event_code` is the raw FTMS op code (first byte); its human-readable
    /// meaning lives in `ftms::describe_status_event` (code, not a DB column)
    /// so the mapping has one source of truth instead of drifting copies.
    pub fn insert_status_event(
        &self,
        session_id: i64,
        ts_ms: i64,
        event_code: u8,
        raw_frame: &[u8],
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO status_events (session_id, ts_ms, event_code, raw_frame) VALUES (?1, ?2, ?3, ?4)",
                params![session_id, ts_ms, event_code, raw_frame],
            )
            .context("insert status_events row")?;
        Ok(())
    }

    /// Reconstruct the *raw* (pre-presence-filter) distance the belt moved over
    /// a wall-clock window `[started_at, ended_at]`, in meters, from the
    /// per-frame `raw_samples` device counter. Unlike the credited
    /// `workouts.distance_m` (walking only), this includes the metres logged
    /// while the belt spun with the operator off it — the amount presence
    /// filtering drops (see `daemon::credit_or_hold`).
    ///
    /// Summing positive frame-to-frame deltas (rather than `MAX − MIN`) keeps
    /// the figure correct when the treadmill power-cycles mid-window and its
    /// cumulative counter resets to zero. Returns `None` when the window holds
    /// no usable samples, so the caller can omit the hint rather than show 0.
    pub fn raw_distance_m(&self, started_at: &str, ended_at: &str) -> Result<Option<i64>> {
        let start_ms = DateTime::parse_from_rfc3339(started_at)
            .context("parse workout started_at")?
            .timestamp_millis();
        let end_ms = DateTime::parse_from_rfc3339(ended_at)
            .context("parse workout ended_at")?
            .timestamp_millis();
        let raw: Option<i64> = self
            .conn
            .query_row(
                "SELECT SUM(CASE WHEN d > 0 THEN d ELSE 0 END) FROM (
                    SELECT distance_m - LAG(distance_m) OVER (ORDER BY ts_ms) AS d
                    FROM raw_samples WHERE ts_ms BETWEEN ?1 AND ?2
                 )",
                params![start_ms, end_ms],
                |row| row.get(0),
            )
            .context("run raw_distance_m query")?;
        Ok(raw)
    }

    /// Belt speeds (km/h) recorded in `raw_samples` over the wall-clock window
    /// `[started_at, ended_at]`, for the computed default-speed estimate (задача
    /// 016). Only positive speeds are returned (crawl/idle-floor and trimming
    /// happen in the pure `default_speed::trimmed_mean_speed`), decoded back from
    /// the stored centi-km/h wire scale. Ordered by time, though the estimate is
    /// order-independent.
    pub fn walking_speeds_in_window(&self, started_at: &str, ended_at: &str) -> Result<Vec<f32>> {
        let start_ms = DateTime::parse_from_rfc3339(started_at)
            .context("parse window started_at")?
            .timestamp_millis();
        let end_ms = DateTime::parse_from_rfc3339(ended_at)
            .context("parse window ended_at")?
            .timestamp_millis();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT speed_centikmh FROM raw_samples
                 WHERE ts_ms BETWEEN ?1 AND ?2 AND speed_centikmh IS NOT NULL AND speed_centikmh > 0
                 ORDER BY ts_ms ASC",
            )
            .context("prepare walking_speeds_in_window query")?;
        let rows = stmt
            .query_map(params![start_ms, end_ms], |row| {
                let centi: i64 = row.get(0)?;
                Ok(centi as f32 / 100.0)
            })
            .context("run walking_speeds_in_window query")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("collect walking_speeds_in_window rows")
    }

    /// All `raw_samples` rows in true processing order (`ts_ms`, then `id` as a
    /// stable tiebreaker for same-millisecond frames), decoded back from their
    /// stored wire scale into the cumulative device counters. Backs the offline
    /// segment replay (`crate::recompute`): sessions can't interleave (one BLE
    /// central at a time), so a change in `session_id` cleanly marks a session
    /// boundary as the caller walks the rows. `raw_frame` is not re-parsed —
    /// the daemon already dropped undecodable frames before insert, so the
    /// stored columns are the authoritative decode.
    pub fn raw_samples_ordered(&self) -> Result<Vec<RawSample>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session_id, ts_ms, speed_centikmh, distance_m, elapsed_s, steps
                 FROM raw_samples ORDER BY ts_ms ASC, id ASC",
            )
            .context("prepare raw_samples_ordered query")?;
        let rows = stmt
            .query_map([], |row| {
                let speed_centikmh: Option<i64> = row.get(2)?;
                let distance_m: Option<i64> = row.get(3)?;
                let elapsed_s: Option<i64> = row.get(4)?;
                let steps: Option<i64> = row.get(5)?;
                Ok(RawSample {
                    session_id: row.get(0)?,
                    ts_ms: row.get(1)?,
                    // Stored as centi-km/h wire integer — lossless via CentiKmh.
                    speed: speed_centikmh
                        .and_then(|c| u16::try_from(c).ok().map(CentiKmh::from_wire)),
                    distance_m: distance_m.map(|v| v as u32),
                    elapsed_s: elapsed_s.map(|v| v as u16),
                    steps: steps.map(|v| v as u32),
                })
            })
            .context("run raw_samples_ordered query")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("collect raw_samples_ordered rows")
    }

    /// Every HR sample in chronological order, with its raw wire frame — the
    /// ground truth `recompute-hr` (задача 034) replays through
    /// `hr::ContactTracker`.
    pub fn hr_samples_ordered(&self) -> Result<Vec<HrRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, ts_ms, raw_frame FROM hr_samples ORDER BY ts_ms ASC, id ASC")
            .context("prepare hr_samples_ordered query")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(HrRow {
                    id: row.get(0)?,
                    ts_ms: row.get(1)?,
                    raw_frame: row.get(2)?,
                })
            })
            .context("run hr_samples_ordered query")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("collect hr_samples_ordered rows")
    }

    /// Delete the given HR samples in one transaction (задача 034). All-or-nothing:
    /// a crash mid-delete must not leave a half-cleaned history whose sequence
    /// structure no longer reflects either state.
    pub fn delete_hr_samples(&mut self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction().context("begin hr cleanup tx")?;
        {
            let mut stmt = tx
                .prepare("DELETE FROM hr_samples WHERE id = ?1")
                .context("prepare hr sample delete")?;
            for id in ids {
                stmt.execute([id]).context("delete hr sample")?;
            }
        }
        tx.commit().context("commit hr cleanup tx")
    }
}

/// The 95th percentile of an ascending-sorted slice — a "peak effort" bpm
/// figure robust against a single sensor spike, unlike a raw maximum. Nearest-
/// rank method, clamped to the last index. Panics on an empty slice (callers
/// only reach this after checking [`MIN_HR_SAMPLES_FOR_SUMMARY`]).
fn percentile_95(sorted_ascending: &[f32]) -> f32 {
    let n = sorted_ascending.len();
    let idx = (((n - 1) as f32) * 0.95).round() as usize;
    sorted_ascending[idx.min(n - 1)]
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use crate::hr::HrMeasurement;

    use super::super::memory_store;
    use super::*;

    fn insert_hr(store: &Store, session_id: i64, ts_ms: i64, bpm: u16) {
        let m = HrMeasurement {
            bpm,
            contact: None,
            rr_ms: vec![],
        };
        store.insert_hr_sample(session_id, ts_ms, &m, &[0]).unwrap();
    }

    #[test]
    fn hr_summary_none_below_minimum_samples() {
        let store = memory_store();
        store.start_session().unwrap();
        for i in 0..5 {
            insert_hr(&store, 1, i * 1000, 120);
        }
        assert_eq!(
            store
                .hr_summary_for("2026-01-01T00:00:00Z", "2026-01-01T01:00:00Z")
                .unwrap(),
            None
        );
    }

    #[test]
    fn hr_summary_trims_and_computes_p95() {
        let store = memory_store();
        store.start_session().unwrap();
        let base = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .timestamp_millis();
        // 85 steady 120s, 5 low outliers, 10 high outliers (>5% of the set, so
        // p95 actually lands inside that tail) — trim should erase both tails
        // from the average, while p95 still reflects a real peak.
        for i in 0..5 {
            insert_hr(&store, 1, base + i * 1000, 60);
        }
        for i in 5..90 {
            insert_hr(&store, 1, base + i * 1000, 120);
        }
        for i in 90..100 {
            insert_hr(&store, 1, base + i * 1000, 170);
        }

        let summary = store
            .hr_summary_for("2026-01-01T00:00:00Z", "2026-01-01T00:05:00Z")
            .unwrap()
            .expect("enough samples for a summary");
        assert_eq!(summary.avg_bpm, 120, "trim erases both outlier tails");
        assert_eq!(summary.max_bpm, 170, "p95 reflects the high outliers");
    }

    #[test]
    fn hr_summary_ignores_samples_outside_the_window() {
        let store = memory_store();
        store.start_session().unwrap();
        for i in 0..20 {
            insert_hr(&store, 1, i * 1000, 100);
        }
        // Window entirely before any sample.
        assert_eq!(
            store
                .hr_summary_for("2020-01-01T00:00:00Z", "2020-01-01T01:00:00Z")
                .unwrap(),
            None
        );
    }
}
