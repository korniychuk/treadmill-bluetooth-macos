//! Schema migration and retention pruning for the SQLite store.

use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use rusqlite::params;

use super::Store;

/// Age after which `status_events` rows are pruned (задача 046). Diagnostic
/// FTMS status frames are not ground truth for recompute; 90 days is enough
/// for incident archaeology without unbounded growth.
const STATUS_EVENTS_RETENTION: Duration = Duration::days(90);

impl Store {
    pub(super) fn migrate(&self) -> Result<()> {
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
                CREATE INDEX IF NOT EXISTS idx_raw_samples_ts ON raw_samples(ts_ms);
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

        // Live belt-speed snapshot columns (задача 029).
        self.add_column_if_missing("ALTER TABLE daemon_status ADD COLUMN last_speed_kmh REAL")?;
        self.add_column_if_missing("ALTER TABLE daemon_status ADD COLUMN last_speed_ts INTEGER")?;

        // Hot readers filter/order by ts_ms (stats, default-speed, recompute);
        // only session_id was indexed historically (задача 046).
        self.conn
            .execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_raw_samples_ts ON raw_samples(ts_ms);
                 CREATE INDEX IF NOT EXISTS idx_status_events_ts ON status_events(ts_ms);",
            )
            .context("create raw_samples/status_events ts indexes")?;

        self.prune_status_events()?;
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

    /// Drop `status_events` older than [`STATUS_EVENTS_RETENTION`] (задача 046).
    /// Runs on open/migrate so growth is bounded without a separate job.
    /// `raw_samples` is intentionally **not** pruned: it is ground truth for
    /// recompute-segments and default-speed (measured ~8MB after months of use).
    fn prune_status_events(&self) -> Result<()> {
        let cutoff_ms = (Utc::now() - STATUS_EVENTS_RETENTION).timestamp_millis();
        self.conn
            .execute(
                "DELETE FROM status_events WHERE ts_ms < ?1",
                params![cutoff_ms],
            )
            .context("prune old status_events")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use rusqlite::params;

    use super::super::memory_store;

    #[test]
    fn prune_status_events_drops_rows_older_than_retention() {
        let store = memory_store();
        let session = store.start_session().unwrap();
        let old_ms = (Utc::now() - Duration::days(120)).timestamp_millis();
        let fresh_ms = Utc::now().timestamp_millis();
        store
            .conn
            .execute(
                "INSERT INTO status_events (session_id, ts_ms, event_code, raw_frame) VALUES (?1, ?2, 1, x'00')",
                params![session, old_ms],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO status_events (session_id, ts_ms, event_code, raw_frame) VALUES (?1, ?2, 2, x'00')",
                params![session, fresh_ms],
            )
            .unwrap();
        store.prune_status_events().unwrap();
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM status_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let remaining: i64 = store
            .conn
            .query_row("SELECT ts_ms FROM status_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, fresh_ms);
    }
}
