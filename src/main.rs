//! `treadmill-bluetooth-macos` — a macOS BLE connector for a Yesoul treadmill.
//!
//! Run with `--help` for the full command list. `scan` (list nearby BLE
//! devices) is the default when no subcommand is given.

mod activity;
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
mod scan;
mod sniff;
mod store;
mod zone_hold;

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use btleplug::api::Peripheral as _;
use btleplug::platform::Adapter;
use chrono::{DateTime, Local, TimeZone, Utc};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use tokio::signal;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::control_command::ControlCommand;

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
    /// Rebuild `activity_segments` from `raw_samples` by replaying the live
    /// presence + credit engine over history (задача 015). One-off, no BLE;
    /// idempotent; leaves `daily_stats`/`raw_samples`/`workouts` untouched.
    RecomputeSegments,
    /// Emit a compact, machine-readable snapshot for a status-bar widget
    /// (tmux/Dracula). Prints one TSV line `state\tworkout_count\tcur_walking_s\t
    /// cur_steps\tcur_distance_m\tday_walking_s\tday_steps\tday_distance_m\t
    /// hr_bpm\thr_battery_pct` while the treadmill is connected and the daemon
    /// heartbeat is fresh, or nothing at all otherwise (so the widget hides).
    /// `cur_*` is the current workout, `day_*` today's calendar totals; both
    /// are presence-filtered walking (no step-away/pause). `hr_bpm` is empty
    /// unless a heart-rate sensor is worn and its reading is fresh (задача
    /// 025); `hr_battery_pct` is the sensor's last-read battery level, empty
    /// if unknown (задача 026). Like `status`,
    /// never opens the BLE adapter. See
    /// docs/tasks/009 and 014.
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
    if let Commands::RecomputeSegments = command {
        return recompute::run();
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
        Commands::Stats { .. }
        | Commands::Status
        | Commands::RecomputeSegments
        | Commands::Widget
        | Commands::NotifyTest
        | Commands::DefaultSpeed
        | Commands::Zone { .. }
        | Commands::Start
        | Commands::Stop
        | Commands::Speed { .. } => {
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

/// Diagnostic: connect to an HR sensor and print live bpm until Ctrl-C
/// (задача 025). Opens BLE directly — fine even while the daemon is running,
/// since the H10 offers two simultaneous connection slots.
async fn run_hr(adapter: &Adapter) -> Result<()> {
    let peripheral = scan::connect_hr(adapter).await?;
    if !scan::subscribe_hr(&peripheral).await {
        bail!("Heart Rate Measurement characteristic (0x2A37) not found on this device");
    }
    match scan::read_hr_battery(&peripheral).await {
        Some(pct) => println!("battery: {pct}%"),
        None => println!("battery: unknown (read failed — see logs)"),
    }

    let mut notifications = peripheral
        .notifications()
        .await
        .context("open HR notification stream")?;

    tokio::select! {
        _ = async {
            while let Some(notification) = notifications.next().await {
                if notification.uuid != hr::HEART_RATE_MEASUREMENT {
                    continue;
                }
                if let Some(m) = hr::parse_hr_measurement(&notification.value) {
                    println!("{} bpm", m.bpm);
                }
            }
        } => {}
        _ = signal::ctrl_c() => info!("interrupted — disconnecting"),
    }

    scan::disconnect_best_effort(&peripheral).await;
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
    // Closures wrap the toasts whose signatures now take arguments (away/pause
    // duration, goal tier) so they still fit the uniform `fn()` smoke-test
    // table. Sample durations/goals are illustrative — this path never touches
    // BLE or the real presence state.
    let sample_away = std::time::Duration::from_secs(157);
    let toasts: [(&str, &dyn Fn()); 10] = [
        ("found", &notify::treadmill_found),
        ("lost", &notify::treadmill_lost),
        ("away", &notify::walker_away),
        (
            "resumed (from away, with duration)",
            &(|| notify::walker_resumed(Some(sample_away))),
        ),
        ("paused", &notify::treadmill_paused),
        (
            "resumed (from pause, duration + speed restore)",
            &(|| {
                notify::treadmill_resumed(
                    Some(sample_away),
                    Some(notify::SpeedRestore {
                        from_kmh: 0.5,
                        to_kmh: 2.5,
                    }),
                )
            }),
        ),
        (
            "default speed applied (workout start)",
            &(|| notify::default_speed_applied(0.5, 2.5)),
        ),
        ("goal tier 1", &(|| notify::goal_reached(8000, 1))),
        ("goal tier 2", &(|| notify::goal_reached(10000, 2))),
        ("goal tier 3", &(|| notify::goal_reached(12000, 3))),
    ];
    for (label, send) in toasts {
        println!("sending: {label}");
        send();
        std::thread::sleep(std::time::Duration::from_millis(800));
    }
    Ok(())
}

/// Print the computed default belt speed the daemon would apply at the next
/// workout start, and which workout it was derived from (задача 016). Read-only.
fn run_default_speed() -> Result<()> {
    let store = store::Store::open()?;
    let gap_minutes = goals::load_workout_gap_minutes();
    match default_speed::compute_default_speed(&store, gap_minutes)? {
        Some(default) => {
            println!("computed default speed: {:.1} km/h", default.kmh);
            println!(
                "  from workout on {} ({} → {}, {} walking)",
                default.source.date,
                format_local_time(&default.source.started_at),
                format_local_time(&default.source.ended_at),
                fmt_duration(default.source.walking_time_s),
            );
            println!(
                "  {} walking samples, {} kept after 15% top/bottom trim",
                default.walking_samples, default.kept_samples,
            );
        }
        None => println!(
            "no qualifying workout yet (need one with \u{2265}30m of credited walking) — \
             the belt would stay at its device default speed"
        ),
    }
    Ok(())
}

/// Dispatch a `tm zone` sub-action (задача 027). Read/write config + SQLite
/// only — no BLE, same constraint as `status`/`widget`.
fn run_zone(action: Option<ZoneAction>) -> Result<()> {
    match action {
        None => print_zone_status(),
        Some(ZoneAction::On) => zone_on(),
        Some(ZoneAction::Off) => set_zone_hold_key("enabled", "false".to_string()).map(|()| {
            println!("Zone Hold disabled.");
        }),
        Some(ZoneAction::Setup) => zone_onboarding_prompt(),
        Some(ZoneAction::Limits { max, min, min_flag }) => zone_limits(max, min.or(min_flag)),
        Some(ZoneAction::Target { zone }) => zone_target(&zone),
        Some(ZoneAction::List) => zone_list(),
        Some(ZoneAction::Mode { tracking }) => zone_mode(&tracking),
    }
}

/// `tm zone on`: enable the master switch, running the interactive onboarding
/// prompt first if `age` isn't configured yet (задача 027, §Onboarding).
fn zone_on() -> Result<()> {
    if zone_hold::load_zone_hold_config().age.is_none() {
        return zone_onboarding_prompt();
    }
    set_zone_hold_key("enabled", "true".to_string())?;
    println!("Zone Hold enabled.");
    Ok(())
}

/// Interactive age/resting-HR prompt (задача 027, §Onboarding) — writes
/// `enabled = true` alongside whatever was entered, since the whole point of
/// running this is to turn Zone Hold on. Used by both `tm zone on` (first run)
/// and `tm zone setup` (reconfigure).
fn zone_onboarding_prompt() -> Result<()> {
    println!("Zone Hold setup");
    let age = prompt_age()?;
    let resting_hr = prompt_optional_resting_hr()?;

    let mut updates = vec![("enabled", "true".to_string()), ("age", age.to_string())];
    if let Some(resting_hr) = resting_hr {
        updates.push(("resting_hr", resting_hr.to_string()));
    }
    let path = zone_hold_config_path()?;
    zone_hold::upsert_zone_hold_keys(&path, &updates)?;

    let hrmax = zone_hold::hrmax_tanaka(age);
    println!("HRmax (Tanaka) \u{2248} {hrmax:.0} bpm.");
    println!(
        "Zone Hold enabled — target zone #{} (default: Aerobic base, 60-70% HRmax).",
        zone_hold::DEFAULT_TARGET_ZONE
    );
    println!(
        "See all zones with `tm zone list`; change the target with \
         `tm zone target <n|id|name>`, tune limits with `tm zone limits`, or edit \
         `[[zone_hold.zones]]` in config.toml directly for custom bounds."
    );
    Ok(())
}

fn prompt_age() -> Result<u32> {
    use std::io::Write;
    loop {
        print!("Your age (for the HRmax estimate): ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("read age from stdin")?;
        match line.trim().parse::<u32>() {
            Ok(age) if (1..=120).contains(&age) => return Ok(age),
            _ => println!("please enter a whole number between 1 and 120"),
        }
    }
}

/// `None` on an empty line (skip) or an implausible value — resting HR is
/// optional, so a bad entry just falls back to skipping it rather than
/// looping forever on an optional field.
fn prompt_optional_resting_hr() -> Result<Option<u16>> {
    use std::io::Write;
    print!("Resting heart rate, bpm (optional — press Enter to skip): ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("read resting HR from stdin")?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match trimmed.parse::<u16>() {
        Ok(hr) if (30..=120).contains(&hr) => Ok(Some(hr)),
        _ => {
            println!("not a plausible resting heart rate — skipping (hrmax method will be used)");
            Ok(None)
        }
    }
}

/// `tm zone limits <max> [<min>]` / `tm zone limits --min <min>` (задача 027,
/// §Min/max) — writes the *global* `max_speed`/`min_speed` keys; per-zone
/// overrides stay a manual config edit.
fn zone_limits(max: Option<f32>, min: Option<f32>) -> Result<()> {
    if max.is_none() && min.is_none() {
        bail!(
            "specify at least a max speed, e.g. `tm zone limits 5` or `tm zone limits --min 2.5`"
        );
    }
    let mut updates = Vec::new();
    if let Some(max) = max {
        updates.push(("max_speed", max.to_string()));
    }
    if let Some(min) = min {
        updates.push(("min_speed", min.to_string()));
    }
    let path = zone_hold_config_path()?;
    zone_hold::upsert_zone_hold_keys(&path, &updates)?;
    println!(
        "Zone Hold limits updated:{}{}",
        max.map(|m| format!(" max {m} km/h")).unwrap_or_default(),
        min.map(|m| format!(" min {m} km/h")).unwrap_or_default(),
    );
    Ok(())
}

/// `tm zone target <n|id|name-substring>` — resolves against the currently
/// configured zones and persists the *canonical id* (not the raw input), so
/// a fuzzy name match today still points at the right zone if `config.toml`
/// is reordered later. Bare numbers keep the legacy numeric form, since that
/// selector has no id to normalise to.
fn zone_target(raw: &str) -> Result<()> {
    let config = zone_hold::load_zone_hold_config();
    let trimmed = raw.trim();
    let selector = match trimmed.parse::<u8>() {
        Ok(n) => zone_hold::ZoneSelector::Number(n),
        Err(_) => zone_hold::ZoneSelector::Id(trimmed.to_string()),
    };
    let Some((number, zone)) = zone_hold::find_zone(&config.zones, &selector) else {
        let known: Vec<String> = config
            .zones
            .iter()
            .enumerate()
            .map(|(i, z)| format!("#{} {} ({})", i + 1, z.id, z.name))
            .collect();
        bail!(
            "no zone matches `{trimmed}` — configured zones: {}",
            known.join(", ")
        );
    };
    let value = match selector {
        zone_hold::ZoneSelector::Number(n) => n.to_string(),
        zone_hold::ZoneSelector::Id(_) => format!("\"{}\"", zone.id),
    };
    set_zone_hold_key("target_zone", value)?;
    println!(
        "Zone Hold target zone set to #{number} {} ({}).",
        zone.id, zone.name
    );
    Ok(())
}

/// `tm zone list` — every configured zone with its id and bpm range, so
/// `target` isn't a guessing game (задача 027 follow-up). Falls back to the
/// raw percent/bpm bounds when `age` isn't configured yet (no HRmax to
/// resolve percent zones against).
fn zone_list() -> Result<()> {
    let config = zone_hold::load_zone_hold_config();
    let hrmax = config.hrmax();
    let method_label = match config.method {
        zone_hold::Method::HrMax => "hrmax",
        zone_hold::Method::Karvonen => "karvonen",
    };
    if hrmax.is_none() {
        println!(
            "Configured zones (age not set — showing raw bounds, not bpm; run `tm zone setup`):"
        );
    } else {
        println!("Configured zones ({method_label}):");
    }
    let target_number = zone_hold::find_zone(&config.zones, &config.target_zone).map(|(n, _)| n);
    for (index, zone) in config.zones.iter().enumerate() {
        let number = index + 1;
        let marker = if target_number == Some(number) {
            "*"
        } else {
            " "
        };
        let range = match hrmax {
            Some(hrmax) => {
                let (low, high) = zone_hold::resolve_zone_bpm(
                    hrmax,
                    config.resting_hr,
                    config.method,
                    zone.bounds,
                );
                format!("{low}-{high} bpm")
            }
            None => match zone.bounds {
                zone_hold::ZoneBounds::Percent { min, max } => format!("{min:.0}-{max:.0}% HRmax"),
                zone_hold::ZoneBounds::Absolute { min_bpm, max_bpm } => {
                    format!("{min_bpm}-{max_bpm} bpm")
                }
            },
        };
        let max_speed = zone.max_speed_kmh.unwrap_or(config.max_speed_kmh);
        println!(
            "{marker} #{number} {:<14} id={:<16} {range:<16} max {max_speed:.1} km/h",
            zone.name, zone.id,
        );
    }
    println!("(* = current target; select with `tm zone target <id|name|number>`)");
    Ok(())
}

/// `tm zone mode <band|center>` (задача 027, §Режимы).
fn zone_mode(tracking: &str) -> Result<()> {
    match tracking {
        "band" | "center" => {
            set_zone_hold_key("tracking", format!("\"{tracking}\""))?;
            println!("Zone Hold tracking mode set to `{tracking}`.");
            Ok(())
        }
        other => bail!("unknown tracking mode `{other}` — use `band` or `center`"),
    }
}

/// Update a single `[zone_hold]` key in place, creating the config directory
/// if this is the first write to it.
fn set_zone_hold_key(key: &str, value: String) -> Result<()> {
    let path = zone_hold_config_path()?;
    zone_hold::upsert_zone_hold_keys(&path, &[(key, value)])
}

fn zone_hold_config_path() -> Result<std::path::PathBuf> {
    let path = zone_hold::config_path().context(
        "could not resolve the config path ($HOME unset) — set TREADMILL_CONFIG explicitly",
    )?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    Ok(path)
}

/// `tm zone` (no sub-action): current config plus, when the daemon has a
/// fresh snapshot, whether the controller is actively engaged right now
/// (задача 027).
fn print_zone_status() -> Result<()> {
    let config = zone_hold::load_zone_hold_config();
    println!("Zone Hold: {}", if config.enabled { "on" } else { "off" });
    if !config.enabled {
        println!("  enable with `tm zone on`");
        return Ok(());
    }

    match config.age {
        None => println!("  not configured yet — run `tm zone setup`"),
        Some(age) => {
            let method = match config.method {
                zone_hold::Method::HrMax => "hrmax",
                zone_hold::Method::Karvonen => "karvonen",
            };
            let tracking = match config.tracking {
                zone_hold::Tracking::Band => "band",
                zone_hold::Tracking::Center => "center",
            };
            println!("  age {age}, method {method}, tracking {tracking}");
            match config.resolve_target_zone() {
                Some(resolved) => println!(
                    "  target zone #{} {} ({}): {}-{} bpm \u{2022} speed {:.1}-{:.1} km/h",
                    resolved.number,
                    resolved.id,
                    resolved.name,
                    resolved.low_bpm,
                    resolved.high_bpm,
                    config.min_speed_kmh,
                    resolved.effective_max_speed_kmh,
                ),
                None => println!(
                    "  target zone not found among {} configured zones — see `tm zone list`",
                    config.zones.len()
                ),
            }
        }
    }

    let store = store::Store::open()?;
    match store.daemon_status()? {
        Some(status) if status.zone_hold_active => {
            let phase = status.zone_hold_phase.as_deref().unwrap_or("?");
            let range = match (status.zone_hold_target_lo, status.zone_hold_target_hi) {
                (Some(lo), Some(hi)) => format!("{lo}-{hi} bpm"),
                _ => "? bpm".to_string(),
            };
            let speed = status
                .zone_hold_last_speed
                .map(|s| format!("{s:.1} km/h"))
                .unwrap_or_else(|| "?".to_string());
            let position = status.zone_hold_position.as_deref().unwrap_or("\u{2014}");
            println!(
                "  active now: phase {phase}, {range}, last speed {speed}, position {position}"
            );
        }
        _ => println!("  not currently engaged (not walking, sensor not worn, or daemon idle)"),
    }
    Ok(())
}

/// Print today's accumulated stats, or every recorded day with `--all` —
/// each followed by its per-workout breakdown (see docs/tasks/006, задача C).
fn run_stats(all: bool) -> Result<()> {
    let store = store::Store::open()?;
    // Read-time workout grouping threshold (задача 014); daily totals below are
    // unaffected (strictly calendar, straight from `daily_stats`).
    let gap_minutes = goals::load_workout_gap_minutes();
    if all {
        for day in store.all_stats()? {
            print_day(&store, &day, gap_minutes)?;
        }
    } else {
        print_day(&store, &store.today_stats()?, gap_minutes)?;
    }
    Ok(())
}

fn print_day(store: &store::Store, day: &store::DailyStats, gap_minutes: i64) -> Result<()> {
    // The day header stays on filtered totals only: raw is shown per workout,
    // where the [started_at, ended_at] window makes reconstruction exact. A
    // day-level raw would have to sum workout spans, but `daily_stats` can
    // credit activity that never landed under this day's workouts (see the
    // midnight edge case in `store`), so that sum would silently understate.
    let hr = day_hr_summary(store, &day.date)
        .map(fmt_hr_summary)
        .unwrap_or_default();
    println!(
        "{}: {} steps, {:.2} km, {} walking{hr}",
        day.date,
        day.steps,
        day.distance_m as f64 / 1000.0,
        fmt_duration(day.walking_time_s),
    );
    for (i, workout) in store
        .workouts_for(&day.date, gap_minutes)?
        .iter()
        .enumerate()
    {
        print_workout_line(store, i + 1, workout, "");
    }
    Ok(())
}

/// One `workouts` row, indented under its day/status header. `marker` is
/// appended verbatim (e.g. `" [in progress]"`) — empty for `stats`, which has
/// no notion of "currently running".
///
/// The start→end range is spaced out with an arrow so the two clock times read
/// as distinct endpoints, not one run-on token. A dim `(raw …)` hint after the
/// distance and after the walking time shows the pre-filter figure — belt
/// distance/time including the moments the operator stepped off while it kept
/// spinning (see `store::raw_distance_m`); omitted when there's nothing extra.
fn print_workout_line(store: &store::Store, num: usize, workout: &store::Workout, marker: &str) {
    let (raw_dist, raw_time) = workout_raw(store, workout);
    let dist_hint = raw_hint(
        raw_dist.is_some_and(|d| d > workout.distance_m),
        &format!("{:.2}", raw_dist.unwrap_or(0) as f64 / 1000.0),
    );
    let time_hint = raw_hint(
        raw_time.is_some_and(|t| t > workout.walking_time_s),
        &fmt_duration(raw_time.unwrap_or(0)),
    );
    let hr = store
        .hr_summary_for(&workout.started_at, &workout.ended_at)
        .ok()
        .flatten()
        .map(fmt_hr_summary)
        .unwrap_or_default();
    // `num` is the workout's 1-based position within its day, not `workout.id`
    // (which is its first segment's id — not sequential after задача 014/015).
    println!(
        "  #{num}  {} \u{2192} {}   {} steps, {:.2} km{dist_hint}, {}{time_hint}{hr}{marker}",
        format_local_time(&workout.started_at),
        format_local_time(&workout.ended_at),
        workout.steps,
        workout.distance_m as f64 / 1000.0,
        fmt_duration(workout.walking_time_s),
        dist_hint = dist_hint,
        time_hint = time_hint,
    );
}

/// `♥ avg/max` suffix for a heart-rate summary (задача 025), agreed with the
/// operator as the default: trimmed-mean average, p95 as a spike-robust peak.
/// A leading three spaces separates it from the preceding field like the other
/// space-joined segments on these lines.
fn fmt_hr_summary(hr: store::HrSummary) -> String {
    format!("   \u{2665} {}/{}", hr.avg_bpm, hr.max_bpm)
}

/// Heart-rate summary for a whole calendar day (local time), or `None` when
/// the date can't be parsed or too few `hr_samples` fall in the window —
/// omitted from the day header rather than shown as a misleading zero.
fn day_hr_summary(store: &store::Store, date: &str) -> Option<store::HrSummary> {
    let (start, end) = day_bounds_rfc3339(date)?;
    store.hr_summary_for(&start, &end).ok().flatten()
}

/// `[local midnight, next local midnight)` for a `YYYY-MM-DD` date, as RFC3339
/// UTC bounds for `hr_summary_for`. `None` on an unparseable date or a
/// (practically impossible) nonexistent local midnight.
fn day_bounds_rfc3339(date: &str) -> Option<(String, String)> {
    let naive = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let midnight = naive.and_hms_opt(0, 0, 0)?;
    let start_local = match Local.from_local_datetime(&midnight) {
        chrono::LocalResult::Single(dt) => dt,
        chrono::LocalResult::Ambiguous(dt, _) => dt,
        chrono::LocalResult::None => return None,
    };
    let end_local = start_local + chrono::Duration::days(1);
    Some((
        start_local.with_timezone(&Utc).to_rfc3339(),
        end_local.with_timezone(&Utc).to_rfc3339(),
    ))
}

/// Raw (pre-filter) distance (meters) and time (seconds) for a workout, or
/// `None` for either when the reconstruction can't be trusted — no samples in
/// the window, or a figure that came back *below* the filtered total (a sign
/// of missing samples, since raw must be a superset of walking). The caller
/// then omits that hint rather than showing a misleading value.
fn workout_raw(store: &store::Store, workout: &store::Workout) -> (Option<i64>, Option<i64>) {
    let dist = store
        .raw_distance_m(&workout.started_at, &workout.ended_at)
        .ok()
        .flatten()
        .filter(|&d| d >= workout.distance_m);
    let time =
        raw_span_s(&workout.started_at, &workout.ended_at).filter(|&t| t >= workout.walking_time_s);
    (dist, time)
}

/// Wall-clock span of a workout in seconds — its raw time, before presence
/// filtering carves out the belt-spinning-but-not-walking gaps. `None` on an
/// unparseable or negative span.
fn raw_span_s(started_at: &str, ended_at: &str) -> Option<i64> {
    let start = DateTime::parse_from_rfc3339(started_at).ok()?;
    let end = DateTime::parse_from_rfc3339(ended_at).ok()?;
    let secs = (end - start).num_seconds();
    (secs >= 0).then_some(secs)
}

fn fmt_duration(seconds: i64) -> String {
    format!("{}m{:02}s", seconds / 60, seconds % 60)
}

/// A dim `" (raw <value>)"` hint when `show` is true, else empty. Dimming uses
/// the ANSI faint code, but only on a TTY — piping `tm stats` into a file or
/// `grep` gets clean text with no escape sequences.
fn raw_hint(show: bool, value: &str) -> String {
    if !show {
        return String::new();
    }
    let hint = format!(" (raw {value})");
    if std::io::stdout().is_terminal() {
        format!("\x1b[2m{hint}\x1b[0m")
    } else {
        hint
    }
}

/// Duplicated from `daemon::WATCHDOG_STALE_THRESHOLD` (private to
/// `daemon.rs`) — same rationale as `daemon.rs`'s own hand-kept duplicate of
/// `scan::SCAN_TIMEOUT`: no clean cross-module export yet, keep in sync by
/// hand. Used only to flag `daemon_status.updated_at` as possibly stale here.
const WATCHDOG_STALE_THRESHOLD_S: i64 = 15 /* scan */ + 20 /* connect */ + 60 /* margin */;

/// Print daemon/treadmill/power state and today's workouts, reading only
/// SQLite (`daemon_status` + `activity_segments`) and `launchctl` — never
/// touches the BLE adapter, so it cannot contend with a running daemon for it.
fn run_status() -> Result<()> {
    let store = store::Store::open()?;
    let status = store.daemon_status()?;
    let daemon_alive = daemon_process_alive();

    println!(
        "daemon process: {}",
        if daemon_alive { "alive" } else { "NOT running" }
    );

    match &status {
        None => {
            println!("daemon status: never recorded (fresh install, or the daemon has never run)")
        }
        Some(status) => {
            if status.connected {
                let presence = status.presence_state.as_deref().unwrap_or("Unknown");
                let since = status
                    .last_connected_at
                    .as_deref()
                    .map(describe_timestamp)
                    .unwrap_or_else(|| "unknown".to_string());
                println!("treadmill: connected, presence = {presence} (since {since})");
            } else {
                let ago = status
                    .last_disconnected_at
                    .as_deref()
                    .map(describe_timestamp)
                    .unwrap_or_else(|| "never connected".to_string());
                println!("treadmill: not connected (last seen {ago})");
            }

            // Heart-rate line (задача 025) — mirrors the freshness gate the
            // widget uses ([`widget_hr_field`]) so `status` never shows a
            // frozen bpm from a sensor that's actually been removed. Battery
            // (задача 026) is appended when known — it's read independently
            // of bpm freshness, so it can be present even right after connect.
            if status.hr_connected && !widget_hr_field(status).is_empty() {
                let battery = status
                    .hr_battery_pct
                    .map(|pct| format!(", battery {pct}%"))
                    .unwrap_or_default();
                println!(
                    "heart rate: sensor connected, {} bpm{battery}",
                    status.last_bpm.unwrap_or(0)
                );
            } else {
                println!("heart rate: no sensor");
            }

            // Zone Hold line (задача 027) — only printed once the mode is
            // configured (age set), same "only show what's actually loaded"
            // stance as the config line below.
            let zh_config = zone_hold::load_zone_hold_config();
            if zh_config.enabled {
                if status.zone_hold_active {
                    let phase = status.zone_hold_phase.as_deref().unwrap_or("?");
                    let range = match (status.zone_hold_target_lo, status.zone_hold_target_hi) {
                        (Some(lo), Some(hi)) => format!("{lo}-{hi} bpm"),
                        _ => "? bpm".to_string(),
                    };
                    println!("zone hold: active, phase {phase}, target {range}");
                } else {
                    println!("zone hold: on (not currently engaged)");
                }
            } else {
                println!("zone hold: off");
            }

            let mode_desc = match status.power_mode.as_str() {
                "ac_scanning" => "on AC power, actively scanning",
                "battery_idle" => "on battery, idling (scanning paused to save power)",
                other => other,
            };
            println!(
                "power mode: {mode_desc}, since {}",
                describe_timestamp(&status.power_mode_since)
            );
            if status.power_mode == "battery_idle" {
                println!(
                    "  exits battery-idle immediately on: AC power restored, or system wake \
                     (event-driven power hooks, no polling delay — see docs/tasks/006, задача A)"
                );
            }

            // Config the daemon currently holds in memory (задача 022): answers
            // "what's loaded right now" and "when did it last read the file".
            // Only printed once a 022-aware daemon has written the snapshot
            // (older rows leave these columns NULL).
            if let Some(loaded_at) = &status.config_loaded_at {
                let goals_desc = status
                    .config_goals
                    .as_deref()
                    .map(format_goal_list)
                    .unwrap_or_else(|| "—".to_string());
                let auto_pause = match status.config_auto_pause_secs {
                    Some(secs) => format_secs_short(secs),
                    None => "off".to_string(),
                };
                println!(
                    "config (in daemon): goals {goals_desc} · auto-pause {auto_pause} · read {}",
                    describe_timestamp(loaded_at)
                );
                // The workout-gap is read-time (задача 014) — the CLI resolves it
                // itself, the daemon does not hold it; shown here for completeness.
                println!(
                    "  workout gap: {}m (read-time, applied when stats are read)",
                    goals::load_workout_gap_minutes()
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
                Err(err) => {
                    tracing::warn!(%err, updated_at = %status.updated_at, "status: unparseable daemon_status.updated_at")
                }
            }
        }
    }

    println!();
    println!("today's workouts:");
    let today = Local::now().format("%Y-%m-%d").to_string();
    let workouts = store.workouts_for(&today, goals::load_workout_gap_minutes())?;
    if workouts.is_empty() {
        println!("  (none yet today)");
    } else {
        let last_id = workouts.last().map(|w| w.id);
        let in_progress = status
            .as_ref()
            .is_some_and(|s| s.connected && s.presence_state.as_deref() == Some("Walking"));
        for (i, workout) in workouts.iter().enumerate() {
            let marker = if in_progress && Some(workout.id) == last_id {
                " [in progress]"
            } else {
                ""
            };
            print_workout_line(&store, i + 1, workout, marker);
        }
    }

    Ok(())
}

/// Emit one TSV line for the status-bar widget, or nothing at all when the
/// treadmill is not on/connected (so the widget hides). Read-only, no BLE —
/// mirrors `run_status`'s constraint. See docs/tasks/009.
///
/// The line is tab-separated with 11 fields (задача 027 extension):
/// `state \t workout_count \t cur_walking_s \t cur_steps \t cur_distance_m \t
/// day_walking_s \t day_steps \t day_distance_m \t hr_bpm \t hr_battery_pct \t
/// hr_zone`.
/// - `state` — `walking | away | paused | unknown`.
/// - `workout_count` — number of TODAY's *merged* workouts (reflects the
///   configured `workout_gap_minutes`), so the widget can pick a single- vs
///   multi-workout layout.
/// - `cur_*` — the current (latest) workout's aggregates (sum of its segments).
/// - `day_*` — today's `daily_stats` totals (credited walking only, so already
///   free of step-away/pauses). `cur_* ≤ day_*` by construction.
/// - `hr_bpm` — live bpm from `daemon_status`, or **empty** when no sensor is
///   worn or its reading has gone stale (same freshness gate as the rest of
///   this snapshot). The field is always present (stable field count); an
///   empty value is the signal to hide the heart glyph.
/// - `hr_battery_pct` — the sensor's last-read battery level (задача 026), or
///   **empty** when not (yet) read or no sensor connected. Always the raw
///   percentage — presentation (e.g. only showing a low-battery glyph below a
///   threshold) is the consumer's job, same split as everything else here.
/// - `hr_zone` — `below | in | above` (задача 027), or **empty** unless Zone
///   Hold is actually engaged (`zone_hold_active` + `Hold` phase) in the
///   current `walking` state — see docs/tasks/027 §Индикация зоны. Empty is
///   the signal for the consumer to colour the heart glyph neutrally.
fn run_widget() -> Result<()> {
    let store = store::Store::open()?;

    // Visibility gate: a `daemon_status` row that is `connected` and whose
    // heartbeat (`updated_at`) is fresh. The daemon touches `updated_at` every
    // idle tick (≤30s) and every telemetry sample (~1s), so a stale row means
    // the daemon is gone or hung — hide rather than show frozen data. This is
    // why no `launchctl`/pid probe is needed on the hot 2s poll path.
    let status = match store.daemon_status()? {
        Some(status) if status.connected && !widget_status_stale(&status) => status,
        _ => return Ok(()),
    };

    let state = widget_state(status.presence_state.as_deref());
    let gap_minutes = goals::load_workout_gap_minutes();

    // Current (latest) workout: `walking_time_s` is the *credited* walking time —
    // the presence filter has already excluded step-away and paused stretches
    // (the `36m27s`, not the `raw 41m42s`, that `stats` prints). It auto-freezes
    // when not walking, since nothing is credited then.
    //
    // Freshness gate: only treat the newest workout as the *current* one while a
    // step now would still merge into it — i.e. its last activity ended ≤
    // `gap_minutes` ago (`workout_is_live`). Otherwise `latest` is a *finished*
    // workout from before the gap; showing it as "current" is how reconnecting
    // after a long pause surfaced a stale 9-step workout as if in progress. When
    // filtered out, `cur_* = 0` (no live workout) and the day context falls back
    // to today.
    let latest = store
        .latest_workout(gap_minutes)?
        .filter(|w| workout_is_live(w, gap_minutes, Utc::now()));
    let (cur_walking_s, cur_steps, cur_distance_m) = match &latest {
        Some(workout) => (workout.walking_time_s, workout.steps, workout.distance_m),
        None => (0, 0, 0),
    };

    // The widget's "day" context follows the CURRENT workout's START date, not
    // the wall-clock calendar day — so a workout that crosses midnight keeps its
    // start-day context (count + totals) instead of the widget resetting to zero
    // at 00:00 mid-walk. Falls back to today when there is no workout yet. Day
    // totals are the sum of that day's workouts (by start-date), so the crossing
    // workout is counted whole; on a normal (non-midnight) day this equals the
    // calendar `daily_stats`. `cur_* ≤ day_*` still holds (the current workout is
    // one of the reference day's workouts). `tm stats` daily lines stay strictly
    // calendar — this start-date view is widget-only, for live-workout continuity.
    let reference_day = latest
        .as_ref()
        .map(|w| w.date.clone())
        .unwrap_or_else(|| Local::now().format("%Y-%m-%d").to_string());
    let workouts = store.workouts_for(&reference_day, gap_minutes)?;
    let workout_count = workouts.len();
    let day_walking_s: i64 = workouts.iter().map(|w| w.walking_time_s).sum();
    let day_steps: i64 = workouts.iter().map(|w| w.steps).sum();
    let day_distance_m: i64 = workouts.iter().map(|w| w.distance_m).sum();

    let hr_bpm = widget_hr_field(&status);
    let hr_battery_pct = status
        .hr_battery_pct
        .map(|pct| pct.to_string())
        .unwrap_or_default();
    let hr_zone = widget_hr_zone_field(&status, state);

    println!(
        "{state}\t{workout_count}\t{cur_walking_s}\t{cur_steps}\t{cur_distance_m}\t{day_walking_s}\t{day_steps}\t{day_distance_m}\t{hr_bpm}\t{hr_battery_pct}\t{hr_zone}",
    );
    Ok(())
}

/// The widget's 9th field: live bpm as a plain string, or empty when the
/// sensor isn't worn or its last reading is stale (задача 025) — same
/// freshness threshold as the rest of the daemon heartbeat
/// ([`WATCHDOG_STALE_THRESHOLD_S`]), so a hung daemon can't leave a frozen bpm
/// showing forever.
fn widget_hr_field(status: &store::DaemonStatus) -> String {
    if !status.hr_connected {
        return String::new();
    }
    match (status.last_bpm, status.last_bpm_ts) {
        (Some(bpm), Some(ts_ms)) => {
            let age_s = (Utc::now().timestamp_millis() - ts_ms) / 1000;
            if age_s <= WATCHDOG_STALE_THRESHOLD_S {
                bpm.to_string()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// The widget's 11th field: `below | in | above`, or empty (задача 027). Per
/// the task doc's operator decision (§Индикация зоны), the heart glyph is only
/// coloured by zone while Zone Hold is *actually* driving corrections in the
/// current `walking` state — everywhere else (paused/away/unknown, disabled,
/// or a `ramp`/`frozen`/`grace` phase that isn't classifying live bpm yet)
/// this stays empty and the consumer keeps the neutral colour.
fn widget_hr_zone_field(status: &store::DaemonStatus, widget_state: &str) -> String {
    if widget_state != "walking" || !status.zone_hold_active {
        return String::new();
    }
    status.zone_hold_position.clone().unwrap_or_default()
}

/// Is the newest merged workout still the *current* (live) one at `now`? True
/// only while a step now would merge into it — its last activity ended no more
/// than `gap_minutes` ago. Mirrors `merge_segments`' inclusive gap boundary, so
/// the widget stops showing a finished workout as "current" exactly when a fresh
/// step would open a new one (e.g. after reconnecting past a long pause). An
/// unparseable `ended_at` is an anomaly (we always write RFC3339) → treat as not
/// live, so a corrupt row never masquerades as an in-progress workout. `now` is
/// injected so the boundary is unit-testable.
fn workout_is_live(workout: &store::Workout, gap_minutes: i64, now: DateTime<Utc>) -> bool {
    match DateTime::parse_from_rfc3339(&workout.ended_at) {
        Ok(ended_at) => {
            now - ended_at.with_timezone(&Utc) <= chrono::Duration::minutes(gap_minutes)
        }
        Err(err) => {
            tracing::warn!(%err, ended_at = %workout.ended_at, "widget: unparseable workout ended_at, treating as not live");
            false
        }
    }
}

/// Is the daemon heartbeat too old to trust? An unparseable timestamp counts as
/// stale (hide) — a malformed row is not evidence the treadmill is on.
fn widget_status_stale(status: &store::DaemonStatus) -> bool {
    match DateTime::parse_from_rfc3339(&status.updated_at) {
        Ok(updated_at) => {
            (Utc::now() - updated_at.with_timezone(&Utc)).num_seconds() > WATCHDOG_STALE_THRESHOLD_S
        }
        Err(err) => {
            tracing::warn!(%err, updated_at = %status.updated_at, "widget: unparseable updated_at, hiding widget");
            true
        }
    }
}

/// Map the persisted presence label to the widget's compact state token. The
/// shell presentation layer keys its icon/colour off this string, so the set is
/// a stable contract: `walking | away | paused | unknown`.
fn widget_state(presence_state: Option<&str>) -> &'static str {
    match presence_state {
        Some("Walking") => "walking",
        Some("AwayWhileRunning") => "away",
        Some("Paused") => "paused",
        _ => "unknown",
    }
}

/// `now (Xm ago)`-style rendering of an RFC3339 timestamp in local time.
/// Render a comma-joined goal list ("8500,10750,13000") for `status`
/// ("8500 / 10750 / 13000"). Kept as the stored CSV in `daemon_status` so the
/// daemon does not couple to a display format (задача 022).
fn format_goal_list(csv: &str) -> String {
    csv.split(',').collect::<Vec<_>>().join(" / ")
}

/// Compact duration for the config line (задача 022): whole minutes as `5m`,
/// anything else as raw seconds (`90s`). Auto-pause is always whole minutes, so
/// the seconds branch is just a defensive fallback.
fn format_secs_short(secs: i64) -> String {
    if secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn describe_timestamp(rfc3339: &str) -> String {
    match DateTime::parse_from_rfc3339(rfc3339) {
        Ok(dt) => {
            let utc = dt.with_timezone(&Utc);
            format!(
                "{} ({})",
                utc.with_timezone(&Local).format("%Y-%m-%d %H:%M"),
                humanize_ago(Utc::now() - utc)
            )
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
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
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
    match std::process::Command::new("launchctl")
        .args(["print", &target])
        .output()
    {
        Ok(output) if output.status.success() => {
            // `launchctl print` succeeds for a *loaded* service even if it
            // crashed and isn't currently running — only a real `pid = N`
            // line means it's actually alive right now.
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.trim_start().starts_with("pid = "))
        }
        Ok(_) => false, // not loaded at all
        Err(err) => {
            tracing::warn!(%err, "status: failed to spawn `launchctl print`, assuming daemon not running");
            false
        }
    }
}

/// How long the CLI waits for the daemon to run an enqueued command before
/// giving up. Comfortably above the daemon's ≤1s pick-up plus one
/// [`daemon::CONTROL_EXEC_TIMEOUT`]-bounded write, but short enough to fail
/// fast and tell the operator to retry.
const CONTROL_POLL_TIMEOUT: Duration = Duration::from_secs(8);

/// How often the CLI re-reads the command row while waiting.
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Route a control command (start/stop/speed). When the daemon owns the live
/// BLE link, enqueue the command and wait for the daemon to run it — the CLI
/// cannot open its own connection then, because the treadmill serves one
/// central at a time and stops advertising while connected (задача 013). When
/// the daemon is off, fall back to the original direct-BLE path. Only the
/// fallback touches the Bluetooth adapter.
async fn run_control(command: ControlCommand) -> Result<()> {
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
fn daemon_holds_link(store: &store::Store) -> bool {
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

/// Whether the daemon heartbeat (`daemon_status.updated_at`) is recent enough
/// to trust — an unparseable timestamp counts as not fresh (route to fallback).
fn daemon_status_fresh(status: &store::DaemonStatus) -> bool {
    match DateTime::parse_from_rfc3339(&status.updated_at) {
        Ok(updated_at) => {
            (Utc::now() - updated_at.with_timezone(&Utc)).num_seconds()
                <= WATCHDOG_STALE_THRESHOLD_S
        }
        Err(err) => {
            tracing::warn!(%err, updated_at = %status.updated_at, "control: unparseable daemon_status.updated_at — treating daemon as not holding the link");
            false
        }
    }
}

/// Enqueue a command for the daemon and poll its row until it resolves or the
/// wait times out. Prints a clear result; a `failed` outcome or a timeout is a
/// non-zero exit so scripts can react.
async fn enqueue_and_wait(store: &store::Store, command: ControlCommand) -> Result<()> {
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
fn describe_control_success(command: &ControlCommand) -> String {
    match command {
        ControlCommand::Start => "belt started".to_string(),
        ControlCommand::Stop => "belt stopped".to_string(),
        ControlCommand::Speed(kmh) => format!("speed set to {kmh} km/h"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widget_state_maps_every_presence_label() {
        assert_eq!(widget_state(Some("Walking")), "walking");
        assert_eq!(widget_state(Some("AwayWhileRunning")), "away");
        assert_eq!(widget_state(Some("Paused")), "paused");
        assert_eq!(widget_state(Some("Unknown")), "unknown");
        assert_eq!(widget_state(None), "unknown");
        // An unrecognised label degrades to `unknown` rather than leaking through.
        assert_eq!(widget_state(Some("Bogus")), "unknown");
    }

    fn workout_ending_at(ended_at: &str) -> store::Workout {
        store::Workout {
            ended_at: ended_at.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn workout_is_live_tracks_the_merge_gap_boundary() {
        let now = DateTime::parse_from_rfc3339("2026-07-05T18:40:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let gap = 15;

        // Ended just now / within the gap → still the current workout.
        assert!(workout_is_live(
            &workout_ending_at("2026-07-05T18:40:00Z"),
            gap,
            now
        ));
        assert!(workout_is_live(
            &workout_ending_at("2026-07-05T18:30:00Z"),
            gap,
            now
        )); // 10m ago
        // Exactly on the (inclusive) boundary → still live, mirroring merge_segments.
        assert!(workout_is_live(
            &workout_ending_at("2026-07-05T18:25:00Z"),
            gap,
            now
        )); // 15m ago
        // Past the gap → finished; the widget must not show it as current. This is
        // the reconnect-after-long-pause case that surfaced the stale workout.
        assert!(!workout_is_live(
            &workout_ending_at("2026-07-05T18:24:59Z"),
            gap,
            now
        ));
        assert!(!workout_is_live(
            &workout_ending_at("2026-07-05T18:00:00Z"),
            gap,
            now
        )); // 40m ago
        // A corrupt timestamp is never treated as a live workout.
        assert!(!workout_is_live(
            &workout_ending_at("not-a-timestamp"),
            gap,
            now
        ));
    }
}
