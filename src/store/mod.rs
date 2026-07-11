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
mod status;

pub use activity::{DailyStats, RawDeltas, Segment, Workout, merge_segments};
pub use samples::{HrRow, HrSummary, RawSample};
pub use status::DaemonStatus;

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::control_command::ControlCommand;

/// Rows in `control_commands` older than this are pruned on every enqueue, so
/// the queue can never grow unbounded. Far larger than the CLI's ~8s poll and
/// the daemon's 30s staleness guard, so a still-relevant row is never dropped.
const CONTROL_COMMAND_RETENTION: Duration = Duration::minutes(5);

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
