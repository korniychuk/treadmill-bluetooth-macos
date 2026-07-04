//! AC power detection, used to skip idle BLE scanning on battery.
//!
//! Scanning for the treadmill while it isn't found keeps the Bluetooth radio
//! in an active state; that's cheap for a single session but wasteful to run
//! unconditionally forever when the laptop is away from the treadmill anyway
//! (the common case when unplugged). `pmset -g batt` is a stable, always-
//! present system tool — its "AC Power" / "Battery Power" line has not
//! changed format across macOS releases, so parsing it is more robust than
//! it looks and avoids pulling in an IOKit binding for one boolean.

use std::process::Command;

use tracing::warn;

/// Whether the machine is currently drawing from AC power.
///
/// Defaults to `true` (i.e. "keep scanning") if `pmset` is missing or its
/// output doesn't parse — failing open means a `pmset` regression degrades
/// back to the old always-scan behavior instead of silently going quiet.
pub fn is_on_ac_power() -> bool {
    match Command::new("pmset").arg("-g").arg("batt").output() {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            match text.lines().next() {
                Some(first_line) => first_line.contains("AC Power"),
                None => {
                    warn!("pmset produced no output — assuming AC power");
                    true
                }
            }
        }
        Err(err) => {
            warn!(%err, "failed to run pmset — assuming AC power");
            true
        }
    }
}
