//! Presence-aware background daemon: auto-discover, auto-reconnect, log.
//!
//! Runs forever under a macOS LaunchAgent (never a LaunchDaemon — toast
//! notifications and the Bluetooth permission prompt only work in the user's
//! Aqua session). On every belt-running sample it derives whether the
//! operator is actually walking (see `presence`) and folds the result into
//! the persistent daily totals and per-workout splits (see `store`),
//! independent of the operator's own speed/pause control via the Bluetooth
//! remote.
//!
//! Caveat: while this daemon holds the BLE connection, the official Yesoul
//! phone app cannot also connect (a peripheral serves one central at a time).
//! The physical remote is a separate RF link, so operator control is
//! unaffected.
//!
//! Disconnect detection does *not* rely on CoreBluetooth's own disconnect
//! event or the notification stream ending: a hard power-off (unplugging the
//! belt) was observed live to leave both silent indefinitely — the BLE
//! supervision timeout that would eventually fire a real disconnect event can
//! take arbitrarily long, and btleplug/CoreBluetooth gave no other signal.
//! Instead, `NOTIFICATION_TIMEOUT` bounds how long we wait for the next
//! `0x2ACD` sample (which the device sends ~1/s whenever connected, even at
//! rest) before treating the link as lost ourselves.
//!
//! ## Power/sleep hooks (incident 2026-07-05, see `docs/tasks/006-...md`)
//!
//! Idle scanning (treadmill not currently found) is skipped while the laptop
//! is on battery, but this is now driven by [`crate::power::spawn_power_event_listener`]
//! (native IOKit notifications) instead of a `pmset` poll: a live incident
//! showed the old poll loop silently stuck in the "on battery" branch for
//! 10+ hours while the machine was actually on AC power the whole time, with
//! no external signal that anything was wrong. The event-driven version
//! reacts within one run-loop tick and every transition is logged at `info!`
//! (never `debug!`) for exactly that reason. An already-open connection is
//! never interrupted by a power-state change, only idle *discovery* is
//! gated — see `run()`.
//!
//! A watchdog (`Watchdog`, задача D) independently guards against a *silent
//! hang* with no power-state cause at all (e.g. stuck deep inside
//! btleplug/CoreBluetooth): every meaningful transition — and, as a
//! backstop, every idle tick and every telemetry sample — refreshes the
//! persisted `daemon_status.updated_at` row; if that stops advancing beyond
//! [`WATCHDOG_STALE_THRESHOLD`] we log a `WARN` so the hang is visible in
//! logs and to the future `status` CLI, without trying to self-heal.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use btleplug::api::Peripheral as _;
use btleplug::platform::{Adapter, Peripheral};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::ftms;
use crate::logger::WorkoutLogger;
use crate::notify;
use crate::power::{self, PowerEvent};
use crate::presence::{PresenceState, PresenceTracker};
use crate::scan;
use crate::store::{DaemonStatus, RawDeltas, Store};

/// Delay before retrying discovery after a scan/connect failure, so a
/// transient Bluetooth hiccup does not spin the CPU in a tight loop.
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// How long to wait for the next Treadmill Data sample before treating the
/// link as lost. The device streams ~1/s even while stationary, so this
/// leaves generous margin above normal jitter while still catching a hard
/// power-off well before a human would otherwise notice.
const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(20);

/// Duplicated from `scan::SCAN_TIMEOUT` (private to `scan.rs`), which bounds
/// the discovery loop in `connect_treadmill`/`connect_by_id`. Not made `pub`
/// here to avoid a premature cross-module coupling — a later 006 stage that
/// touches `scan.rs` (задача D.1: wrapping `connect()`/`discover_services()`
/// in a timeout) should reconcile this with the real constant, e.g. by
/// exporting it from `scan.rs` instead of duplicating. Keep in sync by hand
/// until then.
const ASSUMED_SCAN_TIMEOUT: Duration = Duration::from_secs(15);

/// Expected name/value for the `CONNECT_TIMEOUT` constant the next 006 stage
/// should add in `scan.rs` to bound `peripheral.connect().await` and
/// `peripheral.discover_services().await` (задача D.1 — those calls are
/// currently unbounded). Defined here only so the watchdog threshold below
/// has a concrete number; once `scan.rs` defines its own `CONNECT_TIMEOUT`,
/// this local copy should be removed in favor of importing that one.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How often the idle/scanning loop checks `Watchdog` for staleness.
/// Coarser than the scan cycle since this is just a liveness check, not
/// where the real work happens.
const WATCHDOG_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// How stale `daemon_status.updated_at` may get before we treat it as a
/// possible silent hang (задача D.2). Generous margin above the worst-case
/// single scan+connect cycle so normal latency never trips it, while a
/// genuine hang (the 2026-07-05 incident: silent for 10+ hours) is still
/// caught in minutes, not hours.
const WATCHDOG_STALE_THRESHOLD: Duration =
    Duration::from_secs(ASSUMED_SCAN_TIMEOUT.as_secs() + CONNECT_TIMEOUT.as_secs() + 60);

/// Run the daemon forever: scan → connect → stream with presence tracking →
/// on disconnect, toast and go back to scanning. Reacts to power/sleep
/// events instead of polling — see module docs.
pub async fn run(adapter: &Adapter) -> Result<()> {
    let mut power_events = power::spawn_power_event_listener();
    let mut store = Store::open()?;
    let mut watchdog = Watchdog::new();
    let mut watchdog_tick = tokio::time::interval(WATCHDOG_CHECK_INTERVAL);

    // The listener always sends the current AC/battery state as its first
    // event (see `power::spawn_power_event_listener`), so this seeds `on_ac`
    // without a separate synchronous read.
    let mut on_ac = match power_events.recv().await {
        Some(PowerEvent::AcPowerChanged(on_ac)) => on_ac,
        Some(other) => {
            warn!(?other, "unexpected first power event, assuming AC power");
            true
        }
        None => {
            warn!("power-event channel closed immediately — assuming AC power");
            true
        }
    };

    let mut state = DaemonState::new(on_ac);
    state.persist(&store, &mut watchdog)?;

    loop {
        if !on_ac {
            info!("on battery with no active connection — idling until AC power or wake");
            loop {
                tokio::select! {
                    biased;
                    event = power_events.recv() => {
                        match event {
                            Some(PowerEvent::AcPowerChanged(true)) => {
                                info!("AC power restored — resuming scanning immediately");
                                on_ac = true;
                                state.set_power_mode(true);
                                state.persist(&store, &mut watchdog)?;
                                break;
                            }
                            Some(PowerEvent::DidWake) => {
                                // Re-check rather than assume: waking doesn't
                                // imply AC, just that it's worth looking again.
                                let now_on_ac = power::is_on_ac_power();
                                info!(on_ac = now_on_ac, "system woke while idle — re-checked power state");
                                on_ac = now_on_ac;
                                state.set_power_mode(now_on_ac);
                                state.persist(&store, &mut watchdog)?;
                                if now_on_ac {
                                    break;
                                }
                            }
                            Some(PowerEvent::AcPowerChanged(false)) => {
                                // Duplicate/no-op: already idling on battery.
                            }
                            Some(PowerEvent::WillSleep) => {
                                info!("system will sleep while idle (no active session)");
                                state.persist(&store, &mut watchdog)?;
                            }
                            Some(PowerEvent::WillPowerOff) => {
                                info!("system will power off while idle — no active session to close");
                                state.persist(&store, &mut watchdog)?;
                            }
                            None => {
                                error!("power-event listener channel closed — cannot react to power state anymore");
                                return Err(anyhow!("power-event listener thread died"));
                            }
                        }
                    }
                    _ = watchdog_tick.tick() => {
                        watchdog.check();
                        state.persist(&store, &mut watchdog)?;
                    }
                }
            }
            continue;
        }

        tokio::select! {
            biased;
            event = power_events.recv() => {
                match event {
                    Some(PowerEvent::AcPowerChanged(false)) => {
                        info!("switched to battery with no active connection — going idle");
                        on_ac = false;
                        state.set_power_mode(false);
                        state.persist(&store, &mut watchdog)?;
                    }
                    Some(PowerEvent::AcPowerChanged(true)) => {
                        // Already scanning on AC — nothing to do.
                    }
                    Some(PowerEvent::DidWake) => {
                        info!("system woke — scan loop already active, will retry immediately");
                    }
                    Some(PowerEvent::WillSleep) => {
                        info!("system will sleep (no active session)");
                        state.persist(&store, &mut watchdog)?;
                    }
                    Some(PowerEvent::WillPowerOff) => {
                        info!("system will power off — no active session to close");
                        state.persist(&store, &mut watchdog)?;
                    }
                    None => {
                        error!("power-event listener channel closed — cannot react to power state anymore");
                        return Err(anyhow!("power-event listener thread died"));
                    }
                }
            }
            _ = watchdog_tick.tick() => {
                watchdog.check();
                state.persist(&store, &mut watchdog)?;
            }
            result = scan::connect_treadmill(adapter) => {
                match result {
                    Ok(peripheral) => {
                        notify::treadmill_found();
                        state.connected = true;
                        state.last_connected_at = Some(Utc::now().to_rfc3339());
                        state.persist(&store, &mut watchdog)?;

                        if let Err(err) =
                            stream_with_presence(&peripheral, &mut power_events, &mut store, &mut state, &mut watchdog, &mut on_ac)
                                .await
                        {
                            warn!(%err, "presence stream ended with an error");
                        }
                        let _ = peripheral.disconnect().await;
                        notify::treadmill_lost();

                        state.connected = false;
                        state.presence_state = None;
                        state.last_disconnected_at = Some(Utc::now().to_rfc3339());
                        state.persist(&store, &mut watchdog)?;
                    }
                    Err(err) => {
                        warn!(%err, "treadmill not found this cycle, retrying");
                        sleep(RETRY_DELAY).await;
                    }
                }
            }
        }
    }
}

/// Stream telemetry from an already-connected peripheral, folding presence
/// into workout/daily totals, until the link is judged lost. Also reacts to
/// power events while connected: an active session is never itself
/// interrupted by an AC/battery change (only idle *discovery* is gated —
/// see `run()`), `WillSleep` is just logged/persisted (BLE will drop on its
/// own if the connection doesn't survive sleep), and `WillPowerOff`
/// best-effort closes the session before the process may be killed.
async fn stream_with_presence(
    peripheral: &Peripheral,
    power_events: &mut UnboundedReceiver<PowerEvent>,
    store: &mut Store,
    state: &mut DaemonState,
    watchdog: &mut Watchdog,
    on_ac: &mut bool,
) -> Result<()> {
    scan::subscribe_treadmill_data(peripheral).await?;
    scan::subscribe_treadmill_status(peripheral).await?;
    let mut notifications = peripheral.notifications().await?;

    let session_id = store.start_session()?;
    let mut logger = WorkoutLogger::create()?;
    let mut presence = PresenceTracker::new();
    // Distance/time seen since the last *confirmed* step, not yet credited to
    // today's totals — see `credit_or_hold` for why this can't be applied the
    // instant a sample arrives.
    let mut pending = PendingCredit::default();

    loop {
        tokio::select! {
            biased;
            event = power_events.recv() => {
                match event {
                    Some(PowerEvent::AcPowerChanged(new_on_ac)) => {
                        *on_ac = new_on_ac;
                        state.set_power_mode(new_on_ac);
                        state.persist(store, watchdog)?;
                    }
                    Some(PowerEvent::WillSleep) => {
                        info!("system will sleep — active session left connected, BLE will drop on its own if it doesn't survive");
                        state.persist(store, watchdog)?;
                    }
                    Some(PowerEvent::DidWake) => {
                        info!("system woke while connected — active session unaffected");
                        state.persist(store, watchdog)?;
                    }
                    Some(PowerEvent::WillPowerOff) => {
                        warn!("system will power off — closing active session best-effort before the process may be killed");
                        logger.finish();
                        store.end_session()?;
                        // Return directly (rather than `break`) so the normal
                        // "stream ended" path below — which logs an `error!`
                        // for what would otherwise look like an unexpected
                        // disconnect — never runs for this controlled exit.
                        return Ok(());
                    }
                    None => {
                        error!("power-event channel closed while a session is active — continuing without power-state visibility");
                    }
                }
                continue;
            }
            notification = tokio::time::timeout(NOTIFICATION_TIMEOUT, notifications.next()) => {
                let notification = match notification {
                    Ok(Some(notification)) => notification,
                    Ok(None) => break, // stream closed cleanly (rare, but handle it)
                    Err(_) => {
                        warn!(timeout_s = NOTIFICATION_TIMEOUT.as_secs(), "no telemetry received — treating as disconnected");
                        break;
                    }
                };
                if notification.uuid == ftms::FITNESS_MACHINE_STATUS {
                    let ts_ms = Utc::now().timestamp_millis();
                    if let Some(&event_code) = notification.value.first() {
                        info!(event = ftms::describe_status_event(event_code), code = event_code, "machine status event");
                        store.insert_status_event(session_id, ts_ms, event_code, &notification.value)?;
                    } else {
                        warn!("empty Fitness Machine Status frame");
                    }
                    state.persist(store, watchdog)?;
                    continue;
                }
                if notification.uuid != ftms::TREADMILL_DATA {
                    continue;
                }
                let Some(data) = ftms::parse_treadmill_data(&notification.value) else {
                    warn!(bytes = ?notification.value, "undecodable treadmill frame");
                    continue;
                };
                logger.log(&data)?;
                store.insert_raw_sample(session_id, Utc::now().timestamp_millis(), &data, &notification.value)?;

                let deltas = store.advance_baseline(data.steps, data.total_distance_m, data.elapsed_s)?;

                let prev_state = presence.state();
                if let Some(next_state) = presence.observe(data.speed_kmh, data.steps) {
                    info!(?prev_state, ?next_state, "presence transition");
                    state.presence_state = Some(format!("{next_state:?}"));
                    match next_state {
                        PresenceState::AwayWhileRunning => notify::walker_away(),
                        PresenceState::Walking if prev_state == PresenceState::AwayWhileRunning => {
                            notify::walker_resumed();
                        }
                        PresenceState::Walking if prev_state == PresenceState::Paused => {
                            notify::treadmill_resumed();
                        }
                        // Skip the very first sample after connecting: PresenceState
                        // starts Unknown, so a treadmill discovered already stopped
                        // must not immediately toast "paused".
                        PresenceState::Paused if prev_state != PresenceState::Unknown => {
                            notify::treadmill_paused();
                        }
                        _ => {}
                    }
                }

                credit_or_hold(store, &mut pending, presence.state(), deltas)?;
                state.persist(store, watchdog)?;
            }
        }
    }

    logger.finish();
    store.end_session()?;
    error!("notification stream ended (device disconnected?)");
    Ok(())
}

/// Distance/time accrued since the last confirmed step, held back from
/// `daily_stats`/`workouts` until either a new step confirms it was real
/// walking, or the operator is confirmed away and it gets discarded.
#[derive(Default)]
struct PendingCredit {
    distance_m: i64,
    elapsed_s: i64,
}

/// Decide what to do with this sample's raw deltas.
///
/// Steps only ever advance when a step is genuinely registered, so a
/// non-zero `deltas.steps` is itself the confirmation signal — crediting it
/// immediately is always correct. Distance/time are different: the belt
/// keeps moving and `elapsed_s` keeps ticking during the up-to-
/// `presence::AWAY_THRESHOLD` window before an absence is confirmed, so they
/// are held in `pending` and only flushed to `daily_stats`/`workouts` alongside
/// a confirming step. If the away threshold fires first, `pending` is dropped
/// instead of committed — otherwise every departure would silently credit an
/// extra `AWAY_THRESHOLD` worth of phantom distance/time.
fn credit_or_hold(store: &mut Store, pending: &mut PendingCredit, state: PresenceState, deltas: RawDeltas) -> Result<()> {
    match state {
        PresenceState::Walking => {
            pending.distance_m += deltas.distance_m;
            pending.elapsed_s += deltas.elapsed_s;
            if deltas.steps > 0 {
                // TODO(006/daemon.rs stage): pass the real sample timestamp instead of
                // `Utc::now()` once the daemon plumbs one through, so workout splitting
                // matches wall-clock gaps precisely rather than processing time.
                store.credit_activity(deltas.steps, pending.distance_m, pending.elapsed_s, chrono::Utc::now())?;
                *pending = PendingCredit::default();
            }
        }
        PresenceState::AwayWhileRunning | PresenceState::Paused | PresenceState::Unknown => {
            *pending = PendingCredit::default();
        }
    }
    Ok(())
}

/// In-memory mirror of the `daemon_status` row (see `store::DaemonStatus`),
/// rebuilt and upserted on every transition the daemon observes, so a
/// separate `status` CLI invocation can read current state without racing
/// the daemon for the BLE adapter.
struct DaemonState {
    connected: bool,
    presence_state: Option<String>,
    last_connected_at: Option<String>,
    last_disconnected_at: Option<String>,
    power_mode: &'static str,
    power_mode_since: DateTime<Utc>,
}

impl DaemonState {
    fn new(on_ac: bool) -> Self {
        Self {
            connected: false,
            presence_state: None,
            last_connected_at: None,
            last_disconnected_at: None,
            power_mode: power_mode_label(on_ac),
            power_mode_since: Utc::now(),
        }
    }

    /// Update the power mode, bumping `power_mode_since` only on an actual
    /// change (repeated events for the same mode must not reset the "since"
    /// timestamp shown by the future `status` command).
    fn set_power_mode(&mut self, on_ac: bool) {
        let mode = power_mode_label(on_ac);
        if self.power_mode != mode {
            self.power_mode = mode;
            self.power_mode_since = Utc::now();
        }
    }

    /// Upsert the current state into `daemon_status` and mark the watchdog
    /// as freshly touched. Called on every meaningful transition plus, as a
    /// backstop, on every telemetry sample and every idle/watchdog tick.
    fn persist(&self, store: &Store, watchdog: &mut Watchdog) -> Result<()> {
        store.upsert_daemon_status(&DaemonStatus {
            connected: self.connected,
            presence_state: self.presence_state.clone(),
            last_connected_at: self.last_connected_at.clone(),
            last_disconnected_at: self.last_disconnected_at.clone(),
            power_mode: self.power_mode.to_string(),
            power_mode_since: self.power_mode_since.to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        })?;
        watchdog.touch();
        Ok(())
    }
}

fn power_mode_label(on_ac: bool) -> &'static str {
    if on_ac { "ac_scanning" } else { "battery_idle" }
}

/// Tracks the last time `daemon_status` was refreshed and logs a `WARN` if
/// it goes stale — a diagnosable, unmissable signal for a silent hang
/// (задача D), independent of *why* the daemon stopped advancing (power-gate
/// bug, a wedged btleplug/CoreBluetooth call, or anything else). Does not
/// attempt to self-heal, per the task doc's explicit call-out that an
/// automatic "fix" here is its own risk.
struct Watchdog {
    last_update: Instant,
}

impl Watchdog {
    fn new() -> Self {
        Self { last_update: Instant::now() }
    }

    fn touch(&mut self) {
        self.last_update = Instant::now();
    }

    fn check(&self) {
        let elapsed = self.last_update.elapsed();
        if elapsed > WATCHDOG_STALE_THRESHOLD {
            warn!(
                elapsed_s = elapsed.as_secs(),
                threshold_s = WATCHDOG_STALE_THRESHOLD.as_secs(),
                "daemon_status hasn't been refreshed in a while — possible silent hang"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn memory_store() -> Store {
        Store::open_at(Path::new(":memory:")).expect("open in-memory store")
    }

    #[test]
    fn confirmed_step_flushes_pending_distance_and_time() {
        let mut store = memory_store();
        let mut pending = PendingCredit::default();

        // Ambiguous gap: belt moved 3m/1s but no step registered yet.
        credit_or_hold(&mut store, &mut pending, PresenceState::Walking, RawDeltas { steps: 0, distance_m: 3, elapsed_s: 1 })
            .unwrap();
        assert_eq!(pending.distance_m, 3);

        // A step now confirms the whole gap was real walking — flush it.
        credit_or_hold(&mut store, &mut pending, PresenceState::Walking, RawDeltas { steps: 1, distance_m: 1, elapsed_s: 1 })
            .unwrap();
        assert_eq!(pending.distance_m, 0);
        let today = store.today_stats().unwrap();
        assert_eq!(today.distance_m, 4);
        assert_eq!(today.steps, 1);
        assert_eq!(today.walking_time_s, 2);
    }

    #[test]
    fn confirmed_away_discards_pending_instead_of_crediting_it() {
        let mut store = memory_store();
        let mut pending = PendingCredit::default();

        // The belt kept moving for the whole confirmation window before the
        // tracker flips to AwayWhileRunning — this must never reach daily_stats.
        for _ in 0..10 {
            credit_or_hold(&mut store, &mut pending, PresenceState::Walking, RawDeltas { steps: 0, distance_m: 1, elapsed_s: 1 })
                .unwrap();
        }
        assert_eq!(pending.distance_m, 10);

        credit_or_hold(&mut store, &mut pending, PresenceState::AwayWhileRunning, RawDeltas { steps: 0, distance_m: 1, elapsed_s: 1 })
            .unwrap();

        assert_eq!(pending.distance_m, 0);
        let today = store.today_stats().unwrap();
        assert_eq!(today.distance_m, 0);
        assert_eq!(today.walking_time_s, 0);
    }

    #[test]
    fn daemon_state_persist_roundtrips_and_touches_watchdog() {
        let store = memory_store();
        let mut watchdog = Watchdog::new();
        // Force staleness so we can observe `persist` resetting it.
        watchdog.last_update -= WATCHDOG_STALE_THRESHOLD * 2;
        assert!(watchdog.last_update.elapsed() > WATCHDOG_STALE_THRESHOLD);

        let mut state = DaemonState::new(true);
        state.connected = true;
        state.presence_state = Some("Walking".to_string());
        state.persist(&store, &mut watchdog).unwrap();

        assert!(watchdog.last_update.elapsed() < WATCHDOG_STALE_THRESHOLD);
        let status = store.daemon_status().unwrap().expect("row present");
        assert!(status.connected);
        assert_eq!(status.presence_state.as_deref(), Some("Walking"));
        assert_eq!(status.power_mode, "ac_scanning");
    }

    #[test]
    fn set_power_mode_only_bumps_since_on_actual_change() {
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
