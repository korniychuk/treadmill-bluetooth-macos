//! Persistent daily statistics (SQLite) — survives daemon restarts.
//!
//! The FTMS Treadmill Data counters (`steps`, `total_distance_m`, `elapsed_s`)
//! are cumulative *per device session* — they reset to zero whenever the
//! treadmill starts a fresh workout (power cycle or a new belt start). This
//! store turns those resettable counters into a running daily total by
//! accumulating deltas.
//!
//! Deciding *whether* a given delta counts as real walking (vs. the belt
//! spinning with nobody on it) is [`crate::daemon`]'s job, using
//! [`crate::presence`] plus an in-memory pending buffer — see `daemon.rs` for
//! why the credit has to be buffered rather than applied the instant a
//! sample arrives (the away-confirmation window must not itself count).
//! This module only knows two operations: advance the persisted raw
//! baseline (`advance_baseline`, always safe to call every sample), and
//! credit an already-decided amount to today's totals (`credit_daily`).
//!
//! Restart safety: the last raw device counters are persisted in
//! `device_baseline` after every sample, so a daemon restart mid-session
//! resumes delta accounting without double-counting or losing progress.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{Local, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::ftms::TreadmillData;

/// Aggregated totals for a single calendar day (local time).
#[derive(Debug, Clone, Default)]
pub struct DailyStats {
    pub date: String,
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

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the SQLite database under
    /// `~/Library/Application Support/treadmill-bluetooth-macos/`.
    ///
    /// An absolute, `$HOME`-anchored path is required because the daemon runs
    /// under launchd, where the working directory cannot be relied on.
    pub fn open() -> Result<Self> {
        let path = db_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Self::open_at(&path)
    }

    /// Open a database at an arbitrary path (e.g. `:memory:` in tests).
    pub(crate) fn open_at(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS sessions (
                    id INTEGER PRIMARY KEY,
                    started_at TEXT NOT NULL,
                    ended_at   TEXT
                );
                CREATE TABLE IF NOT EXISTS daily_stats (
                    date TEXT PRIMARY KEY,
                    distance_m INTEGER NOT NULL DEFAULT 0,
                    steps INTEGER NOT NULL DEFAULT 0,
                    walking_time_s INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE IF NOT EXISTS device_baseline (
                    id INTEGER PRIMARY KEY CHECK (id = 0),
                    last_steps INTEGER,
                    last_distance_m INTEGER,
                    last_elapsed_s INTEGER
                );
                CREATE TABLE IF NOT EXISTS raw_samples (
                    id INTEGER PRIMARY KEY,
                    session_id INTEGER NOT NULL REFERENCES sessions(id),
                    ts_ms INTEGER NOT NULL,
                    speed_centikmh INTEGER,
                    avg_speed_centikmh INTEGER,
                    distance_m INTEGER,
                    energy_kcal INTEGER,
                    elapsed_s INTEGER,
                    steps INTEGER,
                    raw_frame BLOB NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_raw_samples_session ON raw_samples(session_id);
                CREATE TABLE IF NOT EXISTS status_events (
                    id INTEGER PRIMARY KEY,
                    session_id INTEGER NOT NULL REFERENCES sessions(id),
                    ts_ms INTEGER NOT NULL,
                    event_code INTEGER NOT NULL,
                    raw_frame BLOB NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_status_events_session ON status_events(session_id);
                ",
            )
            .context("run schema migration")?;
        Ok(())
    }

    /// Record the start of a new BLE connection to the treadmill. Returns the
    /// new session id, used to tag `raw_samples`/`status_events` rows.
    pub fn start_session(&self) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute("INSERT INTO sessions (started_at, ended_at) VALUES (?1, NULL)", params![now])
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
        let tx = self.conn.transaction().context("begin baseline transaction")?;

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
            steps: raw_steps.map(|v| delta_since(v as i64, last_steps)).unwrap_or(0),
            distance_m: raw_distance_m.map(|v| delta_since(v as i64, last_distance)).unwrap_or(0),
            elapsed_s: raw_elapsed_s.map(|v| delta_since(v as i64, last_elapsed)).unwrap_or(0),
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

    /// Credit an already-decided amount of walking to today's totals.
    pub fn credit_daily(&self, steps: i64, distance_m: i64, walking_time_s: i64) -> Result<()> {
        let today = Local::now().format("%Y-%m-%d").to_string();
        self.conn
            .execute(
                "INSERT INTO daily_stats (date, distance_m, steps, walking_time_s)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(date) DO UPDATE SET
                    distance_m = daily_stats.distance_m + excluded.distance_m,
                    steps = daily_stats.steps + excluded.steps,
                    walking_time_s = daily_stats.walking_time_s + excluded.walking_time_s",
                params![today, distance_m, steps, walking_time_s],
            )
            .context("credit daily_stats")?;
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
    pub fn insert_raw_sample(&self, session_id: i64, ts_ms: i64, sample: &TreadmillData, raw_frame: &[u8]) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO raw_samples
                    (session_id, ts_ms, speed_centikmh, avg_speed_centikmh, distance_m, energy_kcal, elapsed_s, steps, raw_frame)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    session_id,
                    ts_ms,
                    sample.speed_kmh.map(|v| (v * 100.0).round() as i64),
                    sample.avg_speed_kmh.map(|v| (v * 100.0).round() as i64),
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

    /// Persist one Fitness Machine Status (`0x2ADA`) event verbatim.
    ///
    /// `event_code` is the raw FTMS op code (first byte); its human-readable
    /// meaning lives in `ftms::describe_status_event` (code, not a DB column)
    /// so the mapping has one source of truth instead of drifting copies.
    pub fn insert_status_event(&self, session_id: i64, ts_ms: i64, event_code: u8, raw_frame: &[u8]) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO status_events (session_id, ts_ms, event_code, raw_frame) VALUES (?1, ?2, ?3, ?4)",
                params![session_id, ts_ms, event_code, raw_frame],
            )
            .context("insert status_events row")?;
        Ok(())
    }

    /// Totals for today (local calendar date), zeroed if nothing recorded yet.
    pub fn today_stats(&self) -> Result<DailyStats> {
        let today = Local::now().format("%Y-%m-%d").to_string();
        self.stats_for(&today).map(|opt| opt.unwrap_or(DailyStats { date: today, ..Default::default() }))
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
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect all_stats rows")
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

fn db_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("$HOME not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/treadmill-bluetooth-macos/treadmill.db"))
}

#[cfg(test)]
mod tests {
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
}
