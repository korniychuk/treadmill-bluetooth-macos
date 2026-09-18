//! Belt control commands (`start`/`stop`/`speed`/`incline`/`led`/`toggle`) and dispatch.

use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use btleplug::platform::Adapter;
use tracing::info;

use crate::belt_intent::{SPEED_MAX, SPEED_MIN, is_supported_target};
use crate::commands::common::{daemon_process_alive, daemon_status_fresh};
use crate::control;
use crate::control_command::{ControlCommand, StepDirection};
use crate::led::LedState;
use crate::scan;
use crate::speed::CentiKmh;
use crate::store;

/// Positional argument for `tm speed`: absolute km/h, or a relative step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SpeedTarget {
    Absolute(CentiKmh),
    Up,
    Down,
}

impl SpeedTarget {
    #[must_use]
    pub(crate) fn into_command(self) -> ControlCommand {
        match self {
            Self::Absolute(speed) => ControlCommand::Speed(speed),
            Self::Up => ControlCommand::SpeedStep(StepDirection::Up),
            Self::Down => ControlCommand::SpeedStep(StepDirection::Down),
        }
    }
}

impl FromStr for SpeedTarget {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "up" => Ok(Self::Up),
            "down" => Ok(Self::Down),
            other => parse_speed(other, "km/h, up, or down").map(Self::Absolute),
        }
    }
}

/// Quantize a CLI km/h value; `expected` names the accepted forms in the error.
fn parse_speed(raw: &str, expected: &str) -> Result<CentiKmh, String> {
    let kmh: f32 = raw
        .parse()
        .map_err(|_| format!("invalid speed {raw:?}; expected {expected}"))?;
    CentiKmh::from_kmh_f32(kmh).ok_or_else(|| format!("speed {kmh} km/h out of range"))
}

/// `tm start --speed` value (задача 065). Range-checked here because the
/// firmware's reject would only arrive after the countdown, long after the CLI
/// reported success.
pub(crate) fn parse_start_speed(raw: &str) -> Result<CentiKmh, String> {
    let speed = parse_speed(raw, "km/h")?;
    if !is_supported_target(speed) {
        return Err(format!(
            "speed {speed} km/h is outside the belt's range {SPEED_MIN}–{SPEED_MAX} km/h"
        ));
    }
    Ok(speed)
}

/// How long the CLI waits for the daemon to run an enqueued command before
/// giving up. Comfortably above the daemon's ≤1s pick-up plus one
/// [`daemon::CONTROL_EXEC_TIMEOUT`]-bounded write, but short enough to fail
/// fast and tell the operator to retry.
pub(crate) const CONTROL_POLL_TIMEOUT: Duration = Duration::from_secs(8);

/// How often the CLI re-reads the command row while waiting.
pub(crate) const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Route a control command (start/stop/speed/led). When the daemon owns the live
/// BLE link, enqueue the command and wait for the daemon to run it — the CLI
/// cannot open its own connection then, because the treadmill serves one
/// central at a time and stops advertising while connected (задача 013). When
/// the daemon is off, fall back to the original direct-BLE path. Only the
/// fallback touches the Bluetooth adapter.
pub(crate) async fn run_control(command: ControlCommand) -> Result<()> {
    let store = store::Store::open()?;
    if daemon_holds_link(&store) {
        return enqueue_and_wait(&store, command).await;
    }

    if command.requires_daemon_intent() {
        bail!(
            "{} needs the daemon connected to the treadmill (live speed and intent memory); check `tm status` — wake the console or start the daemon",
            command.to_wire()
        );
    }

    info!("daemon not holding the link — sending command over a direct connection");
    let adapter = scan::first_adapter().await?;
    let mapped = match command {
        ControlCommand::Start => Command::Start,
        ControlCommand::Stop => Command::Stop,
        ControlCommand::Speed(speed) => Command::Speed(speed),
        ControlCommand::Led(state) => Command::Led(state),
        ControlCommand::StartWithSpeed(_)
        | ControlCommand::SpeedStep(_)
        | ControlCommand::Toggle => {
            unreachable!("daemon-only commands bailed before the direct-BLE path")
        }
    };
    run_command(&adapter, mapped).await?;
    println!("{}", describe_control_success(&command));
    Ok(())
}

/// Whether the daemon is currently the sole owner of the BLE link — alive
/// (real PID), reporting `connected`, and with a fresh heartbeat. All three
/// are required: a dead or hung daemon can leave a stale `connected` row
/// behind, and routing to the queue then would hang the CLI on a command
/// nothing will ever run, when the direct fallback would have worked.
pub(crate) fn daemon_holds_link(store: &store::Store) -> bool {
    let status = match store.daemon_status() {
        Ok(Some(status)) => status,
        Ok(None) => return false,
        Err(err) => {
            tracing::warn!(%err, "control: failed to read daemon_status — falling back to a direct connection");
            return false;
        }
    };
    status.connected && daemon_status_fresh(&status) && daemon_process_alive()
}

/// Enqueue a command for the daemon and poll its row until it resolves or the
/// wait times out. Prints a clear result; a `failed` outcome or a timeout is a
/// non-zero exit so scripts can react.
pub(crate) async fn enqueue_and_wait(store: &store::Store, command: ControlCommand) -> Result<()> {
    let id = store.enqueue_control_command(&command)?;
    info!(id, command = %command.to_wire(), "daemon holds the link — enqueued command, waiting for it to run");

    let deadline = Instant::now() + CONTROL_POLL_TIMEOUT;
    loop {
        match store.control_command_outcome(id)? {
            Some((status, _)) if status == "done" => {
                println!("{}", describe_control_success(&command));
                return Ok(());
            }
            Some((status, error)) if status == "failed" => {
                bail!(
                    "treadmill command failed: {}",
                    error.unwrap_or_else(|| "unknown error".to_string())
                );
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            bail!(
                "daemon did not run the command within {}s — it may be busy or the treadmill just disconnected; try again",
                CONTROL_POLL_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(CONTROL_POLL_INTERVAL).await;
    }
}

/// Human-readable confirmation line printed once a control command succeeds
/// (via either path).
pub(crate) fn describe_control_success(command: &ControlCommand) -> String {
    match command {
        ControlCommand::Start => "belt started".to_string(),
        ControlCommand::StartWithSpeed(speed) => format!(
            "belt starting — {speed} km/h once the countdown ends (right away if already moving)"
        ),
        ControlCommand::Stop => "belt stopped".to_string(),
        ControlCommand::Toggle => "belt toggled".to_string(),
        ControlCommand::Speed(speed) => format!("speed set to {speed} km/h"),
        ControlCommand::SpeedStep(StepDirection::Up) => "speed stepped up".to_string(),
        ControlCommand::SpeedStep(StepDirection::Down) => "speed stepped down".to_string(),
        ControlCommand::Led(state) => format!("led strip turned {state}"),
    }
}

/// A one-shot command issued over a fresh connection.
pub(crate) enum Command {
    Start,
    Stop,
    Speed(CentiKmh),
    Incline(f32),
    Led(LedState),
}

pub(crate) async fn run_command(adapter: &Adapter, command: Command) -> Result<()> {
    let peripheral = scan::connect_treadmill(adapter).await?;
    let controller = control::Controller::take_control(&peripheral).await?;
    match command {
        Command::Start => controller.start().await?,
        Command::Stop => controller.stop().await?,
        Command::Speed(speed) => controller.set_speed(speed).await?,
        Command::Incline(percent) => controller.set_incline(percent).await?,
        Command::Led(state) => controller.set_led(state).await?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_start_speed_checks_supported_range() {
        assert_eq!(parse_start_speed("3.5"), Ok(CentiKmh::from_wire(350)));
        for raw in ["0.4", "6.2", "abc", "NaN", "inf", "-1"] {
            assert!(parse_start_speed(raw).is_err(), "{raw}");
        }
        assert_eq!(
            parse_start_speed("9").unwrap_err(),
            "speed 9 km/h is outside the belt's range 0.5–6.1 km/h"
        );
        assert!("9".parse::<SpeedTarget>().is_ok());
    }

    #[test]
    fn describe_start_speed_covers_both_execution_plans() {
        assert_eq!(
            describe_control_success(&ControlCommand::StartWithSpeed(CentiKmh::from_wire(350))),
            "belt starting — 3.5 km/h once the countdown ends (right away if already moving)"
        );
    }

    #[test]
    fn speed_target_parses_absolute_up_and_down() {
        assert_eq!(
            "3.2".parse::<SpeedTarget>().unwrap(),
            SpeedTarget::Absolute(CentiKmh::from_wire(320))
        );
        assert_eq!("up".parse::<SpeedTarget>().unwrap(), SpeedTarget::Up);
        assert_eq!("down".parse::<SpeedTarget>().unwrap(), SpeedTarget::Down);
        assert!("UP".parse::<SpeedTarget>().is_err());
        assert!("fast".parse::<SpeedTarget>().is_err());
        assert!("-1".parse::<SpeedTarget>().is_err());
    }

    #[test]
    fn speed_target_maps_to_control_commands() {
        assert_eq!(
            SpeedTarget::Absolute(CentiKmh::from_wire(250)).into_command(),
            ControlCommand::Speed(CentiKmh::from_wire(250))
        );
        assert_eq!(
            SpeedTarget::Up.into_command(),
            ControlCommand::SpeedStep(StepDirection::Up)
        );
        assert_eq!(
            SpeedTarget::Down.into_command(),
            ControlCommand::SpeedStep(StepDirection::Down)
        );
    }

    #[test]
    fn describe_control_success_covers_relative_commands() {
        assert_eq!(
            describe_control_success(&ControlCommand::SpeedStep(StepDirection::Up)),
            "speed stepped up"
        );
        assert_eq!(
            describe_control_success(&ControlCommand::SpeedStep(StepDirection::Down)),
            "speed stepped down"
        );
        assert_eq!(
            describe_control_success(&ControlCommand::Toggle),
            "belt toggled"
        );
    }
}
