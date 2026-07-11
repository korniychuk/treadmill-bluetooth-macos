//! DaemonStatus in-memory mirror and DB persist helpers (backlog 010/011).

use chrono::{DateTime, Utc};
use tracing::{error, warn};

use super::watchdog::Watchdog;
use crate::goals::Goal;
use crate::store::{DaemonStatus, Store};

use anyhow::Result;
use std::time::Duration;

/// How many *consecutive* per-sample DB persist failures are tolerated before
/// the process exits for a launchd restart. One-off SQLITE_BUSY under system
/// load is a recoverable anomaly (WARN + skip — the cumulative FTMS counters
/// make the next successful `advance_baseline` self-healing), but a persistent
/// failure (disk full, schema corruption) needs a clean-slate restart: merely
/// ending the stream would flap a healthy BLE link forever, and the watchdog
/// never fires because telemetry keeps touching it (backlog 010).
const DB_PERSIST_FAILURE_LIMIT: u32 = 30;

/// Exit code for a persistent DB failure — distinct from watchdog (86) and
/// scan-wedge (87) for log/`launchctl print` forensics.
const DB_PERSIST_EXIT_CODE: i32 = 88;

/// Shared tolerate-and-escalate for DB writes that must not tear down a
/// healthy session/loop (backlog 010 / 011). Resets `failures` on success;
/// on a transient error increments and runs `on_skip`; after
/// [`DB_PERSIST_FAILURE_LIMIT`] consecutive failures logs ERROR and exits
/// for a launchd restart (never returns).
pub(super) fn tolerate_db_write<T>(
    result: Result<T>,
    failures: &mut u32,
    on_skip: impl FnOnce(&anyhow::Error, u32),
) -> Option<T> {
    match result {
        Ok(value) => {
            *failures = 0;
            Some(value)
        }
        Err(err) if *failures + 1 < DB_PERSIST_FAILURE_LIMIT => {
            *failures += 1;
            on_skip(&err, *failures);
            None
        }
        Err(err) => {
            error!(
                error = %err,
                consecutive = *failures + 1,
                exit_code = DB_PERSIST_EXIT_CODE,
                "DB persist failing persistently — exiting for launchd restart"
            );
            std::process::exit(DB_PERSIST_EXIT_CODE);
        }
    }
}

/// Upsert `daemon_status` without letting a busy/full DB kill the loop.
///
/// On success [`DaemonState::persist`] already calls [`Watchdog::touch`].
/// On a skipped failure we still touch: event-loop progress must not depend
/// on SQLite (Liveness matrix — shell/watchdog row).
pub(super) fn persist_daemon_status(
    state: &DaemonState,
    store: &Store,
    watchdog: &Watchdog,
    failures: &mut u32,
) {
    let _ = tolerate_db_write(
        state.persist(store, watchdog),
        failures,
        |err, consecutive| {
            watchdog.touch();
            warn!(
                error = %err,
                consecutive,
                "daemon status persist failed — skipping upsert, keeping the loop"
            );
        },
    );
}

/// In-memory mirror of the `daemon_status` row (see `store::DaemonStatus`),
/// rebuilt and upserted on every transition the daemon observes, so a
/// separate `status` CLI invocation can read current state without racing
/// the daemon for the BLE adapter.
pub(crate) struct DaemonState {
    pub(crate) connected: bool,
    pub(crate) presence_state: Option<String>,
    pub(crate) last_connected_at: Option<String>,
    pub(crate) last_disconnected_at: Option<String>,
    pub(crate) power_mode: &'static str,
    pub(crate) power_mode_since: DateTime<Utc>,
    // Snapshot of the config the daemon currently holds, surfaced by `tm status`
    // (задача 022): comma-joined goals, auto-pause threshold in seconds (`None` =
    // disabled), and when the config file was last read. Updated by `set_config`
    // at startup and on each mtime-triggered reload.
    pub(crate) config_goals: Option<String>,
    pub(crate) config_auto_pause_secs: Option<i64>,
    pub(crate) config_loaded_at: Option<String>,
    // Heart-rate snapshot (задача 025) — same reasoning as the rest of this
    // struct: mirrors what the daemon just observed so `tm status`/`widget`/
    // `stats` can read it without racing the daemon for BLE.
    pub(crate) hr_connected: bool,
    pub(crate) last_bpm: Option<i64>,
    pub(crate) last_bpm_ts: Option<i64>,
    /// HR sensor battery level, 0-100% (задача 026). `None` until read at
    /// least once this link.
    pub(crate) hr_battery_pct: Option<i64>,
    /// Zone Hold snapshot (задача 027) — mirrors `ZoneHoldPhase`/the resolved
    /// target zone so `tm status`/`tm widget` can read it without racing the
    /// daemon for BLE. See `zh_persist_snapshot`.
    pub(crate) zone_hold_active: bool,
    pub(crate) zone_hold_target_lo: Option<i64>,
    pub(crate) zone_hold_target_hi: Option<i64>,
    pub(crate) zone_hold_last_speed: Option<f64>,
    pub(crate) zone_hold_phase: Option<String>,
    pub(crate) zone_hold_position: Option<String>,
    /// Live belt-speed snapshot (задача 029) — updated on every telemetry
    /// sample regardless of Zone Hold, same reasoning as `last_bpm`/
    /// `last_bpm_ts` above. `last_speed_ts` is Unix millis.
    pub(crate) last_speed_kmh: Option<f64>,
    pub(crate) last_speed_ts: Option<i64>,
}

impl DaemonState {
    pub(crate) fn new(on_ac: bool) -> Self {
        Self {
            connected: false,
            presence_state: None,
            last_connected_at: None,
            last_disconnected_at: None,
            power_mode: power_mode_label(on_ac),
            power_mode_since: Utc::now(),
            config_goals: None,
            config_auto_pause_secs: None,
            config_loaded_at: None,
            hr_connected: false,
            last_bpm: None,
            last_bpm_ts: None,
            hr_battery_pct: None,
            zone_hold_active: false,
            zone_hold_target_lo: None,
            zone_hold_target_hi: None,
            zone_hold_last_speed: None,
            zone_hold_phase: None,
            zone_hold_position: None,
            last_speed_kmh: None,
            last_speed_ts: None,
        }
    }

    /// Snapshot the config the daemon just (re)loaded, stamping the read time —
    /// surfaced by `tm status` (задача 022). Called at startup and whenever the
    /// config file is re-read on the mtime watch (задача 017).
    pub(super) fn set_config(&mut self, goals: &[Goal], auto_pause: Option<Duration>) {
        self.config_goals = Some(
            goals
                .iter()
                .map(|g| g.threshold.to_string())
                .collect::<Vec<_>>()
                .join(","),
        );
        self.config_auto_pause_secs = auto_pause.map(|d| d.as_secs() as i64);
        self.config_loaded_at = Some(Utc::now().to_rfc3339());
    }

    /// Update the power mode, bumping `power_mode_since` only on an actual
    /// change (repeated events for the same mode must not reset the "since"
    /// timestamp shown by the future `status` command).
    pub(super) fn set_power_mode(&mut self, on_ac: bool) {
        let mode = power_mode_label(on_ac);
        if self.power_mode != mode {
            self.power_mode = mode;
            self.power_mode_since = Utc::now();
        }
    }

    /// Upsert the current state into `daemon_status` and mark the watchdog
    /// as freshly touched. Called on every meaningful transition plus, as a
    /// backstop, on every telemetry sample and every idle/watchdog tick.
    pub(super) fn persist(&self, store: &Store, watchdog: &Watchdog) -> Result<()> {
        store.upsert_daemon_status(&DaemonStatus {
            connected: self.connected,
            presence_state: self.presence_state.clone(),
            last_connected_at: self.last_connected_at.clone(),
            last_disconnected_at: self.last_disconnected_at.clone(),
            power_mode: self.power_mode.to_string(),
            power_mode_since: self.power_mode_since.to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            config_goals: self.config_goals.clone(),
            config_auto_pause_secs: self.config_auto_pause_secs,
            config_loaded_at: self.config_loaded_at.clone(),
            hr_connected: self.hr_connected,
            last_bpm: self.last_bpm,
            last_bpm_ts: self.last_bpm_ts,
            hr_battery_pct: self.hr_battery_pct,
            zone_hold_active: self.zone_hold_active,
            zone_hold_target_lo: self.zone_hold_target_lo,
            zone_hold_target_hi: self.zone_hold_target_hi,
            zone_hold_last_speed: self.zone_hold_last_speed,
            zone_hold_phase: self.zone_hold_phase.clone(),
            zone_hold_position: self.zone_hold_position.clone(),
            last_speed_kmh: self.last_speed_kmh,
            last_speed_ts: self.last_speed_ts,
        })?;
        watchdog.touch();
        Ok(())
    }
}

pub(super) fn power_mode_label(on_ac: bool) -> &'static str {
    if on_ac { "ac_scanning" } else { "battery_idle" }
}

#[cfg(test)]
mod tests {
    use super::super::watchdog::{WATCHDOG_STALE_THRESHOLD, Watchdog};
    use super::*;
    use crate::goals::Goal;
    use std::path::Path;
    use std::time::Duration;

    fn memory_store() -> Store {
        Store::open_at(Path::new(":memory:")).expect("open in-memory store")
    }

    #[test]
    fn daemon_state_persist_roundtrips_and_touches_watchdog() {
        let store = memory_store();
        let watchdog = Watchdog::new();
        // Untouched watchdog at a synthetic "far future" instant is stale.
        let far_future = WATCHDOG_STALE_THRESHOLD * 2;
        assert!(watchdog.is_stale_at(far_future));

        let mut state = DaemonState::new(true);
        state.connected = true;
        state.presence_state = Some("Walking".to_string());
        // Loaded-config snapshot is persisted too (задача 022).
        state.set_config(
            &[Goal {
                threshold: 8500,
                tier: 1,
            }],
            Some(Duration::from_secs(300)),
        );
        state.persist(&store, &watchdog).unwrap();

        // `persist` touched the watchdog just now: fresh well inside the
        // threshold, stale again well past it (exact-boundary checks would
        // race the sub-ms gap between the touch and this measurement).
        assert!(!watchdog.is_stale_at(watchdog.anchor.elapsed() + WATCHDOG_STALE_THRESHOLD / 2));
        assert!(watchdog.is_stale_at(watchdog.anchor.elapsed() + WATCHDOG_STALE_THRESHOLD * 2));
        let status = store.daemon_status().unwrap().expect("row present");
        assert!(status.connected);
        assert_eq!(status.presence_state.as_deref(), Some("Walking"));
        assert_eq!(status.power_mode, "ac_scanning");
        assert_eq!(status.config_goals.as_deref(), Some("8500"));
        assert_eq!(status.config_auto_pause_secs, Some(300));
        assert!(status.config_loaded_at.is_some());
    }
    #[test]
    pub(super) fn set_power_mode_only_bumps_since_on_actual_change() {
        let mut state = DaemonState::new(true);
        let since_before = state.power_mode_since;

        // Same mode again — must not reset `power_mode_since`.
        state.set_power_mode(true);
        assert_eq!(state.power_mode_since, since_before);
        assert_eq!(state.power_mode, "ac_scanning");

        state.set_power_mode(false);
        assert_eq!(state.power_mode, "battery_idle");
        assert!(state.power_mode_since >= since_before);
    }
}
