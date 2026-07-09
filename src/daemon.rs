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

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use std::pin::Pin;

use anyhow::{Result, anyhow};
use btleplug::api::{Peripheral as _, ValueNotification};
use btleplug::platform::{Adapter, Peripheral};
use chrono::{DateTime, Local, Utc};
use futures::{Stream, StreamExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::activity::ActivityAccumulator;
use crate::control::Controller;
use crate::control_command::{self, ControlCommand};
use crate::default_speed;
use crate::ftms;
use crate::goals::{self, Goal};
use crate::hr;
use crate::logger::WorkoutLogger;
use crate::notify;
use crate::power::{self, PowerEvent};
use crate::presence::{self, PresenceState};
use crate::scan;
use crate::store::{DaemonStatus, Store};
use crate::zone_hold;

/// Delay before retrying discovery after a scan/connect failure, so a
/// transient Bluetooth hiccup does not spin the CPU in a tight loop.
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// How long to wait for the next Treadmill Data sample before treating the
/// link as lost. The device streams ~1/s even while stationary, so this
/// leaves generous margin above normal jitter while still catching a hard
/// power-off well before a human would otherwise notice.
const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(20);

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

/// Minimum km/h by which the pre-pause speed must exceed the resumed speed to
/// bother restoring — avoids a redundant Control Point write (and a misleading
/// toast) when the machine did not actually slow down on resume.
const SPEED_RESTORE_EPSILON_KMH: f32 = 0.05;

/// How much of the speed history just before a pause to ignore when estimating
/// the walking ("cruising") speed to restore. The belt ramps itself down to a
/// stop over a couple of seconds when paused (measured ~2-3s on the W2 Pro;
/// margin to 10s), so those trailing samples are the deceleration, not the
/// speed the operator was actually walking at. Without this we would capture
/// the ~0.6 km/h tail instead of the real 2.5 (see задача 012 follow-up).
const SPEED_CRUISE_DECEL_SKIP: Duration = Duration::from_secs(10);

/// Samples slower than this are ramp/idle, not walking, and are excluded from
/// the cruising estimate (the belt minimum sits around 0.5 km/h).
const SPEED_CRUISE_FLOOR_KMH: f32 = 0.8;

/// Ceiling on the resumed belt speed for applying the computed default at a
/// workout start (задача 016): apply only when the belt is at/below the device's
/// factory crawl (~0.5), i.e. it just (re)started/reset and sits at its useless
/// default. A belt already moving faster means the operator chose that speed (or
/// a daemon restart landed mid-walk) — never override it. Same value as the
/// cruise floor: below it is not real walking.
const DEFAULT_SPEED_APPLY_CEILING_KMH: f32 = 0.8;

/// How long to retain recent speed samples for the cruising estimate. Covers
/// the decel-skip plus a useful averaging window before it; older samples are
/// pruned every telemetry tick so the buffer stays tiny.
const SPEED_HISTORY_RETENTION: Duration = Duration::from_secs(45);

/// Exit code used when the watchdog kills the process on a detected hang —
/// distinct from panics/normal errors so `launchctl print` / log forensics
/// can tell watchdog restarts apart.
const WATCHDOG_EXIT_CODE: i32 = 86;

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

/// After a *failed* idle-belt auto-pause write (задача 020), how long to wait
/// before retrying while the operator is still away. Long enough not to hammer a
/// wedged Control Point every telemetry sample (~1/s), short enough that a
/// transient BLE glitch does not leave the belt running idle for the whole away
/// spell. A *successful* pause is one-shot per spell (no cooldown needed).
const AUTO_PAUSE_RETRY_COOLDOWN: Duration = Duration::from_secs(15);

/// How long after a successful CLI `tm speed` Zone Hold must not write belt
/// speed (задача 039). In-memory only — not persisted across daemon restarts.
const OPERATOR_OVERRIDE_WINDOW: Duration = Duration::from_secs(60);

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

/// Pure gate: Zone Hold speed writes are suppressed while operator override
/// window is open (задача 039).
fn operator_override_active(now: Instant, until: Option<Instant>) -> bool {
    until.is_some_and(|u| now < u)
}

/// How long to wait for the next Heart Rate Measurement notification before
/// treating the strap as removed/lost (задача 025). A worn H10 sends samples
/// ~1/s, same cadence as the treadmill's own telemetry; generous margin above
/// jitter while still catching a removed strap quickly.
///
/// Must be an **absolute** `sleep_until` deadline in `select!` (задача 035), not
/// a relative `timeout` around `stream.next()` — sibling arms (command_tick 1s,
/// treadmill frames) rebuild every arm and would reset a relative timeout forever
/// (same class as задача 031 for treadmill telemetry).
const HR_NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Max age of `last_bpm` before Zone Hold treats bpm as absent (задача 035
/// defense-in-depth). Matches widget/`tm status` HR freshness (15s): the control
/// path must not feed a frozen bpm into speed corrections when notify has gone
/// silent but the BLE link is still up. Slightly above [`HR_NOTIFICATION_TIMEOUT`]
/// so silence-detection reconnects first when the absolute deadline works; this
/// gate still protects if that path lags.
const ZH_BPM_MAX_AGE: Duration = Duration::from_secs(15);

/// How often the daemon retries finding/connecting an HR sensor while one
/// isn't currently linked (no strap worn, or the last link was lost). Coarser
/// than the treadmill's own reconnect: an HR sensor absence is the common case
/// (not everyone wears the strap every walk), so this must not spam scans.
const HR_RECONNECT_INTERVAL: Duration = Duration::from_secs(30);

/// How often to check whether it's time to re-read the HR sensor's battery
/// level (задача 026) — a cheap in-memory elapsed-time check, same pattern as
/// `CONFIG_RELOAD_INTERVAL`'s mtime check. The actual re-read cadence is
/// [`hr_battery_poll_interval`]; this just bounds how promptly a newly-crossed
/// threshold is noticed.
const HR_BATTERY_CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Default re-read cadence for the HR sensor's battery level. Deliberately
/// coarse: the percentage barely moves hour to hour, and (unlike e.g. the
/// treadmill's own telemetry) re-reading more often would not meaningfully
/// extend the sensor's battery life either — a single-byte GATT read is
/// negligible next to the H10's ~400h battery budget. This is purely about
/// not doing pointless work, not about conserving the sensor.
const HR_BATTERY_POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Tighter re-read cadence once the last known level is at/below
/// [`HR_BATTERY_LOW_THRESHOLD_PCT`] — so a "getting low" warning doesn't sit
/// stale for a whole hour while the strap approaches empty.
const HR_BATTERY_POLL_INTERVAL_LOW: Duration = Duration::from_secs(30 * 60);

/// Battery level at/below which [`HR_BATTERY_POLL_INTERVAL_LOW`] applies, and
/// (in the tmux widget's own presentation logic) a low-battery glyph is shown.
const HR_BATTERY_LOW_THRESHOLD_PCT: u8 = 20;

/// Minimum gap between repeated Zone Hold safety-cap writes (задача 027) — the
/// bpm condition is checked every telemetry sample (~1/s) for responsiveness,
/// but the actual force-reduce/stop write is throttled so a sustained
/// over-threshold HR does not hammer the Control Point every second. Shorter
/// than the normal [`ZoneHoldConfig::correction_interval_seconds`] cadence
/// (a safety condition must not wait the full closed-loop interval).
const ZONE_HOLD_SAFETY_COOLDOWN: Duration = Duration::from_secs(5);

/// HRmax-percent above which, once already at `min_speed`, Zone Hold stops the
/// belt outright instead of merely force-reducing (task doc §Safety: "ниже
/// AHA-85% ... консервативно для unsupervised low-intensity").
const ZONE_HOLD_HARD_STOP_PERCENT: f32 = 85.0;

/// How often to re-read the HR sensor's battery level, given the last known
/// level (`None` = never read yet, so due immediately). Pure/unit-tested —
/// see module docs on why this is about avoiding pointless work, not battery
/// conservation.
fn hr_battery_poll_interval(last_known_pct: Option<u8>) -> Duration {
    match last_known_pct {
        Some(pct) if pct <= HR_BATTERY_LOW_THRESHOLD_PCT => HR_BATTERY_POLL_INTERVAL_LOW,
        _ => HR_BATTERY_POLL_INTERVAL,
    }
}

/// The two hot-reloadable config values, bundled so they thread through the
/// session loop as one `&mut`: they share a lifecycle — loaded together in
/// [`run`], reloaded together on the `config.json` mtime watch, and snapshotted
/// together for `tm status` (задачи 017/020/022). Keeps the config as one
/// cohesive unit rather than two parallel parameters.
struct LiveConfig {
    goals: Vec<Goal>,
    auto_pause: Option<Duration>,
    /// Zone Hold config (задача 027), reloaded on the same mtime watch as
    /// `goals`/`auto_pause` — they share the one config file.
    zone_hold: zone_hold::ZoneHoldConfig,
}

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
                        state.persist(&store, &watchdog)?;
                    }
                }
            }
            continue;
        }

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
                state.persist(&store, &watchdog)?;
            }
            result = scan::connect_treadmill(adapter) => {
                match result {
                    Ok(peripheral) => {
                        notify::treadmill_found();
                        state.connected = true;
                        state.last_connected_at = Some(Utc::now().to_rfc3339());
                        state.persist(&store, &watchdog)?;

                        if let Err(err) = stream_with_presence(
                            adapter,
                            &peripheral,
                            &mut power_events,
                            &mut store,
                            &mut state,
                            &watchdog,
                            &mut on_ac,
                            &mut live_config,
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
                        warn!(%err, "treadmill not found this cycle, retrying");
                        sleep(RETRY_DELAY).await;
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
    // When the current away/pause spell began, for the return toasts (задача
    // 010). `Instant` on macOS does not advance across sleep, but a session
    // that sleeps mid-away drops the BLE link and re-enters here fresh anyway.
    let mut away_since: Option<Instant> = None;
    let mut paused_since: Option<Instant> = None;
    // Idle-belt auto-pause (задача 020): the threshold lives in `config.auto_pause`
    // (loaded in `run()`, hot-reloaded on the mtime watch below), `None` when
    // disabled. `auto_pause_fired` is whether we already paused the current away
    // spell (reset when a fresh spell begins); `auto_pause_last_attempt` gates
    // retries after a failed write by `AUTO_PAUSE_RETRY_COOLDOWN`.
    let mut auto_pause_fired = false;
    let mut auto_pause_last_attempt: Option<Instant> = None;
    // Recent (timestamp, belt speed) samples, used to estimate the walking
    // ("cruising") speed to restore on resume — snapshotted when a pause begins.
    // The machine resets the belt to a crawl (~0.5 km/h) after a pause, and it
    // also ramps *itself* down over a couple of seconds before the pause, so we
    // cannot just take the last non-zero sample (that is the decel tail). See
    // `cruising_speed` (задача 012 follow-up). `last_walking_speed` is the plain
    // last-non-zero fallback for a session too short to have a cruising window.
    let mut speed_history: VecDeque<(Instant, f32)> = VecDeque::new();
    let mut last_walking_speed: Option<f32> = None;
    let mut pre_pause_speed: Option<f32> = None;
    // Whether the computed default speed has already been applied (or attempted)
    // this session (задача 016). Fresh per BLE session, so each physical
    // (re)start of the treadmill gets one attempt; reconnect/new session resets it.
    let mut default_speed_applied = false;
    // Zone Hold (задача 027): per-session controller phase + correction
    // timers. Fresh per BLE session, same reasoning as `default_speed_applied` —
    // a reconnect starts from `Off` and re-engages on the next Walking entry.
    let mut zh_phase = ZoneHoldPhase::Off;
    let mut zh_last_correction_at: Option<Instant> = None;
    let mut zh_last_safety_write_at: Option<Instant> = None;
    // After CLI `tm speed`, suppress zone speed writes for this window (задача 039).
    let mut operator_override_until: Option<Instant> = None;
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
    let mut hr_notifications: Option<HrNotificationStream> = None;
    let mut hr_connect_in_flight = true;
    spawn_hr_connect_attempt(adapter.clone(), hr_tx.clone());
    let mut hr_reconnect_tick = tokio::time::interval(HR_RECONNECT_INTERVAL);
    // Battery level (задача 026): last known percentage + when it was read,
    // so the adaptive re-read cadence (`hr_battery_poll_interval`) can decide
    // whether a fresh read is due. Reset alongside the rest of the HR state
    // whenever the link is lost — a stale percentage from a removed strap
    // must not linger in `daemon_status`.
    let mut hr_battery_pct: Option<u8> = None;
    let mut hr_battery_last_read: Option<Instant> = None;
    let mut hr_battery_check_tick = tokio::time::interval(HR_BATTERY_CHECK_INTERVAL);
    // Contact state, tracked separately from the BLE link (задача 033): a strap
    // taken off keeps notifying with a frozen bpm, so the link says "connected"
    // while the reading is meaningless. The tracker is per-link — reset with the
    // rest of the HR state whenever the link dies.
    let mut hr_contact_tracker = hr::ContactTracker::default();
    let mut hr_contact = hr::Contact::Live;

    // Deadline anchor for the telemetry-silence arm below (задача 031); advanced
    // only by a decoded `0x2ACD` frame, never by HR/control/config activity.
    let mut last_telemetry_at = tokio::time::Instant::now();
    // Absolute HR-silence deadline (задача 035), advanced only by a received
    // `0x2A37` frame or when a new HR stream is installed — never by sibling arms.
    let mut last_hr_at = tokio::time::Instant::now();

    loop {
        tokio::select! {
            biased;
            event = power_events.recv() => {
                match event {
                    Some(PowerEvent::AcPowerChanged(new_on_ac)) => {
                        *on_ac = new_on_ac;
                        state.set_power_mode(new_on_ac);
                        state.persist(store, watchdog)?;
                    }
                    Some(PowerEvent::WillSleep) => {
                        info!("system will sleep — active session left connected, BLE will drop on its own if it doesn't survive");
                        state.persist(store, watchdog)?;
                    }
                    Some(PowerEvent::DidWake) => {
                        info!("system woke while connected — active session unaffected");
                        state.persist(store, watchdog)?;
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
            _ = tokio::time::sleep_until(last_telemetry_at + NOTIFICATION_TIMEOUT) => {
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
                        store.insert_status_event(session_id, ts_ms, event_code, &notification.value)?;
                    } else {
                        warn!("empty Fitness Machine Status frame");
                    }
                    state.persist(store, watchdog)?;
                    continue;
                }
                if notification.uuid != ftms::TREADMILL_DATA {
                    continue;
                }
                let Some(data) = ftms::parse_treadmill_data(&notification.value) else {
                    warn!(bytes = ?notification.value, "undecodable treadmill frame");
                    continue;
                };
                last_telemetry_at = tokio::time::Instant::now();
                watchdog.touch_telemetry();
                logger.log(&data)?;
                store.insert_raw_sample(session_id, Utc::now().timestamp_millis(), &data, &notification.value)?;

                let deltas = store.advance_baseline(data.steps, data.total_distance_m, data.elapsed_s)?;

                // Record the belt speed so it can be restored after a pause (the
                // machine resets the belt to a crawl on resume — задача 012).
                // Keep a short rolling history for the cruising estimate, plus the
                // plain last-non-zero value as a fallback.
                if let Some(speed) = data.speed_kmh {
                    // Live speed snapshot for `tm widget` (задача 029) — every
                    // sample, unconditionally (unlike `last_walking_speed`
                    // below, which only tracks non-zero cruising speed).
                    state.last_speed_kmh = Some(speed as f64);
                    state.last_speed_ts = Some(Utc::now().timestamp_millis());

                    let now = Instant::now();
                    speed_history.push_back((now, speed));
                    while let Some(&(t, _)) = speed_history.front() {
                        if now.saturating_duration_since(t) > SPEED_HISTORY_RETENTION {
                            speed_history.pop_front();
                        } else {
                            break;
                        }
                    }
                    if speed > 0.0 {
                        last_walking_speed = Some(speed);
                    }
                }

                let prev_state = accumulator.state();
                if let Some(next_state) = accumulator.observe(Instant::now(), data.speed_kmh, data.steps) {
                    info!(?prev_state, ?next_state, "presence transition");
                    state.presence_state = Some(format!("{next_state:?}"));
                    // Belt speed as Zone Hold should see it below: starts as this
                    // sample's raw telemetry, but a restore/default-speed write in
                    // this very match (below) lands *after* that sample was taken —
                    // update it whenever one of those writes actually fires, so a
                    // fresh Ramp doesn't start from the pre-write crawl it just left.
                    let mut zh_effective_speed_kmh = data.speed_kmh.unwrap_or(0.0);
                    match next_state {
                        PresenceState::AwayWhileRunning => {
                            away_since = Some(Instant::now());
                            // Arm a fresh auto-pause spell (задача 020): each new
                            // absence gets its own threshold countdown and one
                            // guaranteed attempt, independent of prior spells.
                            auto_pause_fired = false;
                            auto_pause_last_attempt = None;
                            notify::walker_away();
                        }
                        PresenceState::Walking if prev_state == PresenceState::AwayWhileRunning => {
                            notify::walker_resumed(away_duration(away_since.take()));
                        }
                        PresenceState::Walking if prev_state == PresenceState::Paused => {
                            let paused_for = paused_since.take().map(|since| since.elapsed());
                            let resumed_speed = data.speed_kmh.unwrap_or(0.0);
                            match pre_pause_speed.take() {
                                // A real captured walking speed → restore it (задача 012).
                                Some(pre) => {
                                    let restore = try_restore_speed(peripheral, Some(pre), resumed_speed).await;
                                    if let Some(r) = &restore {
                                        zh_effective_speed_kmh = r.to_kmh;
                                    }
                                    notify::treadmill_resumed(paused_for, restore);
                                }
                                // Nothing to restore → this is a fresh start/reset at the
                                // device crawl (scenarios 2 & 3, задача 016): apply the
                                // computed default. Only toasts when it actually applied.
                                None => match try_apply_default_speed(peripheral, store, resumed_speed, &mut default_speed_applied).await {
                                    Some(applied) => {
                                        zh_effective_speed_kmh = applied;
                                        notify::default_speed_applied(resumed_speed, applied);
                                    }
                                    None => notify::treadmill_resumed(paused_for, None),
                                },
                            }
                        }
                        // Connected with the belt already moving (scenario 1, задача 016).
                        // Apply the computed default only if the belt is at its device
                        // crawl (guarded inside `try_apply_default_speed`).
                        PresenceState::Walking if prev_state == PresenceState::Unknown => {
                            let resumed_speed = data.speed_kmh.unwrap_or(0.0);
                            if let Some(applied) =
                                try_apply_default_speed(peripheral, store, resumed_speed, &mut default_speed_applied).await
                            {
                                zh_effective_speed_kmh = applied;
                                notify::default_speed_applied(resumed_speed, applied);
                            }
                        }
                        // Skip the very first sample after connecting: PresenceState
                        // starts Unknown, so a treadmill discovered already stopped
                        // must not immediately toast "paused".
                        PresenceState::Paused if prev_state != PresenceState::Unknown => {
                            paused_since = Some(Instant::now());
                            // Estimate the walking speed from before the belt began
                            // ramping down, not the decel tail; fall back to the
                            // last non-zero sample for a too-short session.
                            pre_pause_speed =
                                cruising_speed(speed_history.make_contiguous(), Instant::now()).or(last_walking_speed);
                            // Suppress the generic "Paused" toast when this pause
                            // is our own auto-pause: the belt going to 0 after our
                            // Stop transitions AwayWhileRunning→Paused, and the
                            // auto-pause toast already told the operator why (020).
                            if !auto_pause_fired {
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
                    // Use `zh_effective_speed_kmh`, not the raw sample: a fresh
                    // Ramp (first arrival at Walking) engages in the same match
                    // above that may have just written a default/restored speed —
                    // the raw telemetry sample still reflects the pre-write crawl.
                    let zh_resumed_kmh = zh_effective_speed_kmh;
                    let zh_default_kmh = default_speed::compute_default_speed(store, goals::load_workout_gap_minutes())
                        .ok()
                        .flatten()
                        .map(|d| d.kmh)
                        .unwrap_or(config.zone_hold.min_speed_kmh);
                    zone_hold_on_transition(
                        &mut zh_phase,
                        prev_state,
                        next_state,
                        &config.zone_hold,
                        zh_resumed_kmh,
                        zh_default_kmh,
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
                    let away_for = away_duration(away_since).unwrap_or_default();
                    let since_last_attempt = auto_pause_last_attempt.map(|t| t.elapsed());
                    if auto_pause_due(config.auto_pause, away_for, auto_pause_fired, since_last_attempt) {
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
                                auto_pause_fired = true;
                                notify::auto_paused(away_for);
                            }
                            Ok(Err(err)) => {
                                warn!(%err, "auto-pause Control Point write failed — retrying after cooldown");
                                auto_pause_last_attempt = Some(Instant::now());
                            }
                            Err(_) => {
                                warn!(
                                    timeout_s = SPEED_RESTORE_TIMEOUT.as_secs(),
                                    "auto-pause timed out (possible CoreBluetooth hang) — retrying after cooldown"
                                );
                                auto_pause_last_attempt = Some(Instant::now());
                            }
                        }
                    }
                }

                // Zone Hold closed-loop correction (задача 027). Checked every
                // sample like auto-pause above: ramp/hold/grace timers all need
                // polling, not just transition edges. A lost/removed HR sensor
                // (`!state.hr_connected`) feeds `None` bpm through, which the
                // ramp phase ignores anyway and the hold phase treats as
                // "nothing to correct on" — the freeze behaviour the task doc
                // asks for, without a separate code path.
                if should_run_zone_hold(config.zone_hold.enabled, &zh_phase) {
                    match config.zone_hold.resolve_target_zone() {
                        Some(resolved) => {
                            let zh_bpm = zh_bpm_if_fresh(
                                state.hr_connected,
                                state.last_bpm,
                                state.last_bpm_ts,
                                Utc::now().timestamp_millis(),
                                ZH_BPM_MAX_AGE,
                            );
                            zone_hold_tick(
                                peripheral,
                                &config.zone_hold,
                                resolved,
                                &mut zh_phase,
                                data.speed_kmh,
                                zh_bpm,
                                &mut zh_last_correction_at,
                                &mut zh_last_safety_write_at,
                                Instant::now(),
                                state,
                                operator_override_until,
                            )
                            .await;
                        }
                        None => {
                            // Config edited mid-session (e.g. age removed) —
                            // nothing left to compute a target zone from.
                            warn!("zone_hold: target zone no longer resolvable — disengaging");
                            disengage_zone_hold(&mut zh_phase, state);
                        }
                    }
                } else if zh_phase != ZoneHoldPhase::Off || state.zone_hold_active {
                    // Disabled in config while a phase was still live, or simply
                    // a stale snapshot left behind — park both (задача 032).
                    disengage_zone_hold(&mut zh_phase, state);
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
                state.persist(store, watchdog)?;
                // Primary control-command check: telemetry arrives ~1/s while
                // connected, so this bounds command latency to ≤1s during an
                // active session (задача 013). The interval arm below is only a
                // backstop for quiet stretches.
                if process_control_commands(peripheral, store).await? {
                    operator_override_until = Some(Instant::now() + OPERATOR_OVERRIDE_WINDOW);
                }
            }
            _ = command_tick.tick() => {
                if process_control_commands(peripheral, store).await? {
                    operator_override_until = Some(Instant::now() + OPERATOR_OVERRIDE_WINDOW);
                }
            }
            _ = config_tick.tick() => {
                // Reload config only when config.json actually changed on disk —
                // one cheap `stat`, re-read/re-log only on a real edit (задача 017).
                let now_mtime = goals::config_mtime();
                if now_mtime != goals_mtime {
                    goals_mtime = now_mtime;
                    let reloaded = goals::load_goals();
                    if reloaded != config.goals {
                        info!(goals = ?reloaded, "goals config changed on disk — reloaded without a daemon restart");
                        config.goals = reloaded;
                    }
                    // Same mtime gate reloads the idle-belt auto-pause threshold
                    // (задача 020) — an edit takes effect without a restart.
                    let reloaded_auto_pause = goals::load_auto_pause();
                    if reloaded_auto_pause != config.auto_pause {
                        info!(?reloaded_auto_pause, "auto-pause threshold changed on disk — reloaded without a daemon restart");
                        config.auto_pause = reloaded_auto_pause;
                    }
                    // Same mtime gate reloads the Zone Hold config (задача 027) —
                    // an edit (e.g. `tm zone` writing new limits) takes effect
                    // without a restart.
                    let reloaded_zone_hold = zone_hold::load_zone_hold_config();
                    if reloaded_zone_hold != config.zone_hold {
                        info!(enabled = reloaded_zone_hold.enabled, "zone_hold config changed on disk — reloaded without a daemon restart");
                        config.zone_hold = reloaded_zone_hold;
                    }
                    // `tm zone off` mid-session must stop the belt corrections
                    // *now*, not at the next presence transition (задача 032):
                    // the phase outlives the config edit, and a live `Ramp` kept
                    // pulling the operator's manual speed back to its target.
                    if !config.zone_hold.enabled && zh_phase != ZoneHoldPhase::Off {
                        info!("zone hold: disabled in config — disengaging mid-session");
                        disengage_zone_hold(&mut zh_phase, state);
                    }
                    // `tm zone on` is routinely run mid-session (as here), not
                    // only before a walk starts. Without this, the phase stays
                    // `Off` until the next presence transition — which on a
                    // long session may never come — leaving "on (not currently
                    // engaged)" stuck for the rest of the workout. Engage the
                    // same way a fresh Unknown→Walking transition would.
                    if zh_phase == ZoneHoldPhase::Off && accumulator.state() == PresenceState::Walking {
                        let zh_resumed_kmh = last_walking_speed.unwrap_or(config.zone_hold.min_speed_kmh);
                        let zh_default_kmh = default_speed::compute_default_speed(store, goals::load_workout_gap_minutes())
                            .ok()
                            .flatten()
                            .map(|d| d.kmh)
                            .unwrap_or(config.zone_hold.min_speed_kmh);
                        zone_hold_on_transition(
                            &mut zh_phase,
                            PresenceState::Unknown,
                            PresenceState::Walking,
                            &config.zone_hold,
                            zh_resumed_kmh,
                            zh_default_kmh,
                            Instant::now(),
                        );
                    }
                    // Refresh the loaded-config snapshot + last-read time shown by
                    // `tm status` (задача 022): the file was actually re-read here.
                    state.set_config(&config.goals, config.auto_pause);
                    state.persist(store, watchdog)?;
                }
            }
            // A background connect attempt finished (задача 025). `NotFound`
            // is the routine case (no strap worn) — just let the reconnect
            // tick below try again later.
            outcome = hr_rx.recv() => {
                hr_connect_in_flight = false;
                match outcome {
                    Some(HrConnectOutcome::Connected(peripheral, stream, battery_pct)) => {
                        info!(?battery_pct, "HR sensor connected and streaming");
                        state.hr_connected = true;
                        // Fresh link ⇒ fresh contact evidence (задача 033).
                        hr_contact_tracker = hr::ContactTracker::default();
                        hr_contact = hr::Contact::Live;
                        hr_notifications = Some(stream);
                        // Give the first frame the full silence window (задача 035).
                        last_hr_at = tokio::time::Instant::now();
                        hr_peripheral = Some(peripheral);
                        hr_battery_pct = battery_pct;
                        hr_battery_last_read = Some(Instant::now());
                        state.hr_battery_pct = battery_pct.map(|p| p as i64);
                        state.persist(store, watchdog)?;
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
                        // Any frame advances the silence clock — including
                        // contact-Lost frozen bpm — so partial silence still
                        // uses the absolute deadline, not contact logic.
                        last_hr_at = tokio::time::Instant::now();
                        if let Some(m) = hr::parse_hr_measurement(&notification.value) {
                            // Contact, not link (задача 033). A strap off the
                            // body still notifies ~1/s with the last bpm frozen;
                            // storing those samples poisons `hr_summary_for`
                            // (a whole workout once read `♥ 111/111`).
                            let frame_ts_ms = Utc::now().timestamp_millis();
                            let contact = hr_contact_tracker.observe(&m, frame_ts_ms);
                            let changed = contact != hr_contact;
                            hr_contact = contact;
                            match contact {
                                hr::Contact::Live => {
                                    if changed {
                                        info!(bpm = m.bpm, "HR sensor contact regained");
                                    }
                                    let ts_ms = frame_ts_ms;
                                    store.insert_hr_sample(session_id, ts_ms, &m, &notification.value)?;
                                    state.hr_connected = true;
                                    state.last_bpm = Some(m.bpm as i64);
                                    state.last_bpm_ts = Some(ts_ms);
                                    state.persist(store, watchdog)?;
                                }
                                hr::Contact::Lost => {
                                    // Log the transition, not every frame: the
                                    // strap can sit on the desk for hours.
                                    if changed {
                                        warn!(
                                            frozen_bpm = m.bpm,
                                            "HR sensor lost skin contact — dropping samples, keeping the BLE link"
                                        );
                                        // The link stays up on purpose: putting
                                        // the strap back on recovers instantly,
                                        // with no 15s rescan. Battery (задача
                                        // 026) is a property of the link, so it
                                        // survives too.
                                        state.hr_connected = false;
                                        state.last_bpm = None;
                                        state.last_bpm_ts = None;
                                        state.persist(store, watchdog)?;
                                    }
                                }
                            }
                        }
                    }
                    Some(_) => {
                        // Non-HR characteristic; still counts as link activity.
                        last_hr_at = tokio::time::Instant::now();
                    }
                    None => {
                        warn!("HR notification stream ended — sensor likely removed");
                        clear_hr_link_state(
                            &mut hr_notifications,
                            &mut hr_contact_tracker,
                            &mut hr_contact,
                            &mut hr_battery_pct,
                            &mut hr_battery_last_read,
                            state,
                        );
                        state.persist(store, watchdog)?;
                        if let Some(p) = hr_peripheral.take() {
                            scan::disconnect_best_effort(&p).await;
                        }
                    }
                }
            }
            // Absolute HR silence deadline (задача 035) — same pattern as the
            // treadmill telemetry arm above (задача 031).
            _ = tokio::time::sleep_until(last_hr_at + HR_NOTIFICATION_TIMEOUT),
                if hr_notifications.is_some() => {
                warn!(
                    timeout_s = HR_NOTIFICATION_TIMEOUT.as_secs(),
                    "no HR telemetry received — treating sensor as removed"
                );
                clear_hr_link_state(
                    &mut hr_notifications,
                    &mut hr_contact_tracker,
                    &mut hr_contact,
                    &mut hr_battery_pct,
                    &mut hr_battery_last_read,
                    state,
                );
                state.persist(store, watchdog)?;
                if let Some(p) = hr_peripheral.take() {
                    scan::disconnect_best_effort(&p).await;
                }
            }
            // No HR link right now (never found, or just lost) — retry
            // periodically rather than hammering CoreBluetooth.
            _ = hr_reconnect_tick.tick(), if hr_notifications.is_none() && !hr_connect_in_flight => {
                hr_connect_in_flight = true;
                spawn_hr_connect_attempt(adapter.clone(), hr_tx.clone());
            }
            // Battery re-read (задача 026): a cheap tick that only acts once
            // the adaptive interval has actually elapsed. Bounded inline read
            // (like the treadmill's own Control Point writes) — fine to block
            // this loop briefly given how rarely it's due (≥30 min).
            _ = hr_battery_check_tick.tick(), if hr_peripheral.is_some() => {
                let due = hr_battery_last_read
                    .is_none_or(|since| since.elapsed() >= hr_battery_poll_interval(hr_battery_pct));
                if due {
                    let peripheral = hr_peripheral.as_ref().expect("guarded by hr_peripheral.is_some()");
                    let read = scan::read_hr_battery(peripheral).await;
                    hr_battery_last_read = Some(Instant::now());
                    if read.is_some() {
                        info!(battery_pct = ?read, "re-read HR sensor battery level");
                        hr_battery_pct = read;
                        state.hr_battery_pct = read.map(|p| p as i64);
                        state.persist(store, watchdog)?;
                    }
                    // A failed read keeps the last known percentage (better a
                    // slightly stale value than flashing to unknown), but still
                    // stamps `hr_battery_last_read` so a persistently failing
                    // sensor doesn't get hammered every tick.
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

/// True away span for the return toast: the belt was already running without a
/// step for [`presence::AWAY_THRESHOLD`] *before* the tracker confirmed the
/// absence (it flips only once that window elapses), so `away_since` is
/// back-dated by that much to report the honest "how long the belt ran while
/// I wasn't walking" the operator asked for (задача 010). `None` if we somehow
/// lost the start instant — the toast then omits the figure.
fn away_duration(away_since: Option<Instant>) -> Option<Duration> {
    away_since.map(|since| since.elapsed() + presence::AWAY_THRESHOLD)
}

/// Estimate the walking ("cruising") speed to restore on resume from recent
/// `(timestamp, speed)` samples, ignoring the deceleration tail in the last
/// [`SPEED_CRUISE_DECEL_SKIP`] before the pause and any sub-[`SPEED_CRUISE_FLOOR_KMH`]
/// ramp/idle samples. Returns the median of the qualifying "walking" samples;
/// if the session was too short to have any (everything is inside the decel
/// window), falls back to the fastest walking sample seen (the belt only ramps
/// *down* into a pause, so the peak is the cruising speed). `None` only when no
/// sample reached the floor at all — the caller then uses its own fallback.
/// Pure and unit-tested; the buffer plumbing lives in the daemon loop.
fn cruising_speed(samples: &[(Instant, f32)], pause_at: Instant) -> Option<f32> {
    let mut walking: Vec<f32> = samples
        .iter()
        .filter(|(t, kmh)| {
            *kmh >= SPEED_CRUISE_FLOOR_KMH
                && pause_at.saturating_duration_since(*t) >= SPEED_CRUISE_DECEL_SKIP
        })
        .map(|(_, kmh)| *kmh)
        .collect();

    if walking.is_empty() {
        // Too short for a cruising window — use the peak walking speed instead
        // of the decel tail (the belt only slows down going into a pause).
        return samples
            .iter()
            .map(|(_, kmh)| *kmh)
            .filter(|kmh| *kmh >= SPEED_CRUISE_FLOOR_KMH)
            .fold(None, |acc, kmh| Some(acc.map_or(kmh, |m: f32| m.max(kmh))));
    }

    walking.sort_by(|a, b| a.partial_cmp(b).expect("belt speeds are never NaN"));
    Some(walking[walking.len() / 2])
}

/// The pre-pause walking speed to re-send on resume, or `None` when there is
/// nothing worth restoring: the machine did not actually slow down (resumed at
/// the pre-pause speed or faster, within [`SPEED_RESTORE_EPSILON_KMH`]). Pure
/// and unit-tested — the BLE write lives in [`restore_speed`].
fn speed_restore_target(pre_pause_kmh: f32, resumed_kmh: f32) -> Option<f32> {
    (pre_pause_kmh > resumed_kmh + SPEED_RESTORE_EPSILON_KMH).then_some(pre_pause_kmh)
}

/// Whether to send an idle-belt auto-pause right now (задача 020). Pure so the
/// policy is unit-testable without a clock or BLE. `threshold` is `None` when
/// auto-pause is disabled; `away_for` is the honest belt-ran-idle duration (see
/// [`away_duration`]); `fired` is whether we already paused this away spell;
/// `since_last_attempt` is how long ago the last *failed* attempt was (`None`
/// if none yet), gating retries by [`AUTO_PAUSE_RETRY_COOLDOWN`] so a wedged
/// Control Point is not hammered every telemetry sample.
fn auto_pause_due(
    threshold: Option<Duration>,
    away_for: Duration,
    fired: bool,
    since_last_attempt: Option<Duration>,
) -> bool {
    let Some(threshold) = threshold else {
        return false; // disabled via config (auto_pause_minutes = 0)
    };
    if fired {
        return false; // already paused for this away spell
    }
    if away_for < threshold {
        return false; // not idle long enough yet
    }
    match since_last_attempt {
        // Cooling down after a failed write — don't retry every ~1s sample.
        Some(elapsed) if elapsed < AUTO_PAUSE_RETRY_COOLDOWN => false,
        _ => true,
    }
}

/// Best-effort restore of the pre-pause belt speed on a pause→walk resume
/// (задача 012, Task D). Returns the applied restore for the toast, or `None`
/// (with a WARN on the abnormal paths) when nothing was applied — a missing
/// captured speed, a no-op, or a failed/timed-out Control Point write must all
/// leave the session running, never crash it.
async fn try_restore_speed(
    peripheral: &Peripheral,
    pre_pause: Option<f32>,
    resumed_kmh: f32,
) -> Option<notify::SpeedRestore> {
    let Some(pre_pause) = pre_pause else {
        // Daemon started already paused, or the pause preceded any walking.
        warn!("resume without a captured pre-pause speed — skipping speed restore");
        return None;
    };
    let target = speed_restore_target(pre_pause, resumed_kmh)?;
    let source = ControlSource::Restore;

    match tokio::time::timeout(SPEED_RESTORE_TIMEOUT, restore_speed(peripheral, target)).await {
        Ok(Ok(())) => {
            info!(
                from = resumed_kmh,
                to = target,
                control_source = source.as_str(),
                "restored pre-pause belt speed on resume"
            );
            Some(notify::SpeedRestore {
                from_kmh: resumed_kmh,
                to_kmh: target,
            })
        }
        Ok(Err(err)) => {
            warn!(%err, target, control_source = source.as_str(), "failed to restore pre-pause speed — leaving resume toast without the restore line");
            None
        }
        Err(_) => {
            warn!(
                timeout_s = SPEED_RESTORE_TIMEOUT.as_secs(),
                target,
                control_source = source.as_str(),
                "speed restore timed out (possible CoreBluetooth hang)"
            );
            None
        }
    }
}

/// Take FTMS control and set the target speed. Split from [`try_restore_speed`]
/// so the whole round-trip can be wrapped in one bounded `timeout` there.
async fn restore_speed(peripheral: &Peripheral, target_kmh: f32) -> Result<()> {
    let controller = Controller::take_control(peripheral).await?;
    controller.set_speed(target_kmh).await
}

/// Apply the computed default belt speed at a workout start (задача 016), when
/// there is no pre-pause speed to restore. Returns the applied km/h for the
/// toast, or `None` when nothing was applied. Guards, in order:
/// - once per session (`applied`) — one attempt per (re)start, no retry storm on
///   a presence flap at the crawl;
/// - the belt must be at/below the device crawl ([`DEFAULT_SPEED_APPLY_CEILING_KMH`])
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
    resumed_kmh: f32,
    applied: &mut bool,
) -> Option<f32> {
    if *applied {
        return None;
    }
    if resumed_kmh > DEFAULT_SPEED_APPLY_CEILING_KMH {
        // Belt already at a real speed — the operator's choice, or a mid-walk
        // reconnect. Not a fresh crawl start; leave it alone (and let a later
        // genuine crawl start still get its one attempt).
        return None;
    }

    let gap_minutes = goals::load_workout_gap_minutes();
    let target = match default_speed::compute_default_speed(store, gap_minutes) {
        Ok(Some(default)) => default.kmh,
        Ok(None) => {
            info!(
                "no qualifying prior workout (≥30m walking) — leaving belt at its device default speed"
            );
            // Nothing to apply; don't recompute on every Walking flap this session.
            *applied = true;
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
    *applied = true;
    let source = ControlSource::DefaultSpeed;
    match tokio::time::timeout(SPEED_RESTORE_TIMEOUT, restore_speed(peripheral, target)).await {
        Ok(Ok(())) => {
            info!(
                from = resumed_kmh,
                to = target,
                control_source = source.as_str(),
                "applied computed default belt speed at workout start"
            );
            Some(target)
        }
        Ok(Err(err)) => {
            warn!(%err, target, control_source = source.as_str(), "failed to apply default belt speed at workout start — leaving belt as is");
            None
        }
        Err(_) => {
            warn!(
                timeout_s = SPEED_RESTORE_TIMEOUT.as_secs(),
                target,
                control_source = source.as_str(),
                "default belt speed write timed out (possible CoreBluetooth hang)"
            );
            None
        }
    }
}

/// Zone Hold controller phase for the current session (задача 027) — mirrors
/// the task doc's life-cycle: `Ramp` (linear warm-up, HR ignored) → `Hold`
/// (closed-loop correction), with `Frozen`/`Grace` bracketing any excursion
/// off the belt (task doc §Сход с ленты). `Off` covers both "disabled in
/// config" and "config incomplete" (no resolvable target zone) — same
/// no-op-degrade stance as a removed HR sensor (задача 025).
#[derive(Debug, Clone, Copy, PartialEq)]
enum ZoneHoldPhase {
    Off,
    Ramp {
        started_at: Instant,
        start_speed_kmh: f32,
        target_speed_kmh: f32,
    },
    Hold,
    /// Presence left `Walking` — HR is fully ignored (task doc: a dropping
    /// pulse while stepping away must never look like "let's speed up").
    Frozen,
    /// Just returned to `Walking` — no corrections until `until` elapses.
    Grace {
        until: Instant,
    },
}

/// Whether Zone Hold may touch the Control Point on this tick (задача 032).
///
/// A live phase is *not* enough: `tm zone off` lands mid-session through the
/// `config.toml` mtime watch (задача 017), and until the next presence
/// transition the phase still says `Ramp`/`Hold`. Both conditions must hold, so
/// a disabled Zone Hold can never drive the belt.
fn should_run_zone_hold(enabled: bool, phase: &ZoneHoldPhase) -> bool {
    enabled && *phase != ZoneHoldPhase::Off
}

/// Bpm for Zone Hold only when the HR link is live *and* the last sample is
/// fresh enough (задача 035). Pure: wall-clock age is injected so unit tests
/// need no BLE. Stale or missing → `None` (controller freezes corrections).
fn zh_bpm_if_fresh(
    hr_connected: bool,
    last_bpm: Option<i64>,
    last_bpm_ts_ms: Option<i64>,
    now_ms: i64,
    max_age: Duration,
) -> Option<u16> {
    if !hr_connected {
        return None;
    }
    let bpm = last_bpm?;
    let ts = last_bpm_ts_ms?;
    let age_ms = now_ms.saturating_sub(ts);
    if age_ms > max_age.as_millis() as i64 {
        return None;
    }
    u16::try_from(bpm).ok()
}

/// Tear down HR *link*-scoped state after stream end or silence (задача 035).
/// Clears bpm snapshot so `hr_connected=false` always implies `last_bpm=None`.
/// Contact-Lost keeps the BLE stream and battery — that path does not call this.
fn clear_hr_link_state(
    hr_notifications: &mut Option<HrNotificationStream>,
    hr_contact_tracker: &mut hr::ContactTracker,
    hr_contact: &mut hr::Contact,
    hr_battery_pct: &mut Option<u8>,
    hr_battery_last_read: &mut Option<Instant>,
    state: &mut DaemonState,
) {
    *hr_notifications = None;
    state.hr_connected = false;
    state.last_bpm = None;
    state.last_bpm_ts = None;
    *hr_contact_tracker = hr::ContactTracker::default();
    *hr_contact = hr::Contact::Live;
    *hr_battery_pct = None;
    *hr_battery_last_read = None;
    state.hr_battery_pct = None;
}

/// Park Zone Hold: phase to `Off` and clear the `tm status`/widget snapshot
/// (задача 032). Shared by every disengage path — config disabled mid-session,
/// target zone no longer resolvable — so none of them can forget a field.
fn disengage_zone_hold(phase: &mut ZoneHoldPhase, state: &mut DaemonState) {
    *phase = ZoneHoldPhase::Off;
    state.zone_hold_active = false;
    state.zone_hold_phase = Some("off".to_string());
    state.zone_hold_position = None;
    state.zone_hold_target_lo = None;
    state.zone_hold_target_hi = None;
    state.zone_hold_last_speed = None;
}

impl ZoneHoldPhase {
    fn label(&self) -> &'static str {
        match self {
            ZoneHoldPhase::Off => "off",
            ZoneHoldPhase::Ramp { .. } => "ramp",
            ZoneHoldPhase::Hold => "hold",
            ZoneHoldPhase::Frozen => "frozen",
            ZoneHoldPhase::Grace { .. } => "grace",
        }
    }
}

/// Engage/freeze/grace Zone Hold on a presence transition (задача 027,
/// §Жизненный цикл + §Сход с ленты). Pure decision over the phase enum — the
/// actual speed corrections happen on the following telemetry ticks via
/// [`zone_hold_tick`], keeping this transition step free of BLE.
///
/// `resumed_kmh` is the belt speed observed on this very sample (the ramp's
/// starting point); `default_kmh` is the operator's computed cruising pace
/// (задача 016) clamped into the configured range — the ramp's destination.
fn zone_hold_on_transition(
    phase: &mut ZoneHoldPhase,
    prev_state: PresenceState,
    next_state: PresenceState,
    config: &zone_hold::ZoneHoldConfig,
    resumed_kmh: f32,
    default_kmh: f32,
    now: Instant,
) {
    if !config.enabled || config.resolve_target_zone().is_none() {
        *phase = ZoneHoldPhase::Off;
        return;
    }
    match (prev_state, next_state) {
        // Any first arrival at Walking while still Off engages a fresh warm-up
        // ramp — not just Unknown→Walking. A freshly (re)connected session
        // often observes Unknown→Paused (belt not moving yet on the first
        // sample) before Paused→Walking (first steps), so gating strictly on
        // `prev_state == Unknown` left Zone Hold permanently stuck at `Off`
        // for the rest of that session (the periodic config-reload catch-up
        // only re-engages on an actual on-disk edit, not as a self-heal poll).
        (_, PresenceState::Walking) if *phase == ZoneHoldPhase::Off => {
            let target = default_kmh.clamp(config.min_speed_kmh, config.max_speed_kmh);
            *phase = ZoneHoldPhase::Ramp {
                started_at: now,
                start_speed_kmh: resumed_kmh,
                target_speed_kmh: target,
            };
            info!(target, "zone hold: engaged, starting warm-up ramp");
        }
        (PresenceState::Paused, PresenceState::Walking)
        | (PresenceState::AwayWhileRunning, PresenceState::Walking) => {
            let grace = Duration::from_secs(config.reentry_grace_seconds as u64);
            *phase = ZoneHoldPhase::Grace { until: now + grace };
            info!(
                grace_s = config.reentry_grace_seconds,
                "zone hold: returned to walking, grace period before corrections resume"
            );
        }
        (PresenceState::Walking, PresenceState::Paused)
        | (PresenceState::Walking, PresenceState::AwayWhileRunning)
            if *phase != ZoneHoldPhase::Off =>
        {
            *phase = ZoneHoldPhase::Frozen;
            info!("zone hold: left the belt — freezing (HR ignored until return)");
        }
        _ => {}
    }
}

/// One telemetry-tick's worth of Zone Hold processing (задача 027): advances
/// ramp/grace timers, runs the safety-cap check, and — when due — computes and
/// applies one closed-loop correction. Called every treadmill sample while
/// `phase != Off`; a disabled/unconfigured Zone Hold never reaches this (see
/// the call site), so it costs nothing on the hot path.
///
/// All BLE writes reuse the same bounded [`restore_speed`]/
/// [`SPEED_RESTORE_TIMEOUT`] path as the rest of the daemon (задачи 007/012) —
/// a failed/timed-out write is logged and swallowed, never tears down the
/// session.
#[allow(clippy::too_many_arguments)]
async fn zone_hold_tick(
    peripheral: &Peripheral,
    config: &zone_hold::ZoneHoldConfig,
    resolved: zone_hold::ResolvedZone,
    phase: &mut ZoneHoldPhase,
    measured_speed_kmh: Option<f32>,
    bpm: Option<u16>,
    last_correction_at: &mut Option<Instant>,
    last_safety_write_at: &mut Option<Instant>,
    now: Instant,
    state: &mut DaemonState,
    operator_override_until: Option<Instant>,
) {
    // A function that writes to the belt owns the check of its own enable flag
    // (задача 032). The call site gates on this too; both stay, so no future
    // path can reach a Control Point write with Zone Hold switched off.
    if !config.enabled {
        return;
    }

    // Instantaneous Speed is absent from a Treadmill Data frame only when
    // FTMS's "More Data" bit splits it across two notifications (legal per
    // spec, see `ftms.rs`) — rare but real. Guessing 0.0 here would read as
    // "belt stopped" and could yank a live, merely-mid-flight speed down to
    // `min_speed_kmh`. Skip this single tick instead; the next sample (well
    // inside one correction interval) has it.
    let Some(measured_speed_kmh) = measured_speed_kmh else {
        return;
    };

    let zone_writes_suppressed = operator_override_active(now, operator_override_until);

    let correction_interval = Duration::from_secs(config.correction_interval_seconds as u64);
    let correction_due = |last: Option<Instant>| {
        last.is_none_or(|t| now.saturating_duration_since(t) >= correction_interval)
    };

    match *phase {
        ZoneHoldPhase::Off | ZoneHoldPhase::Frozen => {}
        ZoneHoldPhase::Grace { until } => {
            if now >= until {
                *phase = ZoneHoldPhase::Hold;
                info!("zone hold: grace period elapsed — resuming closed-loop correction");
            }
        }
        ZoneHoldPhase::Ramp {
            started_at,
            start_speed_kmh,
            target_speed_kmh,
        } => {
            let elapsed = now.saturating_duration_since(started_at);
            let warmup = Duration::from_secs(config.warmup_minutes as u64 * 60);
            if elapsed >= warmup {
                *phase = ZoneHoldPhase::Hold;
                info!("zone hold: warm-up ramp complete — starting closed-loop correction");
            } else if correction_due(*last_correction_at) {
                let target = zone_hold::warmup_target_speed(
                    start_speed_kmh,
                    target_speed_kmh,
                    elapsed,
                    warmup,
                );
                if (target - measured_speed_kmh).abs() > SPEED_RESTORE_EPSILON_KMH {
                    apply_zone_hold_speed(peripheral, target, zone_writes_suppressed).await;
                }
                *last_correction_at = Some(now);
            }
        }
        ZoneHoldPhase::Hold => {
            if let (Some(bpm), Some(safety_cap)) = (bpm, config.safety_cap_bpm())
                && bpm > safety_cap
            {
                let cooling_down = last_safety_write_at
                    .is_some_and(|t| now.saturating_duration_since(t) < ZONE_HOLD_SAFETY_COOLDOWN);
                if !cooling_down {
                    *last_safety_write_at = Some(now);
                    let hard_stop = config
                        .hrmax()
                        .map(|hrmax| zone_hold::safety_cap_bpm(hrmax, ZONE_HOLD_HARD_STOP_PERCENT))
                        .unwrap_or(u16::MAX);
                    if measured_speed_kmh <= config.min_speed_kmh + SPEED_RESTORE_EPSILON_KMH
                        && bpm > hard_stop
                    {
                        warn!(
                            bpm,
                            safety_cap,
                            hard_stop,
                            "zone hold: safety cap exceeded at min speed — stopping belt"
                        );
                        // Hard-stop is safety — not suppressed by operator override.
                        let _ = tokio::time::timeout(
                            SPEED_RESTORE_TIMEOUT,
                            execute_control_command(
                                peripheral,
                                ControlCommand::Stop,
                                ControlSource::Zone,
                            ),
                        )
                        .await;
                    } else if let Some(target) = zone_hold::safety_force_reduce_target(
                        measured_speed_kmh,
                        config.max_step_kmh,
                        config.min_speed_kmh,
                    ) {
                        warn!(
                            bpm,
                            safety_cap,
                            target,
                            "zone hold: safety cap exceeded — force-reducing speed"
                        );
                        apply_zone_hold_speed(peripheral, target, zone_writes_suppressed).await;
                    }
                    // else: already at min within deadband — no write (задача 041).
                }
                zh_persist_snapshot(state, phase, &resolved, Some(bpm), measured_speed_kmh);
                return;
            }
            if correction_due(*last_correction_at) {
                if let Some(bpm) = bpm {
                    let params = zone_hold::ControllerParams {
                        tracking: config.tracking,
                        zone_low_bpm: resolved.low_bpm,
                        zone_high_bpm: resolved.high_bpm,
                        deadband_bpm: config.deadband_bpm,
                        max_step_kmh: config.max_step_kmh,
                        min_speed_kmh: config.min_speed_kmh,
                        max_speed_kmh: resolved.effective_max_speed_kmh,
                    };
                    if let Some(target) = zone_hold::next_speed(&params, measured_speed_kmh, bpm) {
                        apply_zone_hold_speed(peripheral, target, zone_writes_suppressed).await;
                    }
                }
                // No bpm this tick (sensor stale/lost) — nothing to correct on;
                // still stamp the interval so a reconnect doesn't immediately
                // fire a correction from a now-outdated baseline.
                *last_correction_at = Some(now);
            }
        }
    }

    zh_persist_snapshot(state, phase, &resolved, bpm, measured_speed_kmh);
}

/// Mirror the controller's current phase/target/position into the persisted
/// `daemon_status` snapshot (задача 027) — same "daemon publishes what it just
/// decided" pattern as the rest of [`DaemonState`]. `zone_hold_position` is
/// only set while `Hold` is actually classifying a live bpm — everywhere else
/// (ramp/frozen/grace/off) it is cleared, matching the task doc's widget
/// contract ("красим только когда Zone Hold реально управляет").
fn zh_persist_snapshot(
    state: &mut DaemonState,
    phase: &ZoneHoldPhase,
    resolved: &zone_hold::ResolvedZone,
    bpm: Option<u16>,
    measured_speed_kmh: f32,
) {
    state.zone_hold_active = !matches!(phase, ZoneHoldPhase::Off);
    state.zone_hold_phase = Some(phase.label().to_string());
    state.zone_hold_target_lo = Some(resolved.low_bpm as i64);
    state.zone_hold_target_hi = Some(resolved.high_bpm as i64);
    state.zone_hold_last_speed = Some(measured_speed_kmh as f64);
    state.zone_hold_position = match (phase, bpm) {
        (ZoneHoldPhase::Hold, Some(bpm)) => Some(
            zone_hold::classify_position(bpm, resolved.low_bpm, resolved.high_bpm)
                .wire()
                .to_string(),
        ),
        _ => None,
    };
}

/// Apply one Zone Hold speed correction, reusing the bounded
/// [`restore_speed`]/[`SPEED_RESTORE_TIMEOUT`] round-trip (задачи 007/012). A
/// failed/timed-out write is logged, not propagated — the same "never tear
/// down the session over a convenience write" rule as `try_restore_speed`/
/// `try_apply_default_speed`. When `suppressed` (operator override window,
/// задача 039), skip the write and log once at this call site.
async fn apply_zone_hold_speed(peripheral: &Peripheral, target_kmh: f32, suppressed: bool) {
    let source = ControlSource::Zone;
    if suppressed {
        info!(
            target = target_kmh,
            control_source = source.as_str(),
            "zone hold: suppressed, operator override active"
        );
        return;
    }
    match tokio::time::timeout(SPEED_RESTORE_TIMEOUT, restore_speed(peripheral, target_kmh)).await {
        Ok(Ok(())) => info!(
            target = target_kmh,
            control_source = source.as_str(),
            "zone hold: applied speed correction"
        ),
        Ok(Err(err)) => {
            warn!(
                %err,
                target = target_kmh,
                control_source = source.as_str(),
                "zone hold: speed correction write failed"
            )
        }
        Err(_) => warn!(
            timeout_s = SPEED_RESTORE_TIMEOUT.as_secs(),
            target = target_kmh,
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
struct DaemonState {
    connected: bool,
    presence_state: Option<String>,
    last_connected_at: Option<String>,
    last_disconnected_at: Option<String>,
    power_mode: &'static str,
    power_mode_since: DateTime<Utc>,
    // Snapshot of the config the daemon currently holds, surfaced by `tm status`
    // (задача 022): comma-joined goals, auto-pause threshold in seconds (`None` =
    // disabled), and when the config file was last read. Updated by `set_config`
    // at startup and on each mtime-triggered reload.
    config_goals: Option<String>,
    config_auto_pause_secs: Option<i64>,
    config_loaded_at: Option<String>,
    // Heart-rate snapshot (задача 025) — same reasoning as the rest of this
    // struct: mirrors what the daemon just observed so `tm status`/`widget`/
    // `stats` can read it without racing the daemon for BLE.
    hr_connected: bool,
    last_bpm: Option<i64>,
    last_bpm_ts: Option<i64>,
    /// HR sensor battery level, 0-100% (задача 026). `None` until read at
    /// least once this link.
    hr_battery_pct: Option<i64>,
    /// Zone Hold snapshot (задача 027) — mirrors `ZoneHoldPhase`/the resolved
    /// target zone so `tm status`/`tm widget` can read it without racing the
    /// daemon for BLE. See `zh_persist_snapshot`.
    zone_hold_active: bool,
    zone_hold_target_lo: Option<i64>,
    zone_hold_target_hi: Option<i64>,
    zone_hold_last_speed: Option<f64>,
    zone_hold_phase: Option<String>,
    zone_hold_position: Option<String>,
    /// Live belt-speed snapshot (задача 029) — updated on every telemetry
    /// sample regardless of Zone Hold, same reasoning as `last_bpm`/
    /// `last_bpm_ts` above. `last_speed_ts` is Unix millis.
    last_speed_kmh: Option<f64>,
    last_speed_ts: Option<i64>,
}

impl DaemonState {
    fn new(on_ac: bool) -> Self {
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
        let last_telemetry_at = tokio::time::Instant::now();
        let mut sibling = tokio::time::interval(Duration::from_secs(1));
        let mut ticks = 0u32;

        let fired = loop {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(last_telemetry_at + NOTIFICATION_TIMEOUT) => break true,
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
            last_telemetry_at.elapsed(),
            NOTIFICATION_TIMEOUT,
            "deadline must land exactly at the timeout, not drift with the sibling"
        );
    }

    /// Same class as `telemetry_deadline_fires_despite_a_faster_sibling_arm`
    /// (задача 031) for the HR link (задача 035): relative `timeout` around
    /// `stream.next()` never ages while a 1s sibling completes every pass.
    #[tokio::test(start_paused = true)]
    async fn hr_silence_deadline_fires_despite_a_faster_sibling_arm() {
        let last_hr_at = tokio::time::Instant::now();
        let mut sibling = tokio::time::interval(Duration::from_secs(1));
        let mut ticks = 0u32;

        let fired = loop {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(last_hr_at + HR_NOTIFICATION_TIMEOUT) => break true,
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
            last_hr_at.elapsed(),
            HR_NOTIFICATION_TIMEOUT,
            "HR deadline must land exactly at the timeout, not drift with the sibling"
        );
    }

    #[test]
    fn operator_override_active_within_window() {
        let now = Instant::now();
        assert!(!operator_override_active(now, None));
        assert!(operator_override_active(now, Some(now + Duration::from_secs(30))));
        assert!(!operator_override_active(
            now + Duration::from_secs(61),
            Some(now + Duration::from_secs(60))
        ));
    }

    #[test]
    fn zh_bpm_if_fresh_requires_link_and_recent_sample() {
        let max = Duration::from_secs(15);
        let now = 1_000_000_i64;
        // Happy path.
        assert_eq!(
            zh_bpm_if_fresh(true, Some(120), Some(now - 5_000), now, max),
            Some(120)
        );
        // Not connected → None even with a recent ts.
        assert_eq!(
            zh_bpm_if_fresh(false, Some(120), Some(now - 1_000), now, max),
            None
        );
        // Stale (older than max) → None (partial GATT death defense).
        assert_eq!(
            zh_bpm_if_fresh(true, Some(111), Some(now - 16_000), now, max),
            None
        );
        // Missing ts or bpm → None.
        assert_eq!(zh_bpm_if_fresh(true, Some(120), None, now, max), None);
        assert_eq!(zh_bpm_if_fresh(true, None, Some(now), now, max), None);
    }

    #[test]
    fn speed_restore_target_restores_only_a_real_slowdown() {
        // Typical case: paused at 2.5, machine resumed at 0.5 → restore 2.5.
        assert_eq!(speed_restore_target(2.5, 0.5), Some(2.5));
        // No slowdown (resumed at the same speed) → nothing to restore.
        assert_eq!(speed_restore_target(2.5, 2.5), None);
        // Resumed faster than before → nothing to restore.
        assert_eq!(speed_restore_target(2.5, 3.0), None);
        // Within epsilon → treated as no change.
        assert_eq!(speed_restore_target(2.5, 2.48), None);
    }

    #[test]
    fn cruising_speed_ignores_the_deceleration_tail() {
        // arr — steady 2.5 walk, then the belt ramps itself down over the last
        // ~3s into the pause (the real W2 Pro pattern from the logs).
        let pause = Instant::now();
        let mut samples: Vec<(Instant, f32)> = Vec::new();
        for secs_ago in 11..=40 {
            samples.push((pause - Duration::from_secs(secs_ago), 2.5)); // cruising, before decel
        }
        samples.push((pause - Duration::from_secs(3), 1.8)); // decel tail — inside the skip window
        samples.push((pause - Duration::from_secs(2), 1.0));
        samples.push((pause - Duration::from_secs(1), 0.6));

        // act / assert — the tail is excluded, so we get the real walking speed.
        assert_eq!(cruising_speed(&samples, pause), Some(2.5));
    }

    #[test]
    fn cruising_speed_takes_the_median_of_varied_walking() {
        let pause = Instant::now();
        let samples = [
            (pause - Duration::from_secs(30), 2.0),
            (pause - Duration::from_secs(25), 2.5),
            (pause - Duration::from_secs(20), 3.0),
        ];
        assert_eq!(cruising_speed(&samples, pause), Some(2.5));
    }

    #[test]
    fn cruising_speed_falls_back_to_peak_for_a_short_session() {
        // Every sample is inside the decel-skip window (walked only ~5s), so the
        // median path finds nothing and we take the fastest walking sample seen —
        // never the decel tail.
        let pause = Instant::now();
        let samples = [
            (pause - Duration::from_secs(5), 2.5),
            (pause - Duration::from_secs(2), 1.2),
            (pause - Duration::from_secs(1), 0.6),
        ];
        assert_eq!(cruising_speed(&samples, pause), Some(2.5));
    }

    #[test]
    fn cruising_speed_is_none_without_any_walking_sample() {
        // Belt never got above the floor (pure idle/ramp) → caller must fall back.
        let pause = Instant::now();
        let samples = [
            (pause - Duration::from_secs(20), 0.5),
            (pause - Duration::from_secs(2), 0.6),
        ];
        assert_eq!(cruising_speed(&samples, pause), None);
    }

    #[test]
    fn auto_pause_due_is_off_when_disabled_or_already_fired() {
        let away = Duration::from_secs(600);
        // Disabled (no threshold) → never fires regardless of how long away.
        assert!(!auto_pause_due(None, away, false, None));
        // Already paused this spell → never re-fires until a fresh spell resets it.
        assert!(!auto_pause_due(
            Some(Duration::from_secs(300)),
            away,
            true,
            None
        ));
    }

    #[test]
    fn auto_pause_due_waits_for_threshold_then_fires() {
        let threshold = Some(Duration::from_secs(300));
        // Below threshold → not yet.
        assert!(!auto_pause_due(
            threshold,
            Duration::from_secs(299),
            false,
            None
        ));
        // At the threshold with no prior attempt → fire.
        assert!(auto_pause_due(
            threshold,
            Duration::from_secs(300),
            false,
            None
        ));
    }

    #[test]
    fn auto_pause_due_cooldown_gates_retries_after_a_failure() {
        let threshold = Some(Duration::from_secs(300));
        let away = Duration::from_secs(400);
        // A failed attempt 5s ago is inside the cooldown → wait.
        assert!(!auto_pause_due(
            threshold,
            away,
            false,
            Some(Duration::from_secs(5))
        ));
        // Past the cooldown → retry.
        assert!(auto_pause_due(
            threshold,
            away,
            false,
            Some(AUTO_PAUSE_RETRY_COOLDOWN + Duration::from_secs(1))
        ));
    }

    #[test]
    fn away_duration_adds_the_confirmation_window() {
        // None start instant → no figure.
        assert_eq!(away_duration(None), None);
        // The reported span includes AWAY_THRESHOLD (the pre-confirmation gap),
        // so it is always at least that long.
        let reported = away_duration(Some(Instant::now())).expect("some");
        assert!(reported >= presence::AWAY_THRESHOLD);
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

    #[test]
    fn hr_battery_poll_interval_is_generous_when_unknown_or_healthy() {
        assert_eq!(hr_battery_poll_interval(None), HR_BATTERY_POLL_INTERVAL);
        assert_eq!(
            hr_battery_poll_interval(Some(100)),
            HR_BATTERY_POLL_INTERVAL
        );
        assert_eq!(
            hr_battery_poll_interval(Some(HR_BATTERY_LOW_THRESHOLD_PCT + 1)),
            HR_BATTERY_POLL_INTERVAL
        );
    }

    #[test]
    fn hr_battery_poll_interval_tightens_at_and_below_the_low_threshold() {
        assert_eq!(
            hr_battery_poll_interval(Some(HR_BATTERY_LOW_THRESHOLD_PCT)),
            HR_BATTERY_POLL_INTERVAL_LOW
        );
        assert_eq!(
            hr_battery_poll_interval(Some(0)),
            HR_BATTERY_POLL_INTERVAL_LOW
        );
    }

    /// A live phase alone must never authorise a belt write: `tm zone off`
    /// reaches the daemon as a config reload, not as a phase change (задача 032).
    #[test]
    fn should_run_zone_hold_requires_both_enabled_and_a_live_phase() {
        let live = ZoneHoldPhase::Hold;
        assert!(should_run_zone_hold(true, &live));
        assert!(!should_run_zone_hold(false, &live));
        assert!(!should_run_zone_hold(true, &ZoneHoldPhase::Off));
        assert!(!should_run_zone_hold(false, &ZoneHoldPhase::Off));
    }

    #[test]
    fn disengage_zone_hold_parks_the_phase_and_clears_the_whole_snapshot() {
        let mut state = DaemonState::new(true);
        state.zone_hold_active = true;
        state.zone_hold_phase = Some("ramp".to_string());
        state.zone_hold_position = Some("below".to_string());
        state.zone_hold_target_lo = Some(90);
        state.zone_hold_target_hi = Some(110);
        state.zone_hold_last_speed = Some(3.0);
        let mut phase = ZoneHoldPhase::Ramp {
            started_at: Instant::now(),
            start_speed_kmh: 2.5,
            target_speed_kmh: 3.0,
        };

        disengage_zone_hold(&mut phase, &mut state);

        assert_eq!(phase, ZoneHoldPhase::Off);
        assert!(!state.zone_hold_active);
        assert_eq!(state.zone_hold_phase.as_deref(), Some("off"));
        assert_eq!(state.zone_hold_position, None);
        assert_eq!(state.zone_hold_target_lo, None);
        assert_eq!(state.zone_hold_target_hi, None);
        assert_eq!(state.zone_hold_last_speed, None);
    }

    /// The disabled-config path in `zone_hold_on_transition` is the *third*
    /// guard; this pins the contract it has always promised.
    #[test]
    fn zone_hold_on_transition_never_engages_while_disabled() {
        let mut phase = ZoneHoldPhase::Hold;
        let config = zone_hold::ZoneHoldConfig::disabled_default();
        zone_hold_on_transition(
            &mut phase,
            PresenceState::Paused,
            PresenceState::Walking,
            &config,
            2.5,
            3.0,
            Instant::now(),
        );
        assert_eq!(phase, ZoneHoldPhase::Off);
    }
}
