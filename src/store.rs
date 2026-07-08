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

use crate::control_command::ControlCommand;
use crate::default_speed::trimmed_mean_speed;
use crate::ftms::TreadmillData;
use crate::hr::HrMeasurement;

/// Rows in `control_commands` older than this are pruned on every enqueue, so
/// the queue can never grow unbounded. Far larger than the CLI's ~8s poll and
/// the daemon's 30s staleness guard, so a still-relevant row is never dropped.
const CONTROL_COMMAND_RETENTION: Duration = Duration::minutes(5);

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
    /// Snapshot of the config the daemon currently holds in memory, so `tm
    /// status` can show what is actually loaded and when (задача 022). All three
    /// are `None` on a row written by a pre-022 daemon (columns added by
    /// migration default to NULL) — the CLI then just omits the config line.
    /// `config_goals` is the comma-joined step thresholds (e.g. `8500,10750`);
    /// `config_auto_pause_secs` is the auto-pause threshold in seconds (`None` =
    /// disabled); `config_loaded_at` is the RFC3339 time of the last config read.
    pub config_goals: Option<String>,
    pub config_auto_pause_secs: Option<i64>,
    pub config_loaded_at: Option<String>,
    /// Heart-rate snapshot (задача 025), same pattern as the treadmill's own
    /// `connected`: the daemon upserts these on every HR sample/heartbeat so a
    /// separate `stats`/`widget`/`status` invocation can show "is the strap on
    /// right now" without racing the daemon for the BLE adapter. `last_bpm_ts`
    /// is a Unix millis timestamp (not RFC3339, unlike the other timestamp
    /// fields here) so freshness is a plain integer subtraction, matching
    /// `hr_samples.ts_ms`.
    pub hr_connected: bool,
    pub last_bpm: Option<i64>,
    pub last_bpm_ts: Option<i64>,
    /// HR sensor battery level, 0-100% (задача 026). `None` until the daemon
    /// has read it at least once this link (Polar devices only support Read,
    /// not notify, for this value — see `scan::read_hr_battery`).
    pub hr_battery_pct: Option<i64>,
    /// Zone Hold snapshot (задача 027), same "daemon mirrors what it just
    /// decided" pattern as the rest of this struct. `zone_hold_active` is true
    /// only while the controller is actually driving speed corrections (ramp
    /// or closed-loop) — not merely "enabled in config". `zone_hold_phase` is
    /// one of `ramp`/`hold`/`frozen`/`grace`/`off`. `zone_hold_position`
    /// (`below`/`in`/`above`) is what `tm widget`'s `HR_ZONE` field mirrors —
    /// `None` unless `zone_hold_active` is true, so the widget only colours
    /// the heart glyph while the controller is really in charge (§Индикация
    /// зоны in the task doc).
    pub zone_hold_active: bool,
    pub zone_hold_target_lo: Option<i64>,
    pub zone_hold_target_hi: Option<i64>,
    pub zone_hold_last_speed: Option<f64>,
    pub zone_hold_phase: Option<String>,
    pub zone_hold_position: Option<String>,
}

/// Per-sample deltas against the persisted device baseline. Not yet a
/// decision about whether they represent real walking — see module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawDeltas {
    pub steps: i64,
    pub distance_m: i64,
    pub elapsed_s: i64,
}

/// One decoded `raw_samples` row, in the shape the offline replay
/// (`crate::recompute`) needs to re-drive the presence + credit engine: the
/// device session it belongs to, its wall-clock timestamp, and the *cumulative*
/// device counters (already decoded back from their wire scale). See
/// `docs/tasks/015`.
#[derive(Debug, Clone)]
pub struct RawSample {
    pub session_id: i64,
    pub ts_ms: i64,
    pub speed_kmh: Option<f32>,
    pub distance_m: Option<u32>,
    pub elapsed_s: Option<u16>,
    pub steps: Option<u32>,
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

/// `control_commands.status` values. Kept as short string literals (not an
/// enum column) matching the rest of the schema's text-status style.
const CONTROL_STATUS_PENDING: &str = "pending";
const CONTROL_STATUS_DONE: &str = "done";
const CONTROL_STATUS_FAILED: &str = "failed";

/// A pending control command read back from the queue, ready to execute:
/// its row id (for marking the outcome), its enqueue time (for the daemon's
/// staleness guard), and the parsed command.
#[derive(Debug, Clone)]
pub struct QueuedControlCommand {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub command: ControlCommand,
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
        conn.busy_timeout(std::time::Duration::from_secs(3))
            .context("set busy_timeout")?;
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
                CREATE TABLE IF NOT EXISTS hr_samples (
                    id         INTEGER PRIMARY KEY,
                    session_id INTEGER REFERENCES sessions(id),
                    ts_ms      INTEGER NOT NULL,
                    bpm        INTEGER NOT NULL,
                    rr_ms      BLOB,
                    raw_frame  BLOB NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_hr_samples_ts ON hr_samples(ts_ms);
                CREATE TABLE IF NOT EXISTS status_events (
                    id INTEGER PRIMARY KEY,
                    session_id INTEGER NOT NULL REFERENCES sessions(id),
                    ts_ms INTEGER NOT NULL,
                    event_code INTEGER NOT NULL,
                    raw_frame BLOB NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_status_events_session ON status_events(session_id);
                CREATE TABLE IF NOT EXISTS activity_segments (
                    id INTEGER PRIMARY KEY,
                    started_at TEXT NOT NULL,
                    ended_at TEXT NOT NULL,
                    date TEXT NOT NULL,
                    distance_m INTEGER NOT NULL DEFAULT 0,
                    steps INTEGER NOT NULL DEFAULT 0,
                    walking_time_s INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX IF NOT EXISTS idx_activity_segments_date ON activity_segments(date);
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
                CREATE TABLE IF NOT EXISTS control_commands (
                    id INTEGER PRIMARY KEY,
                    created_at TEXT NOT NULL,
                    command TEXT NOT NULL,
                    status TEXT NOT NULL,
                    executed_at TEXT,
                    error TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_control_commands_status ON control_commands(status, id);
                ",
            )
            .context("run schema migration")?;

        // Loaded-config snapshot on daemon_status (задача 022). Added via ALTER so
        // an existing DB migrates in place — the CREATE above only fires on a
        // fresh install, so a long-lived install needs these columns backfilled.
        self.add_column_if_missing("ALTER TABLE daemon_status ADD COLUMN config_goals TEXT")?;
        self.add_column_if_missing(
            "ALTER TABLE daemon_status ADD COLUMN config_auto_pause_secs INTEGER",
        )?;
        self.add_column_if_missing("ALTER TABLE daemon_status ADD COLUMN config_loaded_at TEXT")?;

        // Heart-rate snapshot columns (задача 025). `hr_connected` defaults to 0
        // (not connected) so a pre-025 row reads as "no sensor" rather than NULL.
        self.add_column_if_missing(
            "ALTER TABLE daemon_status ADD COLUMN hr_connected INTEGER NOT NULL DEFAULT 0",
        )?;
        self.add_column_if_missing("ALTER TABLE daemon_status ADD COLUMN last_bpm INTEGER")?;
        self.add_column_if_missing("ALTER TABLE daemon_status ADD COLUMN last_bpm_ts INTEGER")?;
        self.add_column_if_missing("ALTER TABLE daemon_status ADD COLUMN hr_battery_pct INTEGER")?;

        // Zone Hold snapshot columns (задача 027). `zone_hold_active` defaults
        // to 0 (not active) so a pre-027 row reads as "not engaged" rather
        // than NULL, mirroring `hr_connected` above.
        self.add_column_if_missing(
            "ALTER TABLE daemon_status ADD COLUMN zone_hold_active INTEGER NOT NULL DEFAULT 0",
        )?;
        self.add_column_if_missing(
            "ALTER TABLE daemon_status ADD COLUMN zone_hold_target_lo INTEGER",
        )?;
        self.add_column_if_missing(
            "ALTER TABLE daemon_status ADD COLUMN zone_hold_target_hi INTEGER",
        )?;
        self.add_column_if_missing(
            "ALTER TABLE daemon_status ADD COLUMN zone_hold_last_speed REAL",
        )?;
        self.add_column_if_missing("ALTER TABLE daemon_status ADD COLUMN zone_hold_phase TEXT")?;
        self.add_column_if_missing("ALTER TABLE daemon_status ADD COLUMN zone_hold_position TEXT")?;
        Ok(())
    }

    /// Add a column if it isn't already there. SQLite has no `ADD COLUMN IF NOT
    /// EXISTS`, so a "duplicate column name" failure is the expected no-op on an
    /// already-migrated DB; any other error propagates (задача 022).
    fn add_column_if_missing(&self, alter_sql: &str) -> Result<()> {
        match self.conn.execute(alter_sql, []) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                if msg.contains("duplicate column name") =>
            {
                Ok(())
            }
            Err(err) => Err(err).with_context(|| format!("run migration: {alter_sql}")),
        }
    }

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
    /// `open_segment` is the id of the segment the daemon considers open (from
    /// its in-memory `current_segment`): `Some(id)` extends it, `None` opens a
    /// new segment. Returns the id of the segment credited (the existing one, or
    /// the freshly inserted one), which the caller stores back as its open
    /// segment. There is no `open` flag in the DB — "open" lives in daemon
    /// memory, so a daemon restart simply starts a new segment and read-time
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
        open_segment: Option<i64>,
    ) -> Result<i64> {
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

        // Extend the open segment if the daemon still holds one *and* it really
        // exists (a stale id — e.g. a wiped DB under a long-lived daemon — must
        // not silently no-op the UPDATE and lose the credit); otherwise open a
        // new segment.
        let extended = match open_segment {
            Some(id) => {
                let rows = tx
                    .execute(
                        "UPDATE activity_segments SET
                            ended_at = ?1,
                            distance_m = distance_m + ?2,
                            steps = steps + ?3,
                            walking_time_s = walking_time_s + ?4
                         WHERE id = ?5",
                        params![now.to_rfc3339(), distance_m, steps, walking_time_s, id],
                    )
                    .context("extend activity segment")?;
                if rows == 0 {
                    tracing::warn!(
                        id,
                        "open segment id not found — opening a new segment instead"
                    );
                    None
                } else {
                    Some(id)
                }
            }
            None => None,
        };

        let segment_id = match extended {
            Some(id) => id,
            None => {
                tx.execute(
                    "INSERT INTO activity_segments (started_at, ended_at, date, distance_m, steps, walking_time_s)
                     VALUES (?1, ?1, ?2, ?3, ?4, ?5)",
                    params![now.to_rfc3339(), today, distance_m, steps, walking_time_s],
                )
                .context("insert activity segment")?;
                tx.last_insert_rowid()
            }
        };

        tx.commit().context("commit credit_activity transaction")?;
        Ok(segment_id)
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
                    // Stored as centi-km/h (0.01 km/h) integer — mirror the
                    // encode in `insert_raw_sample`.
                    speed_kmh: speed_centikmh.map(|c| c as f32 / 100.0),
                    distance_m: distance_m.map(|v| v as u32),
                    elapsed_s: elapsed_s.map(|v| v as u16),
                    steps: steps.map(|v| v as u32),
                })
            })
            .context("run raw_samples_ordered query")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("collect raw_samples_ordered rows")
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

    /// Upsert the single `daemon_status` row (id=0), same pattern as
    /// `device_baseline` — the daemon calls this on every observed
    /// transition (connect/disconnect, presence change, power mode change).
    pub fn upsert_daemon_status(&self, status: &DaemonStatus) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO daemon_status
                    (id, connected, presence_state, last_connected_at, last_disconnected_at,
                     power_mode, power_mode_since, updated_at,
                     config_goals, config_auto_pause_secs, config_loaded_at,
                     hr_connected, last_bpm, last_bpm_ts, hr_battery_pct,
                     zone_hold_active, zone_hold_target_lo, zone_hold_target_hi,
                     zone_hold_last_speed, zone_hold_phase, zone_hold_position)
                 VALUES (0, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                         ?15, ?16, ?17, ?18, ?19, ?20)
                 ON CONFLICT(id) DO UPDATE SET
                    connected = excluded.connected,
                    presence_state = excluded.presence_state,
                    last_connected_at = excluded.last_connected_at,
                    last_disconnected_at = excluded.last_disconnected_at,
                    power_mode = excluded.power_mode,
                    power_mode_since = excluded.power_mode_since,
                    updated_at = excluded.updated_at,
                    config_goals = excluded.config_goals,
                    config_auto_pause_secs = excluded.config_auto_pause_secs,
                    config_loaded_at = excluded.config_loaded_at,
                    hr_connected = excluded.hr_connected,
                    last_bpm = excluded.last_bpm,
                    last_bpm_ts = excluded.last_bpm_ts,
                    hr_battery_pct = excluded.hr_battery_pct,
                    zone_hold_active = excluded.zone_hold_active,
                    zone_hold_target_lo = excluded.zone_hold_target_lo,
                    zone_hold_target_hi = excluded.zone_hold_target_hi,
                    zone_hold_last_speed = excluded.zone_hold_last_speed,
                    zone_hold_phase = excluded.zone_hold_phase,
                    zone_hold_position = excluded.zone_hold_position",
                params![
                    status.connected,
                    status.presence_state,
                    status.last_connected_at,
                    status.last_disconnected_at,
                    status.power_mode,
                    status.power_mode_since,
                    status.updated_at,
                    status.config_goals,
                    status.config_auto_pause_secs,
                    status.config_loaded_at,
                    status.hr_connected,
                    status.last_bpm,
                    status.last_bpm_ts,
                    status.hr_battery_pct,
                    status.zone_hold_active,
                    status.zone_hold_target_lo,
                    status.zone_hold_target_hi,
                    status.zone_hold_last_speed,
                    status.zone_hold_phase,
                    status.zone_hold_position,
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
                        power_mode, power_mode_since, updated_at,
                        config_goals, config_auto_pause_secs, config_loaded_at,
                        hr_connected, last_bpm, last_bpm_ts, hr_battery_pct,
                        zone_hold_active, zone_hold_target_lo, zone_hold_target_hi,
                        zone_hold_last_speed, zone_hold_phase, zone_hold_position
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
                        config_goals: row.get(7)?,
                        config_auto_pause_secs: row.get(8)?,
                        config_loaded_at: row.get(9)?,
                        hr_connected: row.get(10)?,
                        last_bpm: row.get(11)?,
                        last_bpm_ts: row.get(12)?,
                        hr_battery_pct: row.get(13)?,
                        zone_hold_active: row.get(14)?,
                        zone_hold_target_lo: row.get(15)?,
                        zone_hold_target_hi: row.get(16)?,
                        zone_hold_last_speed: row.get(17)?,
                        zone_hold_phase: row.get(18)?,
                        zone_hold_position: row.get(19)?,
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

    /// Enqueue a control command for the daemon to execute on its live BLE
    /// link (the CLI cannot open its own connection while the daemon holds
    /// the treadmill — see `docs/tasks/013`). Prunes stale rows first so the
    /// queue can never grow unbounded, then returns the new row id so the CLI
    /// can poll its outcome.
    pub fn enqueue_control_command(&self, command: &ControlCommand) -> Result<i64> {
        self.prune_control_commands()?;
        let now = Utc::now().to_rfc3339();
        self.conn
            .execute(
                "INSERT INTO control_commands (created_at, command, status) VALUES (?1, ?2, ?3)",
                params![now, command.to_wire(), CONTROL_STATUS_PENDING],
            )
            .context("enqueue control command")?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Delete resolved/abandoned command rows older than
    /// [`CONTROL_COMMAND_RETENTION`]. Bounds table growth; the cutoff is far
    /// past both the CLI poll window and the daemon staleness guard, so no
    /// still-relevant row is dropped.
    fn prune_control_commands(&self) -> Result<()> {
        let cutoff = (Utc::now() - CONTROL_COMMAND_RETENTION).to_rfc3339();
        self.conn
            .execute(
                "DELETE FROM control_commands WHERE created_at < ?1",
                params![cutoff],
            )
            .context("prune old control commands")?;
        Ok(())
    }

    /// The oldest still-pending control command, parsed. An unparseable row
    /// (corrupt `created_at` or `command`) is a poison pill — it would fail
    /// forever on every poll — so it is marked `failed` and skipped here
    /// rather than propagated, and the next pending row is tried instead.
    pub fn next_pending_control_command(&self) -> Result<Option<QueuedControlCommand>> {
        loop {
            let row: Option<(i64, String, String)> = self
                .conn
                .query_row(
                    "SELECT id, created_at, command FROM control_commands
                     WHERE status = ?1 ORDER BY id ASC LIMIT 1",
                    params![CONTROL_STATUS_PENDING],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .context("query next pending control command")?;
            let Some((id, created_at_s, command_s)) = row else {
                return Ok(None);
            };

            let parsed = DateTime::parse_from_rfc3339(&created_at_s)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(anyhow::Error::from)
                .and_then(|created_at| {
                    ControlCommand::parse(&command_s).map(|command| (created_at, command))
                });
            match parsed {
                Ok((created_at, command)) => {
                    return Ok(Some(QueuedControlCommand {
                        id,
                        created_at,
                        command,
                    }));
                }
                Err(err) => {
                    tracing::warn!(%err, id, command = %command_s, "unparseable control command row — failing and skipping");
                    self.mark_control_command_failed(id, &format!("unparseable: {err}"))?;
                }
            }
        }
    }

    /// Mark a command executed successfully.
    pub fn mark_control_command_done(&self, id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE control_commands SET status = ?1, executed_at = ?2, error = NULL WHERE id = ?3",
                params![CONTROL_STATUS_DONE, Utc::now().to_rfc3339(), id],
            )
            .context("mark control command done")?;
        Ok(())
    }

    /// Mark a command failed, recording the reason (a BLE write error/timeout,
    /// or the staleness guard) so the CLI can surface it.
    pub fn mark_control_command_failed(&self, id: i64, error: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE control_commands SET status = ?1, executed_at = ?2, error = ?3 WHERE id = ?4",
                params![CONTROL_STATUS_FAILED, Utc::now().to_rfc3339(), error, id],
            )
            .context("mark control command failed")?;
        Ok(())
    }

    /// Current `(status, error)` of a queued command, or `None` if the row is
    /// gone (pruned). Backs the CLI's poll-until-resolved wait.
    pub fn control_command_outcome(&self, id: i64) -> Result<Option<(String, Option<String>)>> {
        self.conn
            .query_row(
                "SELECT status, error FROM control_commands WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .context("query control command outcome")
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

/// The 95th percentile of an ascending-sorted slice — a "peak effort" bpm
/// figure robust against a single sensor spike, unlike a raw maximum. Nearest-
/// rank method, clamped to the last index. Panics on an empty slice (callers
/// only reach this after checking [`MIN_HR_SAMPLES_FOR_SUMMARY`]).
fn percentile_95(sorted_ascending: &[f32]) -> f32 {
    let n = sorted_ascending.len();
    let idx = (((n - 1) as f32) * 0.95).round() as usize;
    sorted_ascending[idx.min(n - 1)]
}

fn db_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("$HOME not set")?;
    Ok(PathBuf::from(home)
        .join("Library/Application Support/treadmill-bluetooth-macos/treadmill.db"))
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

    /// Credit one walking burst, threading the daemon's open-segment id.
    fn credit(
        store: &mut Store,
        steps: i64,
        dist: i64,
        time: i64,
        now: DateTime<Utc>,
        open: Option<i64>,
    ) -> i64 {
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
        let id = credit(&mut store, 5, 10, 30, t0, None);
        let id2 = credit(&mut store, 5, 10, 30, t1, Some(id));

        assert_eq!(id, id2, "extending, not opening a new segment");
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
        let id = credit(&mut store, 5, 10, 30, t0, None);
        let id2 = credit(&mut store, 5, 10, 30, t1, None);

        assert_ne!(id, id2);
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
        let id2 = credit(&mut store, 7, 14, 42, t1, Some(999_999));
        assert_ne!(id2, 999_999);
        let segs = store.all_segments_asc().unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(
            segs.iter().map(|s| s.steps).sum::<i64>(),
            12,
            "both credits landed"
        );
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
            config_goals: Some("8500,10750,13000".to_string()),
            config_auto_pause_secs: Some(300),
            config_loaded_at: Some("2026-07-05T09:59:00+00:00".to_string()),
            hr_connected: true,
            last_bpm: Some(118),
            last_bpm_ts: Some(1_720_000_000_000),
            hr_battery_pct: Some(42),
            zone_hold_active: true,
            zone_hold_target_lo: Some(112),
            zone_hold_target_hi: Some(131),
            zone_hold_last_speed: Some(3.2),
            zone_hold_phase: Some("hold".to_string()),
            zone_hold_position: Some("in".to_string()),
        };
        store.upsert_daemon_status(&status).unwrap();

        let read_back = store.daemon_status().unwrap().expect("status row present");
        assert!(read_back.connected);
        assert_eq!(read_back.presence_state.as_deref(), Some("Walking"));
        // Loaded-config snapshot round-trips (задача 022).
        assert_eq!(read_back.config_goals.as_deref(), Some("8500,10750,13000"));
        assert_eq!(read_back.config_auto_pause_secs, Some(300));
        assert_eq!(
            read_back.config_loaded_at.as_deref(),
            Some("2026-07-05T09:59:00+00:00")
        );
        assert!(read_back.hr_connected);
        assert_eq!(read_back.last_bpm, Some(118));
        assert_eq!(read_back.last_bpm_ts, Some(1_720_000_000_000));
        assert_eq!(read_back.hr_battery_pct, Some(42));
        assert!(read_back.zone_hold_active);
        assert_eq!(read_back.zone_hold_target_lo, Some(112));
        assert_eq!(read_back.zone_hold_target_hi, Some(131));
        assert_eq!(read_back.zone_hold_last_speed, Some(3.2));
        assert_eq!(read_back.zone_hold_phase.as_deref(), Some("hold"));
        assert_eq!(read_back.zone_hold_position.as_deref(), Some("in"));

        // Second upsert overwrites in place — still exactly one row (id=0).
        let status2 = DaemonStatus {
            connected: false,
            ..status
        };
        store.upsert_daemon_status(&status2).unwrap();
        let read_back2 = store.daemon_status().unwrap().expect("status row present");
        assert!(!read_back2.connected);
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

    #[test]
    fn control_command_enqueue_pending_then_done() {
        // arr
        let store = memory_store();
        let id = store
            .enqueue_control_command(&ControlCommand::Speed(2.5))
            .unwrap();

        // act — the freshly enqueued command is the oldest pending one.
        let pending = store
            .next_pending_control_command()
            .unwrap()
            .expect("one pending");

        // assert — round-tripped through the wire form, then transitions to done.
        assert_eq!(pending.id, id);
        assert_eq!(pending.command, ControlCommand::Speed(2.5));
        assert_eq!(
            store.control_command_outcome(id).unwrap().unwrap().0,
            "pending"
        );
        store.mark_control_command_done(id).unwrap();
        let (status, error) = store.control_command_outcome(id).unwrap().unwrap();
        assert_eq!(status, "done");
        assert_eq!(error, None);
        assert!(
            store.next_pending_control_command().unwrap().is_none(),
            "no longer pending"
        );
    }

    #[test]
    fn control_command_mark_failed_records_error() {
        let store = memory_store();
        let id = store
            .enqueue_control_command(&ControlCommand::Start)
            .unwrap();
        store
            .mark_control_command_failed(id, "stale, not executed")
            .unwrap();
        let (status, error) = store.control_command_outcome(id).unwrap().unwrap();
        assert_eq!(status, "failed");
        assert_eq!(error.as_deref(), Some("stale, not executed"));
    }

    #[test]
    fn next_pending_control_command_returns_oldest_first() {
        let store = memory_store();
        let first = store
            .enqueue_control_command(&ControlCommand::Start)
            .unwrap();
        let _second = store
            .enqueue_control_command(&ControlCommand::Stop)
            .unwrap();

        // Oldest (lowest id) comes first; failing it surfaces the next.
        assert_eq!(
            store.next_pending_control_command().unwrap().unwrap().id,
            first
        );
        store.mark_control_command_done(first).unwrap();
        assert_eq!(
            store
                .next_pending_control_command()
                .unwrap()
                .unwrap()
                .command,
            ControlCommand::Stop
        );
    }

    #[test]
    fn next_pending_control_command_skips_corrupt_rows() {
        // arr — inject a row the parser can't decode (bypassing enqueue).
        let store = memory_store();
        store
            .conn
            .execute(
                "INSERT INTO control_commands (created_at, command, status) VALUES (?1, ?2, ?3)",
                params![
                    Utc::now().to_rfc3339(),
                    "speed:not-a-number",
                    CONTROL_STATUS_PENDING
                ],
            )
            .unwrap();
        let good = store
            .enqueue_control_command(&ControlCommand::Stop)
            .unwrap();

        // act / assert — the poison row is failed and skipped, the good one surfaces.
        let pending = store
            .next_pending_control_command()
            .unwrap()
            .expect("good command surfaces");
        assert_eq!(pending.id, good);
    }

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

    #[test]
    fn enqueue_prunes_rows_older_than_retention() {
        // arr — an ancient row plus a fresh enqueue that triggers the prune.
        let store = memory_store();
        let ancient = (Utc::now() - Duration::minutes(10)).to_rfc3339();
        store
            .conn
            .execute(
                "INSERT INTO control_commands (created_at, command, status) VALUES (?1, ?2, ?3)",
                params![ancient, "start", CONTROL_STATUS_DONE],
            )
            .unwrap();

        // act
        store
            .enqueue_control_command(&ControlCommand::Start)
            .unwrap();

        // assert — only the fresh row survives; the queue stays bounded.
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM control_commands", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
