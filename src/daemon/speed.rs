//! Pause-resume and default-speed belt writes (задачи 012/016).

use anyhow::Result;
use btleplug::platform::Peripheral;
use tracing::{info, warn};

use super::SPEED_RESTORE_TIMEOUT;
use super::commands::ControlSource;
use crate::control::Controller;
use crate::default_speed;
use crate::goals;
use crate::notify;
use crate::speed::CentiKmh;
use crate::store::Store;
use crate::treadmill_link::TreadmillLink;

/// Minimum amount by which the pre-pause speed must exceed the resumed speed
/// to bother restoring — avoids a redundant Control Point write (and a
/// misleading toast) when the machine did not actually slow down on resume.
/// Same deadband magnitude as [`zone_hold::MIN_SPEED_CHANGE`] (5 centi).
const SPEED_RESTORE_EPSILON: CentiKmh = CentiKmh::from_wire(5);

/// Ceiling on the resumed belt speed for applying the computed default at a
/// workout start (задача 016): apply only when the belt is at/below the device's
/// factory crawl (~0.5), i.e. it just (re)started/reset and sits at its useless
/// default. A belt already moving faster means the operator chose that speed (or
/// a daemon restart landed mid-walk) — never override it. Same value as the
/// cruise floor: below it is not real walking (0.8 km/h).
const DEFAULT_SPEED_APPLY_CEILING: CentiKmh = CentiKmh::from_wire(80);

/// nothing worth restoring: the machine did not actually slow down (resumed at
/// the pre-pause speed or faster, within [`SPEED_RESTORE_EPSILON`]). Pure
/// and unit-tested — the BLE write lives in [`restore_speed`].
pub(super) fn speed_restore_target(pre_pause: CentiKmh, resumed: CentiKmh) -> Option<CentiKmh> {
    (pre_pause.to_wire()
        > resumed
            .to_wire()
            .saturating_add(SPEED_RESTORE_EPSILON.to_wire()))
    .then_some(pre_pause)
}

/// Best-effort restore of the pre-pause belt speed on a pause→walk resume
/// (задача 012, Task D). Returns the applied restore for the toast, or `None`
/// (with a WARN on the abnormal paths) when nothing was applied — a missing
/// captured speed, a no-op, or a failed/timed-out Control Point write must all
/// leave the session running, never crash it.
pub(super) async fn try_restore_speed(
    peripheral: &Peripheral,
    pre_pause: Option<CentiKmh>,
    resumed: CentiKmh,
) -> Option<notify::SpeedRestore> {
    let Some(pre_pause) = pre_pause else {
        // Daemon started already paused, or the pause preceded any walking.
        warn!("resume without a captured pre-pause speed — skipping speed restore");
        return None;
    };
    let target = speed_restore_target(pre_pause, resumed)?;
    let source = ControlSource::Restore;

    match tokio::time::timeout(SPEED_RESTORE_TIMEOUT, restore_speed(peripheral, target)).await {
        Ok(Ok(())) => {
            info!(
                from = %resumed,
                to = %target,
                control_source = source.as_str(),
                "restored pre-pause belt speed on resume"
            );
            Some(notify::SpeedRestore {
                from_kmh: resumed.to_kmh_f32(),
                to_kmh: target.to_kmh_f32(),
            })
        }
        Ok(Err(err)) => {
            warn!(%err, %target, control_source = source.as_str(), "failed to restore pre-pause speed — leaving resume toast without the restore line");
            None
        }
        Err(_) => {
            warn!(
                timeout_s = SPEED_RESTORE_TIMEOUT.as_secs(),
                %target,
                control_source = source.as_str(),
                "speed restore timed out (possible CoreBluetooth hang)"
            );
            None
        }
    }
}

/// Take FTMS control and set the target speed. Split from [`try_restore_speed`]
/// so the whole round-trip can be wrapped in one bounded `timeout` there.
pub(super) async fn restore_speed(peripheral: &Peripheral, target: CentiKmh) -> Result<()> {
    let controller = Controller::take_control(peripheral).await?;
    controller.set_speed(target).await
}

/// Apply the computed default belt speed at a workout start (задача 016), when
/// there is no pre-pause speed to restore. Returns the applied speed, or `None`
/// when nothing was applied. Guards, in order:
/// - once per session (`applied`) — one attempt per (re)start, no retry storm on
///   a presence flap at the crawl;
/// - the belt must be at/below the device crawl ([`DEFAULT_SPEED_APPLY_CEILING`])
///   — a belt already moving faster was set by the operator (or a daemon restart
///   landed mid-walk); never override it;
/// - a qualifying prior workout must exist ([`default_speed::compute_default_speed`]).
///
/// The BLE write reuses the bounded [`restore_speed`]/[`SPEED_RESTORE_TIMEOUT`]
/// path (задачи 007/012); a failed/timed-out write is logged and swallowed —
/// applying a convenience speed must never tear down the session.
pub(super) async fn try_apply_default_speed(
    peripheral: &Peripheral,
    store: &Store,
    resumed: CentiKmh,
    link: &mut TreadmillLink,
) -> Option<CentiKmh> {
    if link.default_speed_applied() {
        return None;
    }
    if resumed > DEFAULT_SPEED_APPLY_CEILING {
        // Belt already at a real speed — the operator's choice, or a mid-walk
        // reconnect. Not a fresh crawl start; leave it alone (and let a later
        // genuine crawl start still get its one attempt).
        return None;
    }

    let gap_minutes = goals::load_workout_gap_minutes();
    let target = match default_speed::compute_default_speed(store, gap_minutes) {
        Ok(Some(default)) => match CentiKmh::from_kmh_f32(default.kmh) {
            Some(t) => t,
            None => {
                warn!(
                    kmh = default.kmh,
                    "computed default speed out of range — skipping"
                );
                return None;
            }
        },
        Ok(None) => {
            info!(
                "no qualifying prior workout (≥30m walking) — leaving belt at its device default speed"
            );
            // Nothing to apply; don't recompute on every Walking flap this session.
            link.mark_default_speed_applied();
            return None;
        }
        Err(err) => {
            // A transient DB error may clear — do not consume the one attempt.
            warn!(%err, "failed to compute default speed — leaving belt at its device default");
            return None;
        }
    };

    // One attempt per session regardless of the write outcome (a failed write
    // must not loop on a presence flap at the crawl start).
    link.mark_default_speed_applied();
    let source = ControlSource::DefaultSpeed;
    match tokio::time::timeout(SPEED_RESTORE_TIMEOUT, restore_speed(peripheral, target)).await {
        Ok(Ok(())) => {
            info!(
                from = %resumed,
                to = %target,
                control_source = source.as_str(),
                "applied computed default belt speed at workout start"
            );
            Some(target)
        }
        Ok(Err(err)) => {
            warn!(%err, %target, control_source = source.as_str(), "failed to apply default belt speed at workout start — leaving belt as is");
            None
        }
        Err(_) => {
            warn!(
                timeout_s = SPEED_RESTORE_TIMEOUT.as_secs(),
                %target,
                control_source = source.as_str(),
                "default belt speed write timed out (possible CoreBluetooth hang)"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_restore_target_restores_only_a_real_slowdown() {
        let c = |kmh: f32| CentiKmh::from_kmh_f32(kmh).expect("test speed");
        // Typical case: paused at 2.5, machine resumed at 0.5 → restore 2.5.
        assert_eq!(speed_restore_target(c(2.5), c(0.5)), Some(c(2.5)));
        // No slowdown (resumed at the same speed) → nothing to restore.
        assert_eq!(speed_restore_target(c(2.5), c(2.5)), None);
        // Resumed faster than before → nothing to restore.
        assert_eq!(speed_restore_target(c(2.5), c(3.0)), None);
        // Within epsilon → treated as no change.
        assert_eq!(speed_restore_target(c(2.5), c(2.48)), None);
    }
}
