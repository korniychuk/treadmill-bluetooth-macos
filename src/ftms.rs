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

/// Fitness Machine Status — `0x2ADA` (notify). Reserved for future use.
#[allow(dead_code)]
pub const FITNESS_MACHINE_STATUS: Uuid = Uuid::from_u128(0x00002ada_0000_1000_8000_00805f9b34fb);

/// A single decoded Treadmill Data notification.
///
/// Only the fields relevant to a first-cut connector are decoded. The flags
/// field of the raw packet is a bitmask describing which optional fields are
/// present; more fields can be added incrementally as they are observed.
#[derive(Debug, Clone, Default)]
pub struct TreadmillData {
    /// Instantaneous speed, km/h.
    pub speed_kmh: Option<f32>,
    /// Instantaneous incline, percent.
    pub incline_percent: Option<f32>,
    /// Total distance, meters.
    pub total_distance_m: Option<u32>,
}

/// Flag bits of the Treadmill Data packet (GATT Specification Supplement).
mod flags {
    /// Bit 0: `0` = instantaneous speed present (note the inverted semantics).
    pub const MORE_DATA: u16 = 1 << 0;
    pub const AVG_SPEED_PRESENT: u16 = 1 << 1;
    pub const TOTAL_DISTANCE_PRESENT: u16 = 1 << 2;
    pub const INCLINATION_PRESENT: u16 = 1 << 3;
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
        // Average Speed (uint16, 0.01 km/h) — skipped for now.
        read_u16(payload, &mut cursor)?;
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

    Some(data)
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
}
