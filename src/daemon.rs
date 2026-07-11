//! Presence-aware background daemon: auto-discover, auto-reconnect, log.
//!
//! Runs forever under a macOS LaunchAgent (never a LaunchDaemon — toast
//! notifications and the Bluetooth permission prompt only work in the user's
//! Aqua session). On every belt-running sample it derives whether the
//! operator is actually walking (see `presence`) and folds the result into
//! the persistent daily totals and per-workout splits (see `store`),
//! independent of the operator's own speed/pause control via the Bluetooth
//! remote.
//!
//! Caveat: while this daemon holds the BLE connection, the official Yesoul
//! phone app cannot also connect (a peripheral serves one central at a time).
//! The physical remote is a separate RF link, so operator control is
//! unaffected.
//!
//! Disconnect detection does *not* rely on CoreBluetooth's own disconnect
//! event or the notification stream ending: a hard power-off (unplugging the
//! belt) was observed live to leave both silent indefinitely — the BLE
//! supervision timeout that would eventually fire a real disconnect event can
//! take arbitrarily long, and btleplug/CoreBluetooth gave no other signal.
//! Instead, `NOTIFICATION_TIMEOUT` bounds how long we wait for the next
//! `0x2ACD` sample (which the device sends ~1/s whenever connected, even at
//! rest) before treating the link as lost ourselves.
//!
//! ## Power/sleep hooks (incident 2026-07-05, see `docs/tasks/006-...md`)
//!
//! Idle scanning (treadmill not currently found) is skipped while the laptop
//! is on battery, but this is now driven by [`crate::power::spawn_power_event_listener`]
//! (native IOKit notifications) instead of a `pmset` poll: a live incident
//! showed the old poll loop silently stuck in the "on battery" branch for
//! 10+ hours while the machine was actually on AC power the whole time, with
//! no external signal that anything was wrong. The event-driven version
//! reacts within one run-loop tick and every transition is logged at `info!`
//! (never `debug!`) for exactly that reason. An already-open connection is
//! never interrupted by a power-state change, only idle *discovery* is
//! gated — see `run()`.
//!
//! A watchdog (`Watchdog`, задачи D/007) independently guards against a
//! *silent hang* with no power-state cause at all (e.g. stuck deep inside
//! btleplug/CoreBluetooth): every meaningful transition — and, as a
//! backstop, every idle tick and every telemetry sample — refreshes the
//! persisted `daemon_status.updated_at` row and touches the watchdog. The
//! watchdog runs on its *own* spawned tokio task (the 2026-07-05 incident
//! showed a `select!`-arm watchdog is blocked by the very hang it guards
//! against, because the hang lives inside another arm's handler body), and
//! when staleness exceeds [`WATCHDOG_STALE_THRESHOLD`] it logs an `ERROR`
//! and exits the process: the hang sits inside CoreBluetooth and cannot be
//! healed in-process, while `KeepAlive=true` in the LaunchAgent plist makes
//! launchd restart the daemon within seconds. SQLite commits per operation
//! and the JSONL log flushes per line, so an exit loses nothing a hung
//! daemon wasn't already losing. See `docs/tasks/007-...md`.
//!
//! ## BLE scan wedge recovery (backlog 009 / задача 051)
//!
//! btleplug 0.12 can panic on a *background* CoreBluetooth callback thread
//! (`Got descriptors for a characteristic we don't know about`) without
//! killing the process. The surviving `CBCentralManager` then fails every
//! `start_scan` instantly (`ScanStartFailed`). Two layers heal this:
//!
//! 1. **Fail-fast panic hook** (installed only in [`run`]): any thread panic
//!    logs under `panic_fail_fast` and `process::exit`([`PANIC_EXIT_CODE`]) so
//!    launchd KeepAlive restarts a clean process (exit 101, distinct from
//!    watchdog 86 and scan-wedged 87).
//! 2. **Adapter recycle** ([`ScanRecovery`]): consecutive typed
//!    `start_scan` failures recycle the adapter (fresh `Manager` /
//!    `CBCentralManager` via [`scan::first_adapter`]); after
//!    [`SCAN_RECYCLE_MAX`] recycles without a successful scan start, exit
//!    [`SCAN_WEDGED_EXIT_CODE`]. Healthy outcomes ("no FTMS treadmill found")
//!    reset both counters — never recycle/exit on a merely powered-off belt.
//!
//! ## Session state (задача 053)
//!
//! `stream_with_presence` is wiring: each `select!` arm calls methods on
//! session structs, then existing side effects (BLE / SQLite / toast).
//!
//! | Struct | Module | Owns |
//! |---|---|---|
//! | [`AutoPause`](crate::auto_pause::AutoPause) | `auto_pause` | away spell + idle-belt pause latch |
//! | [`TreadmillLink`](crate::treadmill_link::TreadmillLink) | `treadmill_link` | telemetry silence + speed memory |
//! | [`HrSession`](crate::hr_session::HrSession) | `hr_session` | HR link/contact/battery/connect latch |
//! | [`ZoneSession`](crate::zone_session::ZoneSession) | `zone_session` | Zone Hold phase + pure `tick` → `ZoneWrite` |
//!
//! Shell keeps BLE handles, intervals, the control/config channel plumbing,
//! and `ActivityAccumulator`. Scan-health streak lives in [`ScanRecovery`]
//! (`run()` scope), not session state.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use std::pin::Pin;

use anyhow::{Result, anyhow};
use btleplug::api::{Central, CentralState, Peripheral as _, ValueNotification};
use btleplug::platform::{Adapter, Peripheral};
use chrono::{DateTime, Local, Utc};
use futures::{Stream, StreamExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::activity::ActivityAccumulator;
use crate::auto_pause::AutoPause;
use crate::config_apply::{self, LiveConfig};
use crate::control::Controller;
use crate::control_command::{self, ControlCommand};
use crate::default_speed;
use crate::ftms;
use crate::goals::{self, Goal};
use crate::hr;
use crate::hr_session::{HR_NOTIFICATION_TIMEOUT, HrFrameAction, HrReconnect, HrSession};
use crate::logger::WorkoutLogger;
use crate::notify;
use crate::power::{self, PowerEvent};
use crate::presence::PresenceState;
use crate::scan;
use crate::speed::CentiKmh;
use crate::store::{DaemonStatus, Store};
use crate::treadmill_link::{NOTIFICATION_TIMEOUT, TreadmillLink};
use crate::zone_hold;
use crate::zone_session::{self, ZH_BPM_MAX_AGE, ZoneSession, ZoneWrite};

/// Delay before retrying discovery after a scan/connect failure, so a
/// transient Bluetooth hiccup does not spin the CPU in a tight loop.
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// How often the idle loop refreshes `daemon_status.updated_at` (and touches the
/// watchdog) while nothing else is happening, so a quiet-but-healthy stretch
/// never looks stale to `status`. Coarse — this is just a heartbeat, not where
/// the real work happens.
const PERSIST_TICK_INTERVAL: Duration = Duration::from_secs(30);

/// How often the standalone watchdog task polls for staleness. Much finer than
/// the persist heartbeat: detection latency is `threshold + up-to-this`, so a
/// coarse poll would dominate the recovery time (a live re-test fired at
/// `stale_s=69` against a 40s threshold purely because the previous poll landed
/// at ~39s and the next was 30s later — задача 018 follow-up). A poll is just an
/// atomic load + `Instant::elapsed`, so 5s is essentially free.
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How stale the watchdog's last touch may get before we treat it as a
/// silent hang and exit for launchd to restart us (задача 007). Generous
/// margin above the worst-case legitimate gap (a full 15s scan cycle plus
/// two 10s bounded CoreBluetooth calls plus the 5s retry delay) so normal
/// latency never trips it, while a genuine hang (2026-07-04/05 incidents:
/// silent for 10+ hours / 79+ minutes) is caught in ~2 minutes. `Instant`
/// on macOS does not advance during system sleep, so waking from a long
/// sleep cannot false-positive here.
/// Shared with CLI status/widget/routing (задача 043): one source of truth for
/// "daemon heartbeat too old to trust". Was previously hand-duplicated in
/// `main.rs` as 95s and had already drifted from this 120s value.
pub(crate) const WATCHDOG_STALE_THRESHOLD: Duration = Duration::from_secs(120);

/// Tighter staleness threshold that applies only while telemetry is actively
/// streaming (задача 018). A connected treadmill sends a sample ~1/s, so silence
/// this long means the link is dead even though CoreBluetooth never fired a
/// disconnect (the fast power-cycle case: btleplug wedged in a blocking FFI call,
/// so even [`NOTIFICATION_TIMEOUT`] can't fire because the executor thread is
/// stuck). Above `NOTIFICATION_TIMEOUT` (20s) so the normal in-loop reconnect
/// still wins when the executor is *not* blocked; far below the general
/// [`WATCHDOG_STALE_THRESHOLD`], which must stay generous for the scan/connect
/// phase. With the 5s [`WATCHDOG_POLL_INTERVAL`], detection is ~30-35s and total
/// recovery ~34s (was ~133s before задача 018). The 10s margin over
/// `NOTIFICATION_TIMEOUT` keeps the graceful in-loop reconnect first when the
/// executor is not blocked (the loop sets `streaming` false at 20s, so this
/// threshold never trips for a clean disconnect).
const STREAMING_STALE_THRESHOLD: Duration = Duration::from_secs(30);

/// Upper bound on the whole pause-resume speed-restore round-trip (take
/// control + set speed, задача 012). Every BLE await in it must be bounded —
/// the watchdog convention (задача 007) — so a wedged CoreBluetooth call here
/// cannot silently stall the stream. Well under [`WATCHDOG_STALE_THRESHOLD`]
/// so a slow-but-legitimate restore never trips the watchdog.
const SPEED_RESTORE_TIMEOUT: Duration = Duration::from_secs(15);

/// Minimum amount by which the pre-pause speed must exceed the resumed speed
/// to bother restoring — avoids a redundant Control Point write (and a
/// misleading toast) when the machine did not actually slow down on resume.
/// Same deadband magnitude as [`zone_hold::MIN_SPEED_CHANGE`] (5 centi).
const SPEED_RESTORE_EPSILON: CentiKmh = CentiKmh::from_wire(5);

/// Ceiling on the resumed belt speed for applying the computed default at a
/// workout start (задача 016): apply only when the belt is at/below the device's
/// factory crawl (~0.5), i.e. it just (re)started/reset and sits at its useless
/// default. A belt already moving faster means the operator chose that speed (or
/// a daemon restart landed mid-walk) — never override it. Same value as the
/// cruise floor: below it is not real walking (0.8 km/h).
const DEFAULT_SPEED_APPLY_CEILING: CentiKmh = CentiKmh::from_wire(80);

/// Exit code used when the watchdog kills the process on a detected hang —
/// distinct from panics/normal errors so `launchctl print` / log forensics
/// can tell watchdog restarts apart.
const WATCHDOG_EXIT_CODE: i32 = 86;

/// How many *consecutive* per-sample DB persist failures are tolerated before
/// the process exits for a launchd restart. One-off SQLITE_BUSY under system
/// load is a recoverable anomaly (WARN + skip — the cumulative FTMS counters
/// make the next successful `advance_baseline` self-healing), but a persistent
/// failure (disk full, schema corruption) needs a clean-slate restart: merely
/// ending the stream would flap a healthy BLE link forever, and the watchdog
/// never fires because telemetry keeps touching it (backlog 010).
const DB_PERSIST_FAILURE_LIMIT: u32 = 30;

/// Exit code for a persistent DB failure — distinct from watchdog (86) and
/// scan-wedge (87) for log/`launchctl print` forensics.
const DB_PERSIST_EXIT_CODE: i32 = 88;

/// Shared tolerate-and-escalate for DB writes that must not tear down a
/// healthy session/loop (backlog 010 / 011). Resets `failures` on success;
/// on a transient error increments and runs `on_skip`; after
/// [`DB_PERSIST_FAILURE_LIMIT`] consecutive failures logs ERROR and exits
/// for a launchd restart (never returns).
fn tolerate_db_write<T>(
    result: Result<T>,
    failures: &mut u32,
    on_skip: impl FnOnce(&anyhow::Error, u32),
) -> Option<T> {
    match result {
        Ok(value) => {
            *failures = 0;
            Some(value)
        }
        Err(err) if *failures + 1 < DB_PERSIST_FAILURE_LIMIT => {
            *failures += 1;
            on_skip(&err, *failures);
            None
        }
        Err(err) => {
            error!(
                error = %err,
                consecutive = *failures + 1,
                exit_code = DB_PERSIST_EXIT_CODE,
                "DB persist failing persistently — exiting for launchd restart"
            );
            std::process::exit(DB_PERSIST_EXIT_CODE);
        }
    }
}

/// Upsert `daemon_status` without letting a busy/full DB kill the loop.
///
/// On success [`DaemonState::persist`] already calls [`Watchdog::touch`].
/// On a skipped failure we still touch: event-loop progress must not depend
/// on SQLite (Liveness matrix — shell/watchdog row).
fn persist_daemon_status(
    state: &DaemonState,
    store: &Store,
    watchdog: &Watchdog,
    failures: &mut u32,
) {
    let _ = tolerate_db_write(
        state.persist(store, watchdog),
        failures,
        |err, consecutive| {
            watchdog.touch();
            warn!(
                error = %err,
                consecutive,
                "daemon status persist failed — skipping upsert, keeping the loop"
            );
        },
    );
}

/// Exit code for the fail-fast panic hook — matches Rust's own exit code for
/// a panicking main thread, distinct from [`WATCHDOG_EXIT_CODE`] (86) and
/// [`SCAN_WEDGED_EXIT_CODE`] (87) for log/`launchctl print` forensics.
const PANIC_EXIT_CODE: i32 = 101;

/// Consecutive `start_scan` failures before recycling the adapter (~15s at
/// [`RETRY_DELAY`] — fast enough for MTTR, wide enough to skip a one-off blip).
const SCAN_START_RECYCLE_THRESHOLD: u32 = 3;

/// Adapter recycles (without a successful scan start in between) before
/// giving up and exiting for a launchd restart.
const SCAN_RECYCLE_MAX: u32 = 2;

/// Exit code when scanning stays wedged after [`SCAN_RECYCLE_MAX`] recycles.
const SCAN_WEDGED_EXIT_CODE: i32 = 87;

/// Decision from [`ScanRecovery`] after a connect-cycle outcome (задача 051).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanRecoveryAction {
    /// Keep using the current adapter; sleep [`RETRY_DELAY`] and try again.
    Retry,
    /// Drop the wedged `CBCentralManager` and open a fresh one via
    /// [`scan::first_adapter`].
    RecycleAdapter,
    /// Recycles exhausted — `process::exit`([`SCAN_WEDGED_EXIT_CODE`]).
    Exit,
}

/// Pure streak counters for wedged-scan recovery (задача 051 / backlog 009).
///
/// No IO or time: callers inject the classification (`is_scan_start_failure`)
/// and apply side effects (`stop_scan`, `first_adapter`, `exit`) themselves —
/// same shape as [`presence::PresenceTracker`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ScanRecovery {
    scan_start_streak: u32,
    recycles: u32,
}

impl ScanRecovery {
    /// Record a failed `connect_treadmill` cycle.
    ///
    /// Non-scan-start failures (belt off, connect/discover errors) mean the
    /// adapter is alive — both counters reset, always [`ScanRecoveryAction::Retry`].
    fn on_connect_failure(&mut self, is_scan_start_failure: bool) -> ScanRecoveryAction {
        if !is_scan_start_failure {
            self.on_scan_ok();
            return ScanRecoveryAction::Retry;
        }

        self.scan_start_streak += 1;
        if self.scan_start_streak < SCAN_START_RECYCLE_THRESHOLD {
            return ScanRecoveryAction::Retry;
        }

        // Streak hit the recycle threshold.
        if self.recycles >= SCAN_RECYCLE_MAX {
            return ScanRecoveryAction::Exit;
        }

        self.scan_start_streak = 0;
        self.recycles += 1;
        ScanRecoveryAction::RecycleAdapter
    }

    /// Any successful scan start (connect `Ok`, or a non-scan-start failure)
    /// proves the adapter is healthy — reset both counters.
    /// Report whether the adapter's radio is powered on, assuming powered-on
    /// when the state query itself fails (the wedge machinery then proceeds —
    /// a broken state query on a healthy radio must not mask a real wedge).
    async fn is_adapter_powered_on(adapter: &Adapter) -> bool {
        match adapter.adapter_state().await {
            Ok(state) => state == CentralState::PoweredOn,
            Err(err) => {
                warn!(
                    target: "scan_recovery",
                    %err,
                    "adapter_state query failed — assuming powered on"
                );
                true
            }
        }
    }

    fn on_scan_ok(&mut self) {
        self.scan_start_streak = 0;
        self.recycles = 0;
    }
}

/// Extract a human-readable message from a panic payload (`&str` / `String` /
/// anything else). Pure so unit tests cover the branches without constructing
/// a real [`std::panic::PanicHookInfo`].
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

/// Format a panic location for structured logs (`file:line`, or a stable
/// placeholder when the runtime provides none).
fn panic_location_message(location: Option<&std::panic::Location<'_>>) -> String {
    match location {
        Some(loc) => format!("{}:{}", loc.file(), loc.line()),
        None => "<unknown>".to_string(),
    }
}

/// Install a process-wide fail-fast panic hook used only in daemon mode
/// (called from [`run`], never from one-shot CLI paths). Logs under
/// `panic_fail_fast`, prints the default backtrace, then exits so launchd
/// KeepAlive restarts a clean process (backlog 009).
fn install_panic_fail_fast_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = panic_payload_message(info.payload());
        let location = panic_location_message(info.location());
        error!(
            target: "panic_fail_fast",
            payload = %payload,
            location = %location,
            exit_code = PANIC_EXIT_CODE,
            "panic detected — exiting so launchd KeepAlive restarts the daemon (backlog 009)"
        );
        default_hook(info);
        std::process::exit(PANIC_EXIT_CODE);
    }));
}

/// Backstop poll cadence for the control-command queue while connected but
/// quiet (no telemetry-driven check). Commands are also processed at the end
/// of every telemetry sample (~1/s), so this only matters during rare silent
/// stretches; keep it snappy but idle-cheap.
const CONTROL_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How often, during an active session, to check whether the goals config file
/// changed on disk and reload it without a daemon restart (задача 017). Only a
/// cheap `stat` per tick — the file is re-read/parsed only when its mtime moved.
/// 5s is a snappy pickup latency for a config edit while negligible in cost.
const CONFIG_RELOAD_INTERVAL: Duration = Duration::from_secs(5);

/// Who initiated a Control Point write (задача 039). Logged on every write so
/// mid-Hold CLI speed overrides are diagnosable; not a priority arbiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlSource {
    Zone,
    Cli,
    AutoPause,
    Restore,
    DefaultSpeed,
}

impl ControlSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Zone => "zone",
            Self::Cli => "cli",
            Self::AutoPause => "auto_pause",
            Self::Restore => "restore",
            Self::DefaultSpeed => "default_speed",
        }
    }
}

/// How often the daemon retries finding/connecting an HR sensor while one
/// isn't currently linked (no strap worn, or the last link was lost). Coarser
/// than the treadmill's own reconnect: an HR sensor absence is the common case
/// (not everyone wears the strap every walk), so this must not spam scans.
const HR_RECONNECT_INTERVAL: Duration = Duration::from_secs(30);

/// How often to check whether it's time to re-read the HR sensor's battery
/// level (задача 026) — a cheap in-memory elapsed-time check, same pattern as
/// `CONFIG_RELOAD_INTERVAL`'s mtime check. The actual re-read cadence is
/// owned by [`HrSession::battery_read_due`]; this just bounds how promptly a
/// newly-crossed threshold is noticed.
const HR_BATTERY_CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// A live notification stream from an HR peripheral (matches the type
/// `btleplug::api::Peripheral::notifications` returns).
type HrNotificationStream = Pin<Box<dyn Stream<Item = ValueNotification> + Send>>;

/// Result of one background HR connect attempt (задача 025), sent back over a
/// channel so scanning (up to [`scan::SCAN_TIMEOUT`] when no strap is worn —
/// the common case) never blocks the main treadmill telemetry loop.
enum HrConnectOutcome {
    /// The initial battery reading (задача 026), taken right after subscribe
    /// while the spawned task is already there — `None` if the read failed
    /// (logged inside `scan::read_hr_battery`), not a reason to abort the
    /// connection itself.
    Connected(Peripheral, HrNotificationStream, Option<u8>),
    NotFound,
}

/// Scan for, connect to, and subscribe an HR sensor (Polar H10) on a spawned
/// task, reporting the outcome back over `tx`. Best-effort throughout: no
/// strap worn is the normal case, not an error — every failure path here logs
/// and reports [`HrConnectOutcome::NotFound`] rather than propagating, so a
/// missing/lost sensor can never affect the treadmill session.
fn spawn_hr_connect_attempt(adapter: Adapter, tx: UnboundedSender<HrConnectOutcome>) {
    tokio::spawn(async move {
        let outcome = match scan::connect_hr(&adapter).await {
            Ok(peripheral) => {
                if !scan::subscribe_hr(&peripheral).await {
                    scan::disconnect_best_effort(&peripheral).await;
                    HrConnectOutcome::NotFound
                } else {
                    match tokio::time::timeout(scan::CONNECT_TIMEOUT, peripheral.notifications())
                        .await
                    {
                        Ok(Ok(stream)) => {
                            let battery_pct = scan::read_hr_battery(&peripheral).await;
                            HrConnectOutcome::Connected(peripheral, stream, battery_pct)
                        }
                        Ok(Err(err)) => {
                            warn!(%err, "failed to open HR notification stream");
                            scan::disconnect_best_effort(&peripheral).await;
                            HrConnectOutcome::NotFound
                        }
                        Err(_) => {
                            warn!(
                                "opening HR notification stream timed out (possible CoreBluetooth hang)"
                            );
                            scan::disconnect_best_effort(&peripheral).await;
                            HrConnectOutcome::NotFound
                        }
                    }
                }
            }
            Err(err) => {
                info!(%err, "no HR sensor found this attempt — will retry");
                HrConnectOutcome::NotFound
            }
        };
        // The receiver only drops when the session ends; a send failure there
        // is a harmless race with teardown, nothing to recover.
        let _ = tx.send(outcome);
    });
}

/// Run the daemon forever: scan → connect → stream with presence tracking →
/// on disconnect, toast and go back to scanning. Reacts to power/sleep
/// events instead of polling — see module docs.
pub async fn run(adapter: &Adapter) -> Result<()> {
    // Fail-fast on any thread panic (incl. btleplug CoreBluetooth callbacks)
    // so launchd KeepAlive restarts a clean process. One-shot CLI never reaches
    // here — see задача 051 / backlog 009.
    install_panic_fail_fast_hook();

    let mut power_events = power::spawn_power_event_listener();
    let mut store = Store::open()?;
    // Loaded once here (not per session): config edits take effect on the next
    // daemon restart or a live hot-reload when config.json changes on disk
    // (задача 017). Bundled as `LiveConfig` and threaded by `&mut` into
    // `stream_with_presence`, which reloads it and keeps the `tm status`
    // snapshot in sync (задачи 020/022). `auto_pause` is `None` when disabled.
    let mut live_config = LiveConfig {
        goals: goals::load_goals(),
        auto_pause: goals::load_auto_pause(),
        zone_hold: zone_hold::load_zone_hold_config(),
    };
    info!(
        goals = ?live_config.goals, auto_pause = ?live_config.auto_pause,
        zone_hold_enabled = live_config.zone_hold.enabled,
        "loaded config (goals + idle-belt auto-pause + zone hold)"
    );
    let watchdog = Watchdog::new();
    watchdog.spawn_monitor();
    // Refreshes `daemon_status.updated_at` (and the watchdog) while idle, so
    // quiet-but-healthy stretches never look like a hang to `status`.
    let mut persist_tick = tokio::time::interval(PERSIST_TICK_INTERVAL);
    // Consecutive DB persist failures (backlog 010/011) — shared across the
    // idle heartbeat and the connected session. Reset on the first success.
    let mut db_persist_failures: u32 = 0;

    // The listener always sends the current AC/battery state as its first
    // event (see `power::spawn_power_event_listener`), so this seeds `on_ac`
    // without a separate synchronous read.
    let mut on_ac = match power_events.recv().await {
        Some(PowerEvent::AcPowerChanged(on_ac)) => on_ac,
        Some(other) => {
            warn!(?other, "unexpected first power event, assuming AC power");
            true
        }
        None => {
            warn!("power-event channel closed immediately — assuming AC power");
            true
        }
    };

    let mut state = DaemonState::new(on_ac);
    // Seed the loaded-config snapshot so `tm status` shows it even before the
    // first connection/session (задача 022).
    state.set_config(&live_config.goals, live_config.auto_pause);
    state.persist(&store, &watchdog)?;

    // Owned override when we recycle a wedged CBCentralManager (задача 051).
    // `run` only borrows the adapter from main, so replacement is local.
    let mut recycled_adapter: Option<Adapter> = None;
    let mut scan_recovery = ScanRecovery::default();

    loop {
        if !on_ac {
            info!("on battery with no active connection — idling until AC power or wake");
            loop {
                tokio::select! {
                    biased;
                    event = power_events.recv() => {
                        match event {
                            Some(PowerEvent::AcPowerChanged(true)) => {
                                info!("AC power restored — resuming scanning immediately");
                                on_ac = true;
                                state.set_power_mode(true);
                                state.persist(&store, &watchdog)?;
                                break;
                            }
                            Some(PowerEvent::DidWake) => {
                                // Re-check rather than assume: waking doesn't
                                // imply AC, just that it's worth looking again.
                                let now_on_ac = power::is_on_ac_power();
                                info!(on_ac = now_on_ac, "system woke while idle — re-checked power state");
                                on_ac = now_on_ac;
                                state.set_power_mode(now_on_ac);
                                state.persist(&store, &watchdog)?;
                                if now_on_ac {
                                    break;
                                }
                            }
                            Some(PowerEvent::AcPowerChanged(false)) => {
                                // Duplicate/no-op: already idling on battery.
                            }
                            Some(PowerEvent::WillSleep) => {
                                info!("system will sleep while idle (no active session)");
                                state.persist(&store, &watchdog)?;
                            }
                            Some(PowerEvent::WillPowerOff) => {
                                info!("system will power off while idle — no active session to close");
                                state.persist(&store, &watchdog)?;
                            }
                            None => {
                                error!("power-event listener channel closed — cannot react to power state anymore");
                                return Err(anyhow!("power-event listener thread died"));
                            }
                        }
                    }
                    _ = persist_tick.tick() => {
                        persist_daemon_status(&state, &store, &watchdog, &mut db_persist_failures);
                    }
                }
            }
            continue;
        }

        // Clone so the select!/stream body does not borrow `recycled_adapter`
        // (we may drop and replace that Option on a recycle — задача 051).
        // `Adapter: Clone` is a handle; old clones in finished HR tasks drop cleanly.
        let active_adapter = recycled_adapter.as_ref().unwrap_or(adapter).clone();

        tokio::select! {
            biased;
            event = power_events.recv() => {
                match event {
                    Some(PowerEvent::AcPowerChanged(false)) => {
                        info!("switched to battery with no active connection — going idle");
                        on_ac = false;
                        state.set_power_mode(false);
                        state.persist(&store, &watchdog)?;
                    }
                    Some(PowerEvent::AcPowerChanged(true)) => {
                        // Already scanning on AC — nothing to do.
                    }
                    Some(PowerEvent::DidWake) => {
                        info!("system woke — scan loop already active, will retry immediately");
                    }
                    Some(PowerEvent::WillSleep) => {
                        info!("system will sleep (no active session)");
                        state.persist(&store, &watchdog)?;
                    }
                    Some(PowerEvent::WillPowerOff) => {
                        info!("system will power off — no active session to close");
                        state.persist(&store, &watchdog)?;
                    }
                    None => {
                        error!("power-event listener channel closed — cannot react to power state anymore");
                        return Err(anyhow!("power-event listener thread died"));
                    }
                }
            }
            _ = persist_tick.tick() => {
                persist_daemon_status(&state, &store, &watchdog, &mut db_persist_failures);
            }
            result = scan::connect_treadmill(&active_adapter) => {
                match result {
                    Ok(peripheral) => {
                        scan_recovery.on_scan_ok();
                        notify::treadmill_found();
                        state.connected = true;
                        state.last_connected_at = Some(Utc::now().to_rfc3339());
                        state.persist(&store, &watchdog)?;

                        if let Err(err) = stream_with_presence(
                            &active_adapter,
                            &peripheral,
                            &mut power_events,
                            &mut store,
                            &mut state,
                            &watchdog,
                            &mut on_ac,
                            &mut live_config,
                            &mut db_persist_failures,
                        )
                        .await
                        {
                            warn!(%err, "presence stream ended with an error");
                        }
                        // Streaming is over (by any exit path). Lift the tight
                        // watchdog threshold before the potentially-slow
                        // disconnect below (whose own hang stays under the general
                        // 120s threshold, as in задача 007). See задача 018.
                        watchdog.set_streaming(false);
                        // Toast + status update must come *before* the BLE
                        // teardown: on a hard power-off `disconnect()` was
                        // observed to hang for hours (задача 007), and the
                        // operator-visible signals must not depend on it.
                        notify::treadmill_lost();
                        state.connected = false;
                        state.presence_state = None;
                        state.last_disconnected_at = Some(Utc::now().to_rfc3339());
                        // The HR link (if any) was torn down inside
                        // `stream_with_presence` along with the session — задача 025.
                        state.hr_connected = false;
                        state.last_bpm = None;
                        state.last_bpm_ts = None;
                        state.hr_battery_pct = None;
                        // Zone Hold's per-session phase dies with the session
                        // (задача 027) — clear the mirrored snapshot too.
                        state.zone_hold_active = false;
                        state.zone_hold_phase = None;
                        state.zone_hold_position = None;
                        state.zone_hold_target_lo = None;
                        state.zone_hold_target_hi = None;
                        state.last_speed_kmh = None;
                        state.last_speed_ts = None;
                        state.zone_hold_last_speed = None;
                        state.persist(&store, &watchdog)?;

                        scan::disconnect_best_effort(&peripheral).await;
                    }
                    Err(err) => {
                        let is_scan_start =
                            err.downcast_ref::<scan::ScanStartFailed>().is_some();
                        // Bluetooth powered off yields the same start_scan error
                        // as a wedged CBCentralManager, but neither an adapter
                        // recycle nor an exit(87) restart can heal a radio that
                        // is simply off — it must not feed the wedge streak, or
                        // the daemon would loop restarts until the radio is back.
                        let powered_on = is_scan_start
                            && ScanRecovery::is_adapter_powered_on(&active_adapter).await;
                        let is_wedge_candidate = is_scan_start && powered_on;
                        if is_scan_start && !powered_on {
                            warn!(
                                target: "scan_recovery",
                                %err,
                                "start_scan failed with Bluetooth not powered on — waiting for the radio, not recycling"
                            );
                        }
                        match scan_recovery.on_connect_failure(is_wedge_candidate) {
                            ScanRecoveryAction::Retry => {
                                if is_wedge_candidate {
                                    warn!(
                                        target: "scan_recovery",
                                        streak = scan_recovery.scan_start_streak,
                                        recycles = scan_recovery.recycles,
                                        %err,
                                        "start_scan failed — retrying (backlog 009)"
                                    );
                                } else if !is_scan_start {
                                    warn!(%err, "treadmill not found this cycle, retrying");
                                }
                                sleep(RETRY_DELAY).await;
                            }
                            ScanRecoveryAction::RecycleAdapter => {
                                warn!(
                                    target: "scan_recovery",
                                    streak = SCAN_START_RECYCLE_THRESHOLD,
                                    recycle = scan_recovery.recycles,
                                    %err,
                                    "start_scan failure streak — recycling BLE adapter (backlog 009)"
                                );
                                match tokio::time::timeout(
                                    scan::CONNECT_TIMEOUT,
                                    active_adapter.stop_scan(),
                                )
                                .await
                                {
                                    Ok(Ok(())) => {}
                                    Ok(Err(stop_err)) => warn!(
                                        target: "scan_recovery",
                                        %stop_err,
                                        "stop_scan failed during adapter recycle — continuing"
                                    ),
                                    Err(_) => warn!(
                                        target: "scan_recovery",
                                        timeout_s = scan::CONNECT_TIMEOUT.as_secs(),
                                        "stop_scan timed out during adapter recycle — continuing"
                                    ),
                                }
                                // Drop any previous owned adapter before opening a
                                // fresh Manager / CBCentralManager (this cycle's
                                // clone drops at end of match).
                                drop(recycled_adapter.take());
                                match scan::first_adapter().await {
                                    Ok(new_adapter) => {
                                        recycled_adapter = Some(new_adapter);
                                    }
                                    Err(adapter_err) => {
                                        error!(
                                            target: "scan_recovery",
                                            %adapter_err,
                                            exit_code = SCAN_WEDGED_EXIT_CODE,
                                            "failed to acquire BLE adapter after recycle — exiting for launchd restart (backlog 009)"
                                        );
                                        std::process::exit(SCAN_WEDGED_EXIT_CODE);
                                    }
                                }
                            }
                            ScanRecoveryAction::Exit => {
                                error!(
                                    target: "scan_recovery",
                                    streak = scan_recovery.scan_start_streak,
                                    recycles = scan_recovery.recycles,
                                    exit_code = SCAN_WEDGED_EXIT_CODE,
                                    %err,
                                    "start_scan still failing after adapter recycles — exiting for launchd restart (backlog 009)"
                                );
                                std::process::exit(SCAN_WEDGED_EXIT_CODE);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Stream telemetry from an already-connected peripheral, folding presence
/// into workout/daily totals, until the link is judged lost. Also reacts to
/// power events while connected: an active session is never itself
/// interrupted by an AC/battery change (only idle *discovery* is gated —
/// see `run()`), `WillSleep` is just logged/persisted (BLE will drop on its
/// own if the connection doesn't survive sleep), and `WillPowerOff`
/// best-effort closes the session before the process may be killed.
// `adapter` (added for задача 025's background HR reconnect) pushes this past
// clippy's default 7-argument threshold; splitting these into a struct would
// just move the same state around without reducing it.
#[allow(clippy::too_many_arguments)]
async fn stream_with_presence(
    adapter: &Adapter,
    peripheral: &Peripheral,
    power_events: &mut UnboundedReceiver<PowerEvent>,
    store: &mut Store,
    state: &mut DaemonState,
    watchdog: &Watchdog,
    on_ac: &mut bool,
    config: &mut LiveConfig,
    db_persist_failures: &mut u32,
) -> Result<()> {
    scan::subscribe_treadmill_data(peripheral).await?;
    scan::subscribe_treadmill_status(peripheral).await?;
    // Bounded like every other CoreBluetooth call — see задача 007.
    let mut notifications = tokio::time::timeout(scan::CONNECT_TIMEOUT, peripheral.notifications())
        .await
        .map_err(|_| {
            anyhow!("opening notification stream timed out (possible CoreBluetooth hang)")
        })??;

    // From here on telemetry should arrive ~1/s. Switch the watchdog to its
    // tight streaming threshold (задача 018) and reset the clock so the
    // (possibly slow) subscribe phase above doesn't count against it. `run()`
    // clears streaming the moment this function returns, by any path.
    // `touch_telemetry` (not `touch`): the streaming phase watches the treadmill
    // clock, which must start now rather than at the anchor (задача 031).
    watchdog.touch_telemetry();
    watchdog.set_streaming(true);

    let session_id = store.start_session()?;
    let mut logger = WorkoutLogger::create()?;
    // Presence + pending-credit + open-segment state, all fresh per session (a
    // daemon restart mid-walk just opens a new segment; read-time
    // `merge_segments` re-joins it to the pre-restart one when the gap is under
    // threshold). This is the *same* engine the offline replay runs — see
    // `crate::activity` and `docs/tasks/015`.
    let mut accumulator = ActivityAccumulator::new();
    // Idle-belt auto-pause (задача 020): threshold in `config.auto_pause`
    // (hot-reloaded); spell state in `AutoPause` (задача 053).
    let mut auto_pause = AutoPause::new();
    // Telemetry silence + speed memory for pause/resume/default (задача 053).
    // Seeded now so the (possibly slow) subscribe above does not count against
    // the silence arm; pairs with `watchdog.touch_telemetry()` above.
    let mut link = TreadmillLink::new(tokio::time::Instant::now());
    // Zone Hold session (задача 027 / 053): phase + timers + override window.
    let mut zone = ZoneSession::new();
    // Backstop poll for queued control commands during quiet stretches; the
    // primary check runs at the end of each telemetry sample below (задача 013).
    let mut command_tick = tokio::time::interval(CONTROL_POLL_INTERVAL);
    // Hot-reload of config.json (задача 017): `None` forces the first tick to
    // reconcile against disk, so a config edited while the daemon was idle is
    // picked up at session start too.
    let mut config_tick = tokio::time::interval(CONFIG_RELOAD_INTERVAL);
    let mut goals_mtime: Option<std::time::SystemTime> = None;

    // Heart-rate sensor (задача 025), best-effort throughout: the daemon is the
    // sole owner of both BLE links (treadmill + HR), but a missing/lost strap
    // must never affect the treadmill session. Connect attempts run on a
    // spawned task (see `spawn_hr_connect_attempt`) so scanning up to
    // `SCAN_TIMEOUT` — the normal outcome when no strap is worn — never blocks
    // this loop's telemetry handling.
    let (hr_tx, mut hr_rx) = tokio::sync::mpsc::unbounded_channel::<HrConnectOutcome>();
    let mut hr_peripheral: Option<Peripheral> = None;
    // BLE stream handle — must stay in sync with `hr.link_up()` (задача 053).
    // Flips only alongside `HrSession::on_connected` / `on_link_lost`.
    let mut hr_notifications: Option<HrNotificationStream> = None;
    let mut hr = HrSession::new_connecting(Instant::now(), tokio::time::Instant::now());
    spawn_hr_connect_attempt(adapter.clone(), hr_tx.clone());
    let mut hr_reconnect_tick = tokio::time::interval(HR_RECONNECT_INTERVAL);
    let mut hr_battery_check_tick = tokio::time::interval(HR_BATTERY_CHECK_INTERVAL);
    // `db_persist_failures` is owned by `run()` and shared with the idle
    // heartbeat (backlog 011) — DB health outlives any single BLE session.

    loop {
        tokio::select! {
            biased;
            event = power_events.recv() => {
                match event {
                    Some(PowerEvent::AcPowerChanged(new_on_ac)) => {
                        *on_ac = new_on_ac;
                        state.set_power_mode(new_on_ac);
                        persist_daemon_status(state, store, watchdog, db_persist_failures);
                    }
                    Some(PowerEvent::WillSleep) => {
                        info!("system will sleep — active session left connected, BLE will drop on its own if it doesn't survive");
                        persist_daemon_status(state, store, watchdog, db_persist_failures);
                    }
                    Some(PowerEvent::DidWake) => {
                        info!("system woke while connected — active session unaffected");
                        persist_daemon_status(state, store, watchdog, db_persist_failures);
                    }
                    Some(PowerEvent::WillPowerOff) => {
                        warn!("system will power off — closing active session best-effort before the process may be killed");
                        logger.finish();
                        store.end_session()?;
                        if let Some(p) = hr_peripheral.take() {
                            scan::disconnect_best_effort(&p).await;
                        }
                        // Return directly (rather than `break`) so the normal
                        // "stream ended" path below — which logs an `error!`
                        // for what would otherwise look like an unexpected
                        // disconnect — never runs for this controlled exit.
                        return Ok(());
                    }
                    None => {
                        error!("power-event channel closed while a session is active — continuing without power-state visibility");
                    }
                }
                continue;
            }
            // Absolute deadline, not `timeout(NOTIFICATION_TIMEOUT, ...)`: `select!`
            // rebuilds every arm's future on each pass, so a relative timeout is
            // reset by whichever sibling arm completes first — and `command_tick`
            // fires every second. `sleep_until` survives the rebuild because the
            // deadline is a point in time, not a duration (задача 031).
            _ = tokio::time::sleep_until(link.silence_deadline()) => {
                warn!(timeout_s = NOTIFICATION_TIMEOUT.as_secs(), "no telemetry received — treating as disconnected");
                break;
            }
            notification = notifications.next() => {
                let Some(notification) = notification else {
                    break; // stream closed cleanly (rare, but handle it)
                };
                if notification.uuid == ftms::FITNESS_MACHINE_STATUS {
                    let ts_ms = Utc::now().timestamp_millis();
                    if let Some(&event_code) = notification.value.first() {
                        info!(event = ftms::describe_status_event(event_code), code = event_code, "machine status event");
                        // Same rationale as the sample persist below: a busy DB
                        // must not kill the stream over an informational event.
                        if let Err(err) = store.insert_status_event(session_id, ts_ms, event_code, &notification.value) {
                            warn!(error = %err, "status event persist failed — skipping event");
                        }
                    } else {
                        warn!("empty Fitness Machine Status frame");
                    }
                    persist_daemon_status(state, store, watchdog, db_persist_failures);
                    continue;
                }
                if notification.uuid != ftms::TREADMILL_DATA {
                    continue;
                }
                let Some(data) = ftms::parse_treadmill_data(&notification.value) else {
                    warn!(bytes = ?notification.value, "undecodable treadmill frame");
                    continue;
                };
                let now = Instant::now();
                let tokio_now = tokio::time::Instant::now();
                link.on_frame_decoded(tokio_now);
                watchdog.touch_telemetry();
                logger.log(&data)?;
                // A failed per-sample persist must not tear down a healthy BLE
                // link: skip the sample (the cumulative FTMS counters make the
                // next successful `advance_baseline` recompute the full delta),
                // escalate only when the failure is persistent (backlog 010).
                let persisted = store
                    .insert_raw_sample(session_id, Utc::now().timestamp_millis(), &data, &notification.value)
                    .and_then(|()| store.advance_baseline(data.steps, data.total_distance_m, data.elapsed_s));
                let Some(deltas) = tolerate_db_write(persisted, db_persist_failures, |err, consecutive| {
                    warn!(
                        error = %err,
                        consecutive,
                        "sample persist failed — skipping sample, keeping the stream"
                    );
                }) else {
                    continue;
                };

                // Speed memory feeds resume-restore and Zone Hold ramp seeding —
                // only persisted samples count (pre-refactor behavior): a
                // restored belt speed must not depend on samples skipped above.
                // Cruising stats stay f32 (estimate domain); convert at the edge.
                link.record_speed(data.speed.map(|s| s.to_kmh_f32()), now);
                // Live speed snapshot for `tm widget` (задача 029) — every sample
                // with speed, unconditionally (unlike `last_walking_speed` on the
                // link, which only tracks non-zero cruising speed).
                if let Some(speed) = data.speed {
                    state.last_speed_kmh = Some(f64::from(speed.to_kmh_f32()));
                    state.last_speed_ts = Some(Utc::now().timestamp_millis());
                }

                let prev_state = accumulator.state();
                if let Some(next_state) = accumulator.observe(Instant::now(), data.speed, data.steps) {
                    info!(?prev_state, ?next_state, "presence transition");
                    state.presence_state = Some(next_state.wire().to_string());
                    // Belt speed as Zone Hold should see it below: starts as this
                    // sample's raw telemetry (`None` when MORE_DATA omits speed —
                    // never fabricate 0.0, задача 036), but a restore/default-speed
                    // write in this very match (below) lands *after* that sample
                    // was taken — update it whenever one of those writes actually
                    // fires, so a fresh Ramp doesn't start from the pre-write crawl.
                    let mut zh_effective = data.speed;
                    match next_state {
                        PresenceState::AwayWhileRunning => {
                            // Arm a fresh auto-pause spell (задача 020 / 053).
                            auto_pause.on_away(Instant::now());
                            notify::walker_away();
                        }
                        PresenceState::Walking if prev_state == PresenceState::AwayWhileRunning => {
                            notify::walker_resumed(auto_pause.on_return(Instant::now()));
                        }
                        PresenceState::Walking if prev_state == PresenceState::Paused => {
                            let resume = link.on_resume(Instant::now());
                            // Speed-dependent restore/default only when measured.
                            if let Some(resumed_speed) = data.speed {
                                match resume.pre_pause_speed {
                                    // A real captured walking speed → restore it (задача 012).
                                    Some(pre_f32) => {
                                        let pre = CentiKmh::from_kmh_f32(pre_f32);
                                        let restore =
                                            try_restore_speed(peripheral, pre, resumed_speed).await;
                                        if let Some(r) = &restore {
                                            zh_effective = CentiKmh::from_kmh_f32(r.to_kmh);
                                        }
                                        notify::treadmill_resumed(resume.paused_for, restore);
                                    }
                                    // Nothing to restore → this is a fresh start/reset at the
                                    // device crawl (scenarios 2 & 3, задача 016): apply the
                                    // computed default. Only toasts when it actually applied.
                                    None => match try_apply_default_speed(
                                        peripheral,
                                        store,
                                        resumed_speed,
                                        &mut link,
                                    )
                                    .await
                                    {
                                        Some(applied) => {
                                            zh_effective = Some(applied);
                                            notify::default_speed_applied(
                                                resumed_speed.to_kmh_f32(),
                                                applied.to_kmh_f32(),
                                            );
                                        }
                                        None => notify::treadmill_resumed(resume.paused_for, None),
                                    },
                                }
                            } else {
                                // pre_pause already taken by on_resume — no second take.
                                notify::treadmill_resumed(resume.paused_for, None);
                            }
                        }
                        // Connected with the belt already moving (scenario 1, задача 016).
                        // Apply the computed default only if the belt is at its device
                        // crawl (guarded inside `try_apply_default_speed`).
                        PresenceState::Walking if prev_state == PresenceState::Unknown => {
                            if let Some(resumed_speed) = data.speed
                                && let Some(applied) = try_apply_default_speed(
                                    peripheral,
                                    store,
                                    resumed_speed,
                                    &mut link,
                                )
                                .await
                            {
                                zh_effective = Some(applied);
                                notify::default_speed_applied(
                                    resumed_speed.to_kmh_f32(),
                                    applied.to_kmh_f32(),
                                );
                            }
                        }
                        // Skip the very first sample after connecting: PresenceState
                        // starts Unknown, so a treadmill discovered already stopped
                        // must not immediately toast "paused".
                        PresenceState::Paused if prev_state != PresenceState::Unknown => {
                            link.on_pause(Instant::now());
                            // Suppress the generic "Paused" toast when this pause
                            // is our own auto-pause: the belt going to 0 after our
                            // Stop transitions AwayWhileRunning→Paused, and the
                            // auto-pause toast already told the operator why (020).
                            if !auto_pause.fired() {
                                notify::treadmill_paused();
                            }
                        }
                        _ => {}
                    }
                    // The open segment is closed inside `accumulator.observe`
                    // on any transition leaving Walking (Paused/AwayWhileRunning):
                    // the next credited step opens a fresh one, and read-time
                    // merge regroups by gap (задача 014). No DB write.

                    // Zone Hold engage/freeze/grace (задача 027). Runs after the
                    // existing default-speed/pre-pause restore above, on purpose:
                    // on a Paused→Walking return the belt speed is already
                    // restored by that code, so Zone Hold's grace window starts
                    // from the *restored* speed, not the crawl (task doc §Сход с
                    // ленты: "Zone Hold не дублирует restore — переиспользует его").
                    // Use `zh_effective`, not the raw sample: a fresh
                    // Ramp (first arrival at Walking) engages in the same match
                    // above that may have just written a default/restored speed —
                    // the raw telemetry sample still reflects the pre-write crawl.
                    let zh_resumed = zh_effective;
                    // Default-speed DB scan only needed when Zone Hold will engage
                    // (задача 047) — skip the history query when disabled.
                    let zh_default = if config.zone_hold.enabled {
                        default_speed::compute_default_speed(store, goals::load_workout_gap_minutes())
                            .ok()
                            .flatten()
                            .and_then(|d| CentiKmh::from_kmh_f32(d.kmh))
                            .unwrap_or(config.zone_hold.min_speed_kmh)
                    } else {
                        config.zone_hold.min_speed_kmh
                    };
                    zone.on_presence_transition(
                        prev_state,
                        next_state,
                        &config.zone_hold,
                        zh_resumed,
                        zh_default,
                        Instant::now(),
                    );
                }

                // Auto-pause an idle belt (задача 020). Checked every sample, not
                // just on transition: staying AwayWhileRunning fires none, so the
                // threshold must be polled while the state persists. Pure decision,
                // then the same bounded Control-Point round-trip as the command
                // queue — a failed/timed-out write is logged and retried after a
                // cooldown, never tears down the session.
                if accumulator.state() == PresenceState::AwayWhileRunning {
                    let now = Instant::now();
                    if auto_pause.due(config.auto_pause, now) {
                        let away_for = auto_pause.away_for(now).unwrap_or_default();
                        match tokio::time::timeout(
                            SPEED_RESTORE_TIMEOUT,
                            execute_control_command(
                                peripheral,
                                ControlCommand::Stop,
                                ControlSource::AutoPause,
                            ),
                        )
                        .await
                        {
                            Ok(Ok(())) => {
                                info!(
                                    away_s = away_for.as_secs(),
                                    control_source = ControlSource::AutoPause.as_str(),
                                    "auto-paused idle belt after inactivity threshold"
                                );
                                auto_pause.on_pause_ok();
                                notify::auto_paused(away_for);
                            }
                            Ok(Err(err)) => {
                                warn!(%err, "auto-pause Control Point write failed — retrying after cooldown");
                                auto_pause.on_pause_failed(Instant::now());
                            }
                            Err(_) => {
                                warn!(
                                    timeout_s = SPEED_RESTORE_TIMEOUT.as_secs(),
                                    "auto-pause timed out (possible CoreBluetooth hang) — retrying after cooldown"
                                );
                                auto_pause.on_pause_failed(Instant::now());
                            }
                        }
                    }
                }

                // Zone Hold closed-loop correction (задача 027 / 053): pure tick
                // then snapshot then BLE write (snapshot-ordering micro-diff —
                // fields independent of write outcome).
                if zone.should_run(config.zone_hold.enabled) {
                    match config.zone_hold.resolve_target_zone() {
                        Some(resolved) => {
                            let zh_bpm = zone_session::bpm_if_fresh(
                                state.hr_connected,
                                state.last_bpm,
                                state.last_bpm_ts,
                                Utc::now().timestamp_millis(),
                                ZH_BPM_MAX_AGE,
                            );
                            let now = Instant::now();
                            let write = zone.tick(
                                &config.zone_hold,
                                &resolved,
                                data.speed,
                                zh_bpm,
                                now,
                            );
                            // Persist before write when we have measured speed
                            // (same gate as pre-053 tick early-return).
                            if let Some(measured) = data.speed {
                                zone.persist_snapshot(state, &resolved, zh_bpm, measured);
                            }
                            if let Some(w) = write {
                                execute_zone_write(peripheral, w).await;
                            }
                        }
                        None => {
                            // Config edited mid-session (e.g. age removed) —
                            // nothing left to compute a target zone from.
                            warn!("zone_hold: target zone no longer resolvable — disengaging");
                            zone.disengage(state);
                        }
                    }
                } else if !zone.is_off() || state.zone_hold_active {
                    // Disabled in config while a phase was still live, or simply
                    // a stale snapshot left behind — park both (задача 032).
                    zone.disengage(state);
                }

                // Credit this sample. `Utc::now()` matches the timestamp already
                // stored on the raw sample above (same loop iteration), which is
                // exactly what the offline replay feeds back — see `docs/tasks/015`.
                accumulator.credit(store, Utc::now(), deltas)?;
                // Daily totals can only have grown when a step was actually
                // credited, so gate the goal check on that to avoid a query
                // every idle second.
                if deltas.steps > 0 {
                    celebrate_reached_goals(store, &config.goals)?;
                }
                persist_daemon_status(state, store, watchdog, db_persist_failures);
                // Primary control-command check: telemetry arrives ~1/s while
                // connected, so this bounds command latency to ≤1s during an
                // active session (задача 013). The interval arm below is only a
                // backstop for quiet stretches.
                if process_control_commands(peripheral, store).await? {
                    zone.note_cli_speed(Instant::now());
                }
            }
            _ = command_tick.tick() => {
                if process_control_commands(peripheral, store).await? {
                    zone.note_cli_speed(Instant::now());
                }
            }
            _ = config_tick.tick() => {
                // Typed hot-reload (задача 052): mtime gate → ConfigDelta →
                // apply_config → effect executor. No silent field copies.
                if let Some(delta) =
                    config_apply::reload_if_changed(&mut goals_mtime, config)
                {
                    // Empty delta (mtime moved, content identical): still refresh
                    // the status snapshot below; no effects, no change logs.
                    if !delta.is_empty() {
                        let snap = config_apply::SessionSnapshot {
                            phase: zone.kind(),
                            walking: accumulator.state() == PresenceState::Walking,
                        };
                        let effects = config_apply::apply_config(config, delta, &snap);
                        execute_config_effects(
                            &effects,
                            config,
                            &mut zone,
                            state,
                            link.last_walking_speed(),
                            store,
                        );
                    }
                    // Refresh the loaded-config snapshot + last-read time shown by
                    // `tm status` (задача 022): the file was actually re-read here
                    // even when the delta is empty (mtime moved, content identical).
                    state.set_config(&config.goals, config.auto_pause);
                    persist_daemon_status(state, store, watchdog, db_persist_failures);
                }
            }
            // A background connect attempt finished (задача 025). `NotFound`
            // is the routine case (no strap worn) — just let the reconnect
            // tick below try again later.
            outcome = hr_rx.recv() => {
                hr.on_connect_finished();
                match outcome {
                    Some(HrConnectOutcome::Connected(peripheral, stream, battery_pct)) => {
                        info!(?battery_pct, "HR sensor connected and streaming");
                        // Keep link_up ↔ hr_notifications in lockstep (задача 053).
                        hr_notifications = Some(stream);
                        hr.on_connected(
                            battery_pct,
                            Instant::now(),
                            tokio::time::Instant::now(),
                            state,
                        );
                        hr_peripheral = Some(peripheral);
                        persist_daemon_status(state, store, watchdog, db_persist_failures);
                    }
                    Some(HrConnectOutcome::NotFound) => {}
                    None => {
                        warn!("HR connect-attempt channel closed unexpectedly — no more HR reconnect attempts this session");
                    }
                }
            }
            // Live HR frames (задача 025). Silence is a separate absolute
            // `sleep_until` arm (задача 035) — never wrap `stream.next()` in a
            // relative `timeout` inside `select!` (sibling arms would reset it).
            // `None` stream uses `pending()` so we never unwrap a missing stream
            // in the future expression (that panic hit live on first treadmill
            // connect before the precondition-vs-rebuild subtlety was known).
            hr_notification = async {
                match hr_notifications.as_mut() {
                    Some(stream) => stream.next().await,
                    None => std::future::pending().await,
                }
            } => {
                match hr_notification {
                    Some(notification) if notification.uuid == hr::HEART_RATE_MEASUREMENT => {
                        let tokio_now = tokio::time::Instant::now();
                        if let Some(m) = hr::parse_hr_measurement(&notification.value) {
                            let frame_ts_ms = Utc::now().timestamp_millis();
                            match hr.on_frame(&m, frame_ts_ms, tokio_now, state) {
                                HrFrameAction::Store { ts_ms } => {
                                    store.insert_hr_sample(
                                        session_id,
                                        ts_ms,
                                        &m,
                                        &notification.value,
                                    )?;
                                    // Snapshot only after the sample is durably
                                    // stored — insert-then-publish, pre-refactor
                                    // order (053 review follow-up).
                                    hr.on_frame_stored(&m, ts_ms, state);
                                    persist_daemon_status(state, store, watchdog, db_persist_failures);
                                }
                                HrFrameAction::Drop { state_changed } => {
                                    if state_changed {
                                        persist_daemon_status(
                                            state,
                                            store,
                                            watchdog,
                                            db_persist_failures,
                                        );
                                    }
                                }
                            }
                        } else {
                            // Undecodable: still advances silence (any frame activity).
                            hr.on_link_activity(tokio_now);
                        }
                    }
                    Some(_) => {
                        // Non-HR characteristic; still counts as link activity.
                        hr.on_link_activity(tokio::time::Instant::now());
                    }
                    None => {
                        warn!("HR notification stream ended — sensor likely removed");
                        // Keep link_up ↔ hr_notifications in lockstep (задача 053).
                        hr_notifications = None;
                        hr.on_link_lost(state);
                        persist_daemon_status(state, store, watchdog, db_persist_failures);
                        if let Some(p) = hr_peripheral.take() {
                            scan::disconnect_best_effort(&p).await;
                        }
                    }
                }
            }
            // Absolute HR silence deadline (задача 035) — same pattern as the
            // treadmill telemetry arm above (задача 031).
            _ = tokio::time::sleep_until(hr.silence_deadline()),
                if hr.link_up() => {
                warn!(
                    timeout_s = HR_NOTIFICATION_TIMEOUT.as_secs(),
                    "no HR telemetry received — treating sensor as removed"
                );
                // Keep link_up ↔ hr_notifications in lockstep (задача 053).
                hr_notifications = None;
                hr.on_link_lost(state);
                persist_daemon_status(state, store, watchdog, db_persist_failures);
                if let Some(p) = hr_peripheral.take() {
                    scan::disconnect_best_effort(&p).await;
                }
            }
            // No HR link right now (never found, or just lost) — retry
            // periodically rather than hammering CoreBluetooth. Also recovers a
            // stuck in-flight latch if the spawn vanished without posting
            // (задача 042).
            _ = hr_reconnect_tick.tick(), if !hr.link_up() => {
                match hr.reconnect_decision(Instant::now()) {
                    HrReconnect::Skip => continue,
                    HrReconnect::Spawn => {
                        spawn_hr_connect_attempt(adapter.clone(), hr_tx.clone());
                    }
                }
            }
            // Battery re-read (задача 026): a cheap tick that only acts once
            // the adaptive interval has actually elapsed. Bounded inline read
            // (like the treadmill's own Control Point writes) — fine to block
            // this loop briefly given how rarely it's due (≥30 min).
            _ = hr_battery_check_tick.tick(), if hr_peripheral.is_some() => {
                let now = Instant::now();
                if hr.battery_read_due(now) {
                    let peripheral = hr_peripheral.as_ref().expect("guarded by hr_peripheral.is_some()");
                    let read = scan::read_hr_battery(peripheral).await;
                    if read.is_some() {
                        info!(battery_pct = ?read, "re-read HR sensor battery level");
                    }
                    // Failed read keeps last known pct; still stamps last_read
                    // so a wedged sensor is not hammered every tick.
                    hr.on_battery_read(read, Instant::now(), state);
                    if read.is_some() {
                        persist_daemon_status(state, store, watchdog, db_persist_failures);
                    }
                }
            }
        }
    }

    logger.finish();
    store.end_session()?;
    if let Some(p) = hr_peripheral.take() {
        scan::disconnect_best_effort(&p).await;
    }
    error!("notification stream ended (device disconnected?)");
    Ok(())
}

/// The pre-pause walking speed to re-send on resume, or `None` when there is
/// nothing worth restoring: the machine did not actually slow down (resumed at
/// the pre-pause speed or faster, within [`SPEED_RESTORE_EPSILON`]). Pure
/// and unit-tested — the BLE write lives in [`restore_speed`].
fn speed_restore_target(pre_pause: CentiKmh, resumed: CentiKmh) -> Option<CentiKmh> {
    (pre_pause.to_wire()
        > resumed
            .to_wire()
            .saturating_add(SPEED_RESTORE_EPSILON.to_wire()))
    .then_some(pre_pause)
}

/// Best-effort restore of the pre-pause belt speed on a pause→walk resume
/// (задача 012, Task D). Returns the applied restore for the toast, or `None`
/// (with a WARN on the abnormal paths) when nothing was applied — a missing
/// captured speed, a no-op, or a failed/timed-out Control Point write must all
/// leave the session running, never crash it.
async fn try_restore_speed(
    peripheral: &Peripheral,
    pre_pause: Option<CentiKmh>,
    resumed: CentiKmh,
) -> Option<notify::SpeedRestore> {
    let Some(pre_pause) = pre_pause else {
        // Daemon started already paused, or the pause preceded any walking.
        warn!("resume without a captured pre-pause speed — skipping speed restore");
        return None;
    };
    let target = speed_restore_target(pre_pause, resumed)?;
    let source = ControlSource::Restore;

    match tokio::time::timeout(SPEED_RESTORE_TIMEOUT, restore_speed(peripheral, target)).await {
        Ok(Ok(())) => {
            info!(
                from = %resumed,
                to = %target,
                control_source = source.as_str(),
                "restored pre-pause belt speed on resume"
            );
            Some(notify::SpeedRestore {
                from_kmh: resumed.to_kmh_f32(),
                to_kmh: target.to_kmh_f32(),
            })
        }
        Ok(Err(err)) => {
            warn!(%err, %target, control_source = source.as_str(), "failed to restore pre-pause speed — leaving resume toast without the restore line");
            None
        }
        Err(_) => {
            warn!(
                timeout_s = SPEED_RESTORE_TIMEOUT.as_secs(),
                %target,
                control_source = source.as_str(),
                "speed restore timed out (possible CoreBluetooth hang)"
            );
            None
        }
    }
}

/// Take FTMS control and set the target speed. Split from [`try_restore_speed`]
/// so the whole round-trip can be wrapped in one bounded `timeout` there.
async fn restore_speed(peripheral: &Peripheral, target: CentiKmh) -> Result<()> {
    let controller = Controller::take_control(peripheral).await?;
    controller.set_speed(target).await
}

/// Apply the computed default belt speed at a workout start (задача 016), when
/// there is no pre-pause speed to restore. Returns the applied speed, or `None`
/// when nothing was applied. Guards, in order:
/// - once per session (`applied`) — one attempt per (re)start, no retry storm on
///   a presence flap at the crawl;
/// - the belt must be at/below the device crawl ([`DEFAULT_SPEED_APPLY_CEILING`])
///   — a belt already moving faster was set by the operator (or a daemon restart
///   landed mid-walk); never override it;
/// - a qualifying prior workout must exist ([`default_speed::compute_default_speed`]).
///
/// The BLE write reuses the bounded [`restore_speed`]/[`SPEED_RESTORE_TIMEOUT`]
/// path (задачи 007/012); a failed/timed-out write is logged and swallowed —
/// applying a convenience speed must never tear down the session.
async fn try_apply_default_speed(
    peripheral: &Peripheral,
    store: &Store,
    resumed: CentiKmh,
    link: &mut TreadmillLink,
) -> Option<CentiKmh> {
    if link.default_speed_applied() {
        return None;
    }
    if resumed > DEFAULT_SPEED_APPLY_CEILING {
        // Belt already at a real speed — the operator's choice, or a mid-walk
        // reconnect. Not a fresh crawl start; leave it alone (and let a later
        // genuine crawl start still get its one attempt).
        return None;
    }

    let gap_minutes = goals::load_workout_gap_minutes();
    let target = match default_speed::compute_default_speed(store, gap_minutes) {
        Ok(Some(default)) => match CentiKmh::from_kmh_f32(default.kmh) {
            Some(t) => t,
            None => {
                warn!(
                    kmh = default.kmh,
                    "computed default speed out of range — skipping"
                );
                return None;
            }
        },
        Ok(None) => {
            info!(
                "no qualifying prior workout (≥30m walking) — leaving belt at its device default speed"
            );
            // Nothing to apply; don't recompute on every Walking flap this session.
            link.mark_default_speed_applied();
            return None;
        }
        Err(err) => {
            // A transient DB error may clear — do not consume the one attempt.
            warn!(%err, "failed to compute default speed — leaving belt at its device default");
            return None;
        }
    };

    // One attempt per session regardless of the write outcome (a failed write
    // must not loop on a presence flap at the crawl start).
    link.mark_default_speed_applied();
    let source = ControlSource::DefaultSpeed;
    match tokio::time::timeout(SPEED_RESTORE_TIMEOUT, restore_speed(peripheral, target)).await {
        Ok(Ok(())) => {
            info!(
                from = %resumed,
                to = %target,
                control_source = source.as_str(),
                "applied computed default belt speed at workout start"
            );
            Some(target)
        }
        Ok(Err(err)) => {
            warn!(%err, %target, control_source = source.as_str(), "failed to apply default belt speed at workout start — leaving belt as is");
            None
        }
        Err(_) => {
            warn!(
                timeout_s = SPEED_RESTORE_TIMEOUT.as_secs(),
                %target,
                control_source = source.as_str(),
                "default belt speed write timed out (possible CoreBluetooth hang)"
            );
            None
        }
    }
}

/// Execute session effects from a config hot-reload (задача 052). Each effect
/// produces exactly one log line; field values are already applied by
/// [`config_apply::apply_config`]. Zone phase mutations go through [`ZoneSession`].
fn execute_config_effects(
    effects: &[config_apply::ConfigEffect],
    config: &LiveConfig,
    zone: &mut ZoneSession,
    state: &mut DaemonState,
    last_walking_speed: Option<f32>,
    store: &Store,
) {
    use config_apply::{ConfigEffect, DisengageReason};

    for effect in effects {
        match effect {
            ConfigEffect::GoalsChanged => {
                info!(
                    goals = ?config.goals,
                    "goals config changed on disk — reloaded without a daemon restart"
                );
            }
            ConfigEffect::AutoPauseChanged => {
                info!(
                    auto_pause = ?config.auto_pause,
                    "auto-pause threshold changed on disk — reloaded without a daemon restart"
                );
            }
            ConfigEffect::ZoneDisengage(DisengageReason::DisabledInConfig) => {
                info!("zone hold: disabled in config — disengaging mid-session");
                zone.disengage(state);
            }
            ConfigEffect::ZoneDisengage(DisengageReason::TargetUnresolvable) => {
                warn!(
                    "zone hold: target zone no longer resolvable after config reload — disengaging"
                );
                zone.disengage(state);
            }
            ConfigEffect::ZoneEngage => {
                // Prefer last measured walking speed; min_speed only as
                // engage seed when we have *some* observation (задача 036
                // forbids inventing 0.0, not a known min floor).
                let zh_resumed = last_walking_speed
                    .and_then(CentiKmh::from_kmh_f32)
                    .or(Some(config.zone_hold.min_speed_kmh));
                let zh_default =
                    default_speed::compute_default_speed(store, goals::load_workout_gap_minutes())
                        .ok()
                        .flatten()
                        .and_then(|d| CentiKmh::from_kmh_f32(d.kmh))
                        .unwrap_or(config.zone_hold.min_speed_kmh);
                zone.on_config_engaged(&config.zone_hold, zh_resumed, zh_default, Instant::now());
            }
            ConfigEffect::ZoneReResolve => {
                match config.zone_hold.resolve_target_zone() {
                    Some(resolved) => {
                        info!(
                            lo = resolved.low_bpm,
                            hi = resolved.high_bpm,
                            zone = %resolved.name,
                            "zone hold: re-resolved target zone after config reload"
                        );
                        state.zone_hold_target_lo = Some(i64::from(resolved.low_bpm));
                        state.zone_hold_target_hi = Some(i64::from(resolved.high_bpm));
                    }
                    None => {
                        // apply_config emits Disengage instead of ReResolve when
                        // unresolvable; keep defense in depth.
                        warn!("zone hold: re-resolve failed after config reload — disengaging");
                        zone.disengage(state);
                    }
                }
            }
            ConfigEffect::ZoneWarmupRetarget {
                old_minutes,
                new_minutes,
            } => {
                info!(
                    old_minutes,
                    new_minutes,
                    "zone hold: warmup_minutes changed mid-ramp — retargeting without restart"
                );
            }
            ConfigEffect::ZoneConfigChanged { fields } => {
                info!(
                    ?fields,
                    "zone hold: config fields changed on disk — applied without a session phase effect"
                );
            }
        }
    }
}

/// Execute a pure [`ZoneWrite`] from [`ZoneSession::tick`] (BLE effect side).
async fn execute_zone_write(peripheral: &Peripheral, write: ZoneWrite) {
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
async fn apply_zone_hold_speed(peripheral: &Peripheral, target: CentiKmh, suppressed: bool) {
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

/// After a step credit, fire a toast for each configured goal today's steps
/// have newly reached and persist that it was celebrated, so a mid-day daemon
/// restart never re-fires it (задача 011). A goal-check failure must not tear
/// down an otherwise-healthy session, so problems are logged, not propagated.
fn celebrate_reached_goals(store: &Store, step_goals: &[Goal]) -> Result<()> {
    if step_goals.is_empty() {
        return Ok(());
    }
    let today = Local::now().format("%Y-%m-%d").to_string();
    let today_steps = store.today_stats()?.steps;
    let already: std::collections::HashSet<i64> =
        store.celebrated_thresholds(&today)?.into_iter().collect();
    for goal in goals::thresholds_to_celebrate(today_steps, step_goals, &already) {
        info!(
            threshold = goal.threshold,
            tier = goal.tier,
            steps = today_steps,
            "daily step goal reached"
        );
        notify::goal_reached(goal.threshold, goal.tier);
        store.mark_goal_celebrated(&today, goal.threshold)?;
    }
    Ok(())
}

/// Execute at most one pending control command on the live BLE link (задача
/// 013). Silent on the empty path — this runs ~1/s, so no happy-path log.
///
/// Returns `true` when a successful CLI `Speed` ran (задача 039 — open the
/// operator-override window so Zone Hold does not immediately overwrite it).
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
async fn process_control_commands(peripheral: &Peripheral, store: &Store) -> Result<bool> {
    let Some(queued) = store.next_pending_control_command()? else {
        return Ok(false);
    };

    if control_command::is_stale(queued.created_at, Utc::now()) {
        warn!(id = queued.id, command = %queued.command.to_wire(), "control command is stale — failing without executing");
        store.mark_control_command_failed(queued.id, "stale, not executed")?;
        return Ok(false);
    }

    let source = ControlSource::Cli;
    let was_speed = matches!(queued.command, ControlCommand::Speed(_));
    let command_wire = queued.command.to_wire();
    match tokio::time::timeout(
        SPEED_RESTORE_TIMEOUT,
        execute_control_command(peripheral, queued.command, source),
    )
    .await
    {
        Ok(Ok(())) => {
            info!(
                id = queued.id,
                command = %command_wire,
                control_source = source.as_str(),
                "executed queued control command"
            );
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

/// Take FTMS control and run one command. Split out so the whole round-trip can
/// be wrapped in a single bounded `timeout` by the caller. Reuses the same
/// take-control path as `restore_speed` and any other Control Point write (see
/// `control::Controller`). `source` is for call-site logging only — this
/// function does not log (the caller owns success/fail messages).
async fn execute_control_command(
    peripheral: &Peripheral,
    command: ControlCommand,
    _source: ControlSource,
) -> Result<()> {
    let controller = Controller::take_control(peripheral).await?;
    match command {
        ControlCommand::Start => controller.start().await,
        ControlCommand::Stop => controller.stop().await,
        ControlCommand::Speed(kmh) => controller.set_speed(kmh).await,
    }
}

/// In-memory mirror of the `daemon_status` row (see `store::DaemonStatus`),
/// rebuilt and upserted on every transition the daemon observes, so a
/// separate `status` CLI invocation can read current state without racing
/// the daemon for the BLE adapter.
pub(crate) struct DaemonState {
    pub(crate) connected: bool,
    pub(crate) presence_state: Option<String>,
    pub(crate) last_connected_at: Option<String>,
    pub(crate) last_disconnected_at: Option<String>,
    pub(crate) power_mode: &'static str,
    pub(crate) power_mode_since: DateTime<Utc>,
    // Snapshot of the config the daemon currently holds, surfaced by `tm status`
    // (задача 022): comma-joined goals, auto-pause threshold in seconds (`None` =
    // disabled), and when the config file was last read. Updated by `set_config`
    // at startup and on each mtime-triggered reload.
    pub(crate) config_goals: Option<String>,
    pub(crate) config_auto_pause_secs: Option<i64>,
    pub(crate) config_loaded_at: Option<String>,
    // Heart-rate snapshot (задача 025) — same reasoning as the rest of this
    // struct: mirrors what the daemon just observed so `tm status`/`widget`/
    // `stats` can read it without racing the daemon for BLE.
    pub(crate) hr_connected: bool,
    pub(crate) last_bpm: Option<i64>,
    pub(crate) last_bpm_ts: Option<i64>,
    /// HR sensor battery level, 0-100% (задача 026). `None` until read at
    /// least once this link.
    pub(crate) hr_battery_pct: Option<i64>,
    /// Zone Hold snapshot (задача 027) — mirrors `ZoneHoldPhase`/the resolved
    /// target zone so `tm status`/`tm widget` can read it without racing the
    /// daemon for BLE. See `zh_persist_snapshot`.
    pub(crate) zone_hold_active: bool,
    pub(crate) zone_hold_target_lo: Option<i64>,
    pub(crate) zone_hold_target_hi: Option<i64>,
    pub(crate) zone_hold_last_speed: Option<f64>,
    pub(crate) zone_hold_phase: Option<String>,
    pub(crate) zone_hold_position: Option<String>,
    /// Live belt-speed snapshot (задача 029) — updated on every telemetry
    /// sample regardless of Zone Hold, same reasoning as `last_bpm`/
    /// `last_bpm_ts` above. `last_speed_ts` is Unix millis.
    pub(crate) last_speed_kmh: Option<f64>,
    pub(crate) last_speed_ts: Option<i64>,
}

impl DaemonState {
    pub(crate) fn new(on_ac: bool) -> Self {
        Self {
            connected: false,
            presence_state: None,
            last_connected_at: None,
            last_disconnected_at: None,
            power_mode: power_mode_label(on_ac),
            power_mode_since: Utc::now(),
            config_goals: None,
            config_auto_pause_secs: None,
            config_loaded_at: None,
            hr_connected: false,
            last_bpm: None,
            last_bpm_ts: None,
            hr_battery_pct: None,
            zone_hold_active: false,
            zone_hold_target_lo: None,
            zone_hold_target_hi: None,
            zone_hold_last_speed: None,
            zone_hold_phase: None,
            zone_hold_position: None,
            last_speed_kmh: None,
            last_speed_ts: None,
        }
    }

    /// Snapshot the config the daemon just (re)loaded, stamping the read time —
    /// surfaced by `tm status` (задача 022). Called at startup and whenever the
    /// config file is re-read on the mtime watch (задача 017).
    fn set_config(&mut self, goals: &[Goal], auto_pause: Option<Duration>) {
        self.config_goals = Some(
            goals
                .iter()
                .map(|g| g.threshold.to_string())
                .collect::<Vec<_>>()
                .join(","),
        );
        self.config_auto_pause_secs = auto_pause.map(|d| d.as_secs() as i64);
        self.config_loaded_at = Some(Utc::now().to_rfc3339());
    }

    /// Update the power mode, bumping `power_mode_since` only on an actual
    /// change (repeated events for the same mode must not reset the "since"
    /// timestamp shown by the future `status` command).
    fn set_power_mode(&mut self, on_ac: bool) {
        let mode = power_mode_label(on_ac);
        if self.power_mode != mode {
            self.power_mode = mode;
            self.power_mode_since = Utc::now();
        }
    }

    /// Upsert the current state into `daemon_status` and mark the watchdog
    /// as freshly touched. Called on every meaningful transition plus, as a
    /// backstop, on every telemetry sample and every idle/watchdog tick.
    fn persist(&self, store: &Store, watchdog: &Watchdog) -> Result<()> {
        store.upsert_daemon_status(&DaemonStatus {
            connected: self.connected,
            presence_state: self.presence_state.clone(),
            last_connected_at: self.last_connected_at.clone(),
            last_disconnected_at: self.last_disconnected_at.clone(),
            power_mode: self.power_mode.to_string(),
            power_mode_since: self.power_mode_since.to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            config_goals: self.config_goals.clone(),
            config_auto_pause_secs: self.config_auto_pause_secs,
            config_loaded_at: self.config_loaded_at.clone(),
            hr_connected: self.hr_connected,
            last_bpm: self.last_bpm,
            last_bpm_ts: self.last_bpm_ts,
            hr_battery_pct: self.hr_battery_pct,
            zone_hold_active: self.zone_hold_active,
            zone_hold_target_lo: self.zone_hold_target_lo,
            zone_hold_target_hi: self.zone_hold_target_hi,
            zone_hold_last_speed: self.zone_hold_last_speed,
            zone_hold_phase: self.zone_hold_phase.clone(),
            zone_hold_position: self.zone_hold_position.clone(),
            last_speed_kmh: self.last_speed_kmh,
            last_speed_ts: self.last_speed_ts,
        })?;
        watchdog.touch();
        Ok(())
    }
}

fn power_mode_label(on_ac: bool) -> &'static str {
    if on_ac { "ac_scanning" } else { "battery_idle" }
}

/// Tracks the last time the daemon made observable progress and, from its
/// own spawned task, kills the process when that stops — the unmissable
/// answer to a silent hang (задачи D/007), independent of *why* the daemon
/// stopped advancing (power-gate bug, a wedged btleplug/CoreBluetooth call,
/// or anything else).
///
/// Runs on a dedicated `tokio::spawn` task rather than a `select!` arm: the
/// 2026-07-05 incident proved an in-loop check never fires while the hang
/// sits inside another arm's handler body. Self-healing is a plain process
/// exit (launchd `KeepAlive` restarts us) because a CoreBluetooth hang is
/// unrecoverable in-process — see the module docs for why this reverses the
/// original задача D "signal only" stance.
struct Watchdog {
    /// Fixed anchor all touch timestamps are measured against.
    anchor: Instant,
    /// Milliseconds since `anchor` at the moment of the last `touch()`,
    /// shared with the monitor task.
    last_touch_ms: Arc<AtomicU64>,
    /// Milliseconds since `anchor` at the moment of the last `touch_telemetry()`,
    /// i.e. the last decoded `0x2ACD` frame (задача 031). Kept apart from
    /// `last_touch_ms` because the general touch rides on `State::persist()`,
    /// which any loop branch (HR frame, control poll, config reload) triggers —
    /// it proves the event loop is alive, never that the treadmill is talking.
    last_telemetry_ms: Arc<AtomicU64>,
    /// Whether telemetry is actively streaming (задача 018). Selects the tighter
    /// [`STREAMING_STALE_THRESHOLD`] over the general [`WATCHDOG_STALE_THRESHOLD`],
    /// shared with the monitor task.
    streaming: Arc<AtomicBool>,
}

impl Watchdog {
    fn new() -> Self {
        Self {
            anchor: Instant::now(),
            last_touch_ms: Arc::new(AtomicU64::new(0)),
            last_telemetry_ms: Arc::new(AtomicU64::new(0)),
            streaming: Arc::new(AtomicBool::new(false)),
        }
    }

    fn touch(&self) {
        self.store_now(&self.last_touch_ms);
    }

    /// Record a treadmill telemetry frame (`0x2ACD`). The only thing the tight
    /// [`STREAMING_STALE_THRESHOLD`] is allowed to watch — see `last_telemetry_ms`.
    fn touch_telemetry(&self) {
        self.store_now(&self.last_telemetry_ms);
        self.touch();
    }

    fn store_now(&self, slot: &AtomicU64) {
        let elapsed_ms = u64::try_from(self.anchor.elapsed().as_millis()).unwrap_or(u64::MAX);
        slot.store(elapsed_ms, Ordering::Relaxed);
    }

    /// Mark whether telemetry is actively streaming, switching which staleness
    /// threshold the monitor applies (задача 018). Set true once the
    /// notification stream is open, false the moment the session ends.
    fn set_streaming(&self, streaming: bool) {
        self.streaming.store(streaming, Ordering::Relaxed);
    }

    /// The staleness threshold for the current phase: tight while streaming
    /// (a connected treadmill is never silent that long), generous otherwise
    /// (scan/connect/teardown have legitimately long gaps).
    fn stale_threshold(&self) -> Duration {
        if self.streaming.load(Ordering::Relaxed) {
            STREAMING_STALE_THRESHOLD
        } else {
            WATCHDOG_STALE_THRESHOLD
        }
    }

    /// Milliseconds since `anchor` of the progress signal the current phase
    /// watches: treadmill frames while streaming, any loop progress otherwise.
    fn last_progress_ms(&self) -> u64 {
        if self.streaming.load(Ordering::Relaxed) {
            self.last_telemetry_ms.load(Ordering::Relaxed)
        } else {
            self.last_touch_ms.load(Ordering::Relaxed)
        }
    }

    /// Whether the watched progress signal is older than the current-phase
    /// threshold ([`Self::stale_threshold`]), given the elapsed-since-anchor
    /// time. Split from `spawn_monitor` so the threshold logic is unit-testable
    /// without a runtime or real waiting.
    fn is_stale_at(&self, elapsed_since_anchor: Duration) -> bool {
        let last = Duration::from_millis(self.last_progress_ms());
        elapsed_since_anchor.saturating_sub(last) > self.stale_threshold()
    }

    /// Start the independent monitor task. On detected staleness it logs an
    /// `ERROR` and exits the whole process with [`WATCHDOG_EXIT_CODE`] so
    /// launchd (`KeepAlive=true`) restarts the daemon cleanly.
    fn spawn_monitor(&self) {
        let probe = Self {
            anchor: self.anchor,
            last_touch_ms: Arc::clone(&self.last_touch_ms),
            last_telemetry_ms: Arc::clone(&self.last_telemetry_ms),
            streaming: Arc::clone(&self.streaming),
        };
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(WATCHDOG_POLL_INTERVAL);
            loop {
                tick.tick().await;
                let elapsed = probe.anchor.elapsed();
                if probe.is_stale_at(elapsed) {
                    let last_touch = Duration::from_millis(probe.last_progress_ms());
                    error!(
                        stale_s = elapsed.saturating_sub(last_touch).as_secs(),
                        threshold_s = probe.stale_threshold().as_secs(),
                        streaming = probe.streaming.load(Ordering::Relaxed),
                        exit_code = WATCHDOG_EXIT_CODE,
                        "silent hang detected — exiting so launchd restarts the daemon"
                    );
                    std::process::exit(WATCHDOG_EXIT_CODE);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn memory_store() -> Store {
        Store::open_at(Path::new(":memory:")).expect("open in-memory store")
    }

    #[test]
    fn daemon_state_persist_roundtrips_and_touches_watchdog() {
        let store = memory_store();
        let watchdog = Watchdog::new();
        // Untouched watchdog at a synthetic "far future" instant is stale.
        let far_future = WATCHDOG_STALE_THRESHOLD * 2;
        assert!(watchdog.is_stale_at(far_future));

        let mut state = DaemonState::new(true);
        state.connected = true;
        state.presence_state = Some("Walking".to_string());
        // Loaded-config snapshot is persisted too (задача 022).
        state.set_config(
            &[Goal {
                threshold: 8500,
                tier: 1,
            }],
            Some(Duration::from_secs(300)),
        );
        state.persist(&store, &watchdog).unwrap();

        // `persist` touched the watchdog just now: fresh well inside the
        // threshold, stale again well past it (exact-boundary checks would
        // race the sub-ms gap between the touch and this measurement).
        assert!(!watchdog.is_stale_at(watchdog.anchor.elapsed() + WATCHDOG_STALE_THRESHOLD / 2));
        assert!(watchdog.is_stale_at(watchdog.anchor.elapsed() + WATCHDOG_STALE_THRESHOLD * 2));
        let status = store.daemon_status().unwrap().expect("row present");
        assert!(status.connected);
        assert_eq!(status.presence_state.as_deref(), Some("Walking"));
        assert_eq!(status.power_mode, "ac_scanning");
        assert_eq!(status.config_goals.as_deref(), Some("8500"));
        assert_eq!(status.config_auto_pause_secs, Some(300));
        assert!(status.config_loaded_at.is_some());
    }

    #[test]
    fn watchdog_uses_tighter_threshold_while_streaming() {
        let watchdog = Watchdog::new();
        // A gap between the two thresholds: stale while streaming, fine otherwise.
        let between = (STREAMING_STALE_THRESHOLD + WATCHDOG_STALE_THRESHOLD) / 2;
        assert!(STREAMING_STALE_THRESHOLD < between && between < WATCHDOG_STALE_THRESHOLD);

        // Not streaming (scan/connect phase): the generous threshold applies.
        assert!(!watchdog.is_stale_at(between));
        // Streaming: the same gap is now a dead link.
        watchdog.set_streaming(true);
        assert!(watchdog.is_stale_at(between));
        assert_eq!(watchdog.stale_threshold(), STREAMING_STALE_THRESHOLD);
        // Lifting streaming restores the generous threshold (teardown/reconnect).
        watchdog.set_streaming(false);
        assert!(!watchdog.is_stale_at(between));
        assert_eq!(watchdog.stale_threshold(), WATCHDOG_STALE_THRESHOLD);
    }

    /// The задача 031 regression: `persist()`-driven `touch()` (HR frames,
    /// control polls, config reloads) must not keep the streaming watchdog alive
    /// while the treadmill itself has gone silent. Only `touch_telemetry()` does.
    #[test]
    fn streaming_watchdog_ignores_non_telemetry_touches() {
        let watchdog = Watchdog::new();
        watchdog.set_streaming(true);
        let dead = STREAMING_STALE_THRESHOLD * 2;

        // A generic touch (as any `persist()` call site does) leaves the
        // treadmill clock untouched — the link still reads as dead.
        watchdog.touch();
        assert!(watchdog.is_stale_at(watchdog.anchor.elapsed() + dead));

        // A decoded `0x2ACD` frame is what clears it.
        watchdog.touch_telemetry();
        assert!(!watchdog.is_stale_at(watchdog.anchor.elapsed() + STREAMING_STALE_THRESHOLD / 2));
    }

    /// The telemetry deadline must survive `select!` rebuilding its arm on every
    /// pass — the bug of задача 031, where a 1s sibling tick reset a relative
    /// `timeout(NOTIFICATION_TIMEOUT, ...)` forever.
    #[tokio::test(start_paused = true)]
    async fn telemetry_deadline_fires_despite_a_faster_sibling_arm() {
        let link = TreadmillLink::new(tokio::time::Instant::now());
        let deadline = link.silence_deadline();
        let mut sibling = tokio::time::interval(Duration::from_secs(1));
        let mut ticks = 0u32;

        let fired = loop {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(link.silence_deadline()) => break true,
                _ = sibling.tick() => {
                    ticks += 1;
                    // Guard against an infinite loop if the deadline never lands.
                    if ticks > NOTIFICATION_TIMEOUT.as_secs() as u32 * 2 {
                        break false;
                    }
                }
            }
        };

        assert!(fired, "telemetry deadline never fired");
        assert_eq!(
            deadline.elapsed(),
            Duration::ZERO,
            "deadline must land exactly at silence_deadline, not drift with the sibling"
        );
        assert_eq!(link.silence_deadline().elapsed(), Duration::ZERO,);
        // Elapsed since construction equals the timeout when the arm fires.
        assert_eq!(
            (deadline - NOTIFICATION_TIMEOUT).elapsed(),
            NOTIFICATION_TIMEOUT,
        );
    }

    /// Same class as `telemetry_deadline_fires_despite_a_faster_sibling_arm`
    /// (задача 031) for the HR link (задача 035): relative `timeout` around
    /// `stream.next()` never ages while a 1s sibling completes every pass.
    #[tokio::test(start_paused = true)]
    async fn hr_silence_deadline_fires_despite_a_faster_sibling_arm() {
        let hr = HrSession::new_connecting(Instant::now(), tokio::time::Instant::now());
        let mut sibling = tokio::time::interval(Duration::from_secs(1));
        let mut ticks = 0u32;

        let fired = loop {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(hr.silence_deadline()) => break true,
                _ = sibling.tick() => {
                    ticks += 1;
                    if ticks > HR_NOTIFICATION_TIMEOUT.as_secs() as u32 * 2 {
                        break false;
                    }
                }
            }
        };

        assert!(fired, "HR silence deadline never fired");
        assert_eq!(
            (hr.silence_deadline() - HR_NOTIFICATION_TIMEOUT).elapsed(),
            HR_NOTIFICATION_TIMEOUT,
            "HR deadline must land exactly at the timeout, not drift with the sibling"
        );
    }

    #[test]
    fn speed_restore_target_restores_only_a_real_slowdown() {
        let c = |kmh: f32| CentiKmh::from_kmh_f32(kmh).expect("test speed");
        // Typical case: paused at 2.5, machine resumed at 0.5 → restore 2.5.
        assert_eq!(speed_restore_target(c(2.5), c(0.5)), Some(c(2.5)));
        // No slowdown (resumed at the same speed) → nothing to restore.
        assert_eq!(speed_restore_target(c(2.5), c(2.5)), None);
        // Resumed faster than before → nothing to restore.
        assert_eq!(speed_restore_target(c(2.5), c(3.0)), None);
        // Within epsilon → treated as no change.
        assert_eq!(speed_restore_target(c(2.5), c(2.48)), None);
    }

    #[test]
    fn set_power_mode_only_bumps_since_on_actual_change() {
        let mut state = DaemonState::new(true);
        let since_before = state.power_mode_since;

        // Same mode again — must not reset `power_mode_since`.
        state.set_power_mode(true);
        assert_eq!(state.power_mode_since, since_before);
        assert_eq!(state.power_mode, "ac_scanning");

        state.set_power_mode(false);
        assert_eq!(state.power_mode, "battery_idle");
        assert!(state.power_mode_since >= since_before);
    }

    // --- задача 051 / backlog 009: scan recovery streak + panic payload ---

    /// Healthy connect failures ("no FTMS treadmill found", connect/discover
    /// errors) must never grow the streak or trigger recycle/exit.
    #[test]
    fn scan_recovery_non_scan_start_always_retries_and_resets() {
        let mut r = ScanRecovery::default();
        for _ in 0..10 {
            assert_eq!(
                r.on_connect_failure(false),
                ScanRecoveryAction::Retry,
                "non-scan-start failure must stay Retry"
            );
            assert_eq!(r.scan_start_streak, 0);
            assert_eq!(r.recycles, 0);
        }
    }

    /// Exactly `SCAN_START_RECYCLE_THRESHOLD` consecutive scan-start failures
    /// yield one recycle and clear the streak.
    #[test]
    fn scan_recovery_threshold_triggers_recycle() {
        let mut r = ScanRecovery::default();
        for i in 1..SCAN_START_RECYCLE_THRESHOLD {
            assert_eq!(
                r.on_connect_failure(true),
                ScanRecoveryAction::Retry,
                "streak {i} should still Retry"
            );
            assert_eq!(r.scan_start_streak, i);
            assert_eq!(r.recycles, 0);
        }
        assert_eq!(
            r.on_connect_failure(true),
            ScanRecoveryAction::RecycleAdapter
        );
        assert_eq!(r.scan_start_streak, 0, "streak cleared after recycle");
        assert_eq!(r.recycles, 1);
    }

    /// A successful scan (or non-scan-start failure) after a partial streak
    /// resets both counters — including an earlier recycle count.
    #[test]
    fn scan_recovery_success_resets_partial_streak_and_recycles() {
        let mut r = ScanRecovery::default();
        // Build one recycle, then a partial streak toward the next.
        for _ in 0..SCAN_START_RECYCLE_THRESHOLD {
            let _ = r.on_connect_failure(true);
        }
        assert_eq!(r.recycles, 1);
        assert_eq!(r.on_connect_failure(true), ScanRecoveryAction::Retry);
        assert_eq!(r.scan_start_streak, 1);

        r.on_scan_ok();
        assert_eq!(r.scan_start_streak, 0);
        assert_eq!(r.recycles, 0);

        // Non-scan-start failure is also a full reset (adapter proved alive).
        for _ in 0..SCAN_START_RECYCLE_THRESHOLD {
            let _ = r.on_connect_failure(true);
        }
        assert_eq!(r.recycles, 1);
        assert_eq!(r.on_connect_failure(false), ScanRecoveryAction::Retry);
        assert_eq!(r.scan_start_streak, 0);
        assert_eq!(r.recycles, 0);
    }

    /// After `SCAN_RECYCLE_MAX` recycles with no healthy scan in between, the
    /// next full streak exits for launchd restart.
    #[test]
    fn scan_recovery_exits_after_max_recycles() {
        let mut r = ScanRecovery::default();
        for expected_recycle in 1..=SCAN_RECYCLE_MAX {
            let mut last = ScanRecoveryAction::Retry;
            for _ in 0..SCAN_START_RECYCLE_THRESHOLD {
                last = r.on_connect_failure(true);
            }
            assert_eq!(
                last,
                ScanRecoveryAction::RecycleAdapter,
                "recycle #{expected_recycle}"
            );
            assert_eq!(r.recycles, expected_recycle);
            assert_eq!(r.scan_start_streak, 0);
        }

        // Next streak must Exit (recycles already at max).
        for i in 1..SCAN_START_RECYCLE_THRESHOLD {
            assert_eq!(r.on_connect_failure(true), ScanRecoveryAction::Retry);
            assert_eq!(r.scan_start_streak, i);
        }
        assert_eq!(r.on_connect_failure(true), ScanRecoveryAction::Exit);
        assert_eq!(r.recycles, SCAN_RECYCLE_MAX);
        assert_eq!(r.scan_start_streak, SCAN_START_RECYCLE_THRESHOLD);
    }

    /// A healthy scan after a recycle clears the recycle budget — the next
    /// wedge starts from recycle #1 again, not Exit.
    #[test]
    fn scan_recovery_success_after_recycle_allows_another_recycle() {
        let mut r = ScanRecovery::default();
        for _ in 0..SCAN_START_RECYCLE_THRESHOLD {
            let _ = r.on_connect_failure(true);
        }
        assert_eq!(r.recycles, 1);

        r.on_scan_ok();
        assert_eq!(r.recycles, 0);

        for _ in 0..SCAN_START_RECYCLE_THRESHOLD {
            let _ = r.on_connect_failure(true);
        }
        assert_eq!(
            r.recycles, 1,
            "post-success streak must recycle again, not exit"
        );
        // Not at max yet with a single recycle — still not Exit.
        assert_ne!(r.on_connect_failure(true), ScanRecoveryAction::Exit);
    }

    #[test]
    fn panic_payload_message_str_string_and_other() {
        let as_str: &str = "boom from &str";
        assert_eq!(
            panic_payload_message(&as_str as &(dyn std::any::Any + Send)),
            "boom from &str"
        );

        let as_string = String::from("boom from String");
        assert_eq!(
            panic_payload_message(&as_string as &(dyn std::any::Any + Send)),
            "boom from String"
        );

        let as_int = 42i32;
        assert_eq!(
            panic_payload_message(&as_int as &(dyn std::any::Any + Send)),
            "<non-string panic payload>"
        );
    }

    #[test]
    fn panic_location_message_with_and_without_location() {
        // `Location::caller()` always yields Some when we pass it explicitly.
        let loc = std::panic::Location::caller();
        let formatted = panic_location_message(Some(loc));
        assert!(
            formatted.contains(':'),
            "expected file:line, got {formatted}"
        );
        assert!(
            formatted.contains("daemon.rs"),
            "expected this source file in {formatted}"
        );

        assert_eq!(panic_location_message(None), "<unknown>");
    }
}
