//! Heart Rate Service (`0x180D`) constants and Heart Rate Measurement parsing.
//!
//! A Polar H10 (and most chest straps) advertise the standard GATT **Heart
//! Rate Service**.
//!
//! **Contact loss is not a disconnect** (задача 033). A removed H10 keeps the
//! BLE link up and keeps notifying ~1/s — but with the *last* valid bpm frozen
//! bit-for-bit, no `bpm==0`, and no sensor-contact bits. Observed live:
//! `10 75 72 03 27 02` (worn: RR present, 117 bpm) → `00 6F` (removed: 111 bpm),
//! and — the trap — the odd `10 6F FB 17` still carrying RR while the strap lies
//! on a desk. So neither `bpm==0`, nor the contact bits, nor RR presence alone
//! can be trusted; [`ContactTracker`] combines them with a frozen-bpm clock.
//!
//! Spec: Bluetooth SIG — Heart Rate Service 1.0 (Heart Rate Measurement 0x2A37).

use uuid::Uuid;

/// Heart Rate Service — `0x180D`.
pub const HEART_RATE_SERVICE: Uuid = Uuid::from_u128(0x0000180d_0000_1000_8000_00805f9b34fb);

/// Heart Rate Measurement characteristic — `0x2A37` (notify).
pub const HEART_RATE_MEASUREMENT: Uuid = Uuid::from_u128(0x00002a37_0000_1000_8000_00805f9b34fb);

/// Battery Service — `0x180F` (задача 026). Kept for documentation/future use
/// (e.g. scoping a service-filtered scan) — `scan::read_hr_battery` looks up
/// the characteristic directly, without checking its parent service.
#[allow(dead_code)]
pub const BATTERY_SERVICE: Uuid = Uuid::from_u128(0x0000180f_0000_1000_8000_00805f9b34fb);

/// Battery Level characteristic — `0x2A19` (read; Polar devices don't push
/// notify updates, so this must be re-read periodically rather than
/// subscribed once).
pub const BATTERY_LEVEL: Uuid = Uuid::from_u128(0x00002a19_0000_1000_8000_00805f9b34fb);

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

/// How many consecutive RR-less frames from an RR-capable sensor mean the strap
/// has left the body (задача 033). Frames arrive ~1/s, so this is ~3s of
/// evidence — long enough to ride out a single dropped/split notification,
/// short enough that the widget's heart disappears about when the operator
/// expects it to.
const CONTACT_LOST_FRAMES: u32 = 3;

/// How long a bit-for-bit identical bpm may persist before the strap is judged
/// off-body (задача 033).
///
/// The RR signal alone is not enough: a removed H10 does not stop sending RR
/// outright, it *intermittently* emits RR-bearing frames (observed live:
/// `10 6F FB 17` between runs of `00 6F`), which keeps resetting the RR-less
/// counter. But a real pulse always jitters. Measured on this operator's own
/// history, the longest constant-bpm run from a worn strap is 13 samples over
/// **16 seconds**; a removed one held 111 bpm for 26 minutes. 60s leaves ~4x
/// headroom over the observed worn-strap maximum.
pub const CONTACT_FROZEN_BPM_MS: i64 = 60_000;

/// Whether the strap is currently on a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contact {
    Live,
    Lost,
}

/// Decides [`Contact`] from a stream of measurements — the *contact* half of
/// the link/contact split (задача 033). Pure: no BLE, and time arrives by
/// injection (`ts_ms`), like [`crate::presence`]. One per HR link; reset when
/// the link itself dies.
///
/// Three signals, in order of trustworthiness:
///
/// 1. `contact == Some(false)` — a strap that honestly reports contact. Trusted
///    immediately.
/// 2. A bpm frozen bit-for-bit for [`CONTACT_FROZEN_BPM_MS`]. A real pulse
///    jitters; a removed H10 repeats its last reading forever. This is the only
///    signal that catches an H10 which keeps interleaving RR-bearing frames
///    while lying on a desk.
/// 3. RR-intervals disappearing from a sensor that was sending them (a fast
///    path: it fires in ~3s where the frozen-bpm rule needs 60). Capability is
///    learned from the stream (any frame with RR ⇒ RR-capable), never hardcoded
///    per vendor.
///
/// A sensor that reports none of these is undetectable here and stays
/// [`Contact::Live`] until the link-level notification timeout fires. Honest
/// degradation: that is exactly today's behaviour for every sensor.
#[derive(Debug, Default)]
pub struct ContactTracker {
    rr_capable: bool,
    frames_without_rr: u32,
    /// Last bpm seen, and when it last *changed* — the frozen-bpm clock.
    last_bpm: Option<u16>,
    last_bpm_change_ts_ms: Option<i64>,
}

impl ContactTracker {
    /// `ts_ms` is the frame's arrival time (`Utc::now()` live, the stored
    /// `hr_samples.ts_ms` on replay — задача 034).
    pub fn observe(&mut self, m: &HrMeasurement, ts_ms: i64) -> Contact {
        let bpm_changed = self.last_bpm != Some(m.bpm);
        if bpm_changed {
            self.last_bpm = Some(m.bpm);
            self.last_bpm_change_ts_ms = Some(ts_ms);
        }

        if m.contact == Some(false) {
            return Contact::Lost;
        }
        // A frozen bpm outranks any RR evidence: a removed H10 goes on emitting
        // RR-bearing frames intermittently, so RR presence must not vouch for a
        // reading that has not moved in a minute.
        if self.bpm_frozen_since(ts_ms) {
            return Contact::Lost;
        }
        if !m.rr_ms.is_empty() {
            self.rr_capable = true;
            self.frames_without_rr = 0;
            return Contact::Live;
        }
        if !self.rr_capable {
            return Contact::Live;
        }
        self.frames_without_rr = self.frames_without_rr.saturating_add(1);
        if self.frames_without_rr >= CONTACT_LOST_FRAMES {
            Contact::Lost
        } else {
            Contact::Live
        }
    }

    /// Whether the *frozen bpm* rule (rather than RR evidence) is what condemns
    /// the frame at `now_ms`. Offline replay uses this to widen the verdict back
    /// over the whole constant-bpm run — see `recompute_hr` (задача 034).
    pub fn bpm_frozen_at(&self, now_ms: i64) -> bool {
        self.bpm_frozen_since(now_ms)
    }

    /// Forget everything that belonged to the *link* — RR capability and the
    /// RR-less run — while keeping the frozen-bpm clock, which belongs to the
    /// *body* (задача 034).
    ///
    /// Offline replay infers link boundaries from `ts_ms` gaps, and a gap is
    /// exactly what deleting a contact-lost run leaves behind. Resetting the
    /// frozen clock there would make each surviving fragment look like a fresh
    /// minute of live pulse, and the cleanup would never converge. A heart that
    /// has not changed rate across a reconnect has still not changed rate.
    pub fn reset_link(&mut self) {
        self.rr_capable = false;
        self.frames_without_rr = 0;
    }

    fn bpm_frozen_since(&self, now_ms: i64) -> bool {
        self.last_bpm_change_ts_ms
            .is_some_and(|since| now_ms.saturating_sub(since) >= CONTACT_FROZEN_BPM_MS)
    }
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

    /// Observe one frame at `t_s` seconds into the (synthetic) session.
    fn observe_at(tracker: &mut ContactTracker, payload: &[u8], t_s: i64) -> Contact {
        let m = parse_hr_measurement(payload).expect("fixture should parse");
        tracker.observe(&m, t_s * 1000)
    }

    /// The exact frames captured from a live H10 in задача 033: worn (RR
    /// present), then removed (frozen bpm, no RR, no contact bits). `WORN2`
    /// carries a different bpm, so a stream of it never looks frozen.
    const WORN: [u8; 6] = [0x10, 0x75, 0x72, 0x03, 0x27, 0x02];
    const WORN2: [u8; 6] = [0x10, 0x76, 0x72, 0x03, 0x27, 0x02];
    const REMOVED: [u8; 2] = [0x00, 0x6f];

    #[test]
    fn contact_is_lost_after_three_rr_less_frames_from_an_rr_capable_sensor() {
        let mut tracker = ContactTracker::default();
        assert_eq!(observe_at(&mut tracker, &WORN, 0), Contact::Live);
        // Two RR-less frames are tolerated (a dropped notification, not a
        // removed strap).
        assert_eq!(observe_at(&mut tracker, &REMOVED, 1), Contact::Live);
        assert_eq!(observe_at(&mut tracker, &REMOVED, 2), Contact::Live);
        assert_eq!(observe_at(&mut tracker, &REMOVED, 3), Contact::Lost);
    }

    #[test]
    fn returning_rr_intervals_restore_contact_and_reset_the_counter() {
        let mut tracker = ContactTracker::default();
        observe_at(&mut tracker, &WORN, 0);
        observe_at(&mut tracker, &REMOVED, 1);
        observe_at(&mut tracker, &REMOVED, 2);
        // Strap back on the chest before the third RR-less frame.
        assert_eq!(observe_at(&mut tracker, &WORN, 3), Contact::Live);
        // Counter cleared: the next two RR-less frames must not trip it.
        assert_eq!(observe_at(&mut tracker, &REMOVED, 4), Contact::Live);
        assert_eq!(observe_at(&mut tracker, &REMOVED, 5), Contact::Live);
    }

    #[test]
    fn contact_recovers_after_being_lost() {
        let mut tracker = ContactTracker::default();
        observe_at(&mut tracker, &WORN, 0);
        for t in 1..=5 {
            observe_at(&mut tracker, &REMOVED, t);
        }
        assert_eq!(observe_at(&mut tracker, &WORN, 6), Contact::Live);
    }

    #[test]
    fn explicit_contact_false_is_lost_immediately_even_with_rr() {
        // flags = 0b0001_0100 = contact supported + NOT detected + RR present.
        let payload = [0b0001_0100, 100, 0x00, 0x04];
        let mut tracker = ContactTracker::default();
        assert_eq!(observe_at(&mut tracker, &payload, 0), Contact::Lost);
    }

    #[test]
    fn a_sensor_that_never_sends_rr_stays_live_while_its_bpm_moves() {
        // Nothing to infer contact from — must not be declared Lost, or such a
        // strap would be permanently invisible.
        let mut tracker = ContactTracker::default();
        for t in 0..10 {
            let payload = [0x00, 90 + (t % 3) as u8];
            assert_eq!(observe_at(&mut tracker, &payload, t), Contact::Live);
        }
    }

    /// The regression that RR-counting alone missed: a removed H10 keeps
    /// interleaving RR-bearing frames, which reset the RR-less counter forever.
    /// Only the frozen bpm gives it away.
    #[test]
    fn a_frozen_bpm_is_lost_even_when_rr_frames_keep_arriving() {
        let mut tracker = ContactTracker::default();
        let frozen_s = CONTACT_FROZEN_BPM_MS / 1000;
        // Alternate RR / no-RR frames, all with the same bpm, as observed live.
        for t in 0..frozen_s {
            let frame: &[u8] = if t % 3 == 0 { &WORN } else { &[0x00, 0x75] };
            assert_eq!(observe_at(&mut tracker, frame, t), Contact::Live, "t={t}");
        }
        // The bpm has not moved for the whole window: contact is gone.
        assert_eq!(observe_at(&mut tracker, &WORN, frozen_s), Contact::Lost);
    }

    #[test]
    fn a_moving_bpm_never_trips_the_frozen_rule() {
        let mut tracker = ContactTracker::default();
        let frozen_s = CONTACT_FROZEN_BPM_MS / 1000;
        for t in 0..frozen_s * 3 {
            // Flip the bpm every 16s — the longest constant run ever observed
            // from this operator's worn strap.
            let frame: &[u8] = if (t / 16) % 2 == 0 { &WORN } else { &WORN2 };
            assert_eq!(observe_at(&mut tracker, frame, t), Contact::Live, "t={t}");
        }
    }

    #[test]
    fn a_changed_bpm_restarts_the_frozen_clock_and_restores_contact() {
        let mut tracker = ContactTracker::default();
        let frozen_s = CONTACT_FROZEN_BPM_MS / 1000;
        observe_at(&mut tracker, &WORN, 0);
        assert_eq!(observe_at(&mut tracker, &WORN, frozen_s), Contact::Lost);
        // Strap back on: a different bpm proves a live pulse again.
        assert_eq!(observe_at(&mut tracker, &WORN2, frozen_s + 1), Contact::Live);
    }
}
