//! Presence detection — is the operator actually walking on the belt?
//!
//! The belt running (`speed_kmh > 0`) does not mean someone is on it: the
//! operator can step off while it keeps spinning. The only ground truth is
//! the vendor step counter (see `ftms::TreadmillData::steps`); live capture
//! showed it advances every ~1s even at the slowest belt speed, with a
//! worst-case single gap of ~2s (see `docs/tasks/005-…`). `AWAY_THRESHOLD`
//! sits well above that noise floor.

use std::time::{Duration, Instant};

/// How long the belt can run without a step-count increase before the
/// operator is considered to have left the treadmill.
pub const AWAY_THRESHOLD: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceState {
    /// Not enough samples yet to tell.
    Unknown,
    /// Belt running and steps advancing — operator is on the belt.
    Walking,
    /// Belt running but steps have not advanced for `AWAY_THRESHOLD`.
    AwayWhileRunning,
    /// Belt stopped (`speed_kmh == 0`) — a normal pause, not an absence.
    Paused,
}

/// Tracks the last seen step count and derives [`PresenceState`] transitions.
pub struct PresenceTracker {
    state: PresenceState,
    last_steps: Option<u32>,
    last_step_change: Option<Instant>,
}

impl PresenceTracker {
    pub fn new() -> Self {
        Self { state: PresenceState::Unknown, last_steps: None, last_step_change: None }
    }

    /// Feed one telemetry sample; returns `Some(new_state)` only on a
    /// transition, `None` if the state is unchanged.
    pub fn observe(&mut self, speed_kmh: Option<f32>, steps: Option<u32>) -> Option<PresenceState> {
        let now = Instant::now();

        if let Some(steps) = steps
            && self.last_steps != Some(steps)
        {
            self.last_steps = Some(steps);
            self.last_step_change = Some(now);
        }

        let next = match speed_kmh {
            Some(speed) if speed <= 0.0 => PresenceState::Paused,
            Some(_) => match self.last_step_change {
                Some(last_change) if now.duration_since(last_change) >= AWAY_THRESHOLD => {
                    PresenceState::AwayWhileRunning
                }
                Some(_) => PresenceState::Walking,
                // Belt running but no step sample seen yet — give it the
                // benefit of the doubt rather than firing an away alert.
                None => PresenceState::Walking,
            },
            None => self.state,
        };

        if next == self.state {
            return None;
        }
        self.state = next;
        Some(next)
    }

    pub fn state(&self) -> PresenceState {
        self.state
    }
}

impl Default for PresenceTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_unknown() {
        let tracker = PresenceTracker::new();
        assert_eq!(tracker.state(), PresenceState::Unknown);
    }

    #[test]
    fn walking_when_speed_and_steps_present() {
        let mut tracker = PresenceTracker::new();
        let transition = tracker.observe(Some(2.5), Some(10));
        assert_eq!(transition, Some(PresenceState::Walking));
        assert_eq!(tracker.state(), PresenceState::Walking);
    }

    #[test]
    fn paused_when_speed_zero() {
        let mut tracker = PresenceTracker::new();
        tracker.observe(Some(2.5), Some(10));
        let transition = tracker.observe(Some(0.0), Some(10));
        assert_eq!(transition, Some(PresenceState::Paused));
    }

    #[test]
    fn no_transition_reported_when_state_unchanged() {
        let mut tracker = PresenceTracker::new();
        tracker.observe(Some(2.5), Some(10));
        let transition = tracker.observe(Some(2.5), Some(11));
        assert_eq!(transition, None);
        assert_eq!(tracker.state(), PresenceState::Walking);
    }

    #[test]
    fn away_after_threshold_without_step_change() {
        let mut tracker = PresenceTracker::new();
        tracker.observe(Some(2.5), Some(10));
        // Simulate the threshold elapsing without any step increase by
        // back-dating last_step_change directly (no real sleep in tests).
        tracker.last_step_change = Some(Instant::now() - AWAY_THRESHOLD - Duration::from_secs(1));
        let transition = tracker.observe(Some(2.5), Some(10));
        assert_eq!(transition, Some(PresenceState::AwayWhileRunning));
    }
}
