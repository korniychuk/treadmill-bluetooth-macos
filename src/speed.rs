//! Belt speed in FTMS wire units (0.01 km/h).
//!
//! FTMS encodes speed as a little-endian `u16` of centi-km/h. Config/CLI enter
//! as floats. Comparing those two float paths bit-wise caused noop Control
//! Point writes (задача 030). This newtype quantizes at the boundary so
//! compare/clamp on wire speeds is exact integer arithmetic.

use std::fmt;

/// Belt speed in FTMS wire units (0.01 km/h). The only type in which wire
/// speeds are compared or clamped — comparisons are exact integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CentiKmh(u16);

impl CentiKmh {
    pub const ZERO: Self = Self(0);
    /// Sane command ceiling (25 km/h), mirrors the old range check in
    /// `control.rs::set_speed`.
    pub const MAX_SANE: Self = Self(2500);

    /// Lossless: the wire `u16` *is* the value.
    #[must_use]
    pub const fn from_wire(raw: u16) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn to_wire(self) -> u16 {
        self.0
    }

    /// Quantize a config/CLI float, half-up: `(kmh * 100.0).round()`.
    /// `None` on NaN / negative / overflow past `u16`.
    #[must_use]
    pub fn from_kmh_f32(kmh: f32) -> Option<Self> {
        if !kmh.is_finite() || kmh < 0.0 {
            return None;
        }
        let rounded = (kmh * 100.0).round();
        if rounded > f32::from(u16::MAX) {
            return None;
        }
        // Non-negative finite and ≤ u16::MAX → safe cast.
        Some(Self(rounded as u16))
    }

    /// Sole reverse conversion for display/statistics.
    #[must_use]
    pub fn to_kmh_f32(self) -> f32 {
        f32::from(self.0) * 0.01
    }

    /// Integer clamp via [`Ord`] (no float path).
    #[must_use]
    pub fn clamp(self, min: Self, max: Self) -> Self {
        if self < min {
            min
        } else if self > max {
            max
        } else {
            self
        }
    }

    #[must_use]
    pub fn abs_diff(self, other: Self) -> u16 {
        self.0.abs_diff(other.0)
    }

    #[must_use]
    pub fn saturating_add_centi(self, delta: u16) -> Self {
        Self(self.0.saturating_add(delta))
    }

    #[must_use]
    pub fn saturating_sub_centi(self, delta: u16) -> Self {
        Self(self.0.saturating_sub(delta))
    }
}

/// Human-readable km/h (`"3.2"`, `"0"`, `"2.05"`) — matches the textual wire
/// form of the `control_commands` queue (`speed:2.5`) and is parseable back
/// via `f32` → [`CentiKmh::from_kmh_f32`].
impl fmt::Display for CentiKmh {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / 100;
        let frac = self.0 % 100;
        if frac == 0 {
            write!(f, "{whole}")
        } else if frac.is_multiple_of(10) {
            write!(f, "{whole}.{}", frac / 10)
        } else {
            write!(f, "{whole}.{frac:02}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_identity_over_full_u16_range() {
        for raw in 0..=u16::MAX {
            let wire = CentiKmh::from_wire(raw);
            let back = CentiKmh::from_kmh_f32(wire.to_kmh_f32());
            assert_eq!(back, Some(wire), "raw={raw}");
        }
    }

    #[test]
    fn repro_030_telemetry_and_config_meet_at_same_centi() {
        // Telemetry path: raw * 0.01 is not exact in binary32 (3.1999998).
        let telemetry = 320f32 * 0.01;
        // Config path: TOML literal 3.2.
        let config = 3.2f32;
        assert_eq!(
            CentiKmh::from_kmh_f32(telemetry),
            Some(CentiKmh::from_wire(320))
        );
        assert_eq!(
            CentiKmh::from_kmh_f32(config),
            Some(CentiKmh::from_wire(320))
        );
        assert_eq!(
            CentiKmh::from_kmh_f32(telemetry),
            CentiKmh::from_kmh_f32(config)
        );
    }

    #[test]
    fn half_up_round_and_rejects_invalid() {
        // Honest f32 examples (3.145 itself is ~3.1449998 in binary32).
        assert_eq!(CentiKmh::from_kmh_f32(0.045), Some(CentiKmh::from_wire(5)));
        assert_eq!(CentiKmh::from_kmh_f32(0.055), Some(CentiKmh::from_wire(6)));
        assert_eq!(CentiKmh::from_kmh_f32(3.2), Some(CentiKmh::from_wire(320)));
        assert_eq!(CentiKmh::from_kmh_f32(f32::NAN), None);
        assert_eq!(CentiKmh::from_kmh_f32(-0.1), None);
        // 700 km/h → 70000 centi > u16::MAX.
        assert_eq!(CentiKmh::from_kmh_f32(700.0), None);
    }

    #[test]
    fn display_is_human_readable_kmh() {
        assert_eq!(CentiKmh::from_wire(0).to_string(), "0");
        assert_eq!(CentiKmh::from_wire(250).to_string(), "2.5");
        assert_eq!(CentiKmh::from_wire(320).to_string(), "3.2");
        assert_eq!(CentiKmh::from_wire(205).to_string(), "2.05");
        assert_eq!(CentiKmh::from_wire(200).to_string(), "2");
    }

    #[test]
    fn clamp_and_saturating_ops() {
        let v = CentiKmh::from_wire(300);
        assert_eq!(
            v.clamp(CentiKmh::from_wire(200), CentiKmh::from_wire(450)),
            CentiKmh::from_wire(300)
        );
        assert_eq!(
            CentiKmh::from_wire(100).clamp(CentiKmh::from_wire(200), CentiKmh::from_wire(450)),
            CentiKmh::from_wire(200)
        );
        assert_eq!(
            CentiKmh::from_wire(500).clamp(CentiKmh::from_wire(200), CentiKmh::from_wire(450)),
            CentiKmh::from_wire(450)
        );
        assert_eq!(
            CentiKmh::from_wire(u16::MAX).saturating_add_centi(1),
            CentiKmh::from_wire(u16::MAX)
        );
        assert_eq!(CentiKmh::ZERO.saturating_sub_centi(1), CentiKmh::ZERO);
        assert_eq!(
            CentiKmh::from_wire(320).abs_diff(CentiKmh::from_wire(315)),
            5
        );
    }

    #[test]
    fn display_round_trips_via_from_kmh_f32() {
        for raw in [0u16, 1, 5, 80, 200, 250, 320, 450, 2500, 9999] {
            let c = CentiKmh::from_wire(raw);
            let parsed: f32 = c.to_string().parse().expect("display is parseable f32");
            assert_eq!(CentiKmh::from_kmh_f32(parsed), Some(c), "display={}", c);
        }
    }
}
