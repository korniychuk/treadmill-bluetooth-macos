//! Zone Hold BLE side-effects from pure [`ZoneWrite`] decisions (задача 053).

use btleplug::platform::Peripheral;
use tracing::{info, warn};

use super::SPEED_RESTORE_TIMEOUT;
use super::commands::{ControlSource, execute_control_command};
use super::speed::restore_speed;
use crate::control_command::ControlCommand;
use crate::speed::CentiKmh;
use crate::zone_session::ZoneWrite;

/// Execute a pure [`ZoneWrite`] from [`ZoneSession::tick`] (BLE effect side).
pub(super) async fn execute_zone_write(peripheral: &Peripheral, write: ZoneWrite) {
    match write {
        ZoneWrite::SetSpeed { target } => {
            apply_zone_hold_speed(peripheral, target, false).await;
        }
        ZoneWrite::Suppressed { target } => {
            apply_zone_hold_speed(peripheral, target, true).await;
        }
        ZoneWrite::Stop => {
            let _ = tokio::time::timeout(
                SPEED_RESTORE_TIMEOUT,
                execute_control_command(peripheral, ControlCommand::Stop, ControlSource::Zone),
            )
            .await;
        }
    }
}

/// Apply one Zone Hold speed correction, reusing the bounded
/// [`restore_speed`]/[`SPEED_RESTORE_TIMEOUT`] round-trip (задачи 007/012). A
/// failed/timed-out write is logged, not propagated — the same "never tear
/// down the session over a convenience write" rule as `try_restore_speed`/
/// `try_apply_default_speed`. When `suppressed` (operator override window,
/// задача 039), skip the write and log once at this call site.
pub(super) async fn apply_zone_hold_speed(
    peripheral: &Peripheral,
    target: CentiKmh,
    suppressed: bool,
) {
    let source = ControlSource::Zone;
    if suppressed {
        info!(
            %target,
            control_source = source.as_str(),
            "zone hold: suppressed, operator override active"
        );
        return;
    }
    match tokio::time::timeout(SPEED_RESTORE_TIMEOUT, restore_speed(peripheral, target)).await {
        Ok(Ok(())) => info!(
            %target,
            control_source = source.as_str(),
            "zone hold: applied speed correction"
        ),
        Ok(Err(err)) => {
            warn!(
                %err,
                %target,
                control_source = source.as_str(),
                "zone hold: speed correction write failed"
            )
        }
        Err(_) => warn!(
            timeout_s = SPEED_RESTORE_TIMEOUT.as_secs(),
            %target,
            control_source = source.as_str(),
            "zone hold: speed correction timed out (possible CoreBluetooth hang)"
        ),
    }
}
