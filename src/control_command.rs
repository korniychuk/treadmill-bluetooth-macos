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

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

/// How old a queued command may get before the daemon refuses to execute it.
///
/// A command queued long ago (or while the daemon was disconnected) must NOT
/// fire unexpectedly when the daemon later reconnects/restarts — that would be
/// a surprise belt-speed change. Larger than the daemon's ≤1s poll latency so
/// a command issued during a live session always runs, yet smaller than the
/// CLI's ~8s give-up so a command the CLI already abandoned is failed rather
/// than executed behind the operator's back.
pub const CONTROL_STALE_THRESHOLD: Duration = Duration::from_secs(30);

/// A one-shot FTMS control command routed through the queue. `Incline` is
/// intentionally absent — the daemon has no incline path and this device
/// rejects it anyway (see `docs/tasks/003`); `tm incline` stays direct-BLE.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ControlCommand {
    Start,
    Stop,
    Speed(f32),
}

impl ControlCommand {
    /// Compact string persisted in `control_commands.command`: `start`,
    /// `stop`, or `speed:<kmh>` (e.g. `speed:2.5`).
    pub fn to_wire(self) -> String {
        match self {
            Self::Start => "start".to_string(),
            Self::Stop => "stop".to_string(),
            Self::Speed(kmh) => format!("speed:{kmh}"),
        }
    }

    /// Parse the wire form back into a command. Errors (rather than silently
    /// defaulting) on an unknown verb or an unparseable speed so a corrupt row
    /// is failed loudly instead of executing something unintended.
    pub fn parse(wire: &str) -> Result<Self> {
        match wire {
            "start" => Ok(Self::Start),
            "stop" => Ok(Self::Stop),
            other => {
                let kmh = other
                    .strip_prefix("speed:")
                    .with_context(|| format!("unknown control command wire form: {other:?}"))?;
                let kmh: f32 = kmh
                    .parse()
                    .with_context(|| format!("unparseable speed in {other:?}"))?;
                Ok(Self::Speed(kmh))
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
    fn wire_round_trips_every_variant() {
        for cmd in [
            ControlCommand::Start,
            ControlCommand::Stop,
            ControlCommand::Speed(2.5),
        ] {
            let parsed = ControlCommand::parse(&cmd.to_wire()).expect("round-trips");
            assert_eq!(parsed, cmd);
        }
    }

    #[test]
    fn speed_wire_form_is_human_readable() {
        assert_eq!(ControlCommand::Speed(2.5).to_wire(), "speed:2.5");
        assert_eq!(ControlCommand::Start.to_wire(), "start");
        assert_eq!(ControlCommand::Stop.to_wire(), "stop");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(ControlCommand::parse("frobnicate").is_err());
        assert!(ControlCommand::parse("speed:fast").is_err());
        assert!(ControlCommand::parse("speed:").is_err());
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
