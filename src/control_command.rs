//! Control command type shared by the CLI (enqueue), the store (persist), and
//! the daemon (execute) — see `docs/tasks/013`.
//!
//! The daemon is the single BLE owner: while it holds the connection the
//! treadmill stops advertising, so a separate CLI process cannot open its own
//! link. Control commands are therefore routed through a SQLite queue instead
//! of each CLI invocation scanning for the machine. This module owns only the
//! pure, testable pieces of that queue: the command's compact wire form and
//! the staleness decision. Persistence lives in [`crate::store`]; execution
//! (take control + write) lives in [`crate::daemon`].

use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};

use crate::led::LedState;
use crate::speed::CentiKmh;

/// Relative speed step queued as `speed_step:up` / `speed_step:down` (задача 063).
/// Resolved against daemon intent memory at execute time, never in the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepDirection {
    Up,
    Down,
}

impl StepDirection {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

/// How old a queued command may get before the daemon refuses to execute it.
///
/// A command queued long ago (or while the daemon was disconnected) must NOT
/// fire unexpectedly when the daemon later reconnects/restarts — that would be
/// a surprise belt-speed change. Larger than the daemon's ≤1s poll latency so
/// a command issued during a live session always runs, yet smaller than the
/// CLI's ~8s give-up so a command the CLI already abandoned is failed rather
/// than executed behind the operator's back.
pub const CONTROL_STALE_THRESHOLD: Duration = Duration::from_secs(30);

/// A one-shot control command routed through the queue. `Incline` is
/// intentionally absent — the daemon has no incline path and this device
/// rejects it anyway (see `docs/tasks/003`); `tm incline` stays direct-BLE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCommand {
    Start,
    StartWithSpeed(CentiKmh),
    Stop,
    Speed(CentiKmh),
    SpeedStep(StepDirection),
    Toggle,
    Led(LedState),
}

impl ControlCommand {
    /// Compact string persisted in `control_commands.command`: `start`,
    /// `start_speed:<kmh>`, `stop`, `toggle`, `speed:<kmh>` (e.g. `speed:2.5`), `speed_step:up` /
    /// `speed_step:down`, or `led:on`/`led:off`. Human-readable km/h outside;
    /// [`CentiKmh`] inside. Relative forms (`toggle`, `speed_step:*`) are
    /// resolved by the daemon (задача 063).
    pub fn to_wire(self) -> String {
        match self {
            Self::Start => "start".to_string(),
            Self::StartWithSpeed(speed) => format!("start_speed:{speed}"),
            Self::Stop => "stop".to_string(),
            Self::Toggle => "toggle".to_string(),
            Self::Speed(speed) => format!("speed:{speed}"),
            Self::SpeedStep(dir) => format!("speed_step:{}", dir.as_str()),
            Self::Led(state) => format!("led:{state}"),
        }
    }

    /// Start-speed and relative intents need the daemon's live
    /// telemetry and intent memory; the CLI must not fall back to a direct
    /// BLE write for them.
    #[must_use]
    pub fn requires_daemon_intent(self) -> bool {
        matches!(
            self,
            Self::StartWithSpeed(_) | Self::SpeedStep(_) | Self::Toggle
        )
    }

    /// Parse the wire form back into a command. Errors (rather than silently
    /// defaulting) on an unknown verb or an unparseable speed so a corrupt row
    /// is failed loudly instead of executing something unintended.
    pub fn parse(wire: &str) -> Result<Self> {
        match wire {
            "start" => Ok(Self::Start),
            "stop" => Ok(Self::Stop),
            "toggle" => Ok(Self::Toggle),
            "speed_step:up" => Ok(Self::SpeedStep(StepDirection::Up)),
            "speed_step:down" => Ok(Self::SpeedStep(StepDirection::Down)),
            other => {
                if let Some(raw) = other.strip_prefix("led:") {
                    let state: LedState = raw.parse().with_context(|| {
                        format!("unknown LED state in {other:?}; expected led:on or led:off")
                    })?;
                    return Ok(Self::Led(state));
                }
                let raw = other
                    .strip_prefix("speed:")
                    .or_else(|| other.strip_prefix("start_speed:"))
                    .with_context(|| format!("unknown control command wire form: {other:?}"))?;
                let kmh: f32 = raw
                    .parse()
                    .with_context(|| format!("unparseable speed in {other:?}"))?;
                let Some(speed) = CentiKmh::from_kmh_f32(kmh) else {
                    bail!("speed out of range in {other:?}");
                };
                if other.starts_with("start_speed:") {
                    return Ok(Self::StartWithSpeed(speed));
                }
                Ok(Self::Speed(speed))
            }
        }
    }
}

/// Whether a command queued at `created_at` is too old to execute at `now`.
/// Pure so the daemon's staleness guard is unit-testable without a clock.
pub fn is_stale(created_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now.signed_duration_since(created_at)
        > chrono::Duration::from_std(CONTROL_STALE_THRESHOLD).expect("30s fits chrono")
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn parse_start_speed_wire_without_device_range_validation() {
        let command = ControlCommand::StartWithSpeed(CentiKmh::from_wire(350));
        assert_eq!(command.to_wire(), "start_speed:3.5");
        assert!(command.requires_daemon_intent());
        for wire in [
            "start_speed:",
            "start_speed:abc",
            "start_speed:NaN",
            "start_speed:-1",
        ] {
            assert!(ControlCommand::parse(wire).is_err(), "{wire}");
        }
        assert_eq!(
            ControlCommand::parse("start_speed:9").unwrap(),
            ControlCommand::StartWithSpeed(CentiKmh::from_wire(900))
        );
    }

    #[test]
    fn wire_round_trips_every_variant() {
        for cmd in [
            ControlCommand::Start,
            ControlCommand::StartWithSpeed(CentiKmh::from_wire(350)),
            ControlCommand::Stop,
            ControlCommand::Toggle,
            ControlCommand::Speed(CentiKmh::from_wire(250)),
            ControlCommand::SpeedStep(StepDirection::Up),
            ControlCommand::SpeedStep(StepDirection::Down),
            ControlCommand::Led(LedState::On),
            ControlCommand::Led(LedState::Off),
        ] {
            let parsed = ControlCommand::parse(&cmd.to_wire()).expect("round-trips");
            assert_eq!(parsed, cmd);
        }
    }

    #[test]
    fn speed_wire_form_is_human_readable() {
        assert_eq!(
            ControlCommand::Speed(CentiKmh::from_wire(250)).to_wire(),
            "speed:2.5"
        );
        assert_eq!(ControlCommand::Start.to_wire(), "start");
        assert_eq!(ControlCommand::Stop.to_wire(), "stop");
        assert_eq!(ControlCommand::Toggle.to_wire(), "toggle");
        assert_eq!(
            ControlCommand::SpeedStep(StepDirection::Up).to_wire(),
            "speed_step:up"
        );
        assert_eq!(
            ControlCommand::SpeedStep(StepDirection::Down).to_wire(),
            "speed_step:down"
        );
        assert_eq!(ControlCommand::Led(LedState::On).to_wire(), "led:on");
        assert_eq!(ControlCommand::Led(LedState::Off).to_wire(), "led:off");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(ControlCommand::parse("frobnicate").is_err());
        assert!(ControlCommand::parse("speed:fast").is_err());
        assert!(ControlCommand::parse("speed:").is_err());
        assert!(ControlCommand::parse("led:").is_err());
        assert!(ControlCommand::parse("led:maybe").is_err());
        assert!(ControlCommand::parse("led").is_err());
        assert!(ControlCommand::parse("speed_step:").is_err());
        assert!(ControlCommand::parse("speed_step:left").is_err());
        assert!(ControlCommand::parse("toggle:on").is_err());
        assert!(ControlCommand::parse("speed_step").is_err());
    }

    #[test]
    fn relative_commands_require_daemon_intent() {
        assert!(ControlCommand::Toggle.requires_daemon_intent());
        assert!(ControlCommand::SpeedStep(StepDirection::Up).requires_daemon_intent());
        assert!(ControlCommand::SpeedStep(StepDirection::Down).requires_daemon_intent());
        assert!(!ControlCommand::Start.requires_daemon_intent());
        assert!(!ControlCommand::Stop.requires_daemon_intent());
        assert!(!ControlCommand::Speed(CentiKmh::from_wire(250)).requires_daemon_intent());
        assert!(!ControlCommand::Led(LedState::On).requires_daemon_intent());
    }

    #[test]
    fn fresh_command_is_not_stale_but_old_one_is() {
        let created = Utc.with_ymd_and_hms(2026, 7, 5, 10, 0, 0).unwrap();
        // arr — one just under, one just over the 30s threshold.
        let fresh = created + chrono::Duration::seconds(5);
        let old = created + chrono::Duration::seconds(45);
        // act / assert
        assert!(!is_stale(created, fresh));
        assert!(is_stale(created, old));
    }
}
