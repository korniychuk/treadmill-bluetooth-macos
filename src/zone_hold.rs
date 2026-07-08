//! Zone Hold — HR-adaptive belt-speed controller (задача 027).
//!
//! Closed-loop mode that reads live bpm from the chest strap (задача 025) and
//! nudges the belt speed to keep the operator inside a target heart-rate zone
//! while walking. The controller targets a **heart rate**, not a fixed speed:
//! cardiovascular drift over a long session means the same physiological
//! effort needs a slowly *decreasing* speed over time, which this design
//! produces for free (see `docs/tasks/027`, §Фундаментальное решение).
//!
//! This module is pure and side-effect free — no BLE, no clock reads. Time and
//! bpm are always passed in by the caller (`crate::daemon`), mirroring
//! `presence.rs`/`activity.rs`, so the whole controller can be exercised with
//! synthetic HR traces in unit tests without a runtime.

use std::path::Path;
use std::time::Duration;

use tracing::warn;

/// Fallback target zone (1-based index into the configured/default zones)
/// when `target_zone` is absent — Zone 2 (Aerobic base), the mode's whole
/// reason for existing (see task doc §Физиология).
pub const DEFAULT_TARGET_ZONE: u8 = 2;
/// Hard floor on commanded belt speed (km/h).
pub const DEFAULT_MIN_SPEED_KMH: f32 = 2.0;
/// Global ceiling on commanded belt speed (km/h); a zone may override it lower
/// or higher via `ZoneDef::max_speed_kmh`.
pub const DEFAULT_MAX_SPEED_KMH: f32 = 4.5;
/// Linear ramp duration at session start, HR ignored throughout (HR-kinetics
/// has not settled in the first few minutes — see task doc §Физиология).
pub const DEFAULT_WARMUP_MINUTES: i64 = 5;
/// Cadence of closed-loop corrections once past warm-up.
pub const DEFAULT_CORRECTION_INTERVAL_SECONDS: i64 = 20;
/// `tracking = "center"` deadband around the zone midpoint, bpm.
pub const DEFAULT_DEADBAND_BPM: i64 = 3;
/// Max speed change applied per correction, km/h.
pub const DEFAULT_MAX_STEP_KMH: f32 = 0.3;
/// Grace window after returning to `Walking` during which no correction runs.
pub const DEFAULT_REENTRY_GRACE_SECONDS: i64 = 45;
/// HR percent-of-HRmax above which the controller force-reduces regardless of
/// the normal band/center logic.
pub const DEFAULT_SAFETY_CAP_PERCENT: f32 = 80.0;

/// Which formula converts age (+ optionally resting HR) into zone bpm bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `% of HRmax` — industry-standard default (Garmin/Polar age-only).
    HrMax,
    /// `% of heart-rate reserve` (`HRmax - HRrest`), needs `resting_hr`.
    Karvonen,
}

/// Which targeting aggressiveness the operator chose (task doc §Режимы).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tracking {
    /// Hold the whole zone — no correction while inside it (deadband = zone width).
    Band,
    /// Hold the zone midpoint with a proportional (P-controller) step.
    Center,
}

/// Where a zone's bpm bounds come from: a percentage (resolved via [`Method`]
/// against this operator's HRmax/HRR), or an absolute bpm pair for manual
/// fine-tuning (task doc §Config — "ЛИБО абсолютные bpm").
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ZoneBounds {
    Percent { min: f32, max: f32 },
    Absolute { min_bpm: u16, max_bpm: u16 },
}

/// One configured heart-rate zone.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneDef {
    /// Stable identifier used by `target_zone` and `tm zone target` — either
    /// an explicit `id = "..."` in the config, or a slug derived from `name`
    /// when absent. Unlike the 1-based position, this survives reordering or
    /// inserting zones in `config.toml`.
    pub id: String,
    pub name: String,
    pub bounds: ZoneBounds,
    /// Per-zone override of the global `max_speed_kmh` — `None` defers to it
    /// (task doc §Min/max: `effective max = zone.max_speed ?? global`).
    pub max_speed_kmh: Option<f32>,
}

/// Derive a stable `id` from a zone `name` when the config doesn't set one
/// explicitly: lowercase, non-alphanumeric runs collapsed to a single `-`,
/// no leading/trailing `-`. E.g. `"Aerobic base"` → `"aerobic-base"`.
fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// The built-in 5-zone table (task doc §Физиология), used whenever the config
/// has no `[[zone_hold.zones]]` entries.
pub fn default_zones() -> Vec<ZoneDef> {
    [
        ("Recovery", 50.0, 60.0),
        ("Aerobic base", 60.0, 70.0),
        ("Tempo", 70.0, 80.0),
        ("Threshold", 80.0, 90.0),
        ("VO2max", 90.0, 100.0),
    ]
    .into_iter()
    .map(|(name, min, max)| ZoneDef {
        id: slugify(name),
        name: name.to_string(),
        bounds: ZoneBounds::Percent { min, max },
        max_speed_kmh: None,
    })
    .collect()
}

/// Which zone `target_zone` points at: the legacy 1-based position, or a
/// stable `id`/name lookup (task doc follow-up — named zones). A bare TOML
/// integer parses as `Number`; a quoted string as `Id`.
#[derive(Debug, Clone, PartialEq)]
pub enum ZoneSelector {
    Number(u8),
    Id(String),
}

/// Find the zone a selector points at, plus its current 1-based position.
/// `Id` matching tries, in order: exact `id` (case-insensitive), exact
/// `name`, then `name` substring — so `"aerobic"` finds `"Aerobic base"`
/// without requiring the exact id.
pub fn find_zone<'a>(
    zones: &'a [ZoneDef],
    selector: &ZoneSelector,
) -> Option<(usize, &'a ZoneDef)> {
    match selector {
        ZoneSelector::Number(n) => {
            let index = n.checked_sub(1)? as usize;
            zones.get(index).map(|z| (index + 1, z))
        }
        ZoneSelector::Id(raw) => {
            let needle = raw.trim().to_lowercase();
            if needle.is_empty() {
                return None;
            }
            zones
                .iter()
                .position(|z| z.id.to_lowercase() == needle)
                .or_else(|| zones.iter().position(|z| z.name.to_lowercase() == needle))
                .or_else(|| {
                    zones
                        .iter()
                        .position(|z| z.name.to_lowercase().contains(&needle))
                })
                .map(|index| (index + 1, &zones[index]))
        }
    }
}

/// Resolved bpm zone target plus identity and the effective max speed for it
/// (per-zone override, falling back to the global cap) — everything the
/// controller, `tm status`, and the widget classifier need for one session.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedZone {
    /// 1-based position among the configured zones — display-only, not a
    /// stable identity (that's `id`).
    pub number: usize,
    pub id: String,
    pub name: String,
    pub low_bpm: u16,
    pub high_bpm: u16,
    pub effective_max_speed_kmh: f32,
}

/// The full `[zone_hold]` configuration, parsed with per-user compiled-in
/// defaults for any absent key — same absent-is-quiet/invalid-is-WARN
/// convention as `goals::GapSetting`/`AutoPauseSetting`.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneHoldConfig {
    pub enabled: bool,
    pub age: Option<u32>,
    pub resting_hr: Option<u16>,
    pub method: Method,
    pub target_zone: ZoneSelector,
    pub min_speed_kmh: f32,
    pub max_speed_kmh: f32,
    pub tracking: Tracking,
    pub warmup_minutes: i64,
    pub correction_interval_seconds: i64,
    pub deadband_bpm: i64,
    pub max_step_kmh: f32,
    pub reentry_grace_seconds: i64,
    pub safety_cap_percent: f32,
    pub zones: Vec<ZoneDef>,
}

impl ZoneHoldConfig {
    /// Compiled-in defaults with the master-switch off — used whenever the
    /// config file is missing, unreadable, or has no `[zone_hold]` table.
    pub fn disabled_default() -> Self {
        Self {
            enabled: false,
            age: None,
            resting_hr: None,
            method: Method::HrMax,
            target_zone: ZoneSelector::Number(DEFAULT_TARGET_ZONE),
            min_speed_kmh: DEFAULT_MIN_SPEED_KMH,
            max_speed_kmh: DEFAULT_MAX_SPEED_KMH,
            tracking: Tracking::Band,
            warmup_minutes: DEFAULT_WARMUP_MINUTES,
            correction_interval_seconds: DEFAULT_CORRECTION_INTERVAL_SECONDS,
            deadband_bpm: DEFAULT_DEADBAND_BPM,
            max_step_kmh: DEFAULT_MAX_STEP_KMH,
            reentry_grace_seconds: DEFAULT_REENTRY_GRACE_SECONDS,
            safety_cap_percent: DEFAULT_SAFETY_CAP_PERCENT,
            zones: default_zones(),
        }
    }

    /// The operator's HRmax via Tanaka (`208 − 0.7·age`, task doc §Физиология),
    /// or `None` when `age` was never configured (onboarding always sets it
    /// alongside `enabled = true`, so this is only `None` for a config edited
    /// by hand without `tm zone setup`).
    pub fn hrmax(&self) -> Option<f32> {
        self.age.map(hrmax_tanaka)
    }

    /// Resolve `target_zone` to its bpm bounds and effective max speed for
    /// this session, or `None` when the selector doesn't match any configured
    /// zone or `age` is unconfigured (nothing to compute HRmax from).
    pub fn resolve_target_zone(&self) -> Option<ResolvedZone> {
        let hrmax = self.hrmax()?;
        let (number, zone) = find_zone(&self.zones, &self.target_zone)?;
        let (low_bpm, high_bpm) =
            resolve_zone_bpm(hrmax, self.resting_hr, self.method, zone.bounds);
        Some(ResolvedZone {
            number,
            id: zone.id.clone(),
            name: zone.name.clone(),
            low_bpm,
            high_bpm,
            effective_max_speed_kmh: zone.max_speed_kmh.unwrap_or(self.max_speed_kmh),
        })
    }

    /// The safety-cap bpm threshold (task doc §Safety) — `None` without a
    /// resolvable HRmax, same precondition as [`Self::resolve_target_zone`].
    pub fn safety_cap_bpm(&self) -> Option<u16> {
        Some(safety_cap_bpm(self.hrmax()?, self.safety_cap_percent))
    }
}

/// Tanaka HRmax formula (JACC 2001, n=18712, r=−0.90) — validated more
/// accurately than the folk `220 − age` (task doc §Физиология).
pub fn hrmax_tanaka(age: u32) -> f32 {
    208.0 - 0.7 * age as f32
}

/// Convert one zone's bounds to absolute `[low_bpm, high_bpm]` given this
/// operator's HRmax (+ resting HR for Karvonen). `Absolute` bounds bypass the
/// method entirely (manual fine-tuning, task doc §Config).
pub fn resolve_zone_bpm(
    hrmax: f32,
    resting_hr: Option<u16>,
    method: Method,
    bounds: ZoneBounds,
) -> (u16, u16) {
    match bounds {
        ZoneBounds::Absolute { min_bpm, max_bpm } => (min_bpm, max_bpm),
        ZoneBounds::Percent { min, max } => match method {
            Method::HrMax => (
                (hrmax * min / 100.0).round() as u16,
                (hrmax * max / 100.0).round() as u16,
            ),
            Method::Karvonen => {
                let resting = resting_hr.unwrap_or(0) as f32;
                let hrr = (hrmax - resting).max(0.0);
                (
                    (resting + hrr * min / 100.0).round() as u16,
                    (resting + hrr * max / 100.0).round() as u16,
                )
            }
        },
    }
}

/// HRmax-percent above which the controller must force-reduce regardless of
/// the normal band/center correction (task doc §Safety).
pub fn safety_cap_bpm(hrmax: f32, safety_cap_percent: f32) -> u16 {
    (hrmax * safety_cap_percent / 100.0).round() as u16
}

/// Where the current bpm sits relative to the target zone — the classification
/// the widget colours (task doc §Индикация зоны).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZonePosition {
    Below,
    In,
    Above,
}

impl ZonePosition {
    /// Stable wire string for `daemon_status.zone_hold_position` / the
    /// `tm widget` `HR_ZONE` field — decoupled from `Debug`'s formatting so a
    /// derive change can never silently alter the contract.
    pub fn wire(self) -> &'static str {
        match self {
            ZonePosition::Below => "below",
            ZonePosition::In => "in",
            ZonePosition::Above => "above",
        }
    }
}

/// Classify `bpm` against `[low_bpm, high_bpm]` (inclusive both ends).
pub fn classify_position(bpm: u16, low_bpm: u16, high_bpm: u16) -> ZonePosition {
    if bpm < low_bpm {
        ZonePosition::Below
    } else if bpm > high_bpm {
        ZonePosition::Above
    } else {
        ZonePosition::In
    }
}

/// Linear warm-up target: `start_kmh` at `elapsed = 0`, `target_kmh` at
/// `elapsed >= warmup`. HR is never read here — see task doc §Жизненный цикл.
/// A zero/negative `warmup` skips straight to `target_kmh` (defensive; the
/// config loader never produces one, but a hand-edited `0` should not divide
/// by zero here).
pub fn warmup_target_speed(
    start_kmh: f32,
    target_kmh: f32,
    elapsed: Duration,
    warmup: Duration,
) -> f32 {
    if warmup.is_zero() || elapsed >= warmup {
        return target_kmh;
    }
    let frac = elapsed.as_secs_f32() / warmup.as_secs_f32();
    start_kmh + (target_kmh - start_kmh) * frac
}

/// Inputs the pure closed-loop controller needs for one correction — bundled
/// so `next_speed` reads as one call instead of an 8-parameter list.
#[derive(Debug, Clone, Copy)]
pub struct ControllerParams {
    pub tracking: Tracking,
    pub zone_low_bpm: u16,
    pub zone_high_bpm: u16,
    pub deadband_bpm: i64,
    pub max_step_kmh: f32,
    pub min_speed_kmh: f32,
    pub max_speed_kmh: f32,
}

/// One closed-loop correction (task doc §Control-loop). `None` means "leave
/// the belt speed alone" — either the bpm is where it should be, or the belt
/// is already pinned at the clamp in the direction the correction would move
/// it (task doc §Границы достижимости: never chase past min/max).
pub fn next_speed(params: &ControllerParams, current_speed_kmh: f32, bpm: u16) -> Option<f32> {
    let low = params.zone_low_bpm as f32;
    let high = params.zone_high_bpm as f32;
    let bpm = bpm as f32;

    let step = match params.tracking {
        Tracking::Band => {
            if bpm >= low && bpm <= high {
                return None;
            }
            if bpm < low {
                params.max_step_kmh // below zone → speed up
            } else {
                -params.max_step_kmh // above zone → slow down
            }
        }
        Tracking::Center => {
            let center = (low + high) / 2.0;
            let half_width = (high - low) / 2.0;
            let error = bpm - center;
            if error.abs() <= params.deadband_bpm as f32 || half_width <= 0.0 {
                return None;
            }
            // Scaled so the step reaches max_step_kmh right at the zone boundary
            // (task doc §Режимы: "у границ зоны шаг максимален").
            let k = params.max_step_kmh / half_width;
            let magnitude = (k * error.abs()).min(params.max_step_kmh);
            if error > 0.0 {
                -magnitude // above centre → slow down
            } else {
                magnitude // below centre → speed up
            }
        }
    };

    let target = (current_speed_kmh + step).clamp(params.min_speed_kmh, params.max_speed_kmh);
    (target != current_speed_kmh).then_some(target)
}

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
fn parse_zone_hold_config(raw: &str) -> ZoneHoldConfig {
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

    let min_speed_kmh = positive_float_or(table, "min_speed", defaults.min_speed_kmh);
    let max_speed_kmh = positive_float_or(table, "max_speed", defaults.max_speed_kmh);

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

    let warmup_minutes = positive_int_or(table, "warmup_minutes", defaults.warmup_minutes);
    let correction_interval_seconds = positive_int_or(
        table,
        "correction_interval_seconds",
        defaults.correction_interval_seconds,
    );
    let deadband_bpm = positive_int_or(table, "deadband_bpm", defaults.deadband_bpm);
    let max_step_kmh = positive_float_or(table, "max_step_kmh", defaults.max_step_kmh);
    let reentry_grace_seconds = positive_int_or(
        table,
        "reentry_grace_seconds",
        defaults.reentry_grace_seconds,
    );
    let safety_cap_percent =
        positive_float_or(table, "safety_cap_percent", defaults.safety_cap_percent);

    let zones = table
        .get("zones")
        .and_then(|v| v.as_array())
        .filter(|arr| !arr.is_empty())
        .map(|arr| arr.iter().filter_map(parse_zone_def).collect::<Vec<_>>())
        .filter(|zones| !zones.is_empty())
        .unwrap_or_else(default_zones);

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

fn parse_zone_def(value: &toml::Value) -> Option<ZoneDef> {
    let name = value.get("name")?.as_str()?.to_string();
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| slugify(&name));
    let max_speed_kmh = value
        .get("max_speed")
        .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|n| n as f64)))
        .map(|n| n as f32)
        .filter(|&n| n > 0.0);

    let bounds = if let (Some(min_bpm), Some(max_bpm)) = (
        value.get("min_bpm").and_then(|v| v.as_integer()),
        value.get("max_bpm").and_then(|v| v.as_integer()),
    ) {
        ZoneBounds::Absolute {
            min_bpm: min_bpm as u16,
            max_bpm: max_bpm as u16,
        }
    } else {
        let min = value
            .get("min_percent")?
            .as_float()
            .or_else(|| value.get("min_percent")?.as_integer().map(|n| n as f64))?
            as f32;
        let max = value
            .get("max_percent")?
            .as_float()
            .or_else(|| value.get("max_percent")?.as_integer().map(|n| n as f64))?
            as f32;
        ZoneBounds::Percent { min, max }
    };

    Some(ZoneDef {
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

/// Write (or update) the `[zone_hold]` table in the per-user config, preserving
/// every other section verbatim. Used by the `tm zone` CLI (on/setup/limits/
/// target/mode) so onboarding never clobbers goals/auto-pause the operator
/// already configured. `updates` are applied as string key/value TOML
/// fragments (already-formatted, e.g. `"age = 34"`) — simple line-based
/// upsert rather than a full TOML AST rewrite, which would risk reformatting
/// (and losing the comments in) the rest of a hand-edited file.
pub fn upsert_zone_hold_keys(path: &Path, updates: &[(&str, String)]) -> anyhow::Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();

    let section_start = lines.iter().position(|l| l.trim() == "[zone_hold]");
    let Some(section_start) = section_start else {
        // No existing section — append a fresh one with just these keys.
        if !existing.is_empty() && !existing.ends_with('\n') {
            lines.push(String::new());
        }
        lines.push("[zone_hold]".to_string());
        for (key, value) in updates {
            lines.push(format!("{key} = {value}"));
        }
        lines.push(String::new());
        std::fs::write(path, lines.join("\n"))?;
        return Ok(());
    };

    let section_end = lines[section_start + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .map(|offset| section_start + 1 + offset)
        .unwrap_or(lines.len());

    for (key, value) in updates {
        let prefix = format!("{key} =");
        let existing_line = lines[section_start + 1..section_end]
            .iter()
            .position(|l| l.trim_start().starts_with(&prefix));
        let new_line = format!("{key} = {value}");
        match existing_line {
            Some(offset) => lines[section_start + 1 + offset] = new_line,
            None => lines.insert(section_end, new_line),
        }
    }

    std::fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hrmax_tanaka_matches_known_values() {
        // 30yo → 208 − 0.7·30 = 187.
        assert!((hrmax_tanaka(30) - 187.0).abs() < 0.01);
        // 50yo → 208 − 0.7·50 = 173.
        assert!((hrmax_tanaka(50) - 173.0).abs() < 0.01);
    }

    #[test]
    fn resolve_zone_bpm_hrmax_matches_task_doc_example() {
        // 30yo, HRmax=187, Zone 2 (60-70%) ≈ 112-131 bpm (task doc example).
        let hrmax = hrmax_tanaka(30);
        let (low, high) = resolve_zone_bpm(
            hrmax,
            None,
            Method::HrMax,
            ZoneBounds::Percent {
                min: 60.0,
                max: 70.0,
            },
        );
        assert_eq!((low, high), (112, 131));
    }

    #[test]
    fn resolve_zone_bpm_karvonen_sits_higher_than_hrmax() {
        // Karvonen zones sit ~30-35bpm above hrmax-method zones at the same
        // percent (task doc §Физиология note on not mixing methods).
        let hrmax = hrmax_tanaka(30);
        let resting = Some(60);
        let (low_hrmax, high_hrmax) = resolve_zone_bpm(
            hrmax,
            resting,
            Method::HrMax,
            ZoneBounds::Percent {
                min: 60.0,
                max: 70.0,
            },
        );
        let (low_karvonen, high_karvonen) = resolve_zone_bpm(
            hrmax,
            resting,
            Method::Karvonen,
            ZoneBounds::Percent {
                min: 60.0,
                max: 70.0,
            },
        );
        assert!(low_karvonen > low_hrmax);
        assert!(high_karvonen > high_hrmax);
    }

    #[test]
    fn resolve_zone_bpm_absolute_ignores_method_and_hrmax() {
        let (low, high) = resolve_zone_bpm(
            999.0,
            None,
            Method::HrMax,
            ZoneBounds::Absolute {
                min_bpm: 110,
                max_bpm: 130,
            },
        );
        assert_eq!((low, high), (110, 130));
    }

    #[test]
    fn safety_cap_bpm_is_80_percent_of_hrmax_by_default() {
        let hrmax = 187.0;
        assert_eq!(safety_cap_bpm(hrmax, 80.0), 150); // 149.6 rounds to 150
    }

    #[test]
    fn resolve_target_zone_picks_the_configured_zone_by_number() {
        let mut config = ZoneHoldConfig::disabled_default();
        config.age = Some(30);
        config.target_zone = ZoneSelector::Number(2);
        let resolved = config.resolve_target_zone().expect("zone 2 exists");
        assert_eq!((resolved.low_bpm, resolved.high_bpm), (112, 131));
        assert_eq!(resolved.effective_max_speed_kmh, DEFAULT_MAX_SPEED_KMH);
    }

    #[test]
    fn resolve_target_zone_by_exact_id() {
        let mut config = ZoneHoldConfig::disabled_default();
        config.age = Some(30);
        config.target_zone = ZoneSelector::Id("aerobic-base".to_string());
        let resolved = config.resolve_target_zone().expect("aerobic-base exists");
        assert_eq!(resolved.number, 2);
        assert_eq!(resolved.id, "aerobic-base");
    }

    #[test]
    fn resolve_target_zone_by_id_is_case_insensitive_and_falls_back_to_name_substring() {
        let mut config = ZoneHoldConfig::disabled_default();
        config.age = Some(30);
        config.target_zone = ZoneSelector::Id("AEROBIC".to_string());
        let resolved = config
            .resolve_target_zone()
            .expect("substring match on name");
        assert_eq!(resolved.id, "aerobic-base");
    }

    #[test]
    fn resolve_target_zone_none_for_unknown_id() {
        let mut config = ZoneHoldConfig::disabled_default();
        config.age = Some(30);
        config.target_zone = ZoneSelector::Id("nonexistent".to_string());
        assert!(config.resolve_target_zone().is_none());
    }

    #[test]
    fn default_zones_have_stable_slug_ids() {
        let ids: Vec<String> = default_zones().into_iter().map(|z| z.id).collect();
        assert_eq!(
            ids,
            vec!["recovery", "aerobic-base", "tempo", "threshold", "vo2max"]
        );
    }

    #[test]
    fn resolve_target_zone_none_without_age() {
        let config = ZoneHoldConfig::disabled_default();
        assert!(config.resolve_target_zone().is_none());
    }

    #[test]
    fn resolve_target_zone_uses_per_zone_max_speed_override() {
        let mut config = ZoneHoldConfig::disabled_default();
        config.age = Some(30);
        config.target_zone = ZoneSelector::Number(2);
        config.zones[1].max_speed_kmh = Some(5.5);
        let resolved = config.resolve_target_zone().expect("zone 2 exists");
        assert_eq!(resolved.effective_max_speed_kmh, 5.5);
    }

    #[test]
    fn classify_position_below_in_above() {
        assert_eq!(classify_position(100, 112, 131), ZonePosition::Below);
        assert_eq!(classify_position(112, 112, 131), ZonePosition::In);
        assert_eq!(classify_position(120, 112, 131), ZonePosition::In);
        assert_eq!(classify_position(131, 112, 131), ZonePosition::In);
        assert_eq!(classify_position(140, 112, 131), ZonePosition::Above);
    }

    #[test]
    fn warmup_ramps_linearly_then_snaps_to_target() {
        let start = 1.0;
        let target = 3.0;
        let warmup = Duration::from_secs(300);
        assert_eq!(
            warmup_target_speed(start, target, Duration::ZERO, warmup),
            1.0
        );
        assert!(
            (warmup_target_speed(start, target, Duration::from_secs(150), warmup) - 2.0).abs()
                < 0.01
        );
        assert_eq!(
            warmup_target_speed(start, target, Duration::from_secs(300), warmup),
            target
        );
        assert_eq!(
            warmup_target_speed(start, target, Duration::from_secs(600), warmup),
            target
        );
    }

    fn band_params() -> ControllerParams {
        ControllerParams {
            tracking: Tracking::Band,
            zone_low_bpm: 112,
            zone_high_bpm: 131,
            deadband_bpm: DEFAULT_DEADBAND_BPM,
            max_step_kmh: DEFAULT_MAX_STEP_KMH,
            min_speed_kmh: DEFAULT_MIN_SPEED_KMH,
            max_speed_kmh: DEFAULT_MAX_SPEED_KMH,
        }
    }

    #[test]
    fn band_mode_does_not_correct_inside_the_zone() {
        let params = band_params();
        assert_eq!(next_speed(&params, 3.0, 112), None);
        assert_eq!(next_speed(&params, 3.0, 120), None);
        assert_eq!(next_speed(&params, 3.0, 131), None);
    }

    #[test]
    fn band_mode_steps_toward_the_zone_when_outside() {
        let params = band_params();
        // Below zone → speed up by max_step.
        assert_eq!(next_speed(&params, 3.0, 100), Some(3.3));
        // Above zone → slow down by max_step.
        assert_eq!(next_speed(&params, 3.0, 140), Some(2.7));
    }

    #[test]
    fn band_mode_clamps_to_min_max_and_reports_no_change_at_the_pin() {
        let params = band_params();
        // Already at max, HR still low → stays pinned, no spurious "change".
        assert_eq!(next_speed(&params, DEFAULT_MAX_SPEED_KMH, 100), None);
        // Already at min, HR still high → stays pinned.
        assert_eq!(next_speed(&params, DEFAULT_MIN_SPEED_KMH, 140), None);
    }

    fn center_params() -> ControllerParams {
        ControllerParams {
            tracking: Tracking::Center,
            ..band_params()
        }
    }

    #[test]
    fn center_mode_does_not_correct_within_deadband_of_the_midpoint() {
        let params = center_params();
        // Midpoint of 112-131 is 121.5.
        assert_eq!(next_speed(&params, 3.0, 122), None); // within ±3
        assert_eq!(next_speed(&params, 3.0, 119), None);
    }

    #[test]
    fn center_mode_is_more_aggressive_near_the_boundary_than_near_the_midpoint() {
        let params = center_params();
        // Small deviation past the deadband → small step.
        let near = next_speed(&params, 3.0, 126).expect("some correction");
        // At the boundary → step saturates at max_step_kmh.
        let at_boundary = next_speed(&params, 3.0, 131).expect("some correction");
        let near_step = (near - 3.0).abs();
        let boundary_step = (at_boundary - 3.0).abs();
        assert!(near_step < boundary_step);
        assert!((boundary_step - DEFAULT_MAX_STEP_KMH).abs() < 0.01);
    }

    #[test]
    fn center_mode_direction_matches_band_mode() {
        let params = center_params();
        // Below centre → speed up; above centre → slow down.
        assert!(next_speed(&params, 3.0, 100).unwrap() > 3.0);
        assert!(next_speed(&params, 3.0, 140).unwrap() < 3.0);
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
        assert_eq!(config.min_speed_kmh, 2.5);
        assert_eq!(config.max_speed_kmh, 5.0);
        assert_eq!(config.tracking, Tracking::Center);
        assert_eq!(config.warmup_minutes, 3);
        assert_eq!(config.correction_interval_seconds, 15);
        assert_eq!(config.deadband_bpm, 4);
        assert_eq!(config.max_step_kmh, 0.2);
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
        assert_eq!(config.max_speed_kmh, DEFAULT_MAX_SPEED_KMH);
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
        assert_eq!(config.zones[0].max_speed_kmh, Some(6.0));
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

    #[test]
    fn upsert_zone_hold_keys_appends_new_section() {
        let dir = std::env::temp_dir().join(format!("tm-zh-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "goals = [8000]\n").unwrap();

        upsert_zone_hold_keys(
            &path,
            &[("enabled", "true".to_string()), ("age", "34".to_string())],
        )
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("[zone_hold]"));
        assert!(written.contains("enabled = true"));
        assert!(written.contains("age = 34"));
        assert!(written.contains("goals = [8000]"));

        // A second upsert must update in place, not duplicate the key.
        upsert_zone_hold_keys(&path, &[("age", "40".to_string())]).unwrap();
        let updated = std::fs::read_to_string(&path).unwrap();
        assert_eq!(updated.matches("age =").count(), 1);
        assert!(updated.contains("age = 40"));

        std::fs::remove_file(&path).ok();
    }
}
