//! Zone Hold session controller (задача 027/032/039 / 053).
//!
//! Owns phase machine, correction/safety timers, and operator-override window.
//! **Decision/effect split:** [`ZoneSession::tick`] is pure (returns
//! [`ZoneWrite`]); the daemon shell performs BLE writes. Snapshot is updated
//! by the shell *before* the write (see [`ZoneSession::persist_snapshot`] —
//! phase/target/position do not depend on write outcome; the only intentional
//! ordering micro-difference from pre-053).

use std::time::{Duration, Instant};

use tracing::{info, warn};

use crate::config_apply;
use crate::daemon::DaemonState;
use crate::presence::PresenceState;
use crate::speed::CentiKmh;
use crate::zone_hold::{self, ResolvedZone, ZoneHoldConfig};

/// Max age of `last_bpm` before Zone Hold treats bpm as absent (задача 035).
pub(crate) const ZH_BPM_MAX_AGE: Duration = Duration::from_secs(15);

/// How long after CLI `tm speed` Zone Hold must not write belt speed (задача 039).
const OPERATOR_OVERRIDE_WINDOW: Duration = Duration::from_secs(60);

/// Minimum gap between repeated safety-cap writes (задача 027).
const ZONE_HOLD_SAFETY_COOLDOWN: Duration = Duration::from_secs(5);

/// HRmax-percent above which, once already at min_speed, Zone Hold hard-stops.
const ZONE_HOLD_HARD_STOP_PERCENT: f32 = 85.0;

/// Control-Point intent from a pure tick (shell executes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneWrite {
    SetSpeed {
        target: CentiKmh,
    },
    /// Suppressed by operator-override window (задача 039) — shell logs, no write.
    Suppressed {
        target: CentiKmh,
    },
    /// Safety hard-stop — never suppressed by override (current behaviour).
    Stop,
}

/// Zone Hold controller phase for the current session (задача 027).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneHoldPhase {
    Off,
    Ramp {
        started_at: Instant,
        start_speed: CentiKmh,
        target_speed: CentiKmh,
    },
    Hold,
    /// Presence left Walking — HR ignored until return.
    Frozen,
    /// Just returned to Walking — no corrections until `until`.
    Grace {
        until: Instant,
    },
}

impl ZoneHoldPhase {
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            ZoneHoldPhase::Off => "off",
            ZoneHoldPhase::Ramp { .. } => "ramp",
            ZoneHoldPhase::Hold => "hold",
            ZoneHoldPhase::Frozen => "frozen",
            ZoneHoldPhase::Grace { .. } => "grace",
        }
    }

    /// Flat phase kind for pure config-apply decisions (задача 052).
    #[must_use]
    pub fn kind(&self) -> config_apply::PhaseKind {
        match self {
            ZoneHoldPhase::Off => config_apply::PhaseKind::Off,
            ZoneHoldPhase::Ramp { .. } => config_apply::PhaseKind::Ramp,
            ZoneHoldPhase::Hold => config_apply::PhaseKind::Hold,
            ZoneHoldPhase::Frozen => config_apply::PhaseKind::Frozen,
            ZoneHoldPhase::Grace { .. } => config_apply::PhaseKind::Grace,
        }
    }
}

/// Per-session Zone Hold state (phase + timers + override window).
#[derive(Debug)]
pub struct ZoneSession {
    phase: ZoneHoldPhase,
    last_correction_at: Option<Instant>,
    last_safety_write_at: Option<Instant>,
    operator_override_until: Option<Instant>,
}

impl Default for ZoneSession {
    fn default() -> Self {
        Self::new()
    }
}

impl ZoneSession {
    #[must_use]
    pub fn new() -> Self {
        Self {
            phase: ZoneHoldPhase::Off,
            last_correction_at: None,
            last_safety_write_at: None,
            operator_override_until: None,
        }
    }

    #[must_use]
    pub fn kind(&self) -> config_apply::PhaseKind {
        self.phase.kind()
    }

    #[must_use]
    pub fn is_off(&self) -> bool {
        matches!(self.phase, ZoneHoldPhase::Off)
    }

    /// Call-site gate (задача 032): enabled *and* a live phase.
    #[must_use]
    pub fn should_run(&self, enabled: bool) -> bool {
        enabled && !self.is_off()
    }

    /// Successful CLI `tm speed` — open override window (задача 039).
    pub fn note_cli_speed(&mut self, now: Instant) {
        self.operator_override_until = Some(now + OPERATOR_OVERRIDE_WINDOW);
    }

    /// Park phase and clear the full widget/status snapshot (задача 032).
    pub fn disengage(&mut self, state: &mut DaemonState) {
        self.phase = ZoneHoldPhase::Off;
        state.zone_hold_active = false;
        state.zone_hold_phase = Some("off".to_string());
        state.zone_hold_position = None;
        state.zone_hold_target_lo = None;
        state.zone_hold_target_hi = None;
        state.zone_hold_last_speed = None;
    }

    /// Engage/freeze/grace on a presence transition (задача 027).
    ///
    /// `resumed` `None` skips Ramp engage (задача 036 — never seed 0.0).
    pub fn on_presence_transition(
        &mut self,
        prev_state: PresenceState,
        next_state: PresenceState,
        config: &ZoneHoldConfig,
        resumed: Option<CentiKmh>,
        default_speed: CentiKmh,
        now: Instant,
    ) {
        if !config.enabled || config.resolve_target_zone().is_none() {
            self.phase = ZoneHoldPhase::Off;
            return;
        }
        match (prev_state, next_state) {
            (_, PresenceState::Walking) if self.phase == ZoneHoldPhase::Off => {
                let Some(start_speed) = resumed else {
                    return;
                };
                let target = default_speed.clamp(config.min_speed_kmh, config.max_speed_kmh);
                self.phase = ZoneHoldPhase::Ramp {
                    started_at: now,
                    start_speed,
                    target_speed: target,
                };
                info!(%target, "zone hold: engaged, starting warm-up ramp");
            }
            (PresenceState::Paused, PresenceState::Walking)
            | (PresenceState::AwayWhileRunning, PresenceState::Walking) => {
                let grace = Duration::from_secs(config.reentry_grace_seconds as u64);
                self.phase = ZoneHoldPhase::Grace { until: now + grace };
                info!(
                    grace_s = config.reentry_grace_seconds,
                    "zone hold: returned to walking, grace period before corrections resume"
                );
            }
            (PresenceState::Walking, PresenceState::Paused)
            | (PresenceState::Walking, PresenceState::AwayWhileRunning)
                if self.phase != ZoneHoldPhase::Off =>
            {
                self.phase = ZoneHoldPhase::Frozen;
                info!("zone hold: left the belt — freezing (HR ignored until return)");
            }
            _ => {}
        }
    }

    /// Pure tick: advance timers / compute a write. Does **not** touch BLE.
    ///
    /// Both 032 gates (enabled + phase) live here; `None` speed → `None`
    /// (задача 036/030). Safety `Stop` is never suppressed by override.
    pub fn tick(
        &mut self,
        config: &ZoneHoldConfig,
        resolved: &ResolvedZone,
        measured: Option<CentiKmh>,
        bpm: Option<u16>,
        now: Instant,
    ) -> Option<ZoneWrite> {
        if !config.enabled {
            return None;
        }
        let measured = measured?;

        let zone_writes_suppressed = operator_override_active(now, self.operator_override_until);
        let correction_interval = Duration::from_secs(config.correction_interval_seconds as u64);
        let correction_due = |last: Option<Instant>| {
            last.is_none_or(|t| now.saturating_duration_since(t) >= correction_interval)
        };

        let mut write = None;

        match self.phase {
            ZoneHoldPhase::Off | ZoneHoldPhase::Frozen => {}
            ZoneHoldPhase::Grace { until } => {
                if now >= until {
                    self.phase = ZoneHoldPhase::Hold;
                    info!("zone hold: grace period elapsed — resuming closed-loop correction");
                }
            }
            ZoneHoldPhase::Ramp {
                started_at,
                start_speed,
                target_speed,
            } => {
                let elapsed = now.saturating_duration_since(started_at);
                let warmup = Duration::from_secs(config.warmup_minutes as u64 * 60);
                if elapsed >= warmup {
                    self.phase = ZoneHoldPhase::Hold;
                    info!("zone hold: warm-up ramp complete — starting closed-loop correction");
                } else if correction_due(self.last_correction_at) {
                    let target =
                        zone_hold::warmup_target_speed(start_speed, target_speed, elapsed, warmup);
                    if target.abs_diff(measured) > zone_hold::MIN_SPEED_CHANGE.to_wire() {
                        write = Some(speed_write(target, zone_writes_suppressed));
                    }
                    self.last_correction_at = Some(now);
                }
            }
            ZoneHoldPhase::Hold => {
                if let (Some(bpm), Some(safety_cap)) = (bpm, config.safety_cap_bpm())
                    && bpm > safety_cap
                {
                    let cooling_down = self.last_safety_write_at.is_some_and(|t| {
                        now.saturating_duration_since(t) < ZONE_HOLD_SAFETY_COOLDOWN
                    });
                    if !cooling_down {
                        self.last_safety_write_at = Some(now);
                        let hard_stop = config
                            .hrmax()
                            .map(|hrmax| {
                                zone_hold::safety_cap_bpm(hrmax, ZONE_HOLD_HARD_STOP_PERCENT)
                            })
                            .unwrap_or(u16::MAX);
                        let at_min = measured
                            <= config
                                .min_speed_kmh
                                .saturating_add_centi(zone_hold::MIN_SPEED_CHANGE.to_wire());
                        if at_min && bpm > hard_stop {
                            warn!(
                                bpm,
                                safety_cap,
                                hard_stop,
                                "zone hold: safety cap exceeded at min speed — stopping belt"
                            );
                            // Hard-stop is safety — not suppressed by operator override.
                            write = Some(ZoneWrite::Stop);
                        } else if let Some(target) = zone_hold::safety_force_reduce_target(
                            measured,
                            config.max_step_kmh,
                            config.min_speed_kmh,
                        ) {
                            warn!(
                                bpm,
                                safety_cap,
                                %target,
                                "zone hold: safety cap exceeded — force-reducing speed"
                            );
                            write = Some(speed_write(target, zone_writes_suppressed));
                        }
                        // else: already at min within deadband — no write (задача 041).
                    }
                    // Safety path: shell still persists snapshot after this return.
                    return write;
                }
                if correction_due(self.last_correction_at) {
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
                        if let Some(target) = zone_hold::next_speed(&params, measured, bpm) {
                            write = Some(speed_write(target, zone_writes_suppressed));
                        }
                    }
                    self.last_correction_at = Some(now);
                }
            }
        }

        write
    }

    /// Mirror phase/target/position into `daemon_status` (задача 027).
    ///
    /// Called by the shell **before** executing a [`ZoneWrite`] (snapshot
    /// fields do not depend on write success — intentional micro-reorder vs
    /// pre-053, which sometimes persisted after the await).
    pub fn persist_snapshot(
        &self,
        state: &mut DaemonState,
        resolved: &ResolvedZone,
        bpm: Option<u16>,
        measured: CentiKmh,
    ) {
        state.zone_hold_active = !matches!(self.phase, ZoneHoldPhase::Off);
        state.zone_hold_phase = Some(self.phase.label().to_string());
        state.zone_hold_target_lo = Some(i64::from(resolved.low_bpm));
        state.zone_hold_target_hi = Some(i64::from(resolved.high_bpm));
        state.zone_hold_last_speed = Some(f64::from(measured.to_kmh_f32()));
        state.zone_hold_position = match (self.phase, bpm) {
            (ZoneHoldPhase::Hold, Some(bpm)) => Some(
                zone_hold::classify_position(bpm, resolved.low_bpm, resolved.high_bpm)
                    .wire()
                    .to_string(),
            ),
            _ => None,
        };
    }

    /// Mid-session engage catch-up (`tm zone on` while Walking) — same path as
    /// a fresh Walking entry (задача 052 ZoneEngage).
    pub fn on_config_engaged(
        &mut self,
        config: &ZoneHoldConfig,
        resumed: Option<CentiKmh>,
        default_speed: CentiKmh,
        now: Instant,
    ) {
        self.on_presence_transition(
            PresenceState::Unknown,
            PresenceState::Walking,
            config,
            resumed,
            default_speed,
            now,
        );
    }
}

fn speed_write(target: CentiKmh, suppressed: bool) -> ZoneWrite {
    if suppressed {
        ZoneWrite::Suppressed { target }
    } else {
        ZoneWrite::SetSpeed { target }
    }
}

/// Pure gate: Zone Hold speed writes suppressed while override window open.
fn operator_override_active(now: Instant, until: Option<Instant>) -> bool {
    until.is_some_and(|u| now < u)
}

/// Bpm for Zone Hold only when HR link is live and sample is fresh (задача 035).
#[must_use]
pub fn bpm_if_fresh(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn c(kmh: f32) -> CentiKmh {
        CentiKmh::from_kmh_f32(kmh).expect("test speed in range")
    }

    fn enabled_config() -> ZoneHoldConfig {
        let mut cfg = ZoneHoldConfig::disabled_default();
        cfg.enabled = true;
        cfg.age = Some(30);
        cfg.warmup_minutes = 1;
        cfg.reentry_grace_seconds = 10;
        cfg.correction_interval_seconds = 20;
        cfg.min_speed_kmh = c(1.0);
        cfg.max_speed_kmh = c(6.0);
        cfg.max_step_kmh = c(0.3);
        cfg
    }

    fn resolved(cfg: &ZoneHoldConfig) -> ResolvedZone {
        cfg.resolve_target_zone().expect("resolvable")
    }

    #[test]
    fn should_run_requires_both_enabled_and_a_live_phase() {
        let mut z = ZoneSession::new();
        z.phase = ZoneHoldPhase::Hold;
        assert!(z.should_run(true));
        assert!(!z.should_run(false));
        z.phase = ZoneHoldPhase::Off;
        assert!(!z.should_run(true));
        assert!(!z.should_run(false));
    }

    #[test]
    fn disengage_parks_phase_and_clears_snapshot() {
        let mut state = DaemonState::new(true);
        state.zone_hold_active = true;
        state.zone_hold_phase = Some("ramp".to_string());
        state.zone_hold_position = Some("below".to_string());
        state.zone_hold_target_lo = Some(90);
        state.zone_hold_target_hi = Some(110);
        state.zone_hold_last_speed = Some(3.0);
        let mut z = ZoneSession::new();
        z.phase = ZoneHoldPhase::Ramp {
            started_at: Instant::now(),
            start_speed: c(2.5),
            target_speed: c(3.0),
        };
        z.disengage(&mut state);
        assert!(z.is_off());
        assert!(!state.zone_hold_active);
        assert_eq!(state.zone_hold_phase.as_deref(), Some("off"));
        assert!(state.zone_hold_position.is_none());
        assert!(state.zone_hold_target_lo.is_none());
        assert!(state.zone_hold_target_hi.is_none());
        assert!(state.zone_hold_last_speed.is_none());
    }

    #[test]
    fn on_transition_never_engages_while_disabled() {
        let mut z = ZoneSession::new();
        z.phase = ZoneHoldPhase::Hold;
        let config = ZoneHoldConfig::disabled_default();
        z.on_presence_transition(
            PresenceState::Paused,
            PresenceState::Walking,
            &config,
            Some(c(2.5)),
            c(3.0),
            Instant::now(),
        );
        assert!(z.is_off());
    }

    #[test]
    fn on_transition_skips_ramp_when_speed_unknown() {
        let mut z = ZoneSession::new();
        let config = enabled_config();
        z.on_presence_transition(
            PresenceState::Unknown,
            PresenceState::Walking,
            &config,
            None,
            c(3.0),
            Instant::now(),
        );
        assert!(z.is_off());
    }

    #[test]
    fn ramp_completes_to_hold_after_warmup() {
        let mut z = ZoneSession::new();
        let config = enabled_config();
        let resolved = resolved(&config);
        let t0 = Instant::now();
        z.on_presence_transition(
            PresenceState::Unknown,
            PresenceState::Walking,
            &config,
            Some(c(2.0)),
            c(3.0),
            t0,
        );
        assert!(matches!(z.phase, ZoneHoldPhase::Ramp { .. }));
        // Warmup is 1 minute.
        let w = z.tick(
            &config,
            &resolved,
            Some(c(2.5)),
            Some(100),
            t0 + Duration::from_secs(60),
        );
        assert!(w.is_none());
        assert_eq!(z.phase, ZoneHoldPhase::Hold);
    }

    #[test]
    fn grace_elapses_to_hold() {
        let mut z = ZoneSession::new();
        let config = enabled_config();
        let resolved = resolved(&config);
        let t0 = Instant::now();
        z.phase = ZoneHoldPhase::Hold;
        z.on_presence_transition(
            PresenceState::Walking,
            PresenceState::Paused,
            &config,
            Some(c(2.5)),
            c(3.0),
            t0,
        );
        assert_eq!(z.phase, ZoneHoldPhase::Frozen);
        z.on_presence_transition(
            PresenceState::Paused,
            PresenceState::Walking,
            &config,
            Some(c(2.5)),
            c(3.0),
            t0,
        );
        assert!(matches!(z.phase, ZoneHoldPhase::Grace { .. }));
        let _ = z.tick(
            &config,
            &resolved,
            Some(c(2.5)),
            Some(100),
            t0 + Duration::from_secs(config.reentry_grace_seconds as u64),
        );
        assert_eq!(z.phase, ZoneHoldPhase::Hold);
    }

    #[test]
    fn tick_skips_when_measured_speed_is_none() {
        let mut z = ZoneSession::new();
        z.phase = ZoneHoldPhase::Hold;
        let config = enabled_config();
        let resolved = resolved(&config);
        assert!(
            z.tick(&config, &resolved, None, Some(120), Instant::now())
                .is_none()
        );
    }

    #[test]
    fn tick_skips_when_disabled_even_if_phase_live() {
        let mut z = ZoneSession::new();
        z.phase = ZoneHoldPhase::Hold;
        let mut config = enabled_config();
        let resolved = resolved(&config);
        config.enabled = false;
        assert!(
            z.tick(&config, &resolved, Some(c(3.0)), Some(120), Instant::now())
                .is_none()
        );
    }

    #[test]
    fn operator_override_suppresses_set_speed_but_not_stop() {
        let mut z = ZoneSession::new();
        let config = enabled_config();
        let resolved = resolved(&config);
        let t0 = Instant::now();
        z.phase = ZoneHoldPhase::Hold;
        z.note_cli_speed(t0);

        // Force a correction: bpm=60 is far below any default zone for age 30
        // (hrmax 187), so band tracking must step up by max_step (2.0 → 2.3) —
        // deterministic, and the override window must downgrade it to Suppressed.
        let w = z.tick(
            &config,
            &resolved,
            Some(c(2.0)),
            Some(60),
            t0 + Duration::from_secs(1),
        );
        assert!(
            matches!(w, Some(ZoneWrite::Suppressed { .. })),
            "expected Suppressed correction under override, got {w:?}"
        );

        // Safety hard-stop: at min speed (1.0) with bpm=200 above both the
        // safety cap and the 85%-hrmax hard stop (~159 for age 30). Override is
        // active — Stop must NOT be suppressed (задача 039).
        let mut z2 = ZoneSession::new();
        z2.phase = ZoneHoldPhase::Hold;
        z2.note_cli_speed(t0);
        let w = z2.tick(
            &config,
            &resolved,
            Some(config.min_speed_kmh),
            Some(200),
            t0 + Duration::from_secs(1),
        );
        assert_eq!(
            w,
            Some(ZoneWrite::Stop),
            "safety hard-stop must fire unsuppressed under operator override"
        );
    }

    #[test]
    fn note_cli_speed_opens_override_window() {
        let now = Instant::now();
        assert!(!operator_override_active(now, None));
        assert!(operator_override_active(
            now,
            Some(now + Duration::from_secs(30))
        ));
        assert!(!operator_override_active(
            now + Duration::from_secs(61),
            Some(now + Duration::from_secs(60))
        ));
        let mut z = ZoneSession::new();
        z.note_cli_speed(now);
        assert!(operator_override_active(
            now + Duration::from_secs(1),
            z.operator_override_until
        ));
    }

    #[test]
    fn bpm_if_fresh_requires_link_and_recent_sample() {
        let max = Duration::from_secs(15);
        let now = 1_000_000_i64;
        assert_eq!(
            bpm_if_fresh(true, Some(120), Some(now - 5_000), now, max),
            Some(120)
        );
        assert_eq!(
            bpm_if_fresh(false, Some(120), Some(now - 1_000), now, max),
            None
        );
        assert_eq!(
            bpm_if_fresh(true, Some(111), Some(now - 16_000), now, max),
            None
        );
        assert_eq!(bpm_if_fresh(true, Some(120), None, now, max), None);
        assert_eq!(bpm_if_fresh(true, None, Some(now), now, max), None);
    }

    #[test]
    fn persist_snapshot_composition_independent_of_write() {
        // Documents snapshot-ordering micro-diff: fields come from phase/bpm/
        // measured speed only — not from whether a ZoneWrite later succeeds.
        let mut z = ZoneSession::new();
        z.phase = ZoneHoldPhase::Hold;
        let config = enabled_config();
        let resolved = resolved(&config);
        let mut state = DaemonState::new(true);
        z.persist_snapshot(&mut state, &resolved, Some(100), c(3.5));
        assert!(state.zone_hold_active);
        assert_eq!(state.zone_hold_phase.as_deref(), Some("hold"));
        assert_eq!(state.zone_hold_last_speed, Some(3.5));
        assert!(state.zone_hold_position.is_some());
        assert_eq!(state.zone_hold_target_lo, Some(i64::from(resolved.low_bpm)));
    }

    #[test]
    fn catchup_engage_mid_session() {
        let mut z = ZoneSession::new();
        let config = enabled_config();
        z.on_config_engaged(&config, Some(c(2.5)), c(3.0), Instant::now());
        assert!(matches!(z.phase, ZoneHoldPhase::Ramp { .. }));
    }
}
