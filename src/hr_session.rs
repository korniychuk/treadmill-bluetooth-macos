//! HR BLE-link + contact + battery session state (задача 025/026/033/035/042 / 053).
//!
//! Owns the HR silence clock, contact tracker (body contact ≠ BLE link), battery
//! poll cadence, and connect-attempt latch. Time is always injected.
//!
//! **`link_up` ↔ `hr_notifications` sync (shell contract):**
//! `link_up` flips only in [`HrSession::on_connected`] / [`HrSession::on_link_lost`].
//! The daemon shell must set/clear `hr_notifications` in those same branches —
//! the stream handle stays in the shell (BLE I/O), so the pair cannot be one type.

use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::daemon::DaemonState;
use crate::hr;

/// How long to wait for the next Heart Rate Measurement before treating the
/// strap as removed (задача 025). Absolute `sleep_until` only (задача 035).
pub(crate) const HR_NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Max time an HR connect attempt may stay in-flight before the latch is cleared
/// (задача 042).
const HR_CONNECT_ATTEMPT_DEADLINE: Duration = Duration::from_secs(60);

const HR_BATTERY_POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);
const HR_BATTERY_POLL_INTERVAL_LOW: Duration = Duration::from_secs(30 * 60);
const HR_BATTERY_LOW_THRESHOLD_PCT: u8 = 20;

/// What the shell should do with a parsed `0x2A37` frame after contact decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HrFrameAction {
    /// Contact Live: write `hr_sample` (snapshot already updated).
    Store { ts_ms: i64 },
    /// Contact Lost (stable or transition). `state_changed` ⇒ shell must persist.
    Drop { state_changed: bool },
}

/// Reconnect-tick decision (задача 042).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HrReconnect {
    /// In-flight attempt still within the deadline — do not spawn another.
    Skip,
    /// Spawn a fresh connect attempt (latch armed by this decision).
    Spawn,
}

/// HR link + contact + battery + connect-latch for one treadmill session.
#[derive(Debug)]
pub struct HrSession {
    /// Mirror of `hr_notifications.is_some()` — see module docs for sync contract.
    link_up: bool,
    last_hr_at: tokio::time::Instant,
    contact_tracker: hr::ContactTracker,
    contact: hr::Contact,
    battery_pct: Option<u8>,
    battery_last_read: Option<Instant>,
    connect_in_flight: bool,
    connect_started_at: Instant,
}

impl HrSession {
    /// Session start: a connect attempt is already spawned (shell).
    #[must_use]
    pub fn new_connecting(now: Instant, tokio_now: tokio::time::Instant) -> Self {
        Self {
            link_up: false,
            last_hr_at: tokio_now,
            contact_tracker: hr::ContactTracker::default(),
            contact: hr::Contact::Live,
            battery_pct: None,
            battery_last_read: None,
            connect_in_flight: true,
            connect_started_at: now,
        }
    }

    /// `HrConnectOutcome::Connected`: seed tracker/contact/battery, raise link.
    ///
    /// Shell must also set `hr_notifications = Some(stream)` in this branch.
    pub fn on_connected(
        &mut self,
        battery_pct: Option<u8>,
        now: Instant,
        tokio_now: tokio::time::Instant,
        state: &mut DaemonState,
    ) {
        self.link_up = true;
        self.contact_tracker = hr::ContactTracker::default();
        self.contact = hr::Contact::Live;
        self.last_hr_at = tokio_now;
        self.battery_pct = battery_pct;
        self.battery_last_read = Some(now);
        state.hr_connected = true;
        state.hr_battery_pct = battery_pct.map(i64::from);
    }

    /// Any connect outcome finished (Connected / NotFound / channel closed).
    pub fn on_connect_finished(&mut self) {
        self.connect_in_flight = false;
    }

    /// Parsed `0x2A37`: advance silence clock, run contact tracker, update snapshot.
    ///
    /// Invariant `hr_connected=false ⇒ last_bpm=None` is maintained here on
    /// Live→Lost (задача 033/035).
    pub fn on_frame(
        &mut self,
        m: &hr::HrMeasurement,
        ts_ms: i64,
        tokio_now: tokio::time::Instant,
        state: &mut DaemonState,
    ) -> HrFrameAction {
        self.last_hr_at = tokio_now;
        let contact = self.contact_tracker.observe(m, ts_ms);
        let changed = contact != self.contact;
        self.contact = contact;
        match contact {
            hr::Contact::Live => {
                if changed {
                    info!(bpm = m.bpm, "HR sensor contact regained");
                }
                state.hr_connected = true;
                state.last_bpm = Some(i64::from(m.bpm));
                state.last_bpm_ts = Some(ts_ms);
                HrFrameAction::Store { ts_ms }
            }
            hr::Contact::Lost => {
                if changed {
                    warn!(
                        frozen_bpm = m.bpm,
                        "HR sensor lost skin contact — dropping samples, keeping the BLE link"
                    );
                    // Link stays up: putting the strap back recovers without rescan.
                    // Battery is a link property — survives contact loss.
                    state.hr_connected = false;
                    state.last_bpm = None;
                    state.last_bpm_ts = None;
                }
                HrFrameAction::Drop {
                    state_changed: changed,
                }
            }
        }
    }

    /// Non-HR characteristic on the HR link, or undecodable frame: silence only.
    pub fn on_link_activity(&mut self, tokio_now: tokio::time::Instant) {
        self.last_hr_at = tokio_now;
    }

    /// Stream end / silence: tear down link-scoped state (not shell handles).
    ///
    /// Shell must also set `hr_notifications = None` and drop the peripheral.
    pub fn on_link_lost(&mut self, state: &mut DaemonState) {
        self.link_up = false;
        state.hr_connected = false;
        state.last_bpm = None;
        state.last_bpm_ts = None;
        self.contact_tracker = hr::ContactTracker::default();
        self.contact = hr::Contact::Live;
        self.battery_pct = None;
        self.battery_last_read = None;
        state.hr_battery_pct = None;
    }

    #[must_use]
    pub fn silence_deadline(&self) -> tokio::time::Instant {
        self.last_hr_at + HR_NOTIFICATION_TIMEOUT
    }

    #[must_use]
    pub fn link_up(&self) -> bool {
        self.link_up
    }

    /// Reconnect tick while `!link_up` (задача 042). Mutates latch on Spawn.
    pub fn reconnect_decision(&mut self, now: Instant) -> HrReconnect {
        if self.connect_in_flight {
            if now.saturating_duration_since(self.connect_started_at) <= HR_CONNECT_ATTEMPT_DEADLINE
            {
                return HrReconnect::Skip;
            }
            warn!(
                deadline_s = HR_CONNECT_ATTEMPT_DEADLINE.as_secs(),
                "HR connect attempt vanished — resetting latch"
            );
        }
        self.connect_in_flight = true;
        self.connect_started_at = now;
        HrReconnect::Spawn
    }

    /// Adaptive battery re-read due? (задача 026).
    #[must_use]
    pub fn battery_read_due(&self, now: Instant) -> bool {
        self.battery_last_read.is_none_or(|since| {
            now.saturating_duration_since(since) >= hr_battery_poll_interval(self.battery_pct)
        })
    }

    /// After a battery GATT read attempt: always stamp `last_read`; update pct
    /// only on success (failed read keeps last known).
    pub fn on_battery_read(
        &mut self,
        pct: Option<u8>,
        now: Instant,
        state: &mut DaemonState,
    ) {
        self.battery_last_read = Some(now);
        if let Some(p) = pct {
            self.battery_pct = Some(p);
            state.hr_battery_pct = Some(i64::from(p));
        }
    }
}

/// How often to re-read battery given last known level (`None` ⇒ due immediately).
pub(crate) fn hr_battery_poll_interval(last_known_pct: Option<u8>) -> Duration {
    match last_known_pct {
        Some(pct) if pct <= HR_BATTERY_LOW_THRESHOLD_PCT => HR_BATTERY_POLL_INTERVAL_LOW,
        _ => HR_BATTERY_POLL_INTERVAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_measurement(bpm: u16) -> hr::HrMeasurement {
        hr::HrMeasurement {
            bpm,
            contact: None,
            rr_ms: vec![800],
        }
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

    #[test]
    fn on_frame_live_stores_and_sets_snapshot() {
        let now = Instant::now();
        let tk = tokio::time::Instant::from_std(now);
        let mut hr = HrSession::new_connecting(now, tk);
        let mut state = DaemonState::new(true);
        hr.on_connected(Some(80), now, tk, &mut state);

        let m = live_measurement(120);
        let action = hr.on_frame(&m, 1_000, tk, &mut state);
        assert_eq!(action, HrFrameAction::Store { ts_ms: 1_000 });
        assert!(state.hr_connected);
        assert_eq!(state.last_bpm, Some(120));
        assert_eq!(state.last_bpm_ts, Some(1_000));
        assert_eq!(state.hr_battery_pct, Some(80));
    }

    #[test]
    fn on_frame_live_to_lost_clears_bpm_keeps_battery() {
        let now = Instant::now();
        let tk = tokio::time::Instant::from_std(now);
        let mut hr = HrSession::new_connecting(now, tk);
        let mut state = DaemonState::new(true);
        hr.on_connected(Some(80), now, tk, &mut state);

        // Seed Live snapshot.
        let m = live_measurement(111);
        let _ = hr.on_frame(&m, 1_000, tk, &mut state);

        // Explicit contact=false is Lost immediately (задача 033).
        let lost = hr::HrMeasurement {
            bpm: 111,
            contact: Some(false),
            rr_ms: vec![],
        };
        let action = hr.on_frame(&lost, 2_000, tk, &mut state);
        assert_eq!(action, HrFrameAction::Drop { state_changed: true });
        assert!(!state.hr_connected);
        assert!(state.last_bpm.is_none());
        assert!(state.last_bpm_ts.is_none());
        // Battery survives contact loss (link still up).
        assert_eq!(state.hr_battery_pct, Some(80));
        assert!(hr.link_up());
        // Invariant.
        assert!(
            state.hr_connected || state.last_bpm.is_none(),
            "hr_connected=false ⇒ last_bpm=None"
        );
    }

    #[test]
    fn on_link_lost_clears_everything_including_battery() {
        let now = Instant::now();
        let tk = tokio::time::Instant::from_std(now);
        let mut hr = HrSession::new_connecting(now, tk);
        let mut state = DaemonState::new(true);
        hr.on_connected(Some(80), now, tk, &mut state);
        let m = live_measurement(120);
        let _ = hr.on_frame(&m, 1_000, tk, &mut state);

        hr.on_link_lost(&mut state);
        assert!(!hr.link_up());
        assert!(!state.hr_connected);
        assert!(state.last_bpm.is_none());
        assert!(state.last_bpm_ts.is_none());
        assert!(state.hr_battery_pct.is_none());
    }

    #[test]
    fn reconnect_decision_skips_fresh_in_flight_then_spawns_after_deadline() {
        let t0 = Instant::now();
        let tk = tokio::time::Instant::from_std(t0);
        let mut hr = HrSession::new_connecting(t0, tk);
        // Fresh in-flight from new_connecting.
        assert_eq!(hr.reconnect_decision(t0 + Duration::from_secs(10)), HrReconnect::Skip);
        // Past deadline → Spawn and re-arm.
        assert_eq!(
            hr.reconnect_decision(t0 + HR_CONNECT_ATTEMPT_DEADLINE + Duration::from_secs(1)),
            HrReconnect::Spawn
        );
        // Immediately after spawn arm → Skip again.
        assert_eq!(
            hr.reconnect_decision(t0 + HR_CONNECT_ATTEMPT_DEADLINE + Duration::from_secs(1)),
            HrReconnect::Skip
        );
    }

    #[test]
    fn reconnect_decision_spawns_when_not_in_flight() {
        let t0 = Instant::now();
        let tk = tokio::time::Instant::from_std(t0);
        let mut hr = HrSession::new_connecting(t0, tk);
        hr.on_connect_finished();
        assert_eq!(hr.reconnect_decision(t0), HrReconnect::Spawn);
    }

    #[test]
    fn battery_read_due_adapts_to_level() {
        let t0 = Instant::now();
        let tk = tokio::time::Instant::from_std(t0);
        let mut hr = HrSession::new_connecting(t0, tk);
        let mut state = DaemonState::new(true);
        // Never read → due.
        assert!(hr.battery_read_due(t0));
        hr.on_connected(Some(50), t0, tk, &mut state);
        assert!(!hr.battery_read_due(t0));
        assert!(!hr.battery_read_due(t0 + Duration::from_secs(30 * 60)));
        assert!(hr.battery_read_due(t0 + HR_BATTERY_POLL_INTERVAL));

        // Low battery tightens interval.
        hr.on_battery_read(Some(15), t0, &mut state);
        assert!(!hr.battery_read_due(t0 + Duration::from_secs(10 * 60)));
        assert!(hr.battery_read_due(t0 + HR_BATTERY_POLL_INTERVAL_LOW));
    }

    #[test]
    fn invariant_hr_connected_false_implies_last_bpm_none_on_all_paths() {
        let now = Instant::now();
        let tk = tokio::time::Instant::from_std(now);
        let mut hr = HrSession::new_connecting(now, tk);
        let mut state = DaemonState::new(true);

        // Path 1: never connected.
        assert!(!state.hr_connected);
        assert!(state.last_bpm.is_none());

        hr.on_connected(Some(90), now, tk, &mut state);
        let m = live_measurement(100);
        let _ = hr.on_frame(&m, 1, tk, &mut state);
        assert!(state.hr_connected && state.last_bpm.is_some());

        // Path 2: contact lost.
        let lost = hr::HrMeasurement {
            bpm: 100,
            contact: Some(false),
            rr_ms: vec![],
        };
        let _ = hr.on_frame(&lost, 2, tk, &mut state);
        assert!(!state.hr_connected);
        assert!(state.last_bpm.is_none());

        // Path 3: full link loss.
        let _ = hr.on_frame(&m, 3, tk, &mut state); // regain
        hr.on_link_lost(&mut state);
        assert!(!state.hr_connected);
        assert!(state.last_bpm.is_none());
    }

    #[test]
    fn silence_deadline_advances_with_activity() {
        let start = tokio::time::Instant::from_std(Instant::now());
        let mut hr = HrSession::new_connecting(Instant::now(), start);
        assert_eq!(hr.silence_deadline(), start + HR_NOTIFICATION_TIMEOUT);
        let later = start + Duration::from_secs(3);
        hr.on_link_activity(later);
        assert_eq!(hr.silence_deadline(), later + HR_NOTIFICATION_TIMEOUT);
    }

    #[test]
    fn hr_connect_latch_deadline_above_scan_window() {
        assert!(HR_CONNECT_ATTEMPT_DEADLINE > Duration::from_secs(15));
        assert!(HR_CONNECT_ATTEMPT_DEADLINE.as_secs() >= 45);
    }
}
