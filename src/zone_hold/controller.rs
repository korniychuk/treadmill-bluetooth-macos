//! Closed-loop Zone Hold controller math (pure, no BLE/clock).

use std::time::Duration;

use super::{MIN_SPEED_CHANGE, Tracking};
use crate::speed::CentiKmh;

/// Linear warm-up target: `start` at `elapsed = 0`, `target` at
/// `elapsed >= warmup`. HR is never read here — see task doc §Жизненный цикл.
/// Interpolation stays in f32 (compute, not compare); the result is re-quantized
/// to [`CentiKmh`] so callers compare/write on integers. A zero `warmup` skips
/// straight to `target` (defensive; a hand-edited `0` must not divide by zero).
pub fn warmup_target_speed(
    start: CentiKmh,
    target: CentiKmh,
    elapsed: Duration,
    warmup: Duration,
) -> CentiKmh {
    if warmup.is_zero() || elapsed >= warmup {
        return target;
    }
    let frac = elapsed.as_secs_f32() / warmup.as_secs_f32();
    let kmh = start.to_kmh_f32() + (target.to_kmh_f32() - start.to_kmh_f32()) * frac;
    CentiKmh::from_kmh_f32(kmh).unwrap_or(target)
}

/// Inputs the pure closed-loop controller needs for one correction — bundled
/// so `next_speed` reads as one call instead of an 8-parameter list.
#[derive(Debug, Clone, Copy)]
pub struct ControllerParams {
    pub tracking: Tracking,
    pub zone_low_bpm: u16,
    pub zone_high_bpm: u16,
    pub deadband_bpm: i64,
    pub max_step_kmh: CentiKmh,
    pub min_speed_kmh: CentiKmh,
    pub max_speed_kmh: CentiKmh,
}

/// One closed-loop correction (task doc §Control-loop). `None` means "leave
/// the belt speed alone" — either the bpm is where it should be, or the belt
/// is already pinned at the clamp in the direction the correction would move
/// it (task doc §Границы достижимости: never chase past min/max).
pub fn next_speed(params: &ControllerParams, current: CentiKmh, bpm: u16) -> Option<CentiKmh> {
    let low = params.zone_low_bpm as f32;
    let high = params.zone_high_bpm as f32;
    let bpm = bpm as f32;

    // Step magnitude may be fractional under Center tracking; quantize after
    // applying so clamp/deadband stay on integer wire units.
    let step_kmh = match params.tracking {
        Tracking::Band => {
            if bpm >= low && bpm <= high {
                return None;
            }
            if bpm < low {
                params.max_step_kmh.to_kmh_f32() // below zone → speed up
            } else {
                -params.max_step_kmh.to_kmh_f32() // above zone → slow down
            }
        }
        Tracking::Center => {
            let center = (low + high) / 2.0;
            let half_width = (high - low) / 2.0;
            let error = bpm - center;
            if error.abs() <= params.deadband_bpm as f32 || half_width <= 0.0 {
                return None;
            }
            // Scaled so the step reaches max_step right at the zone boundary
            // (task doc §Режимы: "у границ зоны шаг максимален").
            let max_step = params.max_step_kmh.to_kmh_f32();
            let k = max_step / half_width;
            let magnitude = (k * error.abs()).min(max_step);
            if error > 0.0 {
                -magnitude // above centre → slow down
            } else {
                magnitude // below centre → speed up
            }
        }
    };

    let unclamped = CentiKmh::from_kmh_f32(current.to_kmh_f32() + step_kmh).unwrap_or({
        if step_kmh >= 0.0 {
            CentiKmh::MAX_SANE
        } else {
            CentiKmh::ZERO
        }
    });
    let target = unclamped.clamp(params.min_speed_kmh, params.max_speed_kmh);
    (target.abs_diff(current) > MIN_SPEED_CHANGE.to_wire()).then_some(target)
}

/// Target for a safety-cap force-reduce write (задача 041): drop by `2 * max_step`,
/// clamp to `min_speed`. Returns `None` when the computed target is within
/// [`MIN_SPEED_CHANGE`] of the measured speed — same deadband as
/// [`next_speed`] — so a belt already at the floor does not spam no-op Control
/// Point writes (and double beeps) every safety cooldown.
pub fn safety_force_reduce_target(
    measured: CentiKmh,
    max_step: CentiKmh,
    min_speed: CentiKmh,
) -> Option<CentiKmh> {
    let drop = max_step.to_wire().saturating_mul(2);
    let target = measured.saturating_sub_centi(drop).max(min_speed);
    (target.abs_diff(measured) > MIN_SPEED_CHANGE.to_wire()).then_some(target)
}

#[cfg(test)]
mod tests {
    use super::super::{
        DEFAULT_DEADBAND_BPM, DEFAULT_MAX_SPEED, DEFAULT_MAX_STEP, DEFAULT_MIN_SPEED, Tracking,
    };
    use super::*;

    #[test]
    fn warmup_ramps_linearly_then_snaps_to_target() {
        let start = CentiKmh::from_wire(100);
        let target = CentiKmh::from_wire(300);
        let warmup = Duration::from_secs(300);
        assert_eq!(
            warmup_target_speed(start, target, Duration::ZERO, warmup),
            start
        );
        assert_eq!(
            warmup_target_speed(start, target, Duration::from_secs(150), warmup),
            CentiKmh::from_wire(200)
        );
        assert_eq!(
            warmup_target_speed(start, target, Duration::from_secs(300), warmup),
            target
        );
        assert_eq!(
            warmup_target_speed(start, target, Duration::from_secs(600), warmup),
            target
        );
    }

    #[test]
    fn warmup_target_is_monotone_non_decreasing_across_ticks() {
        let start = CentiKmh::from_wire(200);
        let target = CentiKmh::from_wire(400);
        let warmup = Duration::from_secs(300);
        let mut prev = start;
        for secs in 0..=300 {
            let next = warmup_target_speed(start, target, Duration::from_secs(secs), warmup);
            assert!(next >= prev, "secs={secs}: {next:?} < {prev:?}");
            prev = next;
        }
        assert_eq!(prev, target);
    }

    fn band_params() -> ControllerParams {
        ControllerParams {
            tracking: Tracking::Band,
            zone_low_bpm: 112,
            zone_high_bpm: 131,
            deadband_bpm: DEFAULT_DEADBAND_BPM,
            max_step_kmh: DEFAULT_MAX_STEP,
            min_speed_kmh: DEFAULT_MIN_SPEED,
            max_speed_kmh: DEFAULT_MAX_SPEED,
        }
    }

    fn c(kmh: f32) -> CentiKmh {
        CentiKmh::from_kmh_f32(kmh).expect("test speed in range")
    }

    #[test]
    fn band_mode_does_not_correct_inside_the_zone() {
        let params = band_params();
        assert_eq!(next_speed(&params, c(3.0), 112), None);
        assert_eq!(next_speed(&params, c(3.0), 120), None);
        assert_eq!(next_speed(&params, c(3.0), 131), None);
    }

    #[test]
    fn band_mode_steps_toward_the_zone_when_outside() {
        let params = band_params();
        // Below zone → speed up by max_step.
        assert_eq!(next_speed(&params, c(3.0), 100), Some(c(3.3)));
        // Above zone → slow down by max_step.
        assert_eq!(next_speed(&params, c(3.0), 140), Some(c(2.7)));
    }

    #[test]
    fn band_mode_clamps_to_min_max_and_reports_no_change_at_the_pin() {
        let params = band_params();
        // Already at max, HR still low → stays pinned, no spurious "change".
        assert_eq!(next_speed(&params, DEFAULT_MAX_SPEED, 100), None);
        // Already at min, HR still high → stays pinned.
        assert_eq!(next_speed(&params, DEFAULT_MIN_SPEED, 140), None);
    }

    #[test]
    fn safety_force_reduce_target_skips_noop_at_min() {
        // Pinned at min → target collapses to min ≈ measured → None (задача 041).
        assert_eq!(
            safety_force_reduce_target(DEFAULT_MIN_SPEED, DEFAULT_MAX_STEP, DEFAULT_MIN_SPEED),
            None
        );
        // Within deadband of min after reduce → still None.
        assert_eq!(
            safety_force_reduce_target(c(2.02), DEFAULT_MAX_STEP, DEFAULT_MIN_SPEED),
            None
        );
        // Real reduce: measured well above min → Some(min) when 2*step would go under.
        assert_eq!(
            safety_force_reduce_target(c(2.3), DEFAULT_MAX_STEP, DEFAULT_MIN_SPEED),
            Some(DEFAULT_MIN_SPEED)
        );
        // Large measured → drop by 2*step without hitting floor.
        assert_eq!(
            safety_force_reduce_target(c(4.0), DEFAULT_MAX_STEP, DEFAULT_MIN_SPEED),
            Some(c(3.4))
        );
    }

    #[test]
    fn quantize_identity_pins_at_clamp_without_epsilon() {
        // Acceptance 054: telemetry-vs-config meet at one CentiKmh, so pinned
        // at the clamp returns None *exactly* (no float glue).
        let params = band_params();
        let tele = CentiKmh::from_kmh_f32(320f32 * 0.01).expect("telemetry path");
        let conf = CentiKmh::from_kmh_f32(3.2).expect("config path");
        assert_eq!(tele, conf);
        let mut pinned = params;
        pinned.max_speed_kmh = conf;
        assert_eq!(next_speed(&pinned, tele, 100), None);
    }

    #[test]
    fn deadband_policy_masks_small_diffs_not_real_corrections() {
        let params = band_params();
        // diff ≤ 5 centi → None (controller deadband).
        assert_eq!(
            next_speed(&params, DEFAULT_MAX_SPEED.saturating_sub_centi(2), 100),
            None
        );
        assert_eq!(
            next_speed(&params, DEFAULT_MAX_SPEED.saturating_sub_centi(5), 100),
            None
        );
        // diff > 5 → Some; a genuine max_step-sized correction is not muted.
        assert_eq!(
            next_speed(&params, DEFAULT_MAX_SPEED.saturating_sub_centi(20), 100),
            Some(DEFAULT_MAX_SPEED)
        );
        assert_eq!(
            next_speed(&params, c(3.0), 100),
            Some(c(3.0).saturating_add_centi(DEFAULT_MAX_STEP.to_wire()))
        );
    }

    fn center_params() -> ControllerParams {
        ControllerParams {
            tracking: Tracking::Center,
            ..band_params()
        }
    }

    #[test]
    fn center_mode_does_not_correct_within_deadband_of_the_midpoint() {
        let params = center_params();
        // Midpoint of 112-131 is 121.5.
        assert_eq!(next_speed(&params, c(3.0), 122), None); // within ±3
        assert_eq!(next_speed(&params, c(3.0), 119), None);
    }

    #[test]
    fn center_mode_is_more_aggressive_near_the_boundary_than_near_the_midpoint() {
        let params = center_params();
        // Small deviation past the deadband → small step.
        let near = next_speed(&params, c(3.0), 126).expect("some correction");
        // At the boundary → step saturates at max_step.
        let at_boundary = next_speed(&params, c(3.0), 131).expect("some correction");
        let near_step = near.abs_diff(c(3.0));
        let boundary_step = at_boundary.abs_diff(c(3.0));
        assert!(near_step < boundary_step);
        assert_eq!(boundary_step, DEFAULT_MAX_STEP.to_wire());
    }

    #[test]
    fn center_mode_direction_matches_band_mode() {
        let params = center_params();
        // Below centre → speed up; above centre → slow down.
        assert!(next_speed(&params, c(3.0), 100).unwrap() > c(3.0));
        assert!(next_speed(&params, c(3.0), 140).unwrap() < c(3.0));
    }
}
