//! FTMS Control Point (0x2AD9) — start/stop and speed control.
//!
//! Protocol (FTMS 1.0, §4.16): write an opcode (+ params) to the Control
//! Point, then wait for the indicated response `[0x80, request_op, result]`
//! where result `0x01` = success. `RequestControl` must succeed before any
//! other command is accepted.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use btleplug::api::{Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures::StreamExt;
use tokio::time::timeout;
use tracing::{info, warn};

use crate::ftms;
use crate::speed::CentiKmh;

/// FTMS Control Point opcodes (subset used here).
mod opcode {
    pub const REQUEST_CONTROL: u8 = 0x00;
    pub const SET_TARGET_SPEED: u8 = 0x02;
    pub const SET_TARGET_INCLINATION: u8 = 0x03;
    pub const START_RESUME: u8 = 0x07;
    pub const STOP_PAUSE: u8 = 0x08;
    /// Prefix of every Control Point response indication.
    pub const RESPONSE: u8 = 0x80;
}

/// Result code meaning "success" in a Control Point response.
const RESULT_SUCCESS: u8 = 0x01;
/// STOP_PAUSE parameter: 0x01 = stop, 0x02 = pause.
const STOP_PARAM: u8 = 0x01;
/// How long to wait for the indicated response to a command.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Handle over an already-connected peripheral for issuing FTMS commands.
pub struct Controller<'a> {
    peripheral: &'a Peripheral,
    control_point: Characteristic,
}

impl<'a> Controller<'a> {
    /// Locate the Control Point, subscribe to its indications, and take
    /// control of the machine (`RequestControl`).
    pub async fn take_control(peripheral: &'a Peripheral) -> Result<Self> {
        let control_point = peripheral
            .characteristics()
            .into_iter()
            .find(|c| c.uuid == ftms::FITNESS_MACHINE_CONTROL_POINT)
            .context("Control Point characteristic (0x2AD9) not found")?;
        peripheral
            .subscribe(&control_point)
            .await
            .context("subscribe to Control Point indications")?;

        let controller = Self {
            peripheral,
            control_point,
        };
        controller.execute(opcode::REQUEST_CONTROL, &[]).await?;
        info!("control granted by fitness machine");
        Ok(controller)
    }

    /// Start (or resume) the belt.
    pub async fn start(&self) -> Result<()> {
        self.execute(opcode::START_RESUME, &[]).await
    }

    /// Stop the belt.
    pub async fn stop(&self) -> Result<()> {
        self.execute(opcode::STOP_PAUSE, &[STOP_PARAM]).await
    }

    /// Set target speed (device range: see Supported Speed Range).
    pub async fn set_speed(&self, speed: CentiKmh) -> Result<()> {
        if speed > CentiKmh::MAX_SANE {
            bail!("speed {speed} km/h out of sane range");
        }
        // FTMS encodes speed as uint16 in units of 0.01 km/h, little-endian.
        self.execute(opcode::SET_TARGET_SPEED, &speed.to_wire().to_le_bytes())
            .await
    }

    /// Set target inclination in percent (FTMS sint16, 0.1 % units).
    ///
    /// The W2 Pro's Feature bitmap advertises no inclination target support —
    /// this exists to empirically probe whether the machine rejects the opcode
    /// (expected result 0x02 = "Op Code not supported").
    pub async fn set_incline(&self, percent: f32) -> Result<()> {
        if !(-10.0..=15.0).contains(&percent) {
            bail!("incline {percent}% out of sane range");
        }
        let raw = (percent * 10.0).round() as i16;
        self.execute(opcode::SET_TARGET_INCLINATION, &raw.to_le_bytes())
            .await
    }

    /// Write `[op, params...]` and wait for the `[0x80, op, result]` indication.
    async fn execute(&self, op: u8, params: &[u8]) -> Result<()> {
        let mut frame = vec![op];
        frame.extend_from_slice(params);

        let mut indications = self
            .peripheral
            .notifications()
            .await
            .context("open indication stream")?;

        self.peripheral
            .write(&self.control_point, &frame, WriteType::WithResponse)
            .await
            .with_context(|| format!("write Control Point opcode {op:#04x}"))?;

        let response = timeout(RESPONSE_TIMEOUT, async {
            while let Some(n) = indications.next().await {
                if n.uuid == ftms::FITNESS_MACHINE_CONTROL_POINT
                    && n.value.first() == Some(&opcode::RESPONSE)
                    && n.value.get(1) == Some(&op)
                {
                    return Some(n.value);
                }
            }
            None
        })
        .await
        .with_context(|| {
            format!("no Control Point response to {op:#04x} within {RESPONSE_TIMEOUT:?}")
        })?
        .with_context(|| format!("indication stream ended awaiting response to {op:#04x}"))?;

        match response.get(2) {
            Some(&RESULT_SUCCESS) => {
                info!(op = format!("{op:#04x}"), "command acknowledged");
                Ok(())
            }
            other => {
                warn!(op = format!("{op:#04x}"), result = ?other, "command rejected");
                bail!("Control Point op {op:#04x} rejected: result {other:?}")
            }
        }
    }
}
