//! Presence-aware background daemon: auto-discover, auto-reconnect, log.
//!
//! Runs forever under a macOS LaunchAgent (never a LaunchDaemon — toast
//! notifications and the Bluetooth permission prompt only work in the user's
//! Aqua session). On every belt-running sample it derives whether the
//! operator is actually walking (see `presence`) and folds the result into
//! the persistent daily totals (see `store`), independent of the operator's
//! own speed/pause control via the Bluetooth remote.
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
//! Idle scanning (treadmill not currently found) is skipped while the laptop
//! is on battery — see `power::is_on_ac_power`. Scanning keeps the Bluetooth
//! radio active; that's negligible for one session but wasteful to run
//! unconditionally forever when the laptop is away from the treadmill anyway
//! (the common case when unplugged). An already-open connection is never
//! interrupted by a power-state change, only idle *discovery* is gated.

use std::time::Duration;

use anyhow::Result;
use btleplug::api::Peripheral as _;
use btleplug::platform::{Adapter, Peripheral};
use futures::StreamExt;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::ftms;
use crate::logger::WorkoutLogger;
use crate::notify;
use crate::power::is_on_ac_power;
use crate::presence::{PresenceState, PresenceTracker};
use crate::scan;
use crate::store::{RawDeltas, Store};

/// Delay before retrying discovery after a scan/connect failure, so a
/// transient Bluetooth hiccup does not spin the CPU in a tight loop.
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// How long to wait for the next Treadmill Data sample before treating the
/// link as lost. The device streams ~1/s even while stationary, so this
/// leaves generous margin above normal jitter while still catching a hard
/// power-off well before a human would otherwise notice.
const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(20);

/// How often to re-check AC power while skipping idle scans on battery.
/// Coarser than the scan cycle since a `pmset` poll is the only thing
/// happening at this cadence — cheap either way, but no need to hurry.
const BATTERY_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Run the daemon forever: scan → connect → stream with presence tracking →
/// on disconnect, toast and go back to scanning.
pub async fn run(adapter: &Adapter) -> Result<()> {
    loop {
        if !is_on_ac_power() {
            debug!("on battery and not connected — skipping idle scan to save power");
            sleep(BATTERY_POLL_INTERVAL).await;
            continue;
        }

        match scan::connect_treadmill(adapter).await {
            Ok(peripheral) => {
                notify::treadmill_found();
                if let Err(err) = stream_with_presence(&peripheral).await {
                    warn!(%err, "presence stream ended with an error");
                }
                let _ = peripheral.disconnect().await;
                notify::treadmill_lost();
            }
            Err(err) => {
                warn!(%err, "treadmill not found this cycle, retrying");
                sleep(RETRY_DELAY).await;
            }
        }
    }
}

async fn stream_with_presence(peripheral: &Peripheral) -> Result<()> {
    scan::subscribe_treadmill_data(peripheral).await?;
    let mut notifications = peripheral.notifications().await?;

    let mut store = Store::open()?;
    store.start_session()?;
    let mut logger = WorkoutLogger::create()?;
    let mut presence = PresenceTracker::new();
    // Distance/time seen since the last *confirmed* step, not yet credited to
    // today's totals — see `credit_or_hold` for why this can't be applied the
    // instant a sample arrives.
    let mut pending = PendingCredit::default();

    loop {
        let notification = match tokio::time::timeout(NOTIFICATION_TIMEOUT, notifications.next()).await {
            Ok(Some(notification)) => notification,
            Ok(None) => break, // stream closed cleanly (rare, but handle it)
            Err(_) => {
                warn!(timeout_s = NOTIFICATION_TIMEOUT.as_secs(), "no telemetry received — treating as disconnected");
                break;
            }
        };
        if notification.uuid != ftms::TREADMILL_DATA {
            continue;
        }
        let Some(data) = ftms::parse_treadmill_data(&notification.value) else {
            warn!(bytes = ?notification.value, "undecodable treadmill frame");
            continue;
        };
        logger.log(&data)?;

        let deltas = store.advance_baseline(data.steps, data.total_distance_m, data.elapsed_s)?;

        let prev_state = presence.state();
        if let Some(next_state) = presence.observe(data.speed_kmh, data.steps) {
            info!(?prev_state, ?next_state, "presence transition");
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

        credit_or_hold(&store, &mut pending, presence.state(), deltas)?;
    }

    logger.finish();
    store.end_session()?;
    error!("notification stream ended (device disconnected?)");
    Ok(())
}

/// Distance/time accrued since the last confirmed step, held back from
/// `daily_stats` until either a new step confirms it was real walking, or the
/// operator is confirmed away and it gets discarded.
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
/// are held in `pending` and only flushed to `daily_stats` alongside a
/// confirming step. If the away threshold fires first, `pending` is dropped
/// instead of committed — otherwise every departure would silently credit an
/// extra `AWAY_THRESHOLD` worth of phantom distance/time.
fn credit_or_hold(store: &Store, pending: &mut PendingCredit, state: PresenceState, deltas: RawDeltas) -> Result<()> {
    match state {
        PresenceState::Walking => {
            pending.distance_m += deltas.distance_m;
            pending.elapsed_s += deltas.elapsed_s;
            if deltas.steps > 0 {
                store.credit_daily(deltas.steps, pending.distance_m, pending.elapsed_s)?;
                *pending = PendingCredit::default();
            }
        }
        PresenceState::AwayWhileRunning | PresenceState::Paused | PresenceState::Unknown => {
            *pending = PendingCredit::default();
        }
    }
    Ok(())
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
        let store = memory_store();
        let mut pending = PendingCredit::default();

        // Ambiguous gap: belt moved 3m/1s but no step registered yet.
        credit_or_hold(&store, &mut pending, PresenceState::Walking, RawDeltas { steps: 0, distance_m: 3, elapsed_s: 1 })
            .unwrap();
        assert_eq!(pending.distance_m, 3);

        // A step now confirms the whole gap was real walking — flush it.
        credit_or_hold(&store, &mut pending, PresenceState::Walking, RawDeltas { steps: 1, distance_m: 1, elapsed_s: 1 })
            .unwrap();
        assert_eq!(pending.distance_m, 0);
        let today = store.today_stats().unwrap();
        assert_eq!(today.distance_m, 4);
        assert_eq!(today.steps, 1);
        assert_eq!(today.walking_time_s, 2);
    }

    #[test]
    fn confirmed_away_discards_pending_instead_of_crediting_it() {
        let store = memory_store();
        let mut pending = PendingCredit::default();

        // The belt kept moving for the whole confirmation window before the
        // tracker flips to AwayWhileRunning — this must never reach daily_stats.
        for _ in 0..10 {
            credit_or_hold(&store, &mut pending, PresenceState::Walking, RawDeltas { steps: 0, distance_m: 1, elapsed_s: 1 })
                .unwrap();
        }
        assert_eq!(pending.distance_m, 10);

        credit_or_hold(&store, &mut pending, PresenceState::AwayWhileRunning, RawDeltas { steps: 0, distance_m: 1, elapsed_s: 1 })
            .unwrap();

        assert_eq!(pending.distance_m, 0);
        let today = store.today_stats().unwrap();
        assert_eq!(today.distance_m, 0);
        assert_eq!(today.walking_time_s, 0);
    }
}
