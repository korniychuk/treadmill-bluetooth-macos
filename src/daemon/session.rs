//! Connected-session select! loop: thin wiring over session structs (задача 053).

use std::time::Instant;

use anyhow::{Result, anyhow};
use btleplug::api::Peripheral as _;
use btleplug::platform::{Adapter, Peripheral};
use chrono::{Local, Utc};
use futures::StreamExt;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{error, info, warn};

use super::SPEED_RESTORE_TIMEOUT;
use super::commands::{
    CONTROL_POLL_INTERVAL, ControlSource, execute_control_command, process_control_commands,
};
use super::config::{CONFIG_RELOAD_INTERVAL, execute_config_effects};
use super::hr::{
    HR_BATTERY_CHECK_INTERVAL, HR_RECONNECT_INTERVAL, HrConnectOutcome, HrNotificationStream,
    spawn_hr_connect_attempt,
};
use super::speed::{try_apply_default_speed, try_restore_speed};
use super::state::{DaemonState, persist_daemon_status, tolerate_db_write};
use super::watchdog::Watchdog;
use super::zone_write::execute_zone_write;

use crate::activity::ActivityAccumulator;
use crate::auto_pause::AutoPause;
use crate::config_apply::{self, LiveConfig};
use crate::control_command::ControlCommand;
use crate::default_speed;
use crate::ftms;
use crate::goals::{self, Goal};
use crate::hr;
use crate::hr_session::{HR_NOTIFICATION_TIMEOUT, HrFrameAction, HrReconnect, HrSession};
use crate::logger::WorkoutLogger;
use crate::notify;
use crate::power::PowerEvent;
use crate::presence::PresenceState;
use crate::scan;
use crate::speed::CentiKmh;
use crate::store::Store;
use crate::treadmill_link::{NOTIFICATION_TIMEOUT, TreadmillLink};
use crate::zone_session::{self, ZH_BPM_MAX_AGE, ZoneSession};

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
pub(super) async fn stream_with_presence(
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
