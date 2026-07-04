//! `treadmill-bluetooth-macos` — a macOS BLE connector for a Yesoul treadmill.
//!
//! Run with `--help` for the full command list. `scan` (list nearby BLE
//! devices) is the default when no subcommand is given.

mod control;
mod daemon;
mod discover;
mod fitshow;
mod ftms;
mod logger;
mod notify;
mod presence;
mod scan;
mod sniff;
mod store;

use anyhow::Result;
use btleplug::platform::Adapter;
use clap::{Parser, Subcommand};
use tokio::signal;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "treadmill-bluetooth-macos", version, about = "macOS BLE connector for a Yesoul treadmill")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// List every nearby BLE device (diagnostic). Default if no command is given.
    Scan,
    /// Connect to the first FTMS treadmill and stream telemetry (console + JSONL log).
    Connect,
    /// Run the presence-aware background daemon: auto-reconnect, SQLite daily
    /// stats, toast notifications. Normally installed as a LaunchAgent — see
    /// `scripts/install-daemon.sh` — but can be run in the foreground too.
    Daemon,
    /// Fire every toast notification once, with no BLE connection required —
    /// a smoke test for the notification pipeline (icon, identity, delivery).
    NotifyTest,
    /// Print accumulated daily walking statistics.
    Stats {
        /// Show every recorded day instead of just today.
        #[arg(long)]
        all: bool,
    },
    /// Start the belt via the FTMS Control Point.
    Start,
    /// Stop the belt via the FTMS Control Point.
    Stop,
    /// Set target speed, km/h.
    Speed {
        /// Target speed in km/h.
        kmh: f32,
    },
    /// Set target incline, percent. Kept for future hardware — this treadmill
    /// rejects it (see docs/tasks/003): no motorized incline over BLE.
    Incline {
        /// Target incline in percent.
        percent: f32,
    },
    /// Dump every GATT service/characteristic/descriptor to
    /// docs/research/gatt-snapshot.json (protocol reverse-engineering).
    Discover,
    /// Same as `discover`, but connects to a specific peripheral by its
    /// (opaque, macOS-assigned) UUID instead of scanning for FTMS.
    DiscoverId {
        /// Peripheral UUID as shown by `scan`.
        id: String,
    },
    /// Subscribe to every notify/indicate characteristic and log raw frames
    /// (protocol reverse-engineering).
    Sniff,
    /// Probe the vendor FitShow-style channels for a response (reverse-
    /// engineering; this firmware stays silent on every channel).
    FitshowProbe,
    /// Send a FitShow-framed speed+incline command (reverse-engineering probe).
    FitshowSet {
        /// Speed in km/h.
        kmh: f32,
        /// Incline level (device-specific units, not percent).
        incline_level: u8,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let command = Cli::parse().command.unwrap_or(Commands::Scan);

    // Reading stats and firing test notifications need no Bluetooth adapter —
    // handle them before touching Bluetooth at all.
    if let Commands::Stats { all } = command {
        return run_stats(all);
    }
    if let Commands::NotifyTest = command {
        return run_notify_test();
    }

    let adapter = scan::first_adapter().await?;
    match command {
        Commands::Scan => scan::scan_and_list(&adapter).await?,
        Commands::Connect => run_connect(&adapter).await?,
        Commands::Daemon => run_daemon(&adapter).await?,
        Commands::Start => run_command(&adapter, Command::Start).await?,
        Commands::Stop => run_command(&adapter, Command::Stop).await?,
        Commands::Speed { kmh } => run_command(&adapter, Command::Speed(kmh)).await?,
        Commands::Incline { percent } => run_command(&adapter, Command::Incline(percent)).await?,
        Commands::Discover => run_discover(&adapter).await?,
        Commands::DiscoverId { id } => {
            let peripheral = scan::connect_by_id(&adapter, &id).await?;
            discover::dump_gatt(&peripheral).await?;
        }
        Commands::Sniff => run_sniff(&adapter).await?,
        Commands::FitshowProbe => {
            let peripheral = scan::connect_treadmill(&adapter).await?;
            let fs = fitshow::FitShow::attach(&peripheral).await?;
            fs.probe_info().await?;
        }
        Commands::FitshowSet { kmh, incline_level } => {
            let peripheral = scan::connect_treadmill(&adapter).await?;
            let fs = fitshow::FitShow::attach(&peripheral).await?;
            fs.set_speed_incline(kmh, incline_level).await?;
        }
        Commands::Stats { .. } | Commands::NotifyTest => {
            unreachable!("handled above, before the adapter was opened")
        }
    }

    Ok(())
}

async fn run_connect(adapter: &Adapter) -> Result<()> {
    let peripheral = scan::connect_treadmill(adapter).await?;

    // Stop streaming on Ctrl-C so the peripheral is dropped (and disconnected)
    // cleanly instead of leaking the CoreBluetooth connection.
    tokio::select! {
        result = scan::stream_treadmill_data(&peripheral) => result?,
        _ = signal::ctrl_c() => info!("interrupted — disconnecting"),
    }

    Ok(())
}

async fn run_discover(adapter: &Adapter) -> Result<()> {
    let peripheral = scan::connect_treadmill(adapter).await?;
    discover::dump_gatt(&peripheral).await
}

async fn run_sniff(adapter: &Adapter) -> Result<()> {
    let peripheral = scan::connect_treadmill(adapter).await?;
    tokio::select! {
        result = sniff::sniff_all(&peripheral) => result?,
        _ = signal::ctrl_c() => info!("interrupted — disconnecting"),
    }
    Ok(())
}

/// Run the presence-aware background daemon: scan → connect → stream →
/// reconnect forever, until interrupted (Ctrl-C or LaunchAgent stop).
async fn run_daemon(adapter: &Adapter) -> Result<()> {
    tokio::select! {
        result = daemon::run(adapter) => result?,
        _ = signal::ctrl_c() => info!("interrupted — shutting down daemon"),
    }
    Ok(())
}

/// Fire every toast once, spaced out so they render as separate banners
/// instead of collapsing into one Notification Center group.
fn run_notify_test() -> Result<()> {
    let toasts: [(&str, fn()); 6] = [
        ("found", notify::treadmill_found),
        ("lost", notify::treadmill_lost),
        ("away", notify::walker_away),
        ("resumed (from away)", notify::walker_resumed),
        ("paused", notify::treadmill_paused),
        ("resumed (from pause)", notify::treadmill_resumed),
    ];
    for (label, send) in toasts {
        println!("sending: {label}");
        send();
        std::thread::sleep(std::time::Duration::from_millis(800));
    }
    Ok(())
}

/// Print today's accumulated stats, or every recorded day with `--all`.
fn run_stats(all: bool) -> Result<()> {
    let store = store::Store::open()?;
    if all {
        for day in store.all_stats()? {
            print_day(&day);
        }
    } else {
        print_day(&store.today_stats()?);
    }
    Ok(())
}

fn print_day(day: &store::DailyStats) {
    let minutes = day.walking_time_s / 60;
    let seconds = day.walking_time_s % 60;
    println!(
        "{}: {} steps, {:.2} km, {}m{:02}s walking",
        day.date,
        day.steps,
        day.distance_m as f64 / 1000.0,
        minutes,
        seconds
    );
}

/// A one-shot FTMS command issued over a fresh connection.
enum Command {
    Start,
    Stop,
    Speed(f32),
    Incline(f32),
}

async fn run_command(adapter: &Adapter, command: Command) -> Result<()> {
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

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("treadmill_bluetooth_macos=info,warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
