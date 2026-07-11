//! `treadmill-bluetooth-macos` — a macOS BLE connector for a Yesoul treadmill.
//!
//! Run with `--help` for the full command list. `scan` (list nearby BLE
//! devices) is the default when no subcommand is given.

mod activity;
mod commands;
mod config_apply;
mod control;
mod control_command;
mod daemon;
mod default_speed;
mod discover;
mod fitshow;
mod ftms;
mod goals;
mod hr;
mod logger;
mod notify;
mod power;
mod presence;
mod recompute;
mod recompute_hr;
mod scan;
mod sniff;
mod store;
mod widget;
mod zone_hold;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::commands::belt::{Command, run_command};
use crate::commands::{
    refuse_if_daemon_live, run_connect, run_control, run_daemon, run_default_speed, run_discover,
    run_doctor, run_fitshow_probe, run_fitshow_set, run_hr, run_notify_test, run_sniff, run_stats,
    run_status, run_zone,
};
use crate::control_command::ControlCommand;
use crate::widget::{run_speed_widget, run_widget};

#[derive(Parser)]
#[command(
    name = "treadmill-bluetooth-macos",
    version,
    about = "macOS BLE connector for a Yesoul treadmill"
)]
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
    /// Liveness matrix for diagnosis (задача 038): process/heartbeat, belt
    /// telemetry age, HR link/contact inference, zone config vs phase. Read-only,
    /// no BLE — same contract as `status`/`widget`.
    Doctor,
    /// Rebuild `activity_segments` from `raw_samples` by replaying the live
    /// presence + credit engine over history (задача 015). One-off, no BLE;
    /// idempotent; leaves `daily_stats`/`raw_samples`/`workouts` untouched.
    RecomputeSegments,
    /// Delete frozen-bpm samples recorded from a strap off the body, by replaying
    /// `hr_samples` through the live contact tracker (задача 034). No BLE;
    /// idempotent; touches nothing but `hr_samples`.
    RecomputeHr {
        /// Report what would be deleted without touching the database.
        #[arg(long)]
        dry_run: bool,
    },
    /// Emit a compact, machine-readable snapshot for a status-bar widget
    /// (tmux/Dracula). Prints one TSV line `state\tworkout_count\tcur_walking_s\t
    /// cur_steps\tcur_distance_m\tday_walking_s\tday_steps\tday_distance_m\t
    /// hr_bpm\thr_battery_pct\thr_zone\tspeed_kmh` while the treadmill is
    /// connected and the daemon heartbeat is fresh, or nothing at all
    /// otherwise (so the widget hides). `cur_*` is the current workout,
    /// `day_*` today's calendar totals; both are presence-filtered walking
    /// (no step-away/pause). `hr_bpm` is empty unless a heart-rate sensor is
    /// worn and its reading is fresh (задача 025); `hr_battery_pct` is the
    /// sensor's last-read battery level, empty if unknown (задача 026).
    /// `speed_kmh` is the live belt speed, empty unless `tm speed-widget on`
    /// (задача 029). Like `status`,
    /// never opens the BLE adapter. See
    /// docs/tasks/009, 014, 027 and 029.
    Widget,
    /// Print the computed default belt speed the daemon would apply at the next
    /// workout start — the trimmed-mean cruising pace of your most recent ≥30min
    /// workout (задача 016). Read-only, no BLE.
    DefaultSpeed,
    /// Diagnostic: connect to a heart-rate sensor (e.g. Polar H10) and print
    /// live bpm to stdout until Ctrl-C. No production use — `stats`/`widget`/
    /// `status` surface heart rate from the daemon instead (see docs/tasks/025).
    Hr,
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
    /// Zone Hold — HR-adaptive belt-speed control (задача 027): `on`/`off`,
    /// `setup` (re-run onboarding), `limits`/`target`/`mode` to tune it, or no
    /// sub-action to print current status. Read/write config only — no BLE,
    /// same as `stats`/`status`; the daemon picks up config edits live
    /// (задача 017's hot-reload).
    Zone {
        #[command(subcommand)]
        action: Option<ZoneAction>,
    },
    /// Toggle the live belt-speed field in `tm widget` (задача 029): `on`/
    /// `off`, or no sub-action to print the current setting. Not `speed` —
    /// that command already sets the belt's *target* speed via the Control
    /// Point. Read/write config only, no BLE.
    SpeedWidget {
        #[command(subcommand)]
        action: Option<SpeedWidgetAction>,
    },
}

#[derive(Subcommand)]
enum SpeedWidgetAction {
    /// Show the live belt speed (km/h) in `tm widget`.
    On,
    /// Hide the live belt speed field.
    Off,
}

#[derive(Subcommand)]
enum ZoneAction {
    /// Enable Zone Hold. Runs the interactive onboarding prompt (age, optional
    /// resting HR) the first time `age` isn't yet configured.
    On,
    /// Disable Zone Hold (master switch off) — no BLE, no corrections.
    Off,
    /// Re-run the interactive onboarding prompt, overwriting age/resting HR.
    Setup,
    /// Set the global speed limits: `tm zone limits <max> [<min>]`, or
    /// `tm zone limits --min <min>` to update only the minimum.
    Limits {
        /// Max speed, km/h (first positional argument).
        max: Option<f32>,
        /// Min speed, km/h (second positional argument).
        min: Option<f32>,
        /// Set only the min speed (use when you don't want to touch max).
        #[arg(long = "min")]
        min_flag: Option<f32>,
    },
    /// Set the target zone: a 1-based number, an `id`, or a (sub)string of
    /// the zone's name — see `tm zone list` for what's configured.
    Target {
        /// e.g. `2`, `aerobic-base`, or `aerobic`.
        zone: String,
    },
    /// List every configured zone with its id, bpm range, and effective max
    /// speed — so `target` isn't a guessing game.
    List,
    /// Interactively add a custom zone (name, id, bpm bounds, optional
    /// per-zone max speed). Appending a custom zone keeps every zone already
    /// configured — including the built-in 5, if none were customised yet.
    Add,
    /// Interactively edit an existing zone's name/bounds/max speed. The `id`
    /// itself isn't editable here (it's what `target_zone` may point at) —
    /// remove and re-add it to rename the id.
    Edit {
        /// e.g. `2`, `aerobic-base`, or `aerobic`.
        zone: String,
    },
    /// Remove a configured zone. Refuses to remove the last remaining one.
    Remove {
        /// e.g. `2`, `aerobic-base`, or `aerobic`.
        zone: String,
    },
    /// Set the targeting aggressiveness: `band` (hold the zone) or `center`
    /// (hold the midpoint, more corrections).
    Mode {
        /// `band` or `center`.
        tracking: String,
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
    if let Commands::Doctor = command {
        return run_doctor();
    }
    if let Commands::RecomputeSegments = command {
        refuse_if_daemon_live("recompute-segments")?;
        return recompute::run();
    }
    if let Commands::RecomputeHr { dry_run } = command {
        refuse_if_daemon_live("recompute-hr")?;
        return recompute_hr::run(dry_run);
    }
    if let Commands::Widget = command {
        return run_widget();
    }
    if let Commands::NotifyTest = command {
        return run_notify_test();
    }
    if let Commands::DefaultSpeed = command {
        return run_default_speed();
    }
    if let Commands::Zone { action } = command {
        return run_zone(action);
    }
    if let Commands::SpeedWidget { action } = command {
        return run_speed_widget(action);
    }
    // Control commands route through the daemon's queue when it holds the BLE
    // link (two processes can't co-own the connection — задача 013), and only
    // fall back to a direct connection when the daemon is off. Handled here,
    // before the adapter is opened, so the enqueue path never touches BLE.
    if let Commands::Start = command {
        return run_control(ControlCommand::Start).await;
    }
    if let Commands::Stop = command {
        return run_control(ControlCommand::Stop).await;
    }
    if let Commands::Speed { kmh } = command {
        return run_control(ControlCommand::Speed(kmh)).await;
    }

    let adapter = scan::first_adapter().await?;
    match command {
        Commands::Scan => scan::scan_and_list(&adapter).await?,
        Commands::Connect => run_connect(&adapter).await?,
        Commands::Hr => run_hr(&adapter).await?,
        Commands::Daemon => run_daemon(&adapter).await?,
        Commands::Incline { percent } => run_command(&adapter, Command::Incline(percent)).await?,
        Commands::Discover => run_discover(&adapter).await?,
        Commands::DiscoverId { id } => {
            let peripheral = scan::connect_by_id(&adapter, &id).await?;
            discover::dump_gatt(&peripheral).await?;
        }
        Commands::Sniff => run_sniff(&adapter).await?,
        Commands::FitshowProbe => run_fitshow_probe(&adapter).await?,
        Commands::FitshowSet { kmh, incline_level } => {
            run_fitshow_set(&adapter, kmh, incline_level).await?
        }
        Commands::Stats { .. }
        | Commands::Status
        | Commands::Doctor
        | Commands::RecomputeSegments
        | Commands::RecomputeHr { .. }
        | Commands::Widget
        | Commands::NotifyTest
        | Commands::DefaultSpeed
        | Commands::Zone { .. }
        | Commands::SpeedWidget { .. }
        | Commands::Start
        | Commands::Stop
        | Commands::Speed { .. } => {
            unreachable!("handled above, before the adapter was opened")
        }
    }

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("treadmill_bluetooth_macos=info,warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
