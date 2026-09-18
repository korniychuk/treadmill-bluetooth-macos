//! Control-command queue drain on the live BLE link (задача 013/039/063).

use anyhow::{Result, bail};
use btleplug::platform::Peripheral;
use chrono::Utc;
use tracing::{info, warn};

use super::SPEED_RESTORE_TIMEOUT;
use crate::belt_intent::{BeltIntent, RunIntent, StartSpeedPlan};
use crate::control::Controller;
use crate::control_command::{self, ControlCommand};
use crate::speed::CentiKmh;
use crate::store::Store;

use std::time::{Duration, Instant};

/// Backstop poll cadence for the control-command queue while connected but
/// quiet (no telemetry-driven check). Commands are also processed at the end
/// of every telemetry sample (~1/s), so this only matters during rare silent
/// stretches; keep it snappy but idle-cheap.
pub(super) const CONTROL_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Who initiated a Control Point write (задача 039). Logged on every write so
/// mid-Hold CLI speed overrides are diagnosable; not a priority arbiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ControlSource {
    Zone,
    Cli,
    AutoPause,
    Restore,
    DefaultSpeed,
}

impl ControlSource {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Zone => "zone",
            Self::Cli => "cli",
            Self::AutoPause => "auto_pause",
            Self::Restore => "restore",
            Self::DefaultSpeed => "default_speed",
        }
    }
}

/// Execute at most one pending control command on the live BLE link (задача
/// 013). Silent on the empty path — this runs ~1/s, so no happy-path log.
///
/// Returns `true` when a successful CLI `Speed` or resolved `speed_step`
/// ran (задача 039/063 — open the operator-override window so Zone Hold does
/// not immediately overwrite it). Toggle does not.
///
/// Two safety properties: a *stale* command (queued long ago, or while the
/// daemon was disconnected) is failed without executing, so it can never fire
/// a surprise belt change when the daemon reconnects/restarts; and a failed or
/// timed-out BLE write is logged and recorded on the row, never propagated —
/// a control write must not tear down an otherwise-healthy session. DB errors
/// still propagate, matching the rest of the loop.
///
/// Drains one command per call so a burst cannot block the select loop for
/// N×[`SPEED_RESTORE_TIMEOUT`] (reused here — the same bounded Control Point
/// round-trip); the next is picked up on the following tick.
pub(super) async fn process_control_commands(
    peripheral: &Peripheral,
    store: &Store,
    intent: &mut BeltIntent,
    live_speed: Option<CentiKmh>,
) -> Result<bool> {
    let Some(queued) = store.next_pending_control_command()? else {
        return Ok(false);
    };

    if control_command::is_stale(queued.created_at, Utc::now()) {
        warn!(id = queued.id, command = %queued.command.to_wire(), "control command is stale — failing without executing");
        store.mark_control_command_failed(queued.id, "stale, not executed")?;
        return Ok(false);
    }

    let now = Instant::now();
    let command_wire = queued.command.to_wire();
    let executed = match resolve_relative_command(queued.command, intent, live_speed, now) {
        RelativeResolve::Ready(cmd) => cmd,
        RelativeResolve::Refused(reason) => {
            warn!(
                id = queued.id,
                command = %command_wire,
                live_speed = live_speed.map(|s| s.to_string()),
                reason,
                "speed step refused"
            );
            store.mark_control_command_failed(queued.id, reason)?;
            return Ok(false);
        }
        RelativeResolve::ToggleUnknown(cmd) => {
            warn!(
                id = queued.id,
                command = %command_wire,
                live_speed = live_speed.map(|s| s.to_string()),
                "toggle with unknown live speed — stopping"
            );
            cmd
        }
    };

    let source = ControlSource::Cli;
    // Open the override only when speed is written now, not during a countdown.
    let was_speed = matches!(executed, ControlCommand::Speed(_));
    match tokio::time::timeout(
        SPEED_RESTORE_TIMEOUT,
        execute_control_command(peripheral, executed, source),
    )
    .await
    {
        Ok(Ok(())) => {
            info!(
                id = queued.id,
                command = %command_wire,
                resolved = %executed.to_wire(),
                control_source = source.as_str(),
                "executed queued control command"
            );
            // Timestamp after the BLE round-trip: a slow RequestControl must not
            // leave the recorded intent already outside INTENT_WINDOW.
            record_cli_intent(intent, executed, Instant::now());
            store.mark_control_command_done(queued.id)?;
            Ok(was_speed)
        }
        Ok(Err(err)) => {
            warn!(
                %err,
                id = queued.id,
                command = %command_wire,
                control_source = source.as_str(),
                "queued control command write failed"
            );
            store.mark_control_command_failed(queued.id, &err.to_string())?;
            Ok(false)
        }
        Err(_) => {
            warn!(
                id = queued.id,
                timeout_s = SPEED_RESTORE_TIMEOUT.as_secs(),
                control_source = source.as_str(),
                "queued control command timed out (possible CoreBluetooth hang)"
            );
            store.mark_control_command_failed(
                queued.id,
                "execution timed out (possible CoreBluetooth hang)",
            )?;
            Ok(false)
        }
    }
}

enum RelativeResolve {
    Ready(ControlCommand),
    Refused(&'static str),
    ToggleUnknown(ControlCommand),
}

fn resolve_relative_command(
    command: ControlCommand,
    intent: &BeltIntent,
    live_speed: Option<CentiKmh>,
    now: Instant,
) -> RelativeResolve {
    match command {
        ControlCommand::StartWithSpeed(target) => match intent.resolve_start_speed(live_speed, now)
        {
            StartSpeedPlan::SetSpeedNow => RelativeResolve::Ready(ControlCommand::Speed(target)),
            StartSpeedPlan::StartThenSpeed => RelativeResolve::Ready(command),
        },
        ControlCommand::SpeedStep(dir) => match intent.resolve_step(dir, live_speed, now) {
            Ok(speed) => RelativeResolve::Ready(ControlCommand::Speed(speed)),
            Err(reason) => RelativeResolve::Refused(reason.as_str()),
        },
        ControlCommand::Toggle => {
            let outcome = intent.resolve_toggle(live_speed, now);
            let cmd = match outcome.run {
                RunIntent::Start => ControlCommand::Start,
                RunIntent::Stop => ControlCommand::Stop,
            };
            if outcome.unknown_live {
                RelativeResolve::ToggleUnknown(cmd)
            } else {
                RelativeResolve::Ready(cmd)
            }
        }
        other => RelativeResolve::Ready(other),
    }
}

fn record_cli_intent(intent: &mut BeltIntent, executed: ControlCommand, now: Instant) {
    match executed {
        ControlCommand::StartWithSpeed(target) => {
            intent.note_run(RunIntent::Start, now);
            intent.arm_start_speed(target, now);
        }
        ControlCommand::Speed(speed) => intent.note_speed(speed, now),
        ControlCommand::Start => intent.note_run(RunIntent::Start, now),
        ControlCommand::Stop => intent.note_run(RunIntent::Stop, now),
        ControlCommand::Led(_) => {}
        ControlCommand::SpeedStep(_) | ControlCommand::Toggle => {
            unreachable!("relative commands are resolved before recording intent")
        }
    }
}

/// Take FTMS control and run one command. Split out so the whole round-trip can
/// be wrapped in a single bounded `timeout` by the caller. Reuses the same
/// take-control path as `restore_speed` and any other Control Point write (see
/// `control::Controller`). `source` is for call-site logging only — this
/// function does not log (the caller owns success/fail messages).
pub(super) async fn execute_control_command(
    peripheral: &Peripheral,
    command: ControlCommand,
    _source: ControlSource,
) -> Result<()> {
    let controller = Controller::take_control(peripheral).await?;
    match command {
        ControlCommand::Start | ControlCommand::StartWithSpeed(_) => controller.start().await,
        ControlCommand::Stop => controller.stop().await,
        ControlCommand::Speed(kmh) => controller.set_speed(kmh).await,
        ControlCommand::Led(state) => controller.set_led(state).await,
        ControlCommand::SpeedStep(_) | ControlCommand::Toggle => {
            bail!(
                "unresolved relative control command {} — resolve before execute",
                command.to_wire()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_start_speed_to_speed_only_when_moving() {
        let now = Instant::now();
        let intent = BeltIntent::new();
        let target = CentiKmh::from_wire(350);
        let start = ControlCommand::StartWithSpeed(target);
        for (live, expected) in [
            (
                Some(CentiKmh::from_wire(300)),
                ControlCommand::Speed(target),
            ),
            (Some(CentiKmh::ZERO), start),
            (None, start),
        ] {
            let RelativeResolve::Ready(actual) =
                resolve_relative_command(start, &intent, live, now)
            else {
                panic!("start-speed must resolve to a ready command");
            };
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn record_start_speed_arms_target_and_notes_start() {
        let now = Instant::now();
        let mut intent = BeltIntent::new();
        let target = CentiKmh::from_wire(350);
        record_cli_intent(&mut intent, ControlCommand::StartWithSpeed(target), now);
        assert_eq!(
            intent.resolve_toggle(Some(CentiKmh::ZERO), now).run,
            RunIntent::Stop
        );
        assert_eq!(intent.take_pending_start_speed(now), Some(target));
        assert_eq!(intent.take_pending_start_speed(now), None);
    }
}
