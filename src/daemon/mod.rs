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

mod commands;
mod config;
mod hr;
mod run_loop;
mod session;
mod speed;
mod state;
mod watchdog;
mod zone_write;

pub use run_loop::run;
pub(crate) use state::DaemonState;
pub(crate) use watchdog::WATCHDOG_STALE_THRESHOLD;

use std::time::Duration;

/// Upper bound on the whole pause-resume speed-restore round-trip (take
/// control + set speed, задача 012). Every BLE await in it must be bounded —
/// the watchdog convention (задача 007) — so a wedged CoreBluetooth call here
/// cannot silently stall the stream. Well under [`WATCHDOG_STALE_THRESHOLD`]
/// so a slow-but-legitimate restore never trips the watchdog.
///
/// Also reused as the generic Control Point write budget (CLI queue, Zone Hold,
/// auto-pause Stop) — same bounded round-trip everywhere.
const SPEED_RESTORE_TIMEOUT: Duration = Duration::from_secs(15);
