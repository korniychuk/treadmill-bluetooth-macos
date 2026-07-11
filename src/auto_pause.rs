//! Idle-belt auto-pause spell state (задача 020 / 053).
//!
//! Owns the away-spell clock and one-shot-per-spell fire latch. Pure decisions:
//! time is always injected (`now: Instant`); no BLE, no wall clock. The daemon
//! shell still performs the bounded Control Point `Stop` write.

use std::time::{Duration, Instant};

use crate::presence;

/// After a *failed* idle-belt auto-pause write, how long to wait before retrying
/// while the operator is still away. Long enough not to hammer a wedged Control
/// Point every telemetry sample (~1/s), short enough that a transient BLE glitch
/// does not leave the belt running idle for the whole away spell. A *successful*
/// pause is one-shot per spell (no cooldown needed).
const AUTO_PAUSE_RETRY_COOLDOWN: Duration = Duration::from_secs(15);

/// Away-spell state for idle-belt auto-pause (задача 020).
///
/// `away_since` arms when presence enters `AwayWhileRunning`; `fired` is whether
/// we already paused this spell; `last_attempt` gates retries after a failed
/// write by [`AUTO_PAUSE_RETRY_COOLDOWN`].
#[derive(Debug, Default, Clone)]
pub struct AutoPause {
    away_since: Option<Instant>,
    fired: bool,
    last_attempt: Option<Instant>,
}

impl AutoPause {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Presence → AwayWhileRunning: arm a fresh spell (reset fired / last_attempt).
    pub fn on_away(&mut self, now: Instant) {
        self.away_since = Some(now);
        self.fired = false;
        self.last_attempt = None;
    }

    /// Return to Walking: honest away duration for the toast (back-dated by
    /// [`presence::AWAY_THRESHOLD`]) and clear the spell start. `fired` /
    /// `last_attempt` stay until the next [`Self::on_away`].
    pub fn on_return(&mut self, now: Instant) -> Option<Duration> {
        let dur = self.away_for(now);
        self.away_since = None;
        dur
    }

    /// Honest "how long the belt ran while I wasn't walking" (задача 010):
    /// elapsed since arm plus the pre-confirmation [`presence::AWAY_THRESHOLD`].
    #[must_use]
    pub fn away_for(&self, now: Instant) -> Option<Duration> {
        self.away_since
            .map(|since| now.saturating_duration_since(since) + presence::AWAY_THRESHOLD)
    }

    /// Whether to send an idle-belt auto-pause right now. `threshold` is `None`
    /// when auto-pause is disabled (`auto_pause_minutes = 0`).
    #[must_use]
    pub fn due(&self, threshold: Option<Duration>, now: Instant) -> bool {
        let Some(threshold) = threshold else {
            return false;
        };
        if self.fired {
            return false;
        }
        let away_for = self.away_for(now).unwrap_or_default();
        if away_for < threshold {
            return false;
        }
        !matches!(
            self.last_attempt,
            Some(t) if now.saturating_duration_since(t) < AUTO_PAUSE_RETRY_COOLDOWN
        )
    }

    /// Successful Control Point Stop — one-shot for this spell.
    pub fn on_pause_ok(&mut self) {
        self.fired = true;
    }

    /// Failed/timed-out write — open the retry cooldown.
    pub fn on_pause_failed(&mut self, now: Instant) {
        self.last_attempt = Some(now);
    }

    /// Whether this spell already auto-paused (suppress generic "Paused" toast).
    #[must_use]
    pub fn fired(&self) -> bool {
        self.fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_is_off_when_disabled_or_already_fired() {
        let now = Instant::now();
        let mut ap = AutoPause::new();
        ap.on_away(now - Duration::from_secs(600));
        // Disabled (no threshold) → never fires.
        assert!(!ap.due(None, now));
        // Already paused this spell → never re-fires until a fresh spell.
        ap.on_pause_ok();
        assert!(!ap.due(Some(Duration::from_secs(300)), now));
    }

    #[test]
    fn due_waits_for_threshold_then_fires() {
        let now = Instant::now();
        let threshold = Some(Duration::from_secs(300));
        let mut ap = AutoPause::new();
        // Arm just now → away_for ≈ AWAY_THRESHOLD ≪ 300 → not yet.
        ap.on_away(now);
        assert!(!ap.due(threshold, now));

        // Helper: arm so away_for(now) == want.
        let arm_for = |ap: &mut AutoPause, want: Duration| {
            let elapsed = want
                .checked_sub(presence::AWAY_THRESHOLD)
                .unwrap_or(Duration::ZERO);
            ap.on_away(now - elapsed);
        };

        arm_for(&mut ap, Duration::from_secs(299));
        assert!(!ap.due(threshold, now));
        arm_for(&mut ap, Duration::from_secs(300));
        assert!(ap.due(threshold, now));
    }

    #[test]
    fn due_cooldown_gates_retries_after_a_failure() {
        let now = Instant::now();
        let threshold = Some(Duration::from_secs(300));
        let mut ap = AutoPause::new();
        // Away long enough.
        let want_away = Duration::from_secs(400);
        let elapsed = want_away
            .checked_sub(presence::AWAY_THRESHOLD)
            .unwrap_or(Duration::ZERO);
        ap.on_away(now - elapsed);
        // Failed attempt 5s ago → inside cooldown.
        ap.on_pause_failed(now - Duration::from_secs(5));
        assert!(!ap.due(threshold, now));
        // Past cooldown → retry.
        ap.on_pause_failed(now - (AUTO_PAUSE_RETRY_COOLDOWN + Duration::from_secs(1)));
        assert!(ap.due(threshold, now));
    }

    #[test]
    fn away_for_adds_the_confirmation_window() {
        let now = Instant::now();
        let ap = AutoPause::new();
        assert_eq!(ap.away_for(now), None);
        let mut ap = AutoPause::new();
        ap.on_away(now);
        let reported = ap.away_for(now).expect("some");
        assert!(reported >= presence::AWAY_THRESHOLD);
        assert_eq!(reported, presence::AWAY_THRESHOLD);
    }

    #[test]
    fn spell_cycle_on_away_due_ok_fired() {
        let now = Instant::now();
        let threshold = Some(Duration::from_secs(300));
        let mut ap = AutoPause::new();
        let want_away = Duration::from_secs(400);
        let elapsed = want_away
            .checked_sub(presence::AWAY_THRESHOLD)
            .unwrap_or(Duration::ZERO);
        ap.on_away(now - elapsed);
        assert!(ap.due(threshold, now));
        assert!(!ap.fired());
        ap.on_pause_ok();
        assert!(ap.fired());
        assert!(!ap.due(threshold, now));
    }

    #[test]
    fn spell_cycle_failed_then_cooldown_then_retry() {
        let t0 = Instant::now();
        let threshold = Some(Duration::from_secs(60));
        let mut ap = AutoPause::new();
        let want_away = Duration::from_secs(120);
        let elapsed = want_away
            .checked_sub(presence::AWAY_THRESHOLD)
            .unwrap_or(Duration::ZERO);
        ap.on_away(t0 - elapsed);
        assert!(ap.due(threshold, t0));
        ap.on_pause_failed(t0);
        assert!(!ap.due(threshold, t0 + Duration::from_secs(5)));
        assert!(ap.due(
            threshold,
            t0 + AUTO_PAUSE_RETRY_COOLDOWN + Duration::from_secs(1)
        ));
    }

    #[test]
    fn on_return_yields_back_dated_duration_and_clears_spell() {
        let now = Instant::now();
        let mut ap = AutoPause::new();
        let arm = now - Duration::from_secs(30);
        ap.on_away(arm);
        let dur = ap.on_return(now).expect("duration");
        assert_eq!(
            dur,
            Duration::from_secs(30) + presence::AWAY_THRESHOLD
        );
        assert_eq!(ap.away_for(now), None);
        // Fresh spell after return is independent.
        ap.on_away(now);
        assert!(!ap.fired());
    }

    #[test]
    fn on_away_resets_fired_and_last_attempt() {
        let now = Instant::now();
        let mut ap = AutoPause::new();
        ap.on_away(now - Duration::from_secs(100));
        ap.on_pause_ok();
        ap.on_pause_failed(now);
        assert!(ap.fired());
        ap.on_away(now);
        assert!(!ap.fired());
        // Cooldown gone: no last_attempt after fresh arm.
        let want_away = Duration::from_secs(400);
        let elapsed = want_away
            .checked_sub(presence::AWAY_THRESHOLD)
            .unwrap_or(Duration::ZERO);
        ap.on_away(now - elapsed);
        assert!(ap.due(Some(Duration::from_secs(300)), now));
    }
}
