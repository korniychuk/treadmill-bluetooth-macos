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

use std::io::IsTerminal;

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
    /// Emit a compact, machine-readable snapshot of the current workout for a
    /// status-bar widget (tmux/Dracula). Prints one TSV line
    /// `state\twalking_s\tsteps\tdistance_m` while the treadmill is connected
    /// and the daemon heartbeat is fresh, or nothing at all otherwise (so the
    /// widget hides). `walking_s` is the presence-filtered walking time (no
    /// step-away/pause). Like `status`, never opens the BLE adapter. See
    /// docs/tasks/009.
    Widget,
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
    if let Commands::Widget = command {
        return run_widget();
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
        Commands::Stats { .. } | Commands::Status | Commands::Widget | Commands::NotifyTest => {
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
    // The day header stays on filtered totals only: raw is shown per workout,
    // where the [started_at, ended_at] window makes reconstruction exact. A
    // day-level raw would have to sum workout spans, but `daily_stats` can
    // credit activity that never became a `workouts` row (see the midnight /
    // gap edge cases in `store`), so that sum would silently understate.
    println!(
        "{}: {} steps, {:.2} km, {} walking",
        day.date,
        day.steps,
        day.distance_m as f64 / 1000.0,
        fmt_duration(day.walking_time_s),
    );
    for workout in store.workouts_for(&day.date)? {
        print_workout_line(store, &workout, "");
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
fn print_workout_line(store: &store::Store, workout: &store::Workout, marker: &str) {
    let (raw_dist, raw_time) = workout_raw(store, workout);
    let dist_hint = raw_hint(
        raw_dist.is_some_and(|d| d > workout.distance_m),
        &format!("{:.2}", raw_dist.unwrap_or(0) as f64 / 1000.0),
    );
    let time_hint = raw_hint(
        raw_time.is_some_and(|t| t > workout.walking_time_s),
        &fmt_duration(raw_time.unwrap_or(0)),
    );
    println!(
        "  #{}  {} \u{2192} {}   {} steps, {:.2} km{dist_hint}, {}{time_hint}{marker}",
        workout.id,
        format_local_time(&workout.started_at),
        format_local_time(&workout.ended_at),
        workout.steps,
        workout.distance_m as f64 / 1000.0,
        fmt_duration(workout.walking_time_s),
        dist_hint = dist_hint,
        time_hint = time_hint,
    );
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
    let time = raw_span_s(&workout.started_at, &workout.ended_at).filter(|&t| t >= workout.walking_time_s);
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
    if std::io::stdout().is_terminal() { format!("\x1b[2m{hint}\x1b[0m") } else { hint }
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
            print_workout_line(&store, workout, marker);
        }
    }

    Ok(())
}

/// Emit one TSV line for the status-bar widget, or nothing at all when the
/// treadmill is not on/connected (so the widget hides). Read-only, no BLE —
/// mirrors `run_status`'s constraint. See docs/tasks/009.
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
    // `walking_time_s` is the *credited* walking time — the presence filter has
    // already excluded step-away and paused stretches (this is the `36m27s`, not
    // the `raw 41m42s`, that `stats` prints). It also auto-freezes when not
    // walking, since nothing is credited then. Exactly the "filtered" time the
    // widget should show.
    let (walking_s, steps, distance_m) = match store.latest_workout()? {
        Some(workout) => (workout.walking_time_s, workout.steps, workout.distance_m),
        None => (0, 0, 0),
    };

    println!("{state}\t{walking_s}\t{steps}\t{distance_m}");
    Ok(())
}

/// Is the daemon heartbeat too old to trust? An unparseable timestamp counts as
/// stale (hide) — a malformed row is not evidence the treadmill is on.
fn widget_status_stale(status: &store::DaemonStatus) -> bool {
    match DateTime::parse_from_rfc3339(&status.updated_at) {
        Ok(updated_at) => (Utc::now() - updated_at.with_timezone(&Utc)).num_seconds() > WATCHDOG_STALE_THRESHOLD_S,
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
}
