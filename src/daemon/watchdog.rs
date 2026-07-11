//! Independent hang watchdog for the daemon process (задачи D/007/018/031).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tracing::error;

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

/// Exit code used when the watchdog kills the process on a detected hang —
/// distinct from panics/normal errors so `launchctl print` / log forensics
/// can tell watchdog restarts apart.
const WATCHDOG_EXIT_CODE: i32 = 86;

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
pub(super) struct Watchdog {
    /// Fixed anchor all touch timestamps are measured against.
    pub(super) anchor: Instant,
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
    pub(super) fn new() -> Self {
        Self {
            anchor: Instant::now(),
            last_touch_ms: Arc::new(AtomicU64::new(0)),
            last_telemetry_ms: Arc::new(AtomicU64::new(0)),
            streaming: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn touch(&self) {
        self.store_now(&self.last_touch_ms);
    }

    /// Record a treadmill telemetry frame (`0x2ACD`). The only thing the tight
    /// [`STREAMING_STALE_THRESHOLD`] is allowed to watch — see `last_telemetry_ms`.
    pub(super) fn touch_telemetry(&self) {
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
    pub(super) fn set_streaming(&self, streaming: bool) {
        self.streaming.store(streaming, Ordering::Relaxed);
    }

    /// The staleness threshold for the current phase: tight while streaming
    /// (a connected treadmill is never silent that long), generous otherwise
    /// (scan/connect/teardown have legitimately long gaps).
    pub(super) fn stale_threshold(&self) -> Duration {
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
    pub(super) fn is_stale_at(&self, elapsed_since_anchor: Duration) -> bool {
        let last = Duration::from_millis(self.last_progress_ms());
        elapsed_since_anchor.saturating_sub(last) > self.stale_threshold()
    }

    /// Start the independent monitor task. On detected staleness it logs an
    /// `ERROR` and exits the whole process with [`WATCHDOG_EXIT_CODE`] so
    /// launchd (`KeepAlive=true`) restarts the daemon cleanly.
    pub(super) fn spawn_monitor(&self) {
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
    use super::*;

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
}
