//! FitShow vendor protocol (service 0xFFF0) — the path to incline control.
//!
//! The W2 Pro shares its ODM reference design with FitShow-compatible
//! treadmills (Sperax RM-01, UREVO, FS-series). Protocol reference:
//! qdomyos-zwift `fitshowtreadmill.cpp`.
//!
//! Wire frame: `[0x02] [cmd] [payload...] [xor] [0x03]` where `xor` is the
//! XOR of all bytes between header and checksum. Commands are written to
//! 0xFFF2; responses arrive on the 0xFFF3/0xFFF4 notify characteristics.
//!
//! SAFETY: the sibling service 0xFF00 looks like a vendor OTA channel — it is
//! deliberately never written to here (firmware changes are forbidden).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use btleplug::api::{Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures::StreamExt;
use tokio::time::timeout;
use tracing::{info, warn};
use uuid::Uuid;

pub const FITSHOW_WRITE: Uuid = Uuid::from_u128(0x0000fff2_0000_1000_8000_00805f9b34fb);
const FITSHOW_NOTIFY_A: Uuid = Uuid::from_u128(0x0000fff3_0000_1000_8000_00805f9b34fb);
const FITSHOW_NOTIFY_B: Uuid = Uuid::from_u128(0x0000fff4_0000_1000_8000_00805f9b34fb);
const FAB_WRITE_1: Uuid = Uuid::from_u128(0x0000fab1_0000_1000_8000_00805f9b34fb);
const FAB_WRITE_2: Uuid = Uuid::from_u128(0x0000fab2_0000_1000_8000_00805f9b34fb);
const FAB_NOTIFY: Uuid = Uuid::from_u128(0x0000fab3_0000_1000_8000_00805f9b34fb);

const PKT_HEADER: u8 = 0x02;
const PKT_FOOTER: u8 = 0x03;

/// FitShow command opcodes (subset).
mod cmd {
    pub const SYS_INFO: u8 = 0x50;
    pub const SYS_STATUS: u8 = 0x51;
    pub const SYS_CONTROL: u8 = 0x53;
    /// SYS_INFO parameter: query supported incline range.
    pub const INFO_INCLINE: u8 = 0x03;
    /// SYS_INFO parameter: query model.
    pub const INFO_MODEL: u8 = 0x00;
    /// SYS_INFO parameter: query speed range.
    pub const INFO_SPEED: u8 = 0x02;
    /// SYS_CONTROL parameter: set target speed + incline while running.
    pub const CONTROL_TARGET_OR_RUN: u8 = 0x02;
}

/// How long to wait for a notify response to each request.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(3);

/// Wrap a payload into a FitShow frame: header + payload + XOR + footer.
fn frame(payload: &[u8]) -> Vec<u8> {
    let xor = payload.iter().fold(0u8, |acc, b| acc ^ b);
    let mut out = Vec::with_capacity(payload.len() + 3);
    out.push(PKT_HEADER);
    out.extend_from_slice(payload);
    out.push(xor);
    out.push(PKT_FOOTER);
    out
}

/// Validate an incoming FitShow frame and return its payload (without
/// header/checksum/footer).
fn parse_frame(raw: &[u8]) -> Option<&[u8]> {
    if raw.len() < 4 || raw[0] != PKT_HEADER || raw[raw.len() - 1] != PKT_FOOTER {
        return None;
    }
    let payload = &raw[1..raw.len() - 2];
    let expected = payload.iter().fold(0u8, |acc, b| acc ^ b);
    if raw[raw.len() - 2] != expected {
        return None;
    }
    Some(payload)
}

/// Handle for talking FitShow to a connected peripheral.
pub struct FitShow<'a> {
    peripheral: &'a Peripheral,
    /// Candidate vendor write characteristics with their required write types.
    write_chars: Vec<(Characteristic, WriteType)>,
}

impl<'a> FitShow<'a> {
    /// Subscribe to the vendor notify characteristics and locate write ones.
    pub async fn attach(peripheral: &'a Peripheral) -> Result<Self> {
        let chars = peripheral.characteristics();
        // 0xFFF2 is plain `write` (needs a response); 0xFAB1/0xFAB2 are
        // write-without-response. Using the wrong type gets silently dropped
        // by CoreBluetooth.
        let write_chars: Vec<(Characteristic, WriteType)> = [
            (FITSHOW_WRITE, WriteType::WithResponse),
            (FAB_WRITE_1, WriteType::WithoutResponse),
            (FAB_WRITE_2, WriteType::WithoutResponse),
        ]
        .iter()
        .filter_map(|(uuid, wt)| {
            chars
                .iter()
                .find(|c| c.uuid == *uuid)
                .map(|c| (c.clone(), *wt))
        })
        .collect();
        if write_chars.is_empty() {
            anyhow::bail!("no vendor write characteristics found");
        }

        for uuid in [FITSHOW_NOTIFY_A, FITSHOW_NOTIFY_B, FAB_NOTIFY] {
            match chars.iter().find(|c| c.uuid == uuid) {
                Some(ch) => match peripheral.subscribe(ch).await {
                    Ok(()) => info!(char = %uuid, "subscribed to vendor notify"),
                    Err(err) => warn!(char = %uuid, %err, "vendor notify subscribe failed"),
                },
                None => warn!(char = %uuid, "vendor notify characteristic missing"),
            }
        }

        Ok(Self {
            peripheral,
            write_chars,
        })
    }

    /// Send one request frame and log every vendor response for a short window.
    ///
    /// FitShow devices reply on the notify channel; some stay silent for
    /// unsupported queries, which is itself a data point — hence logging
    /// instead of hard-failing.
    pub async fn request(&self, payload: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut all = Vec::new();
        for (ch, wt) in &self.write_chars {
            let responses = self.request_via(ch, *wt, payload).await?;
            let got_reply = !responses.is_empty();
            all.extend(responses);
            // First channel that answers is the right one; stop probing others.
            if got_reply {
                break;
            }
        }
        Ok(all)
    }

    async fn request_via(
        &self,
        ch: &Characteristic,
        wt: WriteType,
        payload: &[u8],
    ) -> Result<Vec<Vec<u8>>> {
        let wire = frame(payload);
        let mut notifications = self
            .peripheral
            .notifications()
            .await
            .context("open notification stream")?;

        info!(via = %ch.uuid, tx = %hex(&wire), "fitshow request");
        if let Err(err) = self.peripheral.write(ch, &wire, wt).await {
            // A rejected write on one candidate channel is diagnostic data,
            // not a fatal error — the next channel may accept it.
            warn!(via = %ch.uuid, %err, "vendor write failed");
            return Ok(Vec::new());
        }

        let mut responses = Vec::new();
        // Collect everything arriving within the window; vendor chatter may
        // span multiple frames.
        let _ = timeout(RESPONSE_TIMEOUT, async {
            while let Some(n) = notifications.next().await {
                if n.uuid != FITSHOW_NOTIFY_A && n.uuid != FITSHOW_NOTIFY_B && n.uuid != FAB_NOTIFY
                {
                    continue;
                }
                match parse_frame(&n.value) {
                    Some(payload) => {
                        info!(char = %n.uuid, rx = %hex(&n.value), payload = %hex(payload), "fitshow response");
                    }
                    None => warn!(char = %n.uuid, rx = %hex(&n.value), "unparsable vendor frame"),
                }
                responses.push(n.value.clone());
            }
        })
        .await;

        if responses.is_empty() {
            warn!(via = %ch.uuid, tx = %hex(&wire), "no vendor response within {RESPONSE_TIMEOUT:?}");
        }
        Ok(responses)
    }

    /// Probe the read-only info/status queries (no state change on device).
    pub async fn probe_info(&self) -> Result<()> {
        for (label, payload) in [
            ("model", vec![cmd::SYS_INFO, cmd::INFO_MODEL]),
            ("speed-range", vec![cmd::SYS_INFO, cmd::INFO_SPEED]),
            ("incline-range", vec![cmd::SYS_INFO, cmd::INFO_INCLINE]),
            ("status", vec![cmd::SYS_STATUS]),
        ] {
            info!(query = label, "probing");
            self.request(&payload).await?;
        }
        Ok(())
    }

    /// Set target speed (km/h) and incline level in one control frame.
    ///
    /// FitShow encodes speed in 0.1 km/h units and incline as a raw level.
    pub async fn set_speed_incline(&self, speed_kmh: f32, incline_level: u8) -> Result<()> {
        if !(0.0..=6.5).contains(&speed_kmh) {
            bail!("speed {speed_kmh} km/h out of W2 Pro range");
        }
        if incline_level > 15 {
            bail!("incline level {incline_level} out of sane range");
        }
        let speed_byte = (speed_kmh * 10.0).round() as u8;
        self.request(&[
            cmd::SYS_CONTROL,
            cmd::CONTROL_TARGET_OR_RUN,
            speed_byte,
            incline_level,
        ])
        .await?;
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
