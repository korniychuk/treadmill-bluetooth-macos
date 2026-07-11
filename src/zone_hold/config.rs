//! Zone Hold config load/parse (per-user config.toml `[zone_hold]`).

use tracing::warn;

use super::{
    Method, Tracking, ZoneBounds, ZoneDef, ZoneHoldConfig, ZoneSelector, default_zones, slugify,
};
use crate::speed::CentiKmh;

/// Per-user config path shared with `goals`/`auto_pause` (`~/.config/treadmill-
/// bluetooth-macos/config.toml`, задача 023). Re-resolved here rather than
/// imported so this module has no dependency on `crate::goals`'s internals —
/// only the file path convention is shared. Public so the `tm zone` CLI (in
/// `crate::main`) knows where to write onboarding/limit updates.
pub fn config_path() -> Option<std::path::PathBuf> {
    if let Ok(path) = std::env::var("TREADMILL_CONFIG") {
        return Some(std::path::PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(".config/treadmill-bluetooth-macos/config.toml"))
}

/// Load `[zone_hold]` from the per-user config, falling back to
/// [`ZoneHoldConfig::disabled_default`] on any problem (missing file, no
/// `[zone_hold]` table, malformed TOML). A present-but-invalid *value* inside
/// an otherwise-valid table is logged at WARN and that one field falls back to
/// its default — a single typo must not silently disable the whole mode.
pub fn load_zone_hold_config() -> ZoneHoldConfig {
    match config_path() {
        Some(path) if path.exists() => match std::fs::read_to_string(&path) {
            Ok(raw) => parse_zone_hold_config(&raw),
            Err(err) => {
                warn!(path = %path.display(), %err, "zone_hold config present but unreadable — Zone Hold disabled");
                ZoneHoldConfig::disabled_default()
            }
        },
        // No file is the normal uncustomised case — Zone Hold defaults to off.
        _ => ZoneHoldConfig::disabled_default(),
    }
}

/// Parse the `[zone_hold]` table out of the whole config file's raw TOML text.
/// Pure and unit-tested; the file I/O lives in [`load_zone_hold_config`].
pub(super) fn parse_zone_hold_config(raw: &str) -> ZoneHoldConfig {
    let Ok(value) = toml::from_str::<toml::Value>(raw) else {
        warn!("zone_hold config present but malformed TOML — Zone Hold disabled");
        return ZoneHoldConfig::disabled_default();
    };
    let Some(table) = value.get("zone_hold") else {
        return ZoneHoldConfig::disabled_default();
    };

    let defaults = ZoneHoldConfig::disabled_default();
    let enabled = bool_or(table, "enabled", defaults.enabled);
    let age = table
        .get("age")
        .and_then(|v| v.as_integer())
        .filter(|&n| (1..=120).contains(&n))
        .map(|n| n as u32)
        .or(defaults.age);
    if enabled && age.is_none() && table.get("age").is_some() {
        warn!("zone_hold.age present but not a plausible age (1-120) — HRmax cannot be computed");
    }
    let resting_hr = table
        .get("resting_hr")
        .and_then(|v| v.as_integer())
        .filter(|&n| (30..=120).contains(&n))
        .map(|n| n as u16);

    let method = match table.get("method").and_then(|v| v.as_str()) {
        Some("hrmax") | None => Method::HrMax,
        Some("karvonen") => Method::Karvonen,
        Some(other) => {
            warn!(value = other, "zone_hold.method unrecognised — using hrmax");
            Method::HrMax
        }
    };
    // Once per load (задача 040) — resolve_zone_bpm also falls back algebraically
    // but must stay silent (called every zone-hold tick).
    if method == Method::Karvonen && resting_hr.is_none() {
        warn!(
            "zone_hold: method=karvonen but resting_hr missing — \
             falling back to hrmax percents (zones lower than intended)"
        );
    }

    let target_zone = match table.get("target_zone") {
        None => defaults.target_zone.clone(),
        Some(v) => {
            if let Some(n) = v.as_integer() {
                if n > 0 && n <= u8::MAX as i64 {
                    ZoneSelector::Number(n as u8)
                } else {
                    warn!(
                        value = n,
                        "zone_hold.target_zone out of range — using default"
                    );
                    defaults.target_zone.clone()
                }
            } else if let Some(s) = v.as_str() {
                ZoneSelector::Id(s.to_string())
            } else {
                warn!("zone_hold.target_zone neither a number nor a string — using default");
                defaults.target_zone.clone()
            }
        }
    };

    let min_speed_kmh = positive_centi_or(table, "min_speed", defaults.min_speed_kmh);
    let max_speed_kmh = positive_centi_or(table, "max_speed", defaults.max_speed_kmh);

    let tracking = match table.get("tracking").and_then(|v| v.as_str()) {
        Some("band") | None => Tracking::Band,
        Some("center") => Tracking::Center,
        Some(other) => {
            warn!(
                value = other,
                "zone_hold.tracking unrecognised — using band"
            );
            Tracking::Band
        }
    };

    // `0` is valid: skip warm-up ramp entirely (задача 045).
    let warmup_minutes = non_negative_int_or(table, "warmup_minutes", defaults.warmup_minutes);
    let correction_interval_seconds = positive_int_or(
        table,
        "correction_interval_seconds",
        defaults.correction_interval_seconds,
    );
    let deadband_bpm = positive_int_or(table, "deadband_bpm", defaults.deadband_bpm);
    let max_step_kmh = positive_centi_or(table, "max_step_kmh", defaults.max_step_kmh);
    let reentry_grace_seconds = positive_int_or(
        table,
        "reentry_grace_seconds",
        defaults.reentry_grace_seconds,
    );
    let safety_cap_percent =
        positive_float_or(table, "safety_cap_percent", defaults.safety_cap_percent);

    let raw_zone_count = table
        .get("zones")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let zones = table
        .get("zones")
        .and_then(|v| v.as_array())
        .filter(|arr| !arr.is_empty())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| match parse_zone_def(v) {
                    Ok(z) => Some(z),
                    Err(reason) => {
                        let name = v
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("<unnamed>");
                        warn!(zone = name, %reason, "zone_hold: dropping invalid zone");
                        None
                    }
                })
                .collect::<Vec<_>>()
        })
        .filter(|zones| !zones.is_empty())
        .unwrap_or_else(default_zones);
    if raw_zone_count > zones.len() && matches!(target_zone, ZoneSelector::Number(_)) {
        warn!(
            raw = raw_zone_count,
            kept = zones.len(),
            "zone_hold: one or more zones dropped — 1-based target_zone numbering may have shifted"
        );
    }

    ZoneHoldConfig {
        enabled,
        age,
        resting_hr,
        method,
        target_zone,
        min_speed_kmh,
        max_speed_kmh,
        tracking,
        warmup_minutes,
        correction_interval_seconds,
        deadband_bpm,
        max_step_kmh,
        reentry_grace_seconds,
        safety_cap_percent,
        zones,
    }
}

/// Physiological bpm bounds accepted for absolute zone config (задача 045).
const ABSOLUTE_BPM_MIN: i64 = 30;
const ABSOLUTE_BPM_MAX: i64 = 250;

pub(super) fn parse_zone_def(value: &toml::Value) -> Result<ZoneDef, String> {
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "missing name".to_string())?;
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| slugify(&name));
    let max_speed_kmh = value
        .get("max_speed")
        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|n| n as f64)))
        .map(|n| n as f32)
        .filter(|&n| n > 0.0)
        .and_then(CentiKmh::from_kmh_f32);

    let bounds = if let (Some(min_bpm), Some(max_bpm)) = (
        value.get("min_bpm").and_then(|v| v.as_integer()),
        value.get("max_bpm").and_then(|v| v.as_integer()),
    ) {
        if !(ABSOLUTE_BPM_MIN..=ABSOLUTE_BPM_MAX).contains(&min_bpm)
            || !(ABSOLUTE_BPM_MIN..=ABSOLUTE_BPM_MAX).contains(&max_bpm)
        {
            return Err(format!(
                "absolute bpm out of range ({ABSOLUTE_BPM_MIN}-{ABSOLUTE_BPM_MAX})"
            ));
        }
        if min_bpm >= max_bpm {
            return Err("min_bpm must be < max_bpm".into());
        }
        ZoneBounds::Absolute {
            min_bpm: min_bpm as u16,
            max_bpm: max_bpm as u16,
        }
    } else {
        let min = value
            .get("min_percent")
            .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|n| n as f64)))
            .ok_or_else(|| "missing min_percent/min_bpm pair".to_string())?
            as f32;
        let max = value
            .get("max_percent")
            .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|n| n as f64)))
            .ok_or_else(|| "missing max_percent/max_bpm pair".to_string())?
            as f32;
        if min >= max {
            return Err("min_percent must be < max_percent".into());
        }
        if !(0.0..=100.0).contains(&min) || !(0.0..=100.0).contains(&max) {
            return Err("percent bounds must be in 0..=100".into());
        }
        ZoneBounds::Percent { min, max }
    };

    Ok(ZoneDef {
        id,
        name,
        bounds,
        max_speed_kmh,
    })
}

fn bool_or(table: &toml::Value, key: &str, default: bool) -> bool {
    match table.get(key).and_then(|v| v.as_bool()) {
        Some(b) => b,
        None => {
            if table.get(key).is_some() {
                warn!(key, "zone_hold config value not a bool — using default");
            }
            default
        }
    }
}

fn positive_float_or(table: &toml::Value, key: &str, default: f32) -> f32 {
    let raw = table
        .get(key)
        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|n| n as f64)));
    match raw {
        Some(n) if n > 0.0 => n as f32,
        Some(_) => {
            warn!(key, "zone_hold config value not positive — using default");
            default
        }
        None => {
            if table.get(key).is_some() {
                warn!(key, "zone_hold config value not a number — using default");
            }
            default
        }
    }
}

/// Positive km/h config key quantized to [`CentiKmh`] at parse time.
fn positive_centi_or(table: &toml::Value, key: &str, default: CentiKmh) -> CentiKmh {
    let raw = table
        .get(key)
        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|n| n as f64)));
    match raw {
        Some(n) if n > 0.0 => match CentiKmh::from_kmh_f32(n as f32) {
            Some(c) if c > CentiKmh::ZERO => c,
            _ => {
                warn!(key, "zone_hold speed out of range — using default");
                default
            }
        },
        Some(_) => {
            warn!(key, "zone_hold config value not positive — using default");
            default
        }
        None => {
            if table.get(key).is_some() {
                warn!(key, "zone_hold config value not a number — using default");
            }
            default
        }
    }
}

/// Like [`positive_int_or`] but allows `0` (задача 045: `warmup_minutes = 0`
/// means skip warm-up).
fn non_negative_int_or(table: &toml::Value, key: &str, default: i64) -> i64 {
    match table.get(key).and_then(|v| v.as_integer()) {
        Some(n) if n >= 0 => n,
        Some(_) => {
            warn!(
                key,
                "zone_hold config value not non-negative — using default"
            );
            default
        }
        None => {
            if table.get(key).is_some() {
                warn!(key, "zone_hold config value not an integer — using default");
            }
            default
        }
    }
}

fn positive_int_or(table: &toml::Value, key: &str, default: i64) -> i64 {
    match table.get(key).and_then(|v| v.as_integer()) {
        Some(n) if n > 0 => n,
        Some(_) => {
            warn!(key, "zone_hold config value not positive — using default");
            default
        }
        None => {
            if table.get(key).is_some() {
                warn!(key, "zone_hold config value not an integer — using default");
            }
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        DEFAULT_MAX_SPEED, Method, Tracking, ZoneBounds, ZoneHoldConfig, ZoneSelector,
    };
    use super::*;
    use crate::speed::CentiKmh;

    #[test]
    fn parse_zone_def_rejects_inverted_and_out_of_range_bpm() {
        let bad_order = toml::Value::try_from(toml::toml! {
            name = "x"
            min_bpm = 140
            max_bpm = 100
        })
        .unwrap();
        assert!(parse_zone_def(&bad_order).is_err());

        let wrap = toml::Value::try_from(toml::toml! {
            name = "y"
            min_bpm = 70000
            max_bpm = 70001
        })
        .unwrap();
        assert!(parse_zone_def(&wrap).is_err());

        let ok = toml::Value::try_from(toml::toml! {
            name = "z"
            min_bpm = 100
            max_bpm = 120
        })
        .unwrap();
        assert!(parse_zone_def(&ok).is_ok());
    }

    #[test]
    fn parse_zone_hold_drops_invalid_zone_and_keeps_rest() {
        let raw = r#"
[zone_hold]
enabled = true
age = 30
target_zone = 2
[[zone_hold.zones]]
name = "good"
min_percent = 50
max_percent = 60
[[zone_hold.zones]]
name = "broken"
min_percent = 70
# missing max_percent
[[zone_hold.zones]]
name = "also-good"
min_percent = 60
max_percent = 70
"#;
        let config = parse_zone_hold_config(raw);
        assert_eq!(config.zones.len(), 2);
        assert_eq!(config.zones[0].name, "good");
        assert_eq!(config.zones[1].name, "also-good");
    }

    #[test]
    fn warmup_minutes_zero_is_allowed() {
        let raw = r#"
[zone_hold]
enabled = true
age = 30
warmup_minutes = 0
"#;
        let config = parse_zone_hold_config(raw);
        assert_eq!(config.warmup_minutes, 0);
    }
    #[test]
    fn load_zone_hold_config_absent_file_is_disabled_default() {
        let config = parse_zone_hold_config("goals = [8000]\n");
        assert_eq!(config, ZoneHoldConfig::disabled_default());
    }

    #[test]
    fn parse_zone_hold_config_reads_all_keys() {
        let raw = r#"
            goals = [8000]

            [zone_hold]
            enabled = true
            age = 34
            resting_hr = 65
            method = "karvonen"
            target_zone = 3
            min_speed = 2.5
            max_speed = 5.0
            tracking = "center"
            warmup_minutes = 3
            correction_interval_seconds = 15
            deadband_bpm = 4
            max_step_kmh = 0.2
            reentry_grace_seconds = 30
            safety_cap_percent = 75
        "#;
        let config = parse_zone_hold_config(raw);
        assert!(config.enabled);
        assert_eq!(config.age, Some(34));
        assert_eq!(config.resting_hr, Some(65));
        assert_eq!(config.method, Method::Karvonen);
        assert_eq!(config.target_zone, ZoneSelector::Number(3));
        assert_eq!(config.min_speed_kmh, CentiKmh::from_wire(250));
        assert_eq!(config.max_speed_kmh, CentiKmh::from_wire(500));
        assert_eq!(config.tracking, Tracking::Center);
        assert_eq!(config.warmup_minutes, 3);
        assert_eq!(config.correction_interval_seconds, 15);
        assert_eq!(config.deadband_bpm, 4);
        assert_eq!(config.max_step_kmh, CentiKmh::from_wire(20));
        assert_eq!(config.reentry_grace_seconds, 30);
        assert_eq!(config.safety_cap_percent, 75.0);
    }

    #[test]
    fn parse_zone_hold_config_invalid_value_falls_back_to_default_for_that_key() {
        let raw = r#"
            [zone_hold]
            enabled = true
            age = 999
            max_speed = -1
        "#;
        let config = parse_zone_hold_config(raw);
        assert!(config.enabled);
        assert_eq!(config.age, None, "implausible age rejected");
        assert_eq!(config.max_speed_kmh, DEFAULT_MAX_SPEED);
    }

    #[test]
    fn parse_zone_hold_config_custom_zones_override_defaults() {
        let raw = r#"
            [zone_hold]
            enabled = true
            age = 30

            [[zone_hold.zones]]
            name = "Custom"
            min_percent = 55
            max_percent = 65
            max_speed = 6.0
        "#;
        let config = parse_zone_hold_config(raw);
        assert_eq!(config.zones.len(), 1);
        assert_eq!(config.zones[0].name, "Custom");
        assert_eq!(
            config.zones[0].id, "custom",
            "id derived from name when absent"
        );
        assert_eq!(
            config.zones[0].max_speed_kmh,
            Some(CentiKmh::from_wire(600))
        );
    }

    #[test]
    fn parse_zone_hold_config_custom_zone_explicit_id_overrides_slug() {
        let raw = r#"
            [zone_hold]
            enabled = true
            age = 30

            [[zone_hold.zones]]
            id = "recovery-walk"
            name = "Recovery Walk"
            min_percent = 55
            max_percent = 65
        "#;
        let config = parse_zone_hold_config(raw);
        assert_eq!(config.zones[0].id, "recovery-walk");
    }

    #[test]
    fn parse_zone_hold_config_target_zone_accepts_a_string_id() {
        let raw = r#"
            [zone_hold]
            enabled = true
            age = 30
            target_zone = "tempo"
        "#;
        let config = parse_zone_hold_config(raw);
        assert_eq!(config.target_zone, ZoneSelector::Id("tempo".to_string()));
        let resolved = config.resolve_target_zone().expect("tempo exists");
        assert_eq!(resolved.id, "tempo");
    }

    #[test]
    fn parse_zone_hold_config_zone_with_absolute_bpm_bounds() {
        let raw = r#"
            [zone_hold]
            enabled = true
            age = 30

            [[zone_hold.zones]]
            name = "Manual"
            min_bpm = 110
            max_bpm = 130
        "#;
        let config = parse_zone_hold_config(raw);
        assert_eq!(
            config.zones[0].bounds,
            ZoneBounds::Absolute {
                min_bpm: 110,
                max_bpm: 130
            }
        );
    }
}
