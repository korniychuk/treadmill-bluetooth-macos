//! Background HR sensor connect attempts (задача 025/026).

use std::pin::Pin;
use std::time::Duration;

use btleplug::api::{Peripheral as _, ValueNotification};
use btleplug::platform::{Adapter, Peripheral};
use futures::Stream;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::scan;

/// How often the daemon retries finding/connecting an HR sensor while one
/// isn't currently linked (no strap worn, or the last link was lost). Coarser
/// than the treadmill's own reconnect: an HR sensor absence is the common case
/// (not everyone wears the strap every walk), so this must not spam scans.
pub(super) const HR_RECONNECT_INTERVAL: Duration = Duration::from_secs(30);

/// How often to check whether it's time to re-read the HR sensor's battery
/// level (задача 026) — a cheap in-memory elapsed-time check, same pattern as
/// `CONFIG_RELOAD_INTERVAL`'s mtime check. The actual re-read cadence is
/// owned by [`HrSession::battery_read_due`]; this just bounds how promptly a
/// newly-crossed threshold is noticed.
pub(super) const HR_BATTERY_CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// A live notification stream from an HR peripheral (matches the type
/// `btleplug::api::Peripheral::notifications` returns).
pub(super) type HrNotificationStream = Pin<Box<dyn Stream<Item = ValueNotification> + Send>>;

/// Result of one background HR connect attempt (задача 025), sent back over a
/// channel so scanning (up to [`scan::SCAN_TIMEOUT`] when no strap is worn —
/// the common case) never blocks the main treadmill telemetry loop.
pub(super) enum HrConnectOutcome {
    /// The initial battery reading (задача 026), taken right after subscribe
    /// while the spawned task is already there — `None` if the read failed
    /// (logged inside `scan::read_hr_battery`), not a reason to abort the
    /// connection itself.
    Connected(Peripheral, HrNotificationStream, Option<u8>),
    NotFound,
}

/// Scan for, connect to, and subscribe an HR sensor (Polar H10) on a spawned
/// task, reporting the outcome back over `tx`. Best-effort throughout: no
/// strap worn is the normal case, not an error — every failure path here logs
/// and reports [`HrConnectOutcome::NotFound`] rather than propagating, so a
/// missing/lost sensor can never affect the treadmill session.
pub(super) fn spawn_hr_connect_attempt(adapter: Adapter, tx: UnboundedSender<HrConnectOutcome>) {
    tokio::spawn(async move {
        let outcome = match scan::connect_hr(&adapter).await {
            Ok(peripheral) => {
                if !scan::subscribe_hr(&peripheral).await {
                    scan::disconnect_best_effort(&peripheral).await;
                    HrConnectOutcome::NotFound
                } else {
                    match tokio::time::timeout(scan::CONNECT_TIMEOUT, peripheral.notifications())
                        .await
                    {
                        Ok(Ok(stream)) => {
                            let battery_pct = scan::read_hr_battery(&peripheral).await;
                            HrConnectOutcome::Connected(peripheral, stream, battery_pct)
                        }
                        Ok(Err(err)) => {
                            warn!(%err, "failed to open HR notification stream");
                            scan::disconnect_best_effort(&peripheral).await;
                            HrConnectOutcome::NotFound
                        }
                        Err(_) => {
                            warn!(
                                "opening HR notification stream timed out (possible CoreBluetooth hang)"
                            );
                            scan::disconnect_best_effort(&peripheral).await;
                            HrConnectOutcome::NotFound
                        }
                    }
                }
            }
            Err(err) => {
                info!(%err, "no HR sensor found this attempt — will retry");
                HrConnectOutcome::NotFound
            }
        };
        // The receiver only drops when the session ends; a send failure there
        // is a harmless race with teardown, nothing to recover.
        let _ = tx.send(outcome);
    });
}
