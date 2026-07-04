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
//! unaffected. Caveat: a "lost" toast lags the real power-off by however long
//! the BLE supervision timeout takes to expire — expected, not a bug.

use std::time::Duration;

use anyhow::Result;
use btleplug::api::Peripheral as _;
use btleplug::platform::{Adapter, Peripheral};
use futures::StreamExt;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::ftms;
use crate::logger::WorkoutLogger;
use crate::notify;
use crate::presence::{PresenceState, PresenceTracker};
use crate::scan;
use crate::store::{RawDeltas, Store};

/// Delay before retrying discovery after a scan/connect failure, so a
/// transient Bluetooth hiccup does not spin the CPU in a tight loop.
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// Run the daemon forever: scan → connect → stream with presence tracking →
/// on disconnect, toast and go back to scanning.
pub async fn run(adapter: &Adapter) -> Result<()> {
    loop {
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

    while let Some(notification) = notifications.next().await {
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
