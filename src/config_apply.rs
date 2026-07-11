//! Typed config hot-reload: `ConfigDelta` → session effects (задача 052).
//!
//! Pure decision layer for mid-session config edits. IO is limited to
//! [`reload_if_changed`] (mtime gate + loaders); [`diff`] and [`apply_config`]
//! take no clocks and touch no BLE. The daemon executor turns
//! [`ConfigEffect`]s into phase/snapshot mutations and logs each applied
//! effect once.

use std::time::{Duration, SystemTime};

use crate::goals::{self, Goal};
use crate::zone_hold::{self, ZoneHoldConfig};

/// The three hot-reloadable config values (moved from `daemon.rs` verbatim).
#[derive(Debug, Clone, PartialEq)]
pub struct LiveConfig {
    pub goals: Vec<Goal>,
    pub auto_pause: Option<Duration>,
    pub zone_hold: ZoneHoldConfig,
}

/// What actually changed on disk. `None` field = unchanged.
/// `auto_pause` is `Option<Option<Duration>>`: outer = "changed?",
/// inner = the new value (`None` = auto-pause disabled).
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigDelta {
    pub goals: Option<Vec<Goal>>,
    pub auto_pause: Option<Option<Duration>>,
    pub zone_hold: Option<ZoneHoldConfig>,
}

impl ConfigDelta {
    /// True when no field changed (mtime moved, content identical — задача 022
    /// still refreshes the `tm status` snapshot).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.goals.is_none() && self.auto_pause.is_none() && self.zone_hold.is_none()
    }
}

/// Flat mirror of `ZoneHoldPhase` without `Instant` payload (session-loop stays
/// in `daemon.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseKind {
    Off,
    Ramp,
    Hold,
    Frozen,
    Grace,
}

/// Session context the pure apply path needs to decide engage / disengage /
/// re-resolve / warmup retarget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub phase: PhaseKind,
    /// `accumulator.state() == PresenceState::Walking`
    pub walking: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisengageReason {
    DisabledInConfig,
    /// Age removed, or `target_zone` no longer matches any zone.
    TargetUnresolvable,
}

/// Session-side consequences of applying a [`ConfigDelta`]. Field updates
/// happen inside [`apply_config`]; these variants drive logging and phase
/// machine side-effects in the daemon executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigEffect {
    /// Goals list changed — executor logs old→new and uses the new list.
    GoalsChanged,
    /// Auto-pause threshold changed — executor logs old→new.
    AutoPauseChanged,
    /// Zone Hold must stop driving the belt NOW (enabled ↓, or target zone
    /// no longer resolvable — e.g. age removed). Generalises the 032 ad-hoc fix.
    ZoneDisengage(DisengageReason),
    /// enabled ↑ mid-session while Walking — engage via the same
    /// `zone_hold_on_transition` path a fresh Walking entry uses.
    ZoneEngage,
    /// Zone-target-affecting keys changed while the controller is live
    /// (Hold/Grace/Frozen/Ramp): re-resolve `ResolvedZone` and refresh snapshot.
    ZoneReResolve,
    /// `warmup_minutes` changed mid-Ramp. POLICY: keep the ramp — do NOT restart.
    /// Log-only; next tick reads the new value from live config.
    ZoneWarmupRetarget { old_minutes: i64, new_minutes: i64 },
    /// `[zone_hold]` fields changed without a session phase effect (field-only
    /// reload — e.g. `target_zone` while Off, `deadband` mid-Hold). Log-only;
    /// the live config already holds the new values (backlog 011).
    ZoneConfigChanged {
        /// Stable field names that differed (for the INFO line).
        fields: Vec<&'static str>,
    },
}

/// Pure diff old vs new (each field via `PartialEq`, as the reload branch did).
pub fn diff(old: &LiveConfig, new: &LiveConfig) -> ConfigDelta {
    ConfigDelta {
        goals: if old.goals != new.goals {
            Some(new.goals.clone())
        } else {
            None
        },
        auto_pause: if old.auto_pause != new.auto_pause {
            Some(new.auto_pause)
        } else {
            None
        },
        zone_hold: if old.zone_hold != new.zone_hold {
            Some(new.zone_hold.clone())
        } else {
            None
        },
    }
}

/// `stat` + reload gate. Returns `None` when mtime hasn't moved (the common,
/// every-5s case — no file read, no parse, no logs). Returns `Some(delta)`
/// when the file WAS re-read — delta may still be empty (mtime moved, content
/// identical): the caller must refresh the `tm status` config snapshot
/// (задача 022) in that case too.
pub fn reload_if_changed(
    last_mtime: &mut Option<SystemTime>,
    current: &LiveConfig,
) -> Option<ConfigDelta> {
    let now_mtime = goals::config_mtime();
    if now_mtime == *last_mtime {
        return None;
    }
    *last_mtime = now_mtime;
    let loaded = LiveConfig {
        goals: goals::load_goals(),
        auto_pause: goals::load_auto_pause(),
        zone_hold: zone_hold::load_zone_hold_config(),
    };
    Some(diff(current, &loaded))
}

/// Applies the delta to `config` (field updates) and decides session effects.
/// Pure: no IO, no clocks. Logging of applied effects is the executor's job.
///
/// Effect order is deterministic: disengage first (safety-first), engage last.
pub fn apply_config(
    config: &mut LiveConfig,
    delta: ConfigDelta,
    snap: &SessionSnapshot,
) -> Vec<ConfigEffect> {
    let mut effects = Vec::new();

    if let Some(goals) = delta.goals {
        config.goals = goals;
        effects.push(ConfigEffect::GoalsChanged);
    }

    if let Some(auto_pause) = delta.auto_pause {
        config.auto_pause = auto_pause;
        effects.push(ConfigEffect::AutoPauseChanged);
    }

    if let Some(new_zh) = delta.zone_hold {
        let old_zh = config.zone_hold.clone();
        config.zone_hold = new_zh;
        push_zone_effects(&mut effects, &old_zh, &config.zone_hold, snap);
    }

    order_effects(&mut effects);
    effects
}

/// Zone-target-affecting keys (задача 052 matrix §5–8): a change while the
/// controller is live requires re-resolving bpm bounds / effective max speed.
fn zone_target_affecting_changed(old: &ZoneHoldConfig, new: &ZoneHoldConfig) -> bool {
    old.target_zone != new.target_zone
        || old.zones != new.zones
        || old.max_speed_kmh != new.max_speed_kmh
        || old.method != new.method
        || old.age != new.age
        || old.resting_hr != new.resting_hr
}

/// Field names that differ between two `[zone_hold]` configs (stable order).
fn zone_fields_changed(old: &ZoneHoldConfig, new: &ZoneHoldConfig) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if old.enabled != new.enabled {
        fields.push("enabled");
    }
    if old.age != new.age {
        fields.push("age");
    }
    if old.resting_hr != new.resting_hr {
        fields.push("resting_hr");
    }
    if old.method != new.method {
        fields.push("method");
    }
    if old.target_zone != new.target_zone {
        fields.push("target_zone");
    }
    if old.min_speed_kmh != new.min_speed_kmh {
        fields.push("min_speed_kmh");
    }
    if old.max_speed_kmh != new.max_speed_kmh {
        fields.push("max_speed_kmh");
    }
    if old.tracking != new.tracking {
        fields.push("tracking");
    }
    if old.warmup_minutes != new.warmup_minutes {
        fields.push("warmup_minutes");
    }
    if old.correction_interval_seconds != new.correction_interval_seconds {
        fields.push("correction_interval_seconds");
    }
    if old.deadband_bpm != new.deadband_bpm {
        fields.push("deadband_bpm");
    }
    if old.max_step_kmh != new.max_step_kmh {
        fields.push("max_step_kmh");
    }
    if old.reentry_grace_seconds != new.reentry_grace_seconds {
        fields.push("reentry_grace_seconds");
    }
    if old.safety_cap_percent != new.safety_cap_percent {
        fields.push("safety_cap_percent");
    }
    if old.zones != new.zones {
        fields.push("zones");
    }
    fields
}

fn push_zone_effects(
    effects: &mut Vec<ConfigEffect>,
    old: &ZoneHoldConfig,
    new: &ZoneHoldConfig,
    snap: &SessionSnapshot,
) {
    let phase_live = snap.phase != PhaseKind::Off;
    let before_len = effects.len();

    // enabled is false after this edit: disengage if the phase machine is
    // still live. Wins over retarget/re-resolve on the same edit
    // (safety-first). Covers true→false (the 032 case) AND false→false with
    // a live phase — the latter is an invariant violation (a disabled config
    // must never have a live phase) that the telemetry-loop gate would catch
    // one sample later; repairing it here keeps re-resolve/retarget from ever
    // firing for a disabled controller.
    if !new.enabled {
        if phase_live {
            effects.push(ConfigEffect::ZoneDisengage(
                DisengageReason::DisabledInConfig,
            ));
        }
    } else if phase_live {
        // Target no longer resolvable (age removed, unknown zone id, …).
        if new.resolve_target_zone().is_none() {
            effects.push(ConfigEffect::ZoneDisengage(
                DisengageReason::TargetUnresolvable,
            ));
        } else {
            if zone_target_affecting_changed(old, new) {
                effects.push(ConfigEffect::ZoneReResolve);
            }
            if snap.phase == PhaseKind::Ramp && old.warmup_minutes != new.warmup_minutes {
                effects.push(ConfigEffect::ZoneWarmupRetarget {
                    old_minutes: old.warmup_minutes,
                    new_minutes: new.warmup_minutes,
                });
            }
        }
    } else if !old.enabled && new.enabled && snap.walking {
        // phase == Off: engage only when enabled flips on while already Walking.
        effects.push(ConfigEffect::ZoneEngage);
    }

    // Field-only reload: config applied, no session phase effect — still log
    // which keys moved so `tm zone target 3` off-belt is visible (backlog 011).
    if effects.len() == before_len {
        let fields = zone_fields_changed(old, new);
        if !fields.is_empty() {
            effects.push(ConfigEffect::ZoneConfigChanged { fields });
        }
    }
}

fn effect_rank(effect: &ConfigEffect) -> u8 {
    match effect {
        ConfigEffect::ZoneDisengage(_) => 0,
        ConfigEffect::GoalsChanged => 1,
        ConfigEffect::AutoPauseChanged => 2,
        ConfigEffect::ZoneConfigChanged { .. } => 3,
        ConfigEffect::ZoneReResolve => 4,
        ConfigEffect::ZoneWarmupRetarget { .. } => 5,
        ConfigEffect::ZoneEngage => 6,
    }
}

fn order_effects(effects: &mut [ConfigEffect]) {
    effects.sort_by_key(effect_rank);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zone_hold::{ZoneSelector, warmup_target_speed};

    fn goals(thresholds: &[i64]) -> Vec<Goal> {
        goals::assign_tiers(thresholds)
    }

    fn live(zh: ZoneHoldConfig) -> LiveConfig {
        LiveConfig {
            goals: goals(&[8000, 10000, 12000]),
            auto_pause: Some(Duration::from_secs(5 * 60)),
            zone_hold: zh,
        }
    }

    fn zh_enabled() -> ZoneHoldConfig {
        let mut c = ZoneHoldConfig::disabled_default();
        c.enabled = true;
        c.age = Some(34);
        c
    }

    fn snap(phase: PhaseKind, walking: bool) -> SessionSnapshot {
        SessionSnapshot { phase, walking }
    }

    // --- matrix (задача 052) -------------------------------------------------

    #[test]
    fn apply_config_matrix_items_1_through_12_and_disable_plus_warmup_combo() {
        // (label, phase, walking, mutator old→new, expected effects)
        // Mutator receives an enabled base (or disabled when noted).
        struct Case {
            label: &'static str,
            phase: PhaseKind,
            walking: bool,
            /// Build (old_zh, new_zh) from a fresh `zh_enabled()` / disabled pair.
            build: fn() -> (ZoneHoldConfig, ZoneHoldConfig),
            /// Owned so field-only cases can carry `ZoneConfigChanged { fields }`.
            expect: Vec<ConfigEffect>,
            /// When set, also assert goals/auto_pause field application via full delta.
            extra: ExtraDelta,
        }

        #[derive(Clone, Copy)]
        enum ExtraDelta {
            None,
            Goals,
            AutoPause,
        }

        let cases = [
            Case {
                label: "1 enabled true→false, phase live → Disengage(Disabled)",
                phase: PhaseKind::Hold,
                walking: true,
                build: || {
                    let old = zh_enabled();
                    let mut new = old.clone();
                    new.enabled = false;
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneDisengage(
                    DisengageReason::DisabledInConfig,
                )],
                extra: ExtraDelta::None,
            },
            Case {
                label: "2 enabled true→false, phase Off → field only",
                phase: PhaseKind::Off,
                walking: false,
                build: || {
                    let old = zh_enabled();
                    let mut new = old.clone();
                    new.enabled = false;
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneConfigChanged {
                    fields: vec!["enabled"],
                }],
                extra: ExtraDelta::None,
            },
            Case {
                label: "3 enabled false→true, Off + walking → Engage",
                phase: PhaseKind::Off,
                walking: true,
                build: || {
                    let mut old = zh_enabled();
                    old.enabled = false;
                    let mut new = old.clone();
                    new.enabled = true;
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneEngage],
                extra: ExtraDelta::None,
            },
            Case {
                label: "4 enabled false→true, Off + not walking → field only",
                phase: PhaseKind::Off,
                walking: false,
                build: || {
                    let mut old = zh_enabled();
                    old.enabled = false;
                    let mut new = old.clone();
                    new.enabled = true;
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneConfigChanged {
                    fields: vec!["enabled"],
                }],
                extra: ExtraDelta::None,
            },
            Case {
                label: "5 target_zone changed, phase Hold → ReResolve",
                phase: PhaseKind::Hold,
                walking: true,
                build: || {
                    let old = zh_enabled();
                    let mut new = old.clone();
                    new.target_zone = ZoneSelector::Number(3);
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneReResolve],
                extra: ExtraDelta::None,
            },
            Case {
                label: "5b max_speed changed, phase Grace → ReResolve",
                phase: PhaseKind::Grace,
                walking: true,
                build: || {
                    let old = zh_enabled();
                    let mut new = old.clone();
                    new.max_speed_kmh = 6.5;
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneReResolve],
                extra: ExtraDelta::None,
            },
            Case {
                label: "5c method changed, phase Frozen → ReResolve",
                phase: PhaseKind::Frozen,
                walking: false,
                build: || {
                    let old = zh_enabled();
                    let mut new = old.clone();
                    new.method = zone_hold::Method::Karvonen;
                    new.resting_hr = Some(55);
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneReResolve],
                extra: ExtraDelta::None,
            },
            Case {
                label: "5d target_zone changed, phase Ramp → ReResolve",
                phase: PhaseKind::Ramp,
                walking: true,
                build: || {
                    let old = zh_enabled();
                    let mut new = old.clone();
                    new.target_zone = ZoneSelector::Number(1);
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneReResolve],
                extra: ExtraDelta::None,
            },
            Case {
                label: "6 target_zone changed, phase Off → field only",
                phase: PhaseKind::Off,
                walking: false,
                build: || {
                    let old = zh_enabled();
                    let mut new = old.clone();
                    new.target_zone = ZoneSelector::Number(3);
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneConfigChanged {
                    fields: vec!["target_zone"],
                }],
                extra: ExtraDelta::None,
            },
            Case {
                label: "7 age removed, phase live → Disengage(TargetUnresolvable)",
                phase: PhaseKind::Hold,
                walking: true,
                build: || {
                    let old = zh_enabled();
                    let mut new = old.clone();
                    new.age = None;
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneDisengage(
                    DisengageReason::TargetUnresolvable,
                )],
                extra: ExtraDelta::None,
            },
            Case {
                label: "7b unknown target_zone id, phase Ramp → Disengage(TargetUnresolvable)",
                phase: PhaseKind::Ramp,
                walking: true,
                build: || {
                    let old = zh_enabled();
                    let mut new = old.clone();
                    new.target_zone = ZoneSelector::Id("no-such-zone".into());
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneDisengage(
                    DisengageReason::TargetUnresolvable,
                )],
                extra: ExtraDelta::None,
            },
            Case {
                label: "8 age removed, phase Off → field only",
                phase: PhaseKind::Off,
                walking: false,
                build: || {
                    let old = zh_enabled();
                    let mut new = old.clone();
                    new.age = None;
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneConfigChanged {
                    fields: vec!["age"],
                }],
                extra: ExtraDelta::None,
            },
            Case {
                label: "9 warmup_minutes mid-Ramp → WarmupRetarget",
                phase: PhaseKind::Ramp,
                walking: true,
                build: || {
                    let old = zh_enabled();
                    let mut new = old.clone();
                    new.warmup_minutes = 10;
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneWarmupRetarget {
                    old_minutes: 5, // DEFAULT_WARMUP_MINUTES
                    new_minutes: 10,
                }],
                extra: ExtraDelta::None,
            },
            Case {
                label: "10 warmup_minutes mid-Hold → field only",
                phase: PhaseKind::Hold,
                walking: true,
                build: || {
                    let old = zh_enabled();
                    let mut new = old.clone();
                    new.warmup_minutes = 10;
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneConfigChanged {
                    fields: vec!["warmup_minutes"],
                }],
                extra: ExtraDelta::None,
            },
            Case {
                label: "11 goals changed → GoalsChanged",
                phase: PhaseKind::Hold,
                walking: true,
                build: || {
                    let zh = zh_enabled();
                    (zh.clone(), zh)
                },
                expect: vec![ConfigEffect::GoalsChanged],
                extra: ExtraDelta::Goals,
            },
            Case {
                label: "12 auto_pause changed → AutoPauseChanged",
                phase: PhaseKind::Off,
                walking: false,
                build: || {
                    let zh = zh_enabled();
                    (zh.clone(), zh)
                },
                expect: vec![ConfigEffect::AutoPauseChanged],
                extra: ExtraDelta::AutoPause,
            },
            Case {
                label: "combo enabled stays false + zone edit, phase live → Disengage (invariant repair)",
                phase: PhaseKind::Hold,
                walking: true,
                build: || {
                    let mut old = zh_enabled();
                    old.enabled = false;
                    let mut new = old.clone();
                    new.warmup_minutes = 9;
                    new.max_speed_kmh = 6.0;
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneDisengage(
                    DisengageReason::DisabledInConfig,
                )],
                extra: ExtraDelta::None,
            },
            Case {
                label: "combo enabled↓ + warmup change → only Disengage",
                phase: PhaseKind::Ramp,
                walking: true,
                build: || {
                    let old = zh_enabled();
                    let mut new = old.clone();
                    new.enabled = false;
                    new.warmup_minutes = 99;
                    (old, new)
                },
                expect: vec![ConfigEffect::ZoneDisengage(
                    DisengageReason::DisabledInConfig,
                )],
                extra: ExtraDelta::None,
            },
        ];

        for case in cases {
            let (old_zh, new_zh) = (case.build)();
            let mut config = live(old_zh);
            let new_goals = goals(&[5000, 9000]);
            let new_auto_pause = Some(Duration::from_secs(3 * 60));
            let delta = match case.extra {
                ExtraDelta::None => ConfigDelta {
                    goals: None,
                    auto_pause: None,
                    zone_hold: if old_zh_ne(&config.zone_hold, &new_zh) {
                        Some(new_zh.clone())
                    } else {
                        None
                    },
                },
                ExtraDelta::Goals => ConfigDelta {
                    goals: Some(new_goals.clone()),
                    auto_pause: None,
                    zone_hold: None,
                },
                ExtraDelta::AutoPause => ConfigDelta {
                    goals: None,
                    auto_pause: Some(new_auto_pause),
                    zone_hold: None,
                },
            };
            // Capture for the helper — need old zone before apply when comparing.
            let zone_before = config.zone_hold.clone();
            let effects = apply_config(&mut config, delta, &snap(case.phase, case.walking));
            assert_eq!(
                effects, case.expect,
                "case `{}`: effects mismatch",
                case.label
            );
            match case.extra {
                ExtraDelta::Goals => {
                    assert_eq!(
                        config.goals, new_goals,
                        "case `{}`: goals applied",
                        case.label
                    );
                }
                ExtraDelta::AutoPause => {
                    assert_eq!(
                        config.auto_pause, new_auto_pause,
                        "case `{}`: auto_pause applied",
                        case.label
                    );
                }
                ExtraDelta::None => {
                    if zone_before != new_zh {
                        assert_eq!(
                            config.zone_hold, new_zh,
                            "case `{}`: zone_hold applied",
                            case.label
                        );
                    }
                }
            }
        }
    }

    fn old_zh_ne(a: &ZoneHoldConfig, b: &ZoneHoldConfig) -> bool {
        a != b
    }

    /// Field-only mid-session edits (deadband / min_speed / safety_cap) used
    /// to apply with zero effects — operator saw no daemon log (backlog 011).
    #[test]
    fn field_only_zone_reload_emits_zone_config_changed() {
        let mut config = live(zh_enabled());
        let mut new_zh = config.zone_hold.clone();
        new_zh.deadband_bpm = 12;
        new_zh.min_speed_kmh = 2.5;
        new_zh.safety_cap_percent = 90.0;
        let effects = apply_config(
            &mut config,
            ConfigDelta {
                goals: None,
                auto_pause: None,
                zone_hold: Some(new_zh.clone()),
            },
            &snap(PhaseKind::Hold, true),
        );
        assert_eq!(
            effects,
            vec![ConfigEffect::ZoneConfigChanged {
                fields: vec!["min_speed_kmh", "deadband_bpm", "safety_cap_percent"],
            }]
        );
        assert_eq!(config.zone_hold, new_zh);
    }

    // --- diff ---------------------------------------------------------------

    #[test]
    fn diff_unchanged_is_empty() {
        let cfg = live(zh_enabled());
        let d = diff(&cfg, &cfg);
        assert!(d.is_empty());
        assert!(d.goals.is_none());
        assert!(d.auto_pause.is_none());
        assert!(d.zone_hold.is_none());
    }

    #[test]
    fn diff_each_field_alone_and_all_together() {
        let base = live(zh_enabled());

        let mut only_goals = base.clone();
        only_goals.goals = goals(&[1000]);
        let d = diff(&base, &only_goals);
        assert_eq!(d.goals, Some(only_goals.goals.clone()));
        assert!(d.auto_pause.is_none());
        assert!(d.zone_hold.is_none());

        let mut only_ap = base.clone();
        only_ap.auto_pause = None;
        let d = diff(&base, &only_ap);
        assert!(d.goals.is_none());
        assert_eq!(d.auto_pause, Some(None));
        assert!(d.zone_hold.is_none());

        let mut only_zh = base.clone();
        only_zh.zone_hold.enabled = true;
        only_zh.zone_hold.warmup_minutes = 7;
        let d = diff(&base, &only_zh);
        assert!(d.goals.is_none());
        assert!(d.auto_pause.is_none());
        assert_eq!(d.zone_hold.as_ref().map(|z| z.warmup_minutes), Some(7));

        let mut all = base.clone();
        all.goals = goals(&[2000, 4000]);
        all.auto_pause = Some(Duration::from_secs(60));
        all.zone_hold.max_speed_kmh = 9.0;
        let d = diff(&base, &all);
        assert!(d.goals.is_some());
        assert!(d.auto_pause.is_some());
        assert!(d.zone_hold.is_some());
        assert!(!d.is_empty());
    }

    // --- warmup policy (matrix §9) -----------------------------------------

    /// Shortening `warmup_minutes` below already-elapsed time ends the ramp
    /// on the next tick (`elapsed >= warmup` → Hold), without restarting.
    #[test]
    fn warmup_shorten_below_elapsed_completes_ramp_on_next_tick() {
        let start = 2.0_f32;
        let target = 4.0_f32;
        let elapsed = Duration::from_secs(180); // 3 minutes into ramp
        let new_warmup_minutes = 2_i64; // 2 minutes < elapsed
        let new_warmup = Duration::from_secs(new_warmup_minutes as u64 * 60);

        // Effect is log-only; phase fields (started_at / start / target) stay.
        let mut config = live({
            let mut z = zh_enabled();
            z.warmup_minutes = 5;
            z
        });
        let mut new_zh = zh_enabled();
        new_zh.warmup_minutes = new_warmup_minutes;
        let effects = apply_config(
            &mut config,
            ConfigDelta {
                goals: None,
                auto_pause: None,
                zone_hold: Some(new_zh),
            },
            &snap(PhaseKind::Ramp, true),
        );
        assert_eq!(
            effects,
            vec![ConfigEffect::ZoneWarmupRetarget {
                old_minutes: 5,
                new_minutes: 2,
            }]
        );

        // Same pure rule `zone_hold_tick` uses for Ramp completion.
        assert!(
            elapsed >= new_warmup,
            "next tick must complete the ramp under the shortened warmup"
        );
        assert_eq!(
            warmup_target_speed(start, target, elapsed, new_warmup),
            target,
            "at/after warmup end, target speed is the ramp destination"
        );
    }

    /// Extending warmup recalculates the slope from the same `started_at` /
    /// elapsed; speed does not jump back to `start_speed_kmh`.
    #[test]
    fn warmup_extend_recalculates_from_same_elapsed_without_resetting_to_start() {
        let start = 2.0_f32;
        let target = 4.0_f32;
        let elapsed = Duration::from_secs(150); // 2.5 min
        let old_warmup = Duration::from_secs(5 * 60);
        let new_warmup = Duration::from_secs(10 * 60);

        let speed_at_old = warmup_target_speed(start, target, elapsed, old_warmup);
        let speed_at_new = warmup_target_speed(start, target, elapsed, new_warmup);

        assert!(
            speed_at_new < speed_at_old,
            "longer warmup flattens the slope at fixed elapsed"
        );
        assert!(
            speed_at_new > start,
            "must not reset to start_speed_kmh on retarget"
        );

        // Monotonic in elapsed at fixed endpoints (task doc consequence).
        let later =
            warmup_target_speed(start, target, elapsed + Duration::from_secs(60), new_warmup);
        assert!(later > speed_at_new);
        assert!(later < target);
    }

    #[test]
    fn apply_config_orders_disengage_before_goals_and_engage_last() {
        let mut config = live({
            let mut z = zh_enabled();
            z.enabled = false;
            z
        });
        // Flip enabled on while walking AND change goals in one delta —
        // Engage must be last; Goals in the middle.
        let mut new_zh = config.zone_hold.clone();
        new_zh.enabled = true;
        let effects = apply_config(
            &mut config,
            ConfigDelta {
                goals: Some(goals(&[1000])),
                auto_pause: Some(None),
                zone_hold: Some(new_zh),
            },
            &snap(PhaseKind::Off, true),
        );
        assert_eq!(
            effects,
            vec![
                ConfigEffect::GoalsChanged,
                ConfigEffect::AutoPauseChanged,
                ConfigEffect::ZoneEngage,
            ]
        );
    }

    #[test]
    fn apply_config_disengage_orders_before_other_effects() {
        let mut config = live(zh_enabled());
        let mut new_zh = config.zone_hold.clone();
        new_zh.enabled = false;
        let effects = apply_config(
            &mut config,
            ConfigDelta {
                goals: Some(goals(&[1000])),
                auto_pause: Some(None),
                zone_hold: Some(new_zh),
            },
            &snap(PhaseKind::Hold, true),
        );
        assert_eq!(
            effects,
            vec![
                ConfigEffect::ZoneDisengage(DisengageReason::DisabledInConfig),
                ConfigEffect::GoalsChanged,
                ConfigEffect::AutoPauseChanged,
            ]
        );
    }
}
