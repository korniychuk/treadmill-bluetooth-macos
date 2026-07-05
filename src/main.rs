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
mod power;
mod presence;
mod scan;
mod sniff;
mod store;

use anyhow::Result;
use btleplug::platform::Adapter;
use chrono::{DateTime, Local, Utc};
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
    /// Print accumulated daily walking statistics, including a per-workout breakdown.
    Stats {
        /// Show every recorded day instead of just today.
        #[arg(long)]
        all: bool,
    },
    /// Print daemon/treadmill/power state and today's workouts. Read-only —
    /// never opens the BLE adapter itself, so it cannot contend with a
    /// running daemon for it (see docs/tasks/006, задача B).
    Status,
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

    // Reading stats/status and firing test notifications need no Bluetooth
    // adapter — handle them before touching Bluetooth at all. `status` in
    // particular must never open the adapter: it has to work (and report
    // truthfully) while a daemon is already holding it.
    if let Commands::Stats { all } = command {
        return run_stats(all);
    }
    if let Commands::Status = command {
        return run_status();
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
        Commands::Stats { .. } | Commands::Status | Commands::NotifyTest => {
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

/// Print today's accumulated stats, or every recorded day with `--all` —
/// each followed by its per-workout breakdown (see docs/tasks/006, задача C).
fn run_stats(all: bool) -> Result<()> {
    let store = store::Store::open()?;
    if all {
        for day in store.all_stats()? {
            print_day(&store, &day)?;
        }
    } else {
        print_day(&store, &store.today_stats()?)?;
    }
    Ok(())
}

fn print_day(store: &store::Store, day: &store::DailyStats) -> Result<()> {
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
    for workout in store.workouts_for(&day.date)? {
        print_workout_line(&workout, "");
    }
    Ok(())
}

/// One `workouts` row, indented under its day/status header. `marker` is
/// appended verbatim (e.g. `" [in progress]"`) — empty for `stats`, which has
/// no notion of "currently running".
fn print_workout_line(workout: &store::Workout, marker: &str) {
    let minutes = workout.walking_time_s / 60;
    let seconds = workout.walking_time_s % 60;
    println!(
        "  #{} {}\u{2013}{}: {} steps, {:.2} km, {}m{:02}s{marker}",
        workout.id,
        format_local_time(&workout.started_at),
        format_local_time(&workout.ended_at),
        workout.steps,
        workout.distance_m as f64 / 1000.0,
        minutes,
        seconds
    );
}

/// Duplicated from `daemon::WATCHDOG_STALE_THRESHOLD` (private to
/// `daemon.rs`) — same rationale as `daemon.rs`'s own hand-kept duplicate of
/// `scan::SCAN_TIMEOUT`: no clean cross-module export yet, keep in sync by
/// hand. Used only to flag `daemon_status.updated_at` as possibly stale here.
const WATCHDOG_STALE_THRESHOLD_S: i64 = 15 /* scan */ + 20 /* connect */ + 60 /* margin */;

/// Print daemon/treadmill/power state and today's workouts, reading only
/// SQLite (`daemon_status` + `workouts`) and `launchctl` — never touches the
/// BLE adapter, so it cannot contend with a running daemon for it.
fn run_status() -> Result<()> {
    let store = store::Store::open()?;
    let status = store.daemon_status()?;
    let daemon_alive = daemon_process_alive();

    println!("daemon process: {}", if daemon_alive { "alive" } else { "NOT running" });

    match &status {
        None => println!("daemon status: never recorded (fresh install, or the daemon has never run)"),
        Some(status) => {
            if status.connected {
                let presence = status.presence_state.as_deref().unwrap_or("Unknown");
                let since =
                    status.last_connected_at.as_deref().map(describe_timestamp).unwrap_or_else(|| "unknown".to_string());
                println!("treadmill: connected, presence = {presence} (since {since})");
            } else {
                let ago = status
                    .last_disconnected_at
                    .as_deref()
                    .map(describe_timestamp)
                    .unwrap_or_else(|| "never connected".to_string());
                println!("treadmill: not connected (last seen {ago})");
            }

            let mode_desc = match status.power_mode.as_str() {
                "ac_scanning" => "on AC power, actively scanning",
                "battery_idle" => "on battery, idling (scanning paused to save power)",
                other => other,
            };
            println!("power mode: {mode_desc}, since {}", describe_timestamp(&status.power_mode_since));
            if status.power_mode == "battery_idle" {
                println!(
                    "  exits battery-idle immediately on: AC power restored, or system wake \
                     (event-driven power hooks, no polling delay — see docs/tasks/006, задача A)"
                );
            }

            match DateTime::parse_from_rfc3339(&status.updated_at) {
                Ok(updated_at) => {
                    let stale_s = (Utc::now() - updated_at.with_timezone(&Utc)).num_seconds();
                    if daemon_alive && stale_s > WATCHDOG_STALE_THRESHOLD_S {
                        println!(
                            "  WARNING: daemon_status last updated {stale_s}s ago (> {WATCHDOG_STALE_THRESHOLD_S}s \
                             threshold) while the process is alive — possible silent hang, see docs/tasks/006, задача D"
                        );
                    }
                }
                Err(err) => tracing::warn!(%err, updated_at = %status.updated_at, "status: unparseable daemon_status.updated_at"),
            }
        }
    }

    println!();
    println!("today's workouts:");
    let today = Local::now().format("%Y-%m-%d").to_string();
    let workouts = store.workouts_for(&today)?;
    if workouts.is_empty() {
        println!("  (none yet today)");
    } else {
        let last_id = workouts.last().map(|w| w.id);
        let in_progress = status.as_ref().is_some_and(|s| s.connected && s.presence_state.as_deref() == Some("Walking"));
        for workout in &workouts {
            let marker = if in_progress && Some(workout.id) == last_id { " [in progress]" } else { "" };
            print_workout_line(workout, marker);
        }
    }

    Ok(())
}

/// `now (Xm ago)`-style rendering of an RFC3339 timestamp in local time.
fn describe_timestamp(rfc3339: &str) -> String {
    match DateTime::parse_from_rfc3339(rfc3339) {
        Ok(dt) => {
            let utc = dt.with_timezone(&Utc);
            format!("{} ({})", utc.with_timezone(&Local).format("%Y-%m-%d %H:%M"), humanize_ago(Utc::now() - utc))
        }
        Err(err) => {
            tracing::warn!(%err, rfc3339, "status: unparseable timestamp");
            "unknown".to_string()
        }
    }
}

fn format_local_time(rfc3339: &str) -> String {
    match DateTime::parse_from_rfc3339(rfc3339) {
        Ok(dt) => dt.with_timezone(&Local).format("%H:%M").to_string(),
        Err(_) => rfc3339.to_string(),
    }
}

fn humanize_ago(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Is the daemon LaunchAgent actually running right now (real PID), not just
/// present-but-stale in `daemon_status`? Shells out to `launchctl print`
/// rather than trusting the DB row alone — a dead process can leave a
/// perfectly plausible-looking last-known state behind (see docs/tasks/006,
/// задача B's explicit warning against trusting stale DB rows).
fn daemon_process_alive() -> bool {
    let uid = match std::process::Command::new("id").arg("-u").output() {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout).trim().to_string(),
        Ok(output) => {
            tracing::warn!(code = ?output.status.code(), "status: `id -u` failed, assuming daemon not running");
            return false;
        }
        Err(err) => {
            tracing::warn!(%err, "status: failed to spawn `id -u`, assuming daemon not running");
            return false;
        }
    };

    let target = format!("gui/{uid}/com.korniychuk.treadmill-bluetooth-macos.daemon");
    match std::process::Command::new("launchctl").args(["print", &target]).output() {
        Ok(output) if output.status.success() => {
            // `launchctl print` succeeds for a *loaded* service even if it
            // crashed and isn't currently running — only a real `pid = N`
            // line means it's actually alive right now.
            String::from_utf8_lossy(&output.stdout).lines().any(|line| line.trim_start().starts_with("pid = "))
        }
        Ok(_) => false, // not loaded at all
        Err(err) => {
            tracing::warn!(%err, "status: failed to spawn `launchctl print`, assuming daemon not running");
            false
        }
    }
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
