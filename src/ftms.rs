//! Fitness Machine Service (FTMS) constants and Treadmill Data parsing.
//!
//! Most BLE treadmills expose the standard GATT **Fitness Machine Service**
//! (`0x1826`). Yesoul hardware may additionally expose a vendor-specific
//! service used by its mobile app; that path still needs reverse engineering
//! and is deliberately not assumed here.
//!
//! Spec: Bluetooth SIG — Fitness Machine Service 1.0 / GATT Specification
//! Supplement (Treadmill Data 0x2ACD).

use uuid::Uuid;

/// Fitness Machine Service — `0x1826`.
pub const FITNESS_MACHINE_SERVICE: Uuid = Uuid::from_u128(0x00001826_0000_1000_8000_00805f9b34fb);

/// Treadmill Data characteristic — `0x2ACD` (notify).
pub const TREADMILL_DATA: Uuid = Uuid::from_u128(0x00002acd_0000_1000_8000_00805f9b34fb);

/// Fitness Machine Control Point — `0x2AD9` (write / indicate).
/// Reserved for start/stop/speed/incline commands (not yet wired up).
#[allow(dead_code)]
pub const FITNESS_MACHINE_CONTROL_POINT: Uuid =
    Uuid::from_u128(0x00002ad9_0000_1000_8000_00805f9b34fb);

/// Fitness Machine Status — `0x2ADA` (notify). Device-initiated events
/// (start/stop/pause by the operator's own remote, target changes) —
/// independent of and a cross-check against our own speed/steps-derived
/// presence heuristic.
pub const FITNESS_MACHINE_STATUS: Uuid = Uuid::from_u128(0x00002ada_0000_1000_8000_00805f9b34fb);

/// Human-readable name for a Fitness Machine Status op code (first byte of
/// the `0x2ADA` payload), for logging only — the raw code is what's persisted.
///
/// Spec: GATT Specification Supplement, Fitness Machine Service, Machine
/// Status Op Code.
pub fn describe_status_event(event_code: u8) -> &'static str {
    match event_code {
        0x01 => "Reset",
        0x02 => "StoppedOrPausedByUser",
        0x03 => "StoppedBySafetyKey",
        0x04 => "StartedOrResumedByUser",
        0x05 => "TargetSpeedChanged",
        0x06 => "TargetInclineChanged",
        0x07 => "TargetResistanceLevelChanged",
        0x08 => "TargetPowerChanged",
        0x09 => "TargetHeartRateChanged",
        0x0a => "TargetedExpendedEnergyChanged",
        0x0b => "TargetedNumberOfStepsChanged",
        0x0c => "TargetedNumberOfStridesChanged",
        0x0d => "TargetedDistanceChanged",
        0x0e => "TargetedTrainingTimeChanged",
        0x0f => "TargetedTimeInTwoHeartRateZonesChanged",
        0x10 => "TargetedTimeInThreeHeartRateZonesChanged",
        0x11 => "TargetedTimeInFiveHeartRateZonesChanged",
        0x12 => "IndoorBikeSimulationParametersChanged",
        0x13 => "WheelCircumferenceChanged",
        0x14 => "SpinDownStatus",
        0x15 => "TargetedCadenceChanged",
        0xff => "ControlPermissionLost",
        _ => "Unknown",
    }
}

/// A single decoded Treadmill Data notification.
///
/// Only the fields relevant to a first-cut connector are decoded. The flags
/// field of the raw packet is a bitmask describing which optional fields are
/// present; more fields can be added incrementally as they are observed.
#[derive(Debug, Clone, Default)]
pub struct TreadmillData {
    /// Instantaneous speed, km/h.
    pub speed_kmh: Option<f32>,
    /// Average speed, km/h.
    pub avg_speed_kmh: Option<f32>,
    /// Instantaneous incline, percent.
    pub incline_percent: Option<f32>,
    /// Total distance, meters.
    pub total_distance_m: Option<u32>,
    /// Total expended energy, kcal.
    pub total_energy_kcal: Option<u16>,
    /// Elapsed workout time, seconds.
    pub elapsed_s: Option<u16>,
    /// Step count — Yesoul vendor extension carried in the flag-13 slot
    /// (officially RFU in FTMS 1.0); verified live against the W2 Pro console.
    pub steps: Option<u32>,
}

/// Flag bits of the Treadmill Data packet (GATT Specification Supplement).
mod flags {
    /// Bit 0: `0` = instantaneous speed present (note the inverted semantics).
    pub const MORE_DATA: u16 = 1 << 0;
    pub const AVG_SPEED_PRESENT: u16 = 1 << 1;
    pub const TOTAL_DISTANCE_PRESENT: u16 = 1 << 2;
    pub const INCLINATION_PRESENT: u16 = 1 << 3;
    pub const ELEVATION_GAIN_PRESENT: u16 = 1 << 4;
    pub const INSTANT_PACE_PRESENT: u16 = 1 << 5;
    pub const AVG_PACE_PRESENT: u16 = 1 << 6;
    pub const ENERGY_PRESENT: u16 = 1 << 7;
    pub const HEART_RATE_PRESENT: u16 = 1 << 8;
    pub const MET_PRESENT: u16 = 1 << 9;
    pub const ELAPSED_TIME_PRESENT: u16 = 1 << 10;
    pub const REMAINING_TIME_PRESENT: u16 = 1 << 11;
    pub const FORCE_POWER_PRESENT: u16 = 1 << 12;
    /// RFU in the spec; Yesoul uses it for a uint24 step counter.
    pub const VENDOR_STEPS_PRESENT: u16 = 1 << 13;
}

/// Parse a raw Treadmill Data (`0x2ACD`) notification payload.
///
/// Returns `None` when the payload is too short to even hold the flags field.
/// Unknown / not-yet-decoded optional fields are skipped by advancing the
/// cursor, so partial support stays correct as long as field ordering matches
/// the spec.
pub fn parse_treadmill_data(payload: &[u8]) -> Option<TreadmillData> {
    if payload.len() < 2 {
        return None;
    }

    let flags = u16::from_le_bytes([payload[0], payload[1]]);
    let mut cursor = 2usize;
    let mut data = TreadmillData::default();

    // Instantaneous Speed is present unless the "More Data" bit is set.
    if flags & flags::MORE_DATA == 0 {
        let raw = read_u16(payload, &mut cursor)?;
        // Unit: 0.01 km/h.
        data.speed_kmh = Some(raw as f32 * 0.01);
    }

    if flags & flags::AVG_SPEED_PRESENT != 0 {
        let raw = read_u16(payload, &mut cursor)?;
        // Unit: 0.01 km/h.
        data.avg_speed_kmh = Some(raw as f32 * 0.01);
    }

    if flags & flags::TOTAL_DISTANCE_PRESENT != 0 {
        // Total Distance is a uint24 (meters).
        let raw = read_u24(payload, &mut cursor)?;
        data.total_distance_m = Some(raw);
    }

    if flags & flags::INCLINATION_PRESENT != 0 {
        // Inclination (sint16, 0.1 %). Grade angle (sint16) follows — skipped.
        let raw = read_i16(payload, &mut cursor)?;
        data.incline_percent = Some(raw as f32 * 0.1);
        read_i16(payload, &mut cursor)?; // Ramp Angle Setting.
    }

    if flags & flags::ELEVATION_GAIN_PRESENT != 0 {
        read_u16(payload, &mut cursor)?; // Positive Elevation Gain.
        read_u16(payload, &mut cursor)?; // Negative Elevation Gain.
    }

    if flags & flags::INSTANT_PACE_PRESENT != 0 {
        read_u8(payload, &mut cursor)?;
    }

    if flags & flags::AVG_PACE_PRESENT != 0 {
        read_u8(payload, &mut cursor)?;
    }

    if flags & flags::ENERGY_PRESENT != 0 {
        let total = read_u16(payload, &mut cursor)?;
        // 0xFFFF means "data not available" per spec.
        if total != u16::MAX {
            data.total_energy_kcal = Some(total);
        }
        read_u16(payload, &mut cursor)?; // Energy Per Hour.
        read_u8(payload, &mut cursor)?; // Energy Per Minute.
    }

    if flags & flags::HEART_RATE_PRESENT != 0 {
        read_u8(payload, &mut cursor)?;
    }

    if flags & flags::MET_PRESENT != 0 {
        read_u8(payload, &mut cursor)?;
    }

    if flags & flags::ELAPSED_TIME_PRESENT != 0 {
        data.elapsed_s = Some(read_u16(payload, &mut cursor)?);
    }

    if flags & flags::REMAINING_TIME_PRESENT != 0 {
        read_u16(payload, &mut cursor)?;
    }

    if flags & flags::FORCE_POWER_PRESENT != 0 {
        read_i16(payload, &mut cursor)?; // Force on Belt.
        read_i16(payload, &mut cursor)?; // Power Output.
    }

    if flags & flags::VENDOR_STEPS_PRESENT != 0 {
        data.steps = Some(read_u24(payload, &mut cursor)?);
    }

    Some(data)
}

fn read_u8(buf: &[u8], cursor: &mut usize) -> Option<u8> {
    let byte = *buf.get(*cursor)?;
    *cursor += 1;
    Some(byte)
}

fn read_u16(buf: &[u8], cursor: &mut usize) -> Option<u16> {
    let end = *cursor + 2;
    let slice = buf.get(*cursor..end)?;
    *cursor = end;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_i16(buf: &[u8], cursor: &mut usize) -> Option<i16> {
    read_u16(buf, cursor).map(|v| v as i16)
}

fn read_u24(buf: &[u8], cursor: &mut usize) -> Option<u32> {
    let end = *cursor + 3;
    let slice = buf.get(*cursor..end)?;
    *cursor = end;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], 0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_speed_only() {
        // flags = 0 (speed present, nothing else), speed = 500 -> 5.00 km/h.
        let payload = [0x00, 0x00, 0xf4, 0x01];
        let data = parse_treadmill_data(&payload).expect("should parse");
        assert_eq!(data.speed_kmh, Some(5.0));
        assert_eq!(data.total_distance_m, None);
    }

    #[test]
    fn parses_speed_distance_and_incline() {
        // flags: TOTAL_DISTANCE | INCLINATION = 0b1100 = 0x000c.
        let mut payload = vec![0x0c, 0x00];
        payload.extend_from_slice(&[0xf4, 0x01]); // speed 5.00 km/h
        payload.extend_from_slice(&[0x64, 0x00, 0x00]); // distance 100 m (u24)
        payload.extend_from_slice(&[0x1e, 0x00]); // incline 3.0 %
        payload.extend_from_slice(&[0x00, 0x00]); // ramp angle
        let data = parse_treadmill_data(&payload).expect("should parse");
        assert_eq!(data.speed_kmh, Some(5.0));
        assert_eq!(data.total_distance_m, Some(100));
        assert_eq!(data.incline_percent, Some(3.0));
    }

    #[test]
    fn rejects_truncated_payload() {
        assert!(parse_treadmill_data(&[0x00]).is_none());
    }

    #[test]
    fn parses_real_w2pro_frame() {
        // Captured live from a YS_W2PRO_02395: flags 0x2486 = avg speed,
        // distance, energy, elapsed time, vendor steps (bit 13).
        let payload = [
            0x86, 0x24, // flags
            0xfa, 0x00, // speed 2.50 km/h
            0xd9, 0x00, // avg speed 2.17 km/h
            0xa6, 0x05, 0x00, // distance 1446 m
            0x55, 0x00, // total energy 85 kcal
            0xff, 0xff, // energy/hour: not available
            0xff, // energy/min: not available
            0x57, 0x09, // elapsed 2391 s
            0xf3, 0x0d, 0x00, // steps 3571
        ];
        let data = parse_treadmill_data(&payload).expect("should parse");
        assert_eq!(data.speed_kmh, Some(2.5));
        let avg = data.avg_speed_kmh.expect("avg speed present");
        assert!((avg - 2.17).abs() < 1e-4, "avg speed {avg} != ~2.17");
        assert_eq!(data.total_distance_m, Some(1446));
        assert_eq!(data.total_energy_kcal, Some(85));
        assert_eq!(data.elapsed_s, Some(2391));
        assert_eq!(data.steps, Some(3571));
        assert_eq!(data.incline_percent, None);
    }
}
