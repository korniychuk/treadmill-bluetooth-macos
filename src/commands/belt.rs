//! Belt control commands (`start`/`stop`/`speed`/`incline`) and FTMS dispatch.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use btleplug::platform::Adapter;
use tracing::info;

use crate::commands::common::{daemon_process_alive, daemon_status_fresh};
use crate::control;
use crate::control_command::ControlCommand;
use crate::scan;
use crate::store;

/// How long the CLI waits for the daemon to run an enqueued command before
/// giving up. Comfortably above the daemon's ≤1s pick-up plus one
/// [`daemon::CONTROL_EXEC_TIMEOUT`]-bounded write, but short enough to fail
/// fast and tell the operator to retry.
pub(crate) const CONTROL_POLL_TIMEOUT: Duration = Duration::from_secs(8);

/// How often the CLI re-reads the command row while waiting.
pub(crate) const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Route a control command (start/stop/speed). When the daemon owns the live
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

    info!("daemon not holding the link — sending command over a direct connection");
    let adapter = scan::first_adapter().await?;
    let mapped = match command {
        ControlCommand::Start => Command::Start,
        ControlCommand::Stop => Command::Stop,
        ControlCommand::Speed(kmh) => Command::Speed(kmh),
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
        ControlCommand::Stop => "belt stopped".to_string(),
        ControlCommand::Speed(kmh) => format!("speed set to {kmh} km/h"),
    }
}

/// A one-shot FTMS command issued over a fresh connection.
pub(crate) enum Command {
    Start,
    Stop,
    Speed(f32),
    Incline(f32),
}

pub(crate) async fn run_command(adapter: &Adapter, command: Command) -> Result<()> {
    let peripheral = scan::connect_treadmill(adapter).await?;
    let controller = control::Controller::take_control(&peripheral).await?;
    match command {
        Command::Start => controller.start().await?,
        Command::Stop => controller.stop().await?,
        Command::Speed(kmh) => controller.set_speed(kmh).await?,
        Command::Incline(percent) => controller.set_incline(percent).await?,
    }
    Ok(())
}
