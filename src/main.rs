//! `treadmill-bluetooth-macos` — a macOS BLE connector for a Yesoul treadmill.
//!
//! First-cut CLI. Two modes:
//!   * `scan`    — list every nearby BLE device (diagnostic).
//!   * `connect` — connect to the first FTMS treadmill and stream its data.
//!
//! Run without arguments to `scan`.

mod control;
mod discover;
mod ftms;
mod scan;

use anyhow::{Context, Result};
use tokio::signal;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let mode = std::env::args().nth(1).unwrap_or_else(|| "scan".to_string());
    let adapter = scan::first_adapter().await?;

    match mode.as_str() {
        "scan" => scan::scan_and_list(&adapter).await?,
        "connect" => run_connect(&adapter).await?,
        "discover" => run_discover(&adapter).await?,
        "start" => run_command(&adapter, Command::Start).await?,
        "stop" => run_command(&adapter, Command::Stop).await?,
        "speed" => {
            let kmh: f32 = std::env::args()
                .nth(2)
                .context("usage: speed <km/h>")?
                .parse()
                .context("speed must be a number, km/h")?;
            run_command(&adapter, Command::Speed(kmh)).await?;
        }
        other => {
            error!(
                mode = other,
                "unknown mode; use `scan`, `connect`, `discover`, `start`, `stop`, or `speed <kmh>`"
            );
            std::process::exit(2);
        }
    }

    Ok(())
}

async fn run_connect(adapter: &btleplug::platform::Adapter) -> Result<()> {
    let peripheral = scan::connect_treadmill(adapter).await?;

    // Stop streaming on Ctrl-C so the peripheral is dropped (and disconnected)
    // cleanly instead of leaking the CoreBluetooth connection.
    tokio::select! {
        result = scan::stream_treadmill_data(&peripheral) => result?,
        _ = signal::ctrl_c() => info!("interrupted — disconnecting"),
    }

    Ok(())
}

async fn run_discover(adapter: &btleplug::platform::Adapter) -> Result<()> {
    let peripheral = scan::connect_treadmill(adapter).await?;
    discover::dump_gatt(&peripheral).await
}

/// A one-shot FTMS command issued over a fresh connection.
enum Command {
    Start,
    Stop,
    Speed(f32),
}

async fn run_command(adapter: &btleplug::platform::Adapter, command: Command) -> Result<()> {
    let peripheral = scan::connect_treadmill(adapter).await?;
    let controller = control::Controller::take_control(&peripheral).await?;
    match command {
        Command::Start => controller.start().await?,
        Command::Stop => controller.stop().await?,
        Command::Speed(kmh) => controller.set_speed(kmh).await?,
    }
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("treadmill_bluetooth_macos=info,warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
