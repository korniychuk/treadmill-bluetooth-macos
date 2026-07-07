//! Daily step-goal milestones and their graduated celebrations (задача 011).
//!
//! Up to three goals (default 8000 / 10000 / 12000) each fire exactly one
//! celebratory toast per calendar day when today's cumulative steps cross the
//! threshold. This module owns three orthogonal, individually-testable pieces:
//! loading/resolving the JSON config, mapping thresholds to celebration tiers,
//! and the pure crossing decision. Persisting *which* goals were already
//! celebrated today (restart safety) lives in [`crate::store`]; delivering the
//! toast lives in [`crate::notify`].
//!
//! Config is per-user and lives OUTSIDE this repo — the goal set is the user's
//! personal preference, not application data. It resolves to a `$HOME`-anchored
//! path (see [`config_path`]); each user brings their own file (e.g. symlinked
//! from a personal dotfiles repo). A missing file is normal and falls back to
//! the compiled defaults.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use tracing::{info, warn};

/// Compiled-in fallback thresholds, used when the config file is missing or
/// invalid so the daemon always has *some* goals (edge case → WARN, not fail).
const DEFAULT_THRESHOLDS: [i64; 3] = [8000, 10000, 12000];

/// Default workout-gap in minutes (задача 014): adjacent activity segments
/// separated by a read-time gap ≤ this render as one workout. Used when the
/// config file is missing, the `workout_gap_minutes` key is absent, or its
/// value is invalid.
pub const DEFAULT_WORKOUT_GAP_MINUTES: i64 = 15;

/// Default idle-belt auto-pause threshold in minutes (задача 020): once the belt
/// has run `AwayWhileRunning` (nobody walking) this long, the daemon pauses it so
/// the machine's own built-in shutoff can then power it down. Used when the
/// config file is missing or the `auto_pause_minutes` key is absent. A configured
/// `0` disables auto-pause entirely.
pub const DEFAULT_AUTO_PAUSE_MINUTES: i64 = 5;

/// Hard cap on configured goals — three tiers of celebration copy exist, so a
/// fourth goal has nowhere sensible to land. Extra thresholds are dropped
/// (lowest kept) with a WARN.
const MAX_GOALS: usize = 3;

/// Optional environment variable to point at a goals config in a non-standard
/// location (tests, or a user who does not want the `$HOME`-default path).
/// Normally unset — the `$HOME`-anchored default is used.
const CONFIG_ENV: &str = "TREADMILL_GOALS_CONFIG";

/// Per-user config path relative to `$HOME`. `$HOME`-anchored (not cwd) because
/// the daemon runs under launchd with no reliable working directory — same
/// reasoning as `store::open`. Users own this file (a personal dotfiles repo
/// typically symlinks it here); it is intentionally NOT committed to this repo.
const HOME_CONFIG_RELPATH: &str = ".config/treadmill-bluetooth-macos/goals.json";

/// One configured daily step goal, with its celebration intensity tier
/// (1 = quietest, 3 = loudest) derived from its rank among the goals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Goal {
    pub threshold: i64,
    pub tier: u8,
}

/// Load the configured goals, falling back to [`DEFAULT_THRESHOLDS`] on any
/// problem (missing file, unreadable, malformed JSON, no usable thresholds).
/// Every fallback path logs a WARN — a silently-wrong goal set would be hard
/// to diagnose later.
pub fn load_goals() -> Vec<Goal> {
    let thresholds = match config_path() {
        // Present but bad content is a genuine anomaly the user should notice.
        Some(path) if path.exists() => read_thresholds(&path).unwrap_or_else(|| {
            warn!(path = %path.display(), "goals config present but invalid — using compiled-in defaults");
            DEFAULT_THRESHOLDS.to_vec()
        }),
        // No file is the normal case for a user who never customised goals.
        Some(path) => {
            info!(path = %path.display(), "no goals config file — using compiled-in defaults");
            DEFAULT_THRESHOLDS.to_vec()
        }
        None => {
            warn!("could not resolve a goals config path ($HOME unset) — using compiled-in defaults");
            DEFAULT_THRESHOLDS.to_vec()
        }
    };
    assign_tiers(&thresholds)
}

/// Last-modified time of the resolved config file, or `None` when it can't be
/// stat'd (missing file, unreadable, or no `$HOME`). The daemon polls this to
/// reload goals only when the file actually changes — avoiding a re-read/re-log
/// every tick (задача 017). A `None`→`Some` (or vice-versa) transition on
/// create/delete is itself a change the caller reacts to.
pub fn config_mtime() -> Option<SystemTime> {
    let path = config_path()?;
    std::fs::metadata(&path).ok()?.modified().ok()
}

/// Resolve the config file path: explicit [`CONFIG_ENV`] override first, else
/// the `$HOME`-anchored default. `None` only when `$HOME` is unset.
fn config_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var(CONFIG_ENV) {
        return Some(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(HOME_CONFIG_RELPATH))
}

/// Read and parse `{ "goals": [8000, 10000, 12000] }`. Returns `None` on any
/// failure so the caller can fall back to defaults and log once.
fn read_thresholds(path: &std::path::Path) -> Option<Vec<i64>> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let goals = value.get("goals")?.as_array()?;
    let thresholds: Vec<i64> = goals
        .iter()
        .filter_map(|v| v.as_i64())
        .filter(|&t| t > 0)
        .collect();
    (!thresholds.is_empty()).then_some(thresholds)
}

/// Parse outcome of the optional `workout_gap_minutes` key. The three cases are
/// kept distinct so the caller can log the *anomalous* one (present-but-invalid)
/// without spamming on the *normal* one (key absent — every pre-014 config lacks
/// it) on the hot `tm widget` poll path. See [`load_workout_gap_minutes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GapSetting {
    /// Key present and a positive integer.
    Configured(i64),
    /// Key present but not a positive integer, or the file is unreadable/malformed.
    Invalid,
    /// Key absent (normal for a config written before this key existed).
    Unset,
}

/// Read `workout_gap_minutes` from the per-user config. Pure and unit-tested —
/// the logging/fallback decision lives in [`load_workout_gap_minutes`].
fn read_workout_gap_minutes(path: &std::path::Path) -> GapSetting {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return GapSetting::Invalid;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return GapSetting::Invalid;
    };
    match value.get("workout_gap_minutes") {
        None => GapSetting::Unset,
        Some(v) => match v.as_i64() {
            Some(n) if n > 0 => GapSetting::Configured(n),
            _ => GapSetting::Invalid,
        },
    }
}

/// Load the configured workout-gap (minutes), falling back to
/// [`DEFAULT_WORKOUT_GAP_MINUTES`] when unconfigured or invalid. This is a
/// READ-TIME parameter (задача 014): it groups adjacent activity segments into
/// displayed workouts, so it is loaded by the read commands (`stats`, `status`,
/// `widget`), not the segment-writing daemon.
///
/// Logging is deliberately quiet on the common paths — this runs on `widget`'s
/// ~2s poll: an absent key (every pre-014 config) and a missing file are normal
/// and silent; only a present-but-invalid value is an anomaly worth a WARN.
pub fn load_workout_gap_minutes() -> i64 {
    match config_path() {
        Some(path) if path.exists() => match read_workout_gap_minutes(&path) {
            GapSetting::Configured(minutes) => minutes,
            GapSetting::Unset => DEFAULT_WORKOUT_GAP_MINUTES,
            GapSetting::Invalid => {
                warn!(
                    path = %path.display(),
                    default = DEFAULT_WORKOUT_GAP_MINUTES,
                    "workout_gap_minutes present but not a positive integer — using default",
                );
                DEFAULT_WORKOUT_GAP_MINUTES
            }
        },
        // No file / no resolvable path is the normal uncustomised case.
        _ => DEFAULT_WORKOUT_GAP_MINUTES,
    }
}

/// Parse outcome of the optional `auto_pause_minutes` key (задача 020). Kept
/// distinct like [`GapSetting`] so the caller logs only the anomalous
/// (present-but-invalid) case, not the normal absent one. Note `0` is a *valid*
/// value here (explicitly disables auto-pause), unlike `workout_gap_minutes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoPauseSetting {
    /// Key present and a non-negative integer (`0` = disabled).
    Configured(i64),
    /// Key present but not a non-negative integer, or the file is unreadable/malformed.
    Invalid,
    /// Key absent (normal for a config written before this key existed).
    Unset,
}

/// Read `auto_pause_minutes` from the per-user config. Pure and unit-tested —
/// the logging/fallback decision lives in [`load_auto_pause`].
fn read_auto_pause_minutes(path: &std::path::Path) -> AutoPauseSetting {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return AutoPauseSetting::Invalid;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return AutoPauseSetting::Invalid;
    };
    match value.get("auto_pause_minutes") {
        None => AutoPauseSetting::Unset,
        // `0` disables (kept), negatives/non-integers are a config mistake.
        Some(v) => match v.as_i64() {
            Some(n) if n >= 0 => AutoPauseSetting::Configured(n),
            _ => AutoPauseSetting::Invalid,
        },
    }
}

/// Load the idle-belt auto-pause threshold (задача 020): `Some(duration)` when
/// enabled, `None` when disabled (configured `0`). Falls back to
/// [`DEFAULT_AUTO_PAUSE_MINUTES`] when the key is absent or invalid.
///
/// Like [`load_workout_gap_minutes`], logging is quiet on the common paths (an
/// absent key and a missing file are normal); only a present-but-invalid value
/// is an anomaly worth a WARN. The daemon reloads this on the goals-config
/// mtime watch (задача 017), so an edit takes effect without a restart.
pub fn load_auto_pause() -> Option<Duration> {
    let minutes = match config_path() {
        Some(path) if path.exists() => match read_auto_pause_minutes(&path) {
            AutoPauseSetting::Configured(minutes) => minutes,
            AutoPauseSetting::Unset => DEFAULT_AUTO_PAUSE_MINUTES,
            AutoPauseSetting::Invalid => {
                warn!(
                    path = %path.display(),
                    default = DEFAULT_AUTO_PAUSE_MINUTES,
                    "auto_pause_minutes present but not a non-negative integer — using default",
                );
                DEFAULT_AUTO_PAUSE_MINUTES
            }
        },
        // No file / no resolvable path is the normal uncustomised case.
        _ => DEFAULT_AUTO_PAUSE_MINUTES,
    };
    // 0 = explicitly disabled; any positive count is a real threshold.
    (minutes > 0).then(|| Duration::from_secs(minutes as u64 * 60))
}

/// Turn raw thresholds into tiered [`Goal`]s: sort ascending, dedup, cap to
/// [`MAX_GOALS`], and assign `tier = rank + 1` (lowest goal → tier 1). Chosen
/// over "top goal always tier 3" so a lone modest goal fires a modest toast,
/// not the loudest one.
pub fn assign_tiers(thresholds: &[i64]) -> Vec<Goal> {
    let mut sorted: Vec<i64> = thresholds.iter().copied().filter(|&t| t > 0).collect();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() > MAX_GOALS {
        warn!(
            count = sorted.len(),
            max = MAX_GOALS,
            "more goals than tiers — keeping the lowest three"
        );
        sorted.truncate(MAX_GOALS);
    }
    sorted
        .into_iter()
        .enumerate()
        .map(|(rank, threshold)| Goal {
            threshold,
            tier: (rank + 1) as u8,
        })
        .collect()
}

/// Pure crossing decision: the goals whose threshold today's steps have now
/// reached and that have not yet been celebrated today. Returned ascending by
/// threshold, so a caller firing them in order lands the biggest goal last.
pub fn thresholds_to_celebrate(
    today_steps: i64,
    goals: &[Goal],
    already_celebrated: &HashSet<i64>,
) -> Vec<Goal> {
    let mut due: Vec<Goal> = goals
        .iter()
        .copied()
        .filter(|goal| {
            today_steps >= goal.threshold && !already_celebrated.contains(&goal.threshold)
        })
        .collect();
    due.sort_unstable_by_key(|goal| goal.threshold);
    due
}

#[cfg(test)]
mod tests {
    use super::*;

    fn celebrated(thresholds: &[i64]) -> HashSet<i64> {
        thresholds.iter().copied().collect()
    }

    #[test]
    fn assign_tiers_ranks_ascending_and_caps_at_three() {
        // Unsorted input with a duplicate and a fourth goal.
        let goals = assign_tiers(&[12000, 8000, 10000, 8000, 15000]);
        assert_eq!(goals.len(), 3, "capped at three, deduped");
        assert_eq!(
            goals[0],
            Goal {
                threshold: 8000,
                tier: 1
            }
        );
        assert_eq!(
            goals[1],
            Goal {
                threshold: 10000,
                tier: 2
            }
        );
        assert_eq!(
            goals[2],
            Goal {
                threshold: 12000,
                tier: 3
            }
        );
    }

    #[test]
    fn assign_tiers_single_goal_is_tier_one() {
        assert_eq!(
            assign_tiers(&[8000]),
            vec![Goal {
                threshold: 8000,
                tier: 1
            }]
        );
    }

    #[test]
    fn assign_tiers_two_goals_are_tiers_one_and_two() {
        let goals = assign_tiers(&[10000, 8000]);
        assert_eq!(
            goals,
            vec![
                Goal {
                    threshold: 8000,
                    tier: 1
                },
                Goal {
                    threshold: 10000,
                    tier: 2
                }
            ]
        );
    }

    #[test]
    fn exactly_at_threshold_counts_as_crossed() {
        // arr
        let goals = assign_tiers(&[8000, 10000, 12000]);
        // act
        let due = thresholds_to_celebrate(8000, &goals, &celebrated(&[]));
        // assert
        assert_eq!(
            due,
            vec![Goal {
                threshold: 8000,
                tier: 1
            }]
        );
    }

    #[test]
    fn below_threshold_celebrates_nothing() {
        let goals = assign_tiers(&[8000, 10000, 12000]);
        assert!(thresholds_to_celebrate(7999, &goals, &celebrated(&[])).is_empty());
    }

    #[test]
    fn multiple_thresholds_crossed_in_one_sample_fire_ascending() {
        // arr — a big pending flush jumps steps straight past two goals.
        let goals = assign_tiers(&[8000, 10000, 12000]);
        // act
        let due = thresholds_to_celebrate(11000, &goals, &celebrated(&[]));
        // assert — both due, biggest last.
        assert_eq!(
            due,
            vec![
                Goal {
                    threshold: 8000,
                    tier: 1
                },
                Goal {
                    threshold: 10000,
                    tier: 2
                }
            ]
        );
    }

    #[test]
    fn already_celebrated_are_not_refired_after_restart() {
        // arr — daemon restarted mid-day; 8k + 10k already marked in SQLite.
        let goals = assign_tiers(&[8000, 10000, 12000]);
        // act — steps are past 10k but under 12k.
        let due = thresholds_to_celebrate(11500, &goals, &celebrated(&[8000, 10000]));
        // assert — nothing new to fire.
        assert!(due.is_empty());
    }

    #[test]
    fn restart_still_fires_the_not_yet_reached_goal() {
        let goals = assign_tiers(&[8000, 10000, 12000]);
        let due = thresholds_to_celebrate(12100, &goals, &celebrated(&[8000, 10000]));
        assert_eq!(
            due,
            vec![Goal {
                threshold: 12000,
                tier: 3
            }]
        );
    }

    #[test]
    fn read_thresholds_parses_a_non_default_config() {
        // Non-default set so a silent fall-back to DEFAULT_THRESHOLDS (the same
        // as the committed config) can't disguise a parse regression.
        let dir = std::env::temp_dir().join(format!("tm-goals-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("goals.json");
        std::fs::write(&path, r#"{ "goals": [5000, 7000] }"#).unwrap();

        let parsed = read_thresholds(&path);
        std::fs::remove_file(&path).ok();

        assert_eq!(parsed, Some(vec![5000, 7000]));
    }

    #[test]
    fn read_thresholds_rejects_junk_and_empty() {
        let dir = std::env::temp_dir().join(format!("tm-goals-junk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "not json at all").unwrap();
        assert_eq!(
            read_thresholds(&bad),
            None,
            "malformed JSON → None (caller uses defaults)"
        );

        let empty = dir.join("empty.json");
        std::fs::write(&empty, r#"{ "goals": [] }"#).unwrap();
        assert_eq!(read_thresholds(&empty), None, "no usable thresholds → None");

        let missing = dir.join("does-not-exist.json");
        assert_eq!(read_thresholds(&missing), None, "missing file → None");

        std::fs::remove_file(&bad).ok();
        std::fs::remove_file(&empty).ok();
    }

    #[test]
    fn read_workout_gap_minutes_distinguishes_configured_absent_and_invalid() {
        let dir = std::env::temp_dir().join(format!("tm-gap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let good = dir.join("good.json");
        std::fs::write(&good, r#"{ "goals": [8000], "workout_gap_minutes": 20 }"#).unwrap();
        assert_eq!(read_workout_gap_minutes(&good), GapSetting::Configured(20));

        // Key absent — normal for a config written before задача 014.
        let absent = dir.join("absent.json");
        std::fs::write(&absent, r#"{ "goals": [8000] }"#).unwrap();
        assert_eq!(read_workout_gap_minutes(&absent), GapSetting::Unset);

        // Present but not a positive integer → Invalid (caller WARNs + defaults).
        let zero = dir.join("zero.json");
        std::fs::write(&zero, r#"{ "workout_gap_minutes": 0 }"#).unwrap();
        assert_eq!(read_workout_gap_minutes(&zero), GapSetting::Invalid);
        let neg = dir.join("neg.json");
        std::fs::write(&neg, r#"{ "workout_gap_minutes": -5 }"#).unwrap();
        assert_eq!(read_workout_gap_minutes(&neg), GapSetting::Invalid);
        let str_val = dir.join("str.json");
        std::fs::write(&str_val, r#"{ "workout_gap_minutes": "15" }"#).unwrap();
        assert_eq!(read_workout_gap_minutes(&str_val), GapSetting::Invalid);

        // Malformed JSON → Invalid.
        let junk = dir.join("junk.json");
        std::fs::write(&junk, "not json").unwrap();
        assert_eq!(read_workout_gap_minutes(&junk), GapSetting::Invalid);

        for f in [good, absent, zero, neg, str_val, junk] {
            std::fs::remove_file(f).ok();
        }
    }

    #[test]
    fn read_auto_pause_minutes_distinguishes_configured_disabled_absent_and_invalid() {
        let dir = std::env::temp_dir().join(format!("tm-autopause-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let good = dir.join("good.json");
        std::fs::write(&good, r#"{ "goals": [8000], "auto_pause_minutes": 7 }"#).unwrap();
        assert_eq!(
            read_auto_pause_minutes(&good),
            AutoPauseSetting::Configured(7)
        );

        // 0 is a valid value here — it disables auto-pause (not Invalid).
        let disabled = dir.join("disabled.json");
        std::fs::write(&disabled, r#"{ "auto_pause_minutes": 0 }"#).unwrap();
        assert_eq!(
            read_auto_pause_minutes(&disabled),
            AutoPauseSetting::Configured(0),
        );

        // Key absent — normal for a config written before задача 020.
        let absent = dir.join("absent.json");
        std::fs::write(&absent, r#"{ "goals": [8000] }"#).unwrap();
        assert_eq!(read_auto_pause_minutes(&absent), AutoPauseSetting::Unset);

        // Negative / non-integer → Invalid (caller WARNs + defaults).
        let neg = dir.join("neg.json");
        std::fs::write(&neg, r#"{ "auto_pause_minutes": -3 }"#).unwrap();
        assert_eq!(read_auto_pause_minutes(&neg), AutoPauseSetting::Invalid);
        let str_val = dir.join("str.json");
        std::fs::write(&str_val, r#"{ "auto_pause_minutes": "5" }"#).unwrap();
        assert_eq!(read_auto_pause_minutes(&str_val), AutoPauseSetting::Invalid);
        let junk = dir.join("junk.json");
        std::fs::write(&junk, "not json").unwrap();
        assert_eq!(read_auto_pause_minutes(&junk), AutoPauseSetting::Invalid);

        for f in [good, disabled, absent, neg, str_val, junk] {
            std::fs::remove_file(f).ok();
        }
    }

    #[test]
    fn subset_of_goals_only_celebrates_configured_ones() {
        // Only two goals configured — 12k is not a goal, so 12100 steps still
        // only celebrates the configured 10k (8k already done).
        let goals = assign_tiers(&[8000, 10000]);
        let due = thresholds_to_celebrate(12100, &goals, &celebrated(&[8000]));
        assert_eq!(
            due,
            vec![Goal {
                threshold: 10000,
                tier: 2
            }]
        );
    }
}
