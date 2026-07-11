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

mod cli_config;
mod config;
mod controller;

pub use cli_config::{replace_zones, upsert_zone_hold_keys};
pub use config::{config_path, load_zone_hold_config};
pub use controller::{
    ControllerParams, next_speed, safety_force_reduce_target, warmup_target_speed,
};

use tracing::info;

use crate::speed::CentiKmh;

/// Fallback target zone (1-based index into the configured/default zones)
/// when `target_zone` is absent — Zone 2 (Aerobic base), the mode's whole
/// reason for existing (see task doc §Физиология).
pub const DEFAULT_TARGET_ZONE: u8 = 2;
/// Hard floor on commanded belt speed (2.0 km/h).
pub const DEFAULT_MIN_SPEED: CentiKmh = CentiKmh::from_wire(200);
/// Global ceiling on commanded belt speed (4.5 km/h); a zone may override it
/// lower or higher via `ZoneDef::max_speed_kmh`.
pub const DEFAULT_MAX_SPEED: CentiKmh = CentiKmh::from_wire(450);
/// Linear ramp duration at session start, HR ignored throughout (HR-kinetics
/// has not settled in the first few minutes — see task doc §Физиология).
pub const DEFAULT_WARMUP_MINUTES: i64 = 5;
/// Cadence of closed-loop corrections once past warm-up.
pub const DEFAULT_CORRECTION_INTERVAL_SECONDS: i64 = 20;
/// `tracking = "center"` deadband around the zone midpoint, bpm.
pub const DEFAULT_DEADBAND_BPM: i64 = 3;
/// Max speed change applied per correction (0.3 km/h).
pub const DEFAULT_MAX_STEP: CentiKmh = CentiKmh::from_wire(30);
/// Controller deadband: minimum speed delta worth a Control Point write
/// (5 centi = 0.05 km/h). Not float glue — with [`CentiKmh`] the 030
/// representation gap is gone by construction; this remains a policy
/// ("don't jiggle the belt for a 1-centi wiggle") that also absorbs genuine
/// device-reported jitter if a model has any. Well below `max_step`, so real
/// corrections are unaffected. See задача 030 / 054.
pub const MIN_SPEED_CHANGE: CentiKmh = CentiKmh::from_wire(5);
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
    pub max_speed_kmh: Option<CentiKmh>,
}

/// Derive a stable `id` from a zone `name` when the config doesn't set one
/// explicitly: lowercase, non-alphanumeric runs collapsed to a single `-`,
/// no leading/trailing `-`. E.g. `"Aerobic base"` → `"aerobic-base"`.
pub fn slugify(name: &str) -> String {
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
                    let matches: Vec<usize> = zones
                        .iter()
                        .enumerate()
                        .filter(|(_, z)| z.name.to_lowercase().contains(&needle))
                        .map(|(i, _)| i)
                        .collect();
                    if let Some(&first) = matches.first() {
                        // Order-dependent when multiple names contain the needle
                        // (задача 045) — surface so operators notice ambiguity.
                        if matches.len() > 1 {
                            info!(
                                needle = %raw,
                                first_id = %zones[first].id,
                                match_count = matches.len(),
                                "zone_hold: target matched by name substring (first wins)"
                            );
                        } else {
                            info!(
                                needle = %raw,
                                id = %zones[first].id,
                                "zone_hold: target matched by name substring"
                            );
                        }
                        Some(first)
                    } else {
                        None
                    }
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
    pub effective_max_speed_kmh: CentiKmh,
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
    pub min_speed_kmh: CentiKmh,
    pub max_speed_kmh: CentiKmh,
    pub tracking: Tracking,
    pub warmup_minutes: i64,
    pub correction_interval_seconds: i64,
    pub deadband_bpm: i64,
    pub max_step_kmh: CentiKmh,
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
            min_speed_kmh: DEFAULT_MIN_SPEED,
            max_speed_kmh: DEFAULT_MAX_SPEED,
            tracking: Tracking::Band,
            warmup_minutes: DEFAULT_WARMUP_MINUTES,
            correction_interval_seconds: DEFAULT_CORRECTION_INTERVAL_SECONDS,
            deadband_bpm: DEFAULT_DEADBAND_BPM,
            max_step_kmh: DEFAULT_MAX_STEP,
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
                // Missing/0 resting → algebraically identical to HRmax percents
                // (задача 040). WARN lives at config load (not here): this pure
                // function runs every zone-hold tick and must not spam logs.
                // Zones sit systematically lower than true Karvonen with resting.
                let resting = resting_hr.filter(|&r| r > 0).unwrap_or(0) as f32;
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
    fn resolve_zone_bpm_karvonen_without_resting_equals_hrmax() {
        let hrmax = 187.0;
        let bounds = ZoneBounds::Percent {
            min: 60.0,
            max: 70.0,
        };
        let with_hrmax = resolve_zone_bpm(hrmax, None, Method::HrMax, bounds);
        let with_karvonen_missing = resolve_zone_bpm(hrmax, None, Method::Karvonen, bounds);
        let with_karvonen_zero = resolve_zone_bpm(hrmax, Some(0), Method::Karvonen, bounds);
        assert_eq!(with_karvonen_missing, with_hrmax);
        assert_eq!(with_karvonen_zero, with_hrmax);
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
        assert_eq!(resolved.effective_max_speed_kmh, DEFAULT_MAX_SPEED);
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
        config.zones[1].max_speed_kmh = Some(CentiKmh::from_wire(550));
        let resolved = config.resolve_target_zone().expect("zone 2 exists");
        assert_eq!(resolved.effective_max_speed_kmh, CentiKmh::from_wire(550));
    }

    #[test]
    fn classify_position_below_in_above() {
        assert_eq!(classify_position(100, 112, 131), ZonePosition::Below);
        assert_eq!(classify_position(112, 112, 131), ZonePosition::In);
        assert_eq!(classify_position(120, 112, 131), ZonePosition::In);
        assert_eq!(classify_position(131, 112, 131), ZonePosition::In);
        assert_eq!(classify_position(140, 112, 131), ZonePosition::Above);
    }
}
