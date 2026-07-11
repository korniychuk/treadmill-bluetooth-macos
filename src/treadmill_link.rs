//! Treadmill BLE-link session state (задача 053).
//!
//! Owns the telemetry silence clock (`last_telemetry_at`) and speed memory used
//! for pause/resume restore and once-per-session default-speed apply. Time is
//! always injected — no `*::now()` inside methods. Watchdog `touch_telemetry`
//! stays on the call site (those clocks are not ours).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long to wait for the next Treadmill Data sample before treating the
/// link as lost. The device streams ~1/s even while stationary.
pub(crate) const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(20);

/// How much of the speed history just before a pause to ignore when estimating
/// the walking ("cruising") speed to restore (задача 012 follow-up).
const SPEED_CRUISE_DECEL_SKIP: Duration = Duration::from_secs(10);

/// Samples slower than this are ramp/idle, not walking.
const SPEED_CRUISE_FLOOR_KMH: f32 = 0.8;

/// How long to retain recent speed samples for the cruising estimate.
const SPEED_HISTORY_RETENTION: Duration = Duration::from_secs(45);

/// Snapshot returned by [`TreadmillLink::on_resume`]: both pause fields taken
/// in one place (no dual `take()` across branches).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResumeSnapshot {
    pub paused_for: Option<Duration>,
    pub pre_pause_speed: Option<f32>,
}

/// Telemetry silence clock + belt speed memory for one connected session.
#[derive(Debug)]
pub struct TreadmillLink {
    last_telemetry_at: tokio::time::Instant,
    speed_history: VecDeque<(Instant, f32)>,
    last_walking_speed: Option<f32>,
    pre_pause_speed: Option<f32>,
    paused_since: Option<Instant>,
    default_speed_applied: bool,
}

impl TreadmillLink {
    /// Fresh session: silence clock starts at `tokio_now` (call site pairs this
    /// with `watchdog.touch_telemetry()` so streaming phase clocks align).
    #[must_use]
    pub fn new(tokio_now: tokio::time::Instant) -> Self {
        Self {
            last_telemetry_at: tokio_now,
            speed_history: VecDeque::new(),
            last_walking_speed: None,
            pre_pause_speed: None,
            paused_since: None,
            default_speed_applied: false,
        }
    }

    /// Each decoded `0x2ACD`: advance silence anchor; if speed present, push
    /// history (prune by [`SPEED_HISTORY_RETENTION`]) and update last walking.
    pub fn on_telemetry(
        &mut self,
        speed_kmh: Option<f32>,
        now: Instant,
        tokio_now: tokio::time::Instant,
    ) {
        self.last_telemetry_at = tokio_now;
        let Some(speed) = speed_kmh else {
            return;
        };
        self.speed_history.push_back((now, speed));
        while let Some(&(t, _)) = self.speed_history.front() {
            if now.saturating_duration_since(t) > SPEED_HISTORY_RETENTION {
                self.speed_history.pop_front();
            } else {
                break;
            }
        }
        if speed > 0.0 {
            self.last_walking_speed = Some(speed);
        }
    }

    /// Absolute `sleep_until` deadline for the telemetry-silence arm (задача 031).
    #[must_use]
    pub fn silence_deadline(&self) -> tokio::time::Instant {
        self.last_telemetry_at + NOTIFICATION_TIMEOUT
    }

    /// Walking → Paused: arm pause clock and capture cruising/fallback speed.
    pub fn on_pause(&mut self, now: Instant) {
        self.paused_since = Some(now);
        self.pre_pause_speed =
            cruising_speed(self.speed_history.make_contiguous(), now).or(self.last_walking_speed);
    }

    /// Paused → Walking: take pause duration and pre-pause speed together.
    pub fn on_resume(&mut self, now: Instant) -> ResumeSnapshot {
        let paused_for = self
            .paused_since
            .take()
            .map(|since| now.saturating_duration_since(since));
        let pre_pause_speed = self.pre_pause_speed.take();
        ResumeSnapshot {
            paused_for,
            pre_pause_speed,
        }
    }

    #[must_use]
    pub fn last_walking_speed(&self) -> Option<f32> {
        self.last_walking_speed
    }

    #[must_use]
    pub fn default_speed_applied(&self) -> bool {
        self.default_speed_applied
    }

    /// Mark the once-per-session default-speed attempt as consumed.
    pub fn mark_default_speed_applied(&mut self) {
        self.default_speed_applied = true;
    }
}

/// Estimate the walking ("cruising") speed to restore on resume from recent
/// `(timestamp, speed)` samples, ignoring the deceleration tail in the last
/// [`SPEED_CRUISE_DECEL_SKIP`] before the pause and any sub-[`SPEED_CRUISE_FLOOR_KMH`]
/// ramp/idle samples. Returns the median of the qualifying "walking" samples;
/// if the session was too short to have any, falls back to the fastest walking
/// sample seen. `None` only when no sample reached the floor at all.
pub(crate) fn cruising_speed(samples: &[(Instant, f32)], pause_at: Instant) -> Option<f32> {
    let mut walking: Vec<f32> = samples
        .iter()
        .filter(|(t, kmh)| {
            *kmh >= SPEED_CRUISE_FLOOR_KMH
                && pause_at.saturating_duration_since(*t) >= SPEED_CRUISE_DECEL_SKIP
        })
        .map(|(_, kmh)| *kmh)
        .collect();

    if walking.is_empty() {
        return samples
            .iter()
            .map(|(_, kmh)| *kmh)
            .filter(|kmh| *kmh >= SPEED_CRUISE_FLOOR_KMH)
            .fold(None, |acc, kmh| Some(acc.map_or(kmh, |m: f32| m.max(kmh))));
    }

    walking.sort_by(|a, b| a.partial_cmp(b).expect("belt speeds are never NaN"));
    Some(walking[walking.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cruising_speed_ignores_the_deceleration_tail() {
        let pause = Instant::now();
        let mut samples: Vec<(Instant, f32)> = Vec::new();
        for secs_ago in 11..=40 {
            samples.push((pause - Duration::from_secs(secs_ago), 2.5));
        }
        samples.push((pause - Duration::from_secs(3), 1.8));
        samples.push((pause - Duration::from_secs(2), 1.0));
        samples.push((pause - Duration::from_secs(1), 0.6));

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
        let pause = Instant::now();
        let samples = [
            (pause - Duration::from_secs(20), 0.5),
            (pause - Duration::from_secs(2), 0.6),
        ];
        assert_eq!(cruising_speed(&samples, pause), None);
    }

    #[test]
    fn on_telemetry_prunes_history_past_retention() {
        let mut link = TreadmillLink::new(tokio::time::Instant::from_std(Instant::now()));
        let t0 = Instant::now();
        // Old sample outside retention.
        link.on_telemetry(
            Some(2.0),
            t0 - SPEED_HISTORY_RETENTION - Duration::from_secs(5),
            tokio::time::Instant::from_std(t0),
        );
        // Fresh sample triggers prune of the old one.
        link.on_telemetry(Some(3.0), t0, tokio::time::Instant::from_std(t0));
        assert_eq!(link.speed_history.len(), 1);
        assert_eq!(link.speed_history[0].1, 3.0);
        assert_eq!(link.last_walking_speed(), Some(3.0));
    }

    #[test]
    fn on_telemetry_zero_speed_does_not_overwrite_last_walking() {
        let mut link = TreadmillLink::new(tokio::time::Instant::from_std(Instant::now()));
        let now = Instant::now();
        let tk = tokio::time::Instant::from_std(now);
        link.on_telemetry(Some(2.5), now, tk);
        link.on_telemetry(Some(0.0), now, tk);
        assert_eq!(link.last_walking_speed(), Some(2.5));
    }

    #[test]
    fn on_pause_prefers_cruising_over_last_walking_fallback() {
        let mut link = TreadmillLink::new(tokio::time::Instant::from_std(Instant::now()));
        let pause = Instant::now();
        let tk = tokio::time::Instant::from_std(pause);
        // Steady cruise well outside decel window, then a last non-zero crawl.
        for secs_ago in 15..=40 {
            link.on_telemetry(
                Some(2.5),
                pause - Duration::from_secs(secs_ago),
                tk,
            );
        }
        link.on_telemetry(Some(0.6), pause - Duration::from_secs(1), tk);
        // last_walking would be 0.6 if we only took last non-zero; cruise is 2.5.
        assert_eq!(link.last_walking_speed(), Some(0.6));
        link.on_pause(pause);
        assert_eq!(link.pre_pause_speed, Some(2.5));
    }

    #[test]
    fn on_pause_falls_back_to_last_walking_when_no_cruise_window() {
        let mut link = TreadmillLink::new(tokio::time::Instant::from_std(Instant::now()));
        let pause = Instant::now();
        let tk = tokio::time::Instant::from_std(pause);
        // All samples inside decel skip — cruising_speed uses peak (≥ floor).
        link.on_telemetry(Some(2.5), pause - Duration::from_secs(5), tk);
        link.on_telemetry(Some(1.2), pause - Duration::from_secs(2), tk);
        link.on_pause(pause);
        // Peak walking inside short window is still 2.5 via cruising_speed fallback.
        assert_eq!(link.pre_pause_speed, Some(2.5));
    }

    #[test]
    fn on_resume_takes_pause_fields_once() {
        let mut link = TreadmillLink::new(tokio::time::Instant::from_std(Instant::now()));
        let t0 = Instant::now();
        link.paused_since = Some(t0 - Duration::from_secs(12));
        link.pre_pause_speed = Some(2.5);
        let snap = link.on_resume(t0);
        assert_eq!(snap.pre_pause_speed, Some(2.5));
        assert_eq!(snap.paused_for, Some(Duration::from_secs(12)));
        assert!(link.paused_since.is_none());
        assert!(link.pre_pause_speed.is_none());
        // Second resume is empty.
        let snap2 = link.on_resume(t0);
        assert_eq!(snap2.pre_pause_speed, None);
        assert_eq!(snap2.paused_for, None);
    }

    #[test]
    fn silence_deadline_is_last_plus_timeout() {
        let start = tokio::time::Instant::from_std(Instant::now());
        let mut link = TreadmillLink::new(start);
        assert_eq!(link.silence_deadline(), start + NOTIFICATION_TIMEOUT);
        let later = start + Duration::from_secs(7);
        link.on_telemetry(Some(1.0), Instant::now(), later);
        assert_eq!(link.silence_deadline(), later + NOTIFICATION_TIMEOUT);
    }

    #[test]
    fn default_speed_once_per_session() {
        let mut link = TreadmillLink::new(tokio::time::Instant::from_std(Instant::now()));
        assert!(!link.default_speed_applied());
        link.mark_default_speed_applied();
        assert!(link.default_speed_applied());
    }
}
