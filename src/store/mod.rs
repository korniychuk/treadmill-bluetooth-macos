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

mod activity;
mod samples;
mod schema;

pub use activity::{DailyStats, RawDeltas, Segment, Workout, merge_segments};
pub use samples::{HrRow, HrSummary, RawSample};

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::control_command::ControlCommand;

/// Rows in `control_commands` older than this are pruned on every enqueue, so
/// the queue can never grow unbounded. Far larger than the CLI's ~8s poll and
/// the daemon's 30s staleness guard, so a still-relevant row is never dropped.
const CONTROL_COMMAND_RETENTION: Duration = Duration::minutes(5);

/// Persistent daemon status snapshot — one row, upserted on every transition
/// the daemon observes (connect/disconnect, presence change, power mode
/// change). Exists so a separate `status` CLI invocation (which must not
/// itself open the BLE adapter) can report state without racing the daemon
/// for the adapter. See `docs/tasks/006-...md`, задача B.
#[derive(Debug, Clone, Default)]
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
    /// Live belt speed snapshot (задача 029), same "daemon mirrors what it
    /// just observed" pattern as `last_bpm`/`last_bpm_ts` — updated on every
    /// telemetry sample regardless of Zone Hold (unlike `zone_hold_last_speed`,
    /// which is `None` whenever the controller isn't active). `last_speed_ts`
    /// is Unix millis, matching `last_bpm_ts`.
    pub last_speed_kmh: Option<f64>,
    pub last_speed_ts: Option<i64>,
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
                     zone_hold_last_speed, zone_hold_phase, zone_hold_position,
                     last_speed_kmh, last_speed_ts)
                 VALUES (0, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                         ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
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
                    zone_hold_position = excluded.zone_hold_position,
                    last_speed_kmh = excluded.last_speed_kmh,
                    last_speed_ts = excluded.last_speed_ts",
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
                    status.last_speed_kmh,
                    status.last_speed_ts,
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
                        zone_hold_last_speed, zone_hold_phase, zone_hold_position,
                        last_speed_kmh, last_speed_ts
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
                        last_speed_kmh: row.get(20)?,
                        last_speed_ts: row.get(21)?,
                    })
                },
            )
            .optional()
            .context("query daemon_status")
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

fn db_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("$HOME not set")?;
    Ok(PathBuf::from(home)
        .join("Library/Application Support/treadmill-bluetooth-macos/treadmill.db"))
}

#[cfg(test)]
pub(super) fn memory_store() -> Store {
    // Fix the process timezone so date-of-day assertions (in particular
    // the midnight-crossing test) don't depend on the machine/CI's local
    // timezone — chrono's `Local` reads `TZ` per call on unix.
    unsafe {
        std::env::set_var("TZ", "UTC");
    }
    Store::open_at(std::path::Path::new(":memory:")).expect("open in-memory store")
}

#[cfg(test)]
mod tests {
    use super::*;

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
            last_speed_kmh: Some(3.1),
            last_speed_ts: Some(1_720_000_000_500),
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
        assert_eq!(read_back.last_speed_kmh, Some(3.1));
        assert_eq!(read_back.last_speed_ts, Some(1_720_000_000_500));

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
