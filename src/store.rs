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
use chrono::{DateTime, Duration, Local, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::ftms::TreadmillData;

/// Gap between two credited activity bursts past which we consider it a new
/// workout rather than a continuation (see module docs on `credit_activity`).
const WORKOUT_GAP_THRESHOLD_S: i64 = 15 * 60;

/// Aggregated totals for a single calendar day (local time).
#[derive(Debug, Clone, Default)]
pub struct DailyStats {
    pub date: String,
    pub distance_m: i64,
    pub steps: i64,
    pub walking_time_s: i64,
}

/// One contiguous burst of walking, split from adjacent bursts by a gap of
/// more than [`WORKOUT_GAP_THRESHOLD_S`] with no credited activity.
///
/// `date` is fixed to the calendar date (local time) of `started_at` and
/// never changes, even if the workout is still open when the credit that
/// extends `ended_at` crosses into the next calendar day — see
/// `credit_activity` docs for the midnight-crossing edge case this implies
/// for `daily_stats` vs. `workouts` reconciliation.
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

/// Persistent daemon status snapshot — one row, upserted on every transition
/// the daemon observes (connect/disconnect, presence change, power mode
/// change). Exists so a separate `status` CLI invocation (which must not
/// itself open the BLE adapter) can report state without racing the daemon
/// for the adapter. See `docs/tasks/006-...md`, задача B.
#[derive(Debug, Clone)]
pub struct DaemonStatus {
    pub connected: bool,
    pub presence_state: Option<String>,
    pub last_connected_at: Option<String>,
    pub last_disconnected_at: Option<String>,
    pub power_mode: String,
    pub power_mode_since: String,
    pub updated_at: String,
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
        // A short-lived reader (e.g. the 2s `widget` poll) can open the DB while
        // the daemon is mid-write; wait out the write lock instead of erroring
        // with SQLITE_BUSY.
        conn.busy_timeout(std::time::Duration::from_secs(3)).context("set busy_timeout")?;
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
                CREATE TABLE IF NOT EXISTS workouts (
                    id INTEGER PRIMARY KEY,
                    date TEXT NOT NULL,
                    started_at TEXT NOT NULL,
                    ended_at TEXT NOT NULL,
                    distance_m INTEGER NOT NULL DEFAULT 0,
                    steps INTEGER NOT NULL DEFAULT 0,
                    walking_time_s INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX IF NOT EXISTS idx_workouts_date ON workouts(date);
                CREATE TABLE IF NOT EXISTS daemon_status (
                    id INTEGER PRIMARY KEY CHECK (id = 0),
                    connected INTEGER NOT NULL,
                    presence_state TEXT,
                    last_connected_at TEXT,
                    last_disconnected_at TEXT,
                    power_mode TEXT NOT NULL,
                    power_mode_since TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS goal_celebrations (
                    date TEXT NOT NULL,
                    threshold INTEGER NOT NULL,
                    celebrated_at TEXT NOT NULL,
                    PRIMARY KEY (date, threshold)
                );
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

    /// Credit an already-decided amount of walking to both today's totals
    /// (`daily_stats`, unchanged behavior) and the current workout
    /// (`workouts`), splitting into a new workout row when the gap since the
    /// most recently credited activity exceeds [`WORKOUT_GAP_THRESHOLD_S`]
    /// (or none has been credited yet). Both updates happen in one
    /// transaction so a crash mid-credit cannot leave them disagreeing.
    ///
    /// `now` is threaded through explicitly (rather than calling
    /// `Utc::now()` internally, like the rest of this module) so the workout
    /// split/continue decision is deterministic in tests.
    ///
    /// There is no explicit "closed" flag on `workouts` rows — the most
    /// recently started row is implicitly the open one; "closing" it just
    /// means the next credit starts a new row instead of extending it.
    ///
    /// Midnight-crossing edge case: `workouts.date` is fixed to the calendar
    /// date of `started_at` and never changes while the workout is extended,
    /// even past local midnight. So "daily total == sum of that day's
    /// workouts" holds for the day a workout *started* in, but not
    /// necessarily for the day it *ended* in if those differ — `daily_stats`
    /// still gets the correct split by calendar day of each individual
    /// credit, since it does not know about workout boundaries at all.
    pub fn credit_activity(&mut self, steps: i64, distance_m: i64, walking_time_s: i64, now: DateTime<Utc>) -> Result<()> {
        let today = now.with_timezone(&Local).format("%Y-%m-%d").to_string();
        let tx = self.conn.transaction().context("begin credit_activity transaction")?;

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

        let latest: Option<(i64, String)> = tx
            .query_row("SELECT id, ended_at FROM workouts ORDER BY id DESC LIMIT 1", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()
            .context("read latest workout")?;

        let continues_latest = latest.as_ref().is_some_and(|(_, ended_at)| {
            match DateTime::parse_from_rfc3339(ended_at) {
                Ok(prev) => now - prev.with_timezone(&Utc) <= Duration::seconds(WORKOUT_GAP_THRESHOLD_S),
                Err(err) => {
                    // Corrupt/foreign timestamp — treat as a gap rather than
                    // fail the whole credit; log so it's not silently masked.
                    tracing::warn!(%err, ended_at, "workouts.ended_at not RFC3339, starting a new workout");
                    false
                }
            }
        });

        if let Some((id, _)) = latest.filter(|_| continues_latest) {
            tx.execute(
                "UPDATE workouts SET
                    ended_at = ?1,
                    distance_m = distance_m + ?2,
                    steps = steps + ?3,
                    walking_time_s = walking_time_s + ?4
                 WHERE id = ?5",
                params![now.to_rfc3339(), distance_m, steps, walking_time_s, id],
            )
            .context("extend workout")?;
        } else {
            tx.execute(
                "INSERT INTO workouts (date, started_at, ended_at, distance_m, steps, walking_time_s)
                 VALUES (?1, ?2, ?2, ?3, ?4, ?5)",
                params![today, now.to_rfc3339(), distance_m, steps, walking_time_s],
            )
            .context("insert workout")?;
        }

        tx.commit().context("commit credit_activity transaction")?;
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

    /// Workouts whose `date` (start day) is the given `YYYY-MM-DD`, oldest first.
    pub fn workouts_for(&self, date: &str) -> Result<Vec<Workout>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, date, started_at, ended_at, distance_m, steps, walking_time_s
                 FROM workouts WHERE date = ?1 ORDER BY id ASC",
            )
            .context("prepare workouts_for query")?;
        let rows = stmt.query_map(params![date], workout_from_row).context("run workouts_for query")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect workouts_for rows")
    }

    /// All recorded workouts, most recently started first.
    pub fn all_workouts(&self) -> Result<Vec<Workout>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, date, started_at, ended_at, distance_m, steps, walking_time_s
                 FROM workouts ORDER BY id DESC",
            )
            .context("prepare all_workouts query")?;
        let rows = stmt.query_map([], workout_from_row).context("run all_workouts query")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect all_workouts rows")
    }

    /// The most recently started workout, or `None` if none recorded. Backs the
    /// `widget` command, which surfaces the current session's live metrics.
    pub fn latest_workout(&self) -> Result<Option<Workout>> {
        self.conn
            .query_row(
                "SELECT id, date, started_at, ended_at, distance_m, steps, walking_time_s
                 FROM workouts ORDER BY id DESC LIMIT 1",
                [],
                workout_from_row,
            )
            .optional()
            .context("query latest_workout")
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
        let start_ms = DateTime::parse_from_rfc3339(started_at).context("parse workout started_at")?.timestamp_millis();
        let end_ms = DateTime::parse_from_rfc3339(ended_at).context("parse workout ended_at")?.timestamp_millis();
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

    /// Upsert the single `daemon_status` row (id=0), same pattern as
    /// `device_baseline` — the daemon calls this on every observed
    /// transition (connect/disconnect, presence change, power mode change).
    pub fn upsert_daemon_status(&self, status: &DaemonStatus) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO daemon_status
                    (id, connected, presence_state, last_connected_at, last_disconnected_at,
                     power_mode, power_mode_since, updated_at)
                 VALUES (0, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                    connected = excluded.connected,
                    presence_state = excluded.presence_state,
                    last_connected_at = excluded.last_connected_at,
                    last_disconnected_at = excluded.last_disconnected_at,
                    power_mode = excluded.power_mode,
                    power_mode_since = excluded.power_mode_since,
                    updated_at = excluded.updated_at",
                params![
                    status.connected,
                    status.presence_state,
                    status.last_connected_at,
                    status.last_disconnected_at,
                    status.power_mode,
                    status.power_mode_since,
                    status.updated_at,
                ],
            )
            .context("upsert daemon_status")?;
        Ok(())
    }

    /// Read the single `daemon_status` row, or `None` if the daemon has
    /// never written one yet (e.g. fresh install, daemon never ran).
    pub fn daemon_status(&self) -> Result<Option<DaemonStatus>> {
        self.conn
            .query_row(
                "SELECT connected, presence_state, last_connected_at, last_disconnected_at,
                        power_mode, power_mode_since, updated_at
                 FROM daemon_status WHERE id = 0",
                [],
                |row| {
                    Ok(DaemonStatus {
                        connected: row.get(0)?,
                        presence_state: row.get(1)?,
                        last_connected_at: row.get(2)?,
                        last_disconnected_at: row.get(3)?,
                        power_mode: row.get(4)?,
                        power_mode_since: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                },
            )
            .optional()
            .context("query daemon_status")
    }

    /// Step-goal thresholds already celebrated on the given `YYYY-MM-DD`
    /// (local calendar date). Read before deciding what to fire so a daemon
    /// restart mid-day never re-celebrates a goal (задача 011).
    pub fn celebrated_thresholds(&self, date: &str) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT threshold FROM goal_celebrations WHERE date = ?1")
            .context("prepare celebrated_thresholds query")?;
        let rows = stmt.query_map(params![date], |row| row.get(0)).context("run celebrated_thresholds query")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect celebrated_thresholds rows")
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

fn workout_from_row(row: &rusqlite::Row) -> rusqlite::Result<Workout> {
    Ok(Workout {
        id: row.get(0)?,
        date: row.get(1)?,
        started_at: row.get(2)?,
        ended_at: row.get(3)?,
        distance_m: row.get(4)?,
        steps: row.get(5)?,
        walking_time_s: row.get(6)?,
    })
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
    use std::path::Path;

    use chrono::TimeZone;

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

    fn memory_store() -> Store {
        // Fix the process timezone so date-of-day assertions (in particular
        // the midnight-crossing test) don't depend on the machine/CI's local
        // timezone — chrono's `Local` reads `TZ` per call on unix.
        unsafe {
            std::env::set_var("TZ", "UTC");
        }
        Store::open_at(Path::new(":memory:")).expect("open in-memory store")
    }

    #[test]
    fn credit_activity_continues_workout_within_gap_threshold() {
        let mut store = memory_store();
        let t0 = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();
        let t1 = t0 + Duration::minutes(10); // < 15 min threshold

        store.credit_activity(5, 10, 30, t0).unwrap();
        store.credit_activity(5, 10, 30, t1).unwrap();

        let workouts = store.all_workouts().unwrap();
        assert_eq!(workouts.len(), 1, "single continuous workout, not two");
        assert_eq!(workouts[0].steps, 10);
        assert_eq!(workouts[0].distance_m, 20);
        assert_eq!(workouts[0].walking_time_s, 60);
        assert_eq!(workouts[0].started_at, t0.to_rfc3339());
        assert_eq!(workouts[0].ended_at, t1.to_rfc3339());
    }

    #[test]
    fn credit_activity_splits_after_gap_exceeds_threshold() {
        let mut store = memory_store();
        let t0 = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();
        let t1 = t0 + Duration::minutes(20); // > 15 min threshold

        store.credit_activity(5, 10, 30, t0).unwrap();
        store.credit_activity(5, 10, 30, t1).unwrap();

        let workouts = store.all_workouts().unwrap();
        assert_eq!(workouts.len(), 2, "gap past the threshold must start a new workout");
        // all_workouts is most-recent-first.
        assert_eq!(workouts[0].started_at, t1.to_rfc3339());
        assert_eq!(workouts[1].started_at, t0.to_rfc3339());
    }

    #[test]
    fn credit_activity_first_workout_of_day_has_no_prior_activity() {
        let mut store = memory_store();
        let t0 = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();

        store.credit_activity(5, 10, 30, t0).unwrap();

        let workouts = store.all_workouts().unwrap();
        assert_eq!(workouts.len(), 1);
        assert_eq!(workouts[0].date, "2026-07-05");
        assert_eq!(workouts[0].steps, 5);
    }

    #[test]
    fn credit_activity_across_midnight_keeps_start_date_but_splits_daily_stats() {
        let mut store = memory_store();
        // One continuous workout (each gap well under 15 min) that starts
        // before local midnight and ends after it.
        let before_midnight = Utc.with_ymd_and_hms(2026, 7, 5, 23, 50, 0).unwrap();
        let after_midnight = Utc.with_ymd_and_hms(2026, 7, 6, 0, 0, 0).unwrap(); // 10 min gap, < 15 min threshold

        store.credit_activity(10, 20, 60, before_midnight).unwrap();
        store.credit_activity(20, 40, 120, after_midnight).unwrap();

        let workouts = store.all_workouts().unwrap();
        assert_eq!(workouts.len(), 1, "single workout spanning midnight");
        assert_eq!(workouts[0].date, "2026-07-05", "date is fixed to the start day");
        assert_eq!(workouts[0].steps, 30);
        assert_eq!(workouts[0].ended_at, after_midnight.to_rfc3339());

        // daily_stats, in contrast, is genuinely split by calendar day.
        let day1 = store.stats_for("2026-07-05").unwrap().expect("day1 stats present");
        assert_eq!(day1.steps, 10);
        assert_eq!(day1.distance_m, 20);
        let day2 = store.stats_for("2026-07-06").unwrap().expect("day2 stats present");
        assert_eq!(day2.steps, 20);
        assert_eq!(day2.distance_m, 40);
    }

    #[test]
    fn daemon_status_upsert_roundtrips() {
        let store = memory_store();
        assert!(store.daemon_status().unwrap().is_none());

        let status = DaemonStatus {
            connected: true,
            presence_state: Some("Walking".to_string()),
            last_connected_at: Some("2026-07-05T10:00:00+00:00".to_string()),
            last_disconnected_at: None,
            power_mode: "ac_scanning".to_string(),
            power_mode_since: "2026-07-05T09:00:00+00:00".to_string(),
            updated_at: "2026-07-05T10:00:01+00:00".to_string(),
        };
        store.upsert_daemon_status(&status).unwrap();

        let read_back = store.daemon_status().unwrap().expect("status row present");
        assert!(read_back.connected);
        assert_eq!(read_back.presence_state.as_deref(), Some("Walking"));

        // Second upsert overwrites in place — still exactly one row (id=0).
        let status2 = DaemonStatus { connected: false, ..status };
        store.upsert_daemon_status(&status2).unwrap();
        let read_back2 = store.daemon_status().unwrap().expect("status row present");
        assert!(!read_back2.connected);
    }

    #[test]
    fn goal_celebrations_are_idempotent_and_scoped_to_date() {
        let store = memory_store();
        assert!(store.celebrated_thresholds("2026-07-05").unwrap().is_empty());

        store.mark_goal_celebrated("2026-07-05", 8000).unwrap();
        store.mark_goal_celebrated("2026-07-05", 10000).unwrap();
        // Re-marking the same (date, threshold) must not duplicate or error.
        store.mark_goal_celebrated("2026-07-05", 8000).unwrap();

        let mut today = store.celebrated_thresholds("2026-07-05").unwrap();
        today.sort_unstable();
        assert_eq!(today, vec![8000, 10000]);

        // A different day starts fresh.
        assert!(store.celebrated_thresholds("2026-07-06").unwrap().is_empty());
    }
}
