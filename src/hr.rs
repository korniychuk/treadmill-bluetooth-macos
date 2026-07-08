//! Heart Rate Service (`0x180D`) constants and Heart Rate Measurement parsing.
//!
//! A Polar H10 (and most chest straps) advertise the standard GATT **Heart
//! Rate Service**. Sensor-contact loss makes the strap stop advertising
//! entirely once removed — a natural "sensor off" signal, no polling needed.
//!
//! Spec: Bluetooth SIG — Heart Rate Service 1.0 (Heart Rate Measurement 0x2A37).

use uuid::Uuid;

/// Heart Rate Service — `0x180D`.
pub const HEART_RATE_SERVICE: Uuid = Uuid::from_u128(0x0000180d_0000_1000_8000_00805f9b34fb);

/// Heart Rate Measurement characteristic — `0x2A37` (notify).
pub const HEART_RATE_MEASUREMENT: Uuid = Uuid::from_u128(0x00002a37_0000_1000_8000_00805f9b34fb);

/// Flag bits of the Heart Rate Measurement packet (Bluetooth SIG GATT spec).
mod flags {
    /// Bit 0: `0` = bpm is UINT8, `1` = bpm is UINT16.
    pub const BPM_U16: u8 = 1 << 0;
    /// Bit 1: sensor contact detected (only meaningful if bit 2 is set).
    pub const CONTACT_DETECTED: u8 = 1 << 1;
    /// Bit 2: sensor-contact feature is supported by this device at all.
    pub const CONTACT_SUPPORTED: u8 = 1 << 2;
    pub const ENERGY_PRESENT: u8 = 1 << 3;
    pub const RR_PRESENT: u8 = 1 << 4;
}

/// A single decoded Heart Rate Measurement notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HrMeasurement {
    pub bpm: u16,
    /// `None` when the device doesn't support sensor-contact reporting at all.
    pub contact: Option<bool>,
    /// RR-intervals in milliseconds, decoded from the 1/1024s wire unit.
    pub rr_ms: Vec<u16>,
}

/// Parse a raw Heart Rate Measurement (`0x2A37`) notification payload.
///
/// Returns `None` for a too-short payload, or when the decoded bpm is `0` —
/// the H10 sends `bpm==0` on contact loss, which is normal (removed/repositioned
/// strap) rather than an error, so it is dropped here at DEBUG rather than
/// surfaced as a WARN-worthy anomaly.
pub fn parse_hr_measurement(payload: &[u8]) -> Option<HrMeasurement> {
    if payload.is_empty() {
        return None;
    }
    let flag_byte = payload[0];
    let mut cursor = 1usize;

    let bpm = if flag_byte & flags::BPM_U16 != 0 {
        read_u16(payload, &mut cursor)?
    } else {
        read_u8(payload, &mut cursor)? as u16
    };

    if bpm == 0 {
        tracing::debug!("heart rate measurement with bpm=0 (sensor contact lost?) — dropping");
        return None;
    }

    let contact = if flag_byte & flags::CONTACT_SUPPORTED != 0 {
        Some(flag_byte & flags::CONTACT_DETECTED != 0)
    } else {
        None
    };

    if flag_byte & flags::ENERGY_PRESENT != 0 {
        read_u16(payload, &mut cursor)?; // Energy Expended (kJ) — not decoded yet.
    }

    let mut rr_ms = Vec::new();
    if flag_byte & flags::RR_PRESENT != 0 {
        while cursor + 2 <= payload.len() {
            let raw = read_u16(payload, &mut cursor)?;
            // RR-Interval unit is 1/1024 second.
            rr_ms.push(((raw as f32 / 1024.0) * 1000.0).round() as u16);
        }
    }

    Some(HrMeasurement {
        bpm,
        contact,
        rr_ms,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_u8_bpm_no_contact_no_rr() {
        // flags = 0x00 (u8 bpm, no contact support, no energy, no RR).
        let payload = [0x00, 118];
        let m = parse_hr_measurement(&payload).expect("should parse");
        assert_eq!(m.bpm, 118);
        assert_eq!(m.contact, None);
        assert!(m.rr_ms.is_empty());
    }

    #[test]
    fn parses_u16_bpm() {
        // flags = 0x01 (u16 bpm), bpm = 300 (le).
        let payload = [0x01, 0x2c, 0x01];
        let m = parse_hr_measurement(&payload).expect("should parse");
        assert_eq!(m.bpm, 300);
    }

    #[test]
    fn parses_contact_detected_and_not_detected() {
        // flags = 0b0000_0110 = contact supported + detected.
        let detected = [0b0000_0110, 100];
        let m = parse_hr_measurement(&detected).expect("should parse");
        assert_eq!(m.contact, Some(true));

        // flags = 0b0000_0100 = contact supported, NOT detected.
        let not_detected = [0b0000_0100, 100];
        let m = parse_hr_measurement(&not_detected).expect("should parse");
        assert_eq!(m.contact, Some(false));
    }

    #[test]
    fn parses_rr_intervals() {
        // flags = 0x10 (RR present), u8 bpm = 90, one RR interval of 1024 (=1000ms).
        let payload = [0x10, 90, 0x00, 0x04];
        let m = parse_hr_measurement(&payload).expect("should parse");
        assert_eq!(m.bpm, 90);
        assert_eq!(m.rr_ms, vec![1000]);
    }

    #[test]
    fn parses_multiple_rr_intervals() {
        // flags = 0x10, u8 bpm = 90, two RR intervals: 1024 (1000ms), 512 (500ms).
        let payload = [0x10, 90, 0x00, 0x04, 0x00, 0x02];
        let m = parse_hr_measurement(&payload).expect("should parse");
        assert_eq!(m.rr_ms, vec![1000, 500]);
    }

    #[test]
    fn bpm_zero_is_dropped_as_contact_loss() {
        let payload = [0x00, 0];
        assert_eq!(parse_hr_measurement(&payload), None);
    }

    #[test]
    fn rejects_empty_payload() {
        assert_eq!(parse_hr_measurement(&[]), None);
    }

    #[test]
    fn rejects_truncated_u16_bpm() {
        // flags say u16 bpm but only one byte follows.
        assert_eq!(parse_hr_measurement(&[0x01, 0x2c]), None);
    }
}
