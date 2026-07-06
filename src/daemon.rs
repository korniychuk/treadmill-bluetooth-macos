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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use btleplug::api::Peripheral as _;
use btleplug::platform::{Adapter, Peripheral};
use chrono::{DateTime, Local, Utc};
use futures::StreamExt;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::activity::ActivityAccumulator;
use crate::control::Controller;
use crate::control_command::{self, ControlCommand};
use crate::default_speed;
use crate::ftms;
use crate::goals::{self, Goal};
use crate::logger::WorkoutLogger;
use crate::notify;
use crate::power::{self, PowerEvent};
use crate::presence::{self, PresenceState};
use crate::scan;
use crate::store::{DaemonStatus, Store};

/// Delay before retrying discovery after a scan/connect failure, so a
/// transient Bluetooth hiccup does not spin the CPU in a tight loop.
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// How long to wait for the next Treadmill Data sample before treating the
/// link as lost. The device streams ~1/s even while stationary, so this
/// leaves generous margin above normal jitter while still catching a hard
/// power-off well before a human would otherwise notice.
const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(20);

/// How often the standalone watchdog task checks for staleness. Coarser than
/// the scan cycle since this is just a liveness check, not where the real
/// work happens.
const WATCHDOG_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// How stale the watchdog's last touch may get before we treat it as a
/// silent hang and exit for launchd to restart us (задача 007). Generous
/// margin above the worst-case legitimate gap (a full 15s scan cycle plus
/// two 10s bounded CoreBluetooth calls plus the 5s retry delay) so normal
/// latency never trips it, while a genuine hang (2026-07-04/05 incidents:
/// silent for 10+ hours / 79+ minutes) is caught in ~2 minutes. `Instant`
/// on macOS does not advance during system sleep, so waking from a long
/// sleep cannot false-positive here.
const WATCHDOG_STALE_THRESHOLD: Duration = Duration::from_secs(120);

/// Tighter staleness threshold that applies only while telemetry is actively
/// streaming (задача 018). A connected treadmill sends a sample ~1/s, so silence
/// this long means the link is dead even though CoreBluetooth never fired a
/// disconnect (the fast power-cycle case: btleplug wedged in a blocking FFI call,
/// so even [`NOTIFICATION_TIMEOUT`] can't fire because the executor thread is
/// stuck). Above `NOTIFICATION_TIMEOUT` (20s) so the normal in-loop reconnect
/// still wins when the executor is *not* blocked; far below the general
/// [`WATCHDOG_STALE_THRESHOLD`], which must stay generous for the scan/connect
/// phase. Cuts the untracked window from ~133s (observed) to ~44s.
const STREAMING_STALE_THRESHOLD: Duration = Duration::from_secs(40);

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

/// Run the daemon forever: scan → connect → stream with presence tracking →
/// on disconnect, toast and go back to scanning. Reacts to power/sleep
/// events instead of polling — see module docs.
pub async fn run(adapter: &Adapter) -> Result<()> {
    let mut power_events = power::spawn_power_event_listener();
    let mut store = Store::open()?;
    // Loaded once here (not per session): config edits take effect on the next
    // daemon restart, matching the "edit the committed repo file" workflow.
    // Mutable so a live session can hot-reload it when goals.json changes on
    // disk, without a daemon restart (задача 017).
    let mut step_goals = goals::load_goals();
    info!(goals = ?step_goals, "loaded daily step goals");
    let watchdog = Watchdog::new();
    watchdog.spawn_monitor();
    // Refreshes `daemon_status.updated_at` (and the watchdog) while idle, so
    // quiet-but-healthy stretches never look like a hang to `status`.
    let mut persist_tick = tokio::time::interval(WATCHDOG_CHECK_INTERVAL);

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
                            &peripheral,
                            &mut power_events,
                            &mut store,
                            &mut state,
                            &watchdog,
                            &mut on_ac,
                            &mut step_goals,
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
async fn stream_with_presence(
    peripheral: &Peripheral,
    power_events: &mut UnboundedReceiver<PowerEvent>,
    store: &mut Store,
    state: &mut DaemonState,
    watchdog: &Watchdog,
    on_ac: &mut bool,
    step_goals: &mut Vec<Goal>,
) -> Result<()> {
    scan::subscribe_treadmill_data(peripheral).await?;
    scan::subscribe_treadmill_status(peripheral).await?;
    // Bounded like every other CoreBluetooth call — see задача 007.
    let mut notifications = tokio::time::timeout(scan::CONNECT_TIMEOUT, peripheral.notifications())
        .await
        .map_err(|_| anyhow!("opening notification stream timed out (possible CoreBluetooth hang)"))??;

    // From here on telemetry should arrive ~1/s. Switch the watchdog to its
    // tight streaming threshold (задача 018) and reset the clock so the
    // (possibly slow) subscribe phase above doesn't count against it. `run()`
    // clears streaming the moment this function returns, by any path.
    watchdog.touch();
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
    // Backstop poll for queued control commands during quiet stretches; the
    // primary check runs at the end of each telemetry sample below (задача 013).
    let mut command_tick = tokio::time::interval(CONTROL_POLL_INTERVAL);
    // Hot-reload of goals.json (задача 017): `None` forces the first tick to
    // reconcile against disk, so a config edited while the daemon was idle is
    // picked up at session start too.
    let mut config_tick = tokio::time::interval(CONFIG_RELOAD_INTERVAL);
    let mut goals_mtime: Option<std::time::SystemTime> = None;

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
            notification = tokio::time::timeout(NOTIFICATION_TIMEOUT, notifications.next()) => {
                let notification = match notification {
                    Ok(Some(notification)) => notification,
                    Ok(None) => break, // stream closed cleanly (rare, but handle it)
                    Err(_) => {
                        warn!(timeout_s = NOTIFICATION_TIMEOUT.as_secs(), "no telemetry received — treating as disconnected");
                        break;
                    }
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
                logger.log(&data)?;
                store.insert_raw_sample(session_id, Utc::now().timestamp_millis(), &data, &notification.value)?;

                let deltas = store.advance_baseline(data.steps, data.total_distance_m, data.elapsed_s)?;

                // Record the belt speed so it can be restored after a pause (the
                // machine resets the belt to a crawl on resume — задача 012).
                // Keep a short rolling history for the cruising estimate, plus the
                // plain last-non-zero value as a fallback.
                if let Some(speed) = data.speed_kmh {
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
                    match next_state {
                        PresenceState::AwayWhileRunning => {
                            away_since = Some(Instant::now());
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
                                    notify::treadmill_resumed(paused_for, restore);
                                }
                                // Nothing to restore → this is a fresh start/reset at the
                                // device crawl (scenarios 2 & 3, задача 016): apply the
                                // computed default. Only toasts when it actually applied.
                                None => match try_apply_default_speed(peripheral, store, resumed_speed, &mut default_speed_applied).await {
                                    Some(applied) => notify::default_speed_applied(resumed_speed, applied),
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
                            notify::treadmill_paused();
                        }
                        _ => {}
                    }
                    // The open segment is closed inside `accumulator.observe`
                    // on any transition leaving Walking (Paused/AwayWhileRunning):
                    // the next credited step opens a fresh one, and read-time
                    // merge regroups by gap (задача 014). No DB write.
                }

                // Credit this sample. `Utc::now()` matches the timestamp already
                // stored on the raw sample above (same loop iteration), which is
                // exactly what the offline replay feeds back — see `docs/tasks/015`.
                accumulator.credit(store, Utc::now(), deltas)?;
                // Daily totals can only have grown when a step was actually
                // credited, so gate the goal check on that to avoid a query
                // every idle second.
                if deltas.steps > 0 {
                    celebrate_reached_goals(store, step_goals)?;
                }
                state.persist(store, watchdog)?;
                // Primary control-command check: telemetry arrives ~1/s while
                // connected, so this bounds command latency to ≤1s during an
                // active session (задача 013). The interval arm below is only a
                // backstop for quiet stretches.
                process_control_commands(peripheral, store).await?;
            }
            _ = command_tick.tick() => {
                process_control_commands(peripheral, store).await?;
            }
            _ = config_tick.tick() => {
                // Reload goals only when goals.json actually changed on disk —
                // one cheap `stat`, re-read/re-log only on a real edit (задача 017).
                let now_mtime = goals::config_mtime();
                if now_mtime != goals_mtime {
                    goals_mtime = now_mtime;
                    let reloaded = goals::load_goals();
                    if reloaded != *step_goals {
                        info!(goals = ?reloaded, "goals config changed on disk — reloaded without a daemon restart");
                        *step_goals = reloaded;
                    }
                }
            }
        }
    }

    logger.finish();
    store.end_session()?;
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
            *kmh >= SPEED_CRUISE_FLOOR_KMH && pause_at.saturating_duration_since(*t) >= SPEED_CRUISE_DECEL_SKIP
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

/// Best-effort restore of the pre-pause belt speed on a pause→walk resume
/// (задача 012, Task D). Returns the applied restore for the toast, or `None`
/// (with a WARN on the abnormal paths) when nothing was applied — a missing
/// captured speed, a no-op, or a failed/timed-out Control Point write must all
/// leave the session running, never crash it.
async fn try_restore_speed(peripheral: &Peripheral, pre_pause: Option<f32>, resumed_kmh: f32) -> Option<notify::SpeedRestore> {
    let Some(pre_pause) = pre_pause else {
        // Daemon started already paused, or the pause preceded any walking.
        warn!("resume without a captured pre-pause speed — skipping speed restore");
        return None;
    };
    let target = speed_restore_target(pre_pause, resumed_kmh)?;

    match tokio::time::timeout(SPEED_RESTORE_TIMEOUT, restore_speed(peripheral, target)).await {
        Ok(Ok(())) => {
            info!(from = resumed_kmh, to = target, "restored pre-pause belt speed on resume");
            Some(notify::SpeedRestore { from_kmh: resumed_kmh, to_kmh: target })
        }
        Ok(Err(err)) => {
            warn!(%err, target, "failed to restore pre-pause speed — leaving resume toast without the restore line");
            None
        }
        Err(_) => {
            warn!(timeout_s = SPEED_RESTORE_TIMEOUT.as_secs(), target, "speed restore timed out (possible CoreBluetooth hang)");
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
            info!("no qualifying prior workout (≥30m walking) — leaving belt at its device default speed");
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
    match tokio::time::timeout(SPEED_RESTORE_TIMEOUT, restore_speed(peripheral, target)).await {
        Ok(Ok(())) => {
            info!(from = resumed_kmh, to = target, "applied computed default belt speed at workout start");
            Some(target)
        }
        Ok(Err(err)) => {
            warn!(%err, target, "failed to apply default belt speed at workout start — leaving belt as is");
            None
        }
        Err(_) => {
            warn!(
                timeout_s = SPEED_RESTORE_TIMEOUT.as_secs(),
                target, "default belt speed write timed out (possible CoreBluetooth hang)"
            );
            None
        }
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
    let already: std::collections::HashSet<i64> = store.celebrated_thresholds(&today)?.into_iter().collect();
    for goal in goals::thresholds_to_celebrate(today_steps, step_goals, &already) {
        info!(threshold = goal.threshold, tier = goal.tier, steps = today_steps, "daily step goal reached");
        notify::goal_reached(goal.threshold, goal.tier);
        store.mark_goal_celebrated(&today, goal.threshold)?;
    }
    Ok(())
}

/// Execute at most one pending control command on the live BLE link (задача
/// 013). Silent on the empty path — this runs ~1/s, so no happy-path log.
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
async fn process_control_commands(peripheral: &Peripheral, store: &Store) -> Result<()> {
    let Some(queued) = store.next_pending_control_command()? else {
        return Ok(());
    };

    if control_command::is_stale(queued.created_at, Utc::now()) {
        warn!(id = queued.id, command = %queued.command.to_wire(), "control command is stale — failing without executing");
        store.mark_control_command_failed(queued.id, "stale, not executed")?;
        return Ok(());
    }

    match tokio::time::timeout(SPEED_RESTORE_TIMEOUT, execute_control_command(peripheral, queued.command)).await {
        Ok(Ok(())) => {
            info!(id = queued.id, command = %queued.command.to_wire(), "executed queued control command");
            store.mark_control_command_done(queued.id)?;
        }
        Ok(Err(err)) => {
            warn!(%err, id = queued.id, command = %queued.command.to_wire(), "queued control command write failed");
            store.mark_control_command_failed(queued.id, &err.to_string())?;
        }
        Err(_) => {
            warn!(
                id = queued.id,
                timeout_s = SPEED_RESTORE_TIMEOUT.as_secs(),
                "queued control command timed out (possible CoreBluetooth hang)"
            );
            store.mark_control_command_failed(queued.id, "execution timed out (possible CoreBluetooth hang)")?;
        }
    }
    Ok(())
}

/// Take FTMS control and run one command. Split out so the whole round-trip can
/// be wrapped in a single bounded `timeout` by the caller. Reuses the same
/// take-control path as `restore_speed` and any other Control Point write (see
/// `control::Controller`).
async fn execute_control_command(peripheral: &Peripheral, command: ControlCommand) -> Result<()> {
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
        }
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
            streaming: Arc::new(AtomicBool::new(false)),
        }
    }

    fn touch(&self) {
        let elapsed_ms = u64::try_from(self.anchor.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_touch_ms.store(elapsed_ms, Ordering::Relaxed);
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

    /// Whether the last touch is older than the current-phase threshold
    /// ([`Self::stale_threshold`]), given the elapsed-since-anchor time. Split
    /// from `spawn_monitor` so the threshold logic is unit-testable without a
    /// runtime or real waiting.
    fn is_stale_at(&self, elapsed_since_anchor: Duration) -> bool {
        let last_touch = Duration::from_millis(self.last_touch_ms.load(Ordering::Relaxed));
        elapsed_since_anchor.saturating_sub(last_touch) > self.stale_threshold()
    }

    /// Start the independent monitor task. On detected staleness it logs an
    /// `ERROR` and exits the whole process with [`WATCHDOG_EXIT_CODE`] so
    /// launchd (`KeepAlive=true`) restarts the daemon cleanly.
    fn spawn_monitor(&self) {
        let anchor = self.anchor;
        let last_touch_ms = Arc::clone(&self.last_touch_ms);
        let streaming = Arc::clone(&self.streaming);
        let probe = Self { anchor, last_touch_ms, streaming };
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(WATCHDOG_CHECK_INTERVAL);
            loop {
                tick.tick().await;
                let elapsed = probe.anchor.elapsed();
                if probe.is_stale_at(elapsed) {
                    let last_touch = Duration::from_millis(probe.last_touch_ms.load(Ordering::Relaxed));
                    error!(
                        stale_s = (elapsed - last_touch).as_secs(),
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
        let samples = [(pause - Duration::from_secs(20), 0.5), (pause - Duration::from_secs(2), 0.6)];
        assert_eq!(cruising_speed(&samples, pause), None);
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
}
