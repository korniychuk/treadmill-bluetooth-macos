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
mod control_queue;
mod samples;
mod schema;
mod status;

pub use activity::{DailyStats, RawDeltas, Segment, Workout, merge_segments};
// Re-exported for public API parity even when no local path names the type.
#[allow(unused_imports)]
pub use control_queue::QueuedControlCommand;
pub use samples::{HrRow, HrSummary, RawSample};
pub use status::DaemonStatus;

use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::Connection;

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
        // WAL keeps readers (the 2s `widget` poll, `tm stats`) from blocking
        // the daemon's writes entirely: under the default rollback journal a
        // slow reader on a loaded machine held its SHARED lock past the 3s
        // busy_timeout and `advance_baseline` failed mid-workout, tearing
        // down the live BLE stream (backlog 010). WAL is persistent per DB
        // file, so this is a one-time flip. `:memory:` reports "memory".
        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .context("set journal_mode=WAL")?;
        if journal_mode != "wal" && journal_mode != "memory" {
            tracing::warn!(journal_mode, "unexpected journal_mode after requesting WAL");
        }
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
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
    fn open_at_enables_wal_on_file_backed_db() {
        let dir = std::env::temp_dir().join(format!("tm-wal-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("treadmill.db");
        let store = Store::open_at(&path).expect("open file-backed store");
        let mode: String = store
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("read journal_mode");
        assert_eq!(mode, "wal");
        drop(store);
        std::fs::remove_dir_all(&dir).expect("clean temp dir");
    }
}
