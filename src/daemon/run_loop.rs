//! Outer daemon loop: power-idle, scan/connect, scan-wedge recovery (задача 051).

use std::time::Duration;

use anyhow::{Result, anyhow};
use btleplug::api::{Central, CentralState};
use btleplug::platform::Adapter;
use chrono::Utc;
use tokio::time::sleep;
use tracing::{error, info, warn};

use super::session::stream_with_presence;
use super::state::{DaemonState, persist_daemon_status};
use super::watchdog::Watchdog;
use crate::config_apply::LiveConfig;
use crate::goals;
use crate::notify;
use crate::power::{self, PowerEvent};
use crate::scan;
use crate::store::Store;
use crate::zone_hold;

/// Delay before retrying discovery after a scan/connect failure, so a
/// transient Bluetooth hiccup does not spin the CPU in a tight loop.
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// How often the idle loop refreshes `daemon_status.updated_at` (and touches the
/// watchdog) while nothing else is happening, so a quiet-but-healthy stretch
/// never looks stale to `status`. Coarse — this is just a heartbeat, not where
/// the real work happens.
const PERSIST_TICK_INTERVAL: Duration = Duration::from_secs(30);

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

#[cfg(test)]
mod tests {
    use super::*;

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
            formatted.contains("run_loop.rs"),
            "expected this source file in {formatted}"
        );

        assert_eq!(panic_location_message(None), "<unknown>");
    }
}
