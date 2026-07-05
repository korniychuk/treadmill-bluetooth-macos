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

use tracing::{info, warn};

/// Compiled-in fallback thresholds, used when the config file is missing or
/// invalid so the daemon always has *some* goals (edge case → WARN, not fail).
const DEFAULT_THRESHOLDS: [i64; 3] = [8000, 10000, 12000];

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
    let thresholds: Vec<i64> = goals.iter().filter_map(|v| v.as_i64()).filter(|&t| t > 0).collect();
    (!thresholds.is_empty()).then_some(thresholds)
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
        warn!(count = sorted.len(), max = MAX_GOALS, "more goals than tiers — keeping the lowest three");
        sorted.truncate(MAX_GOALS);
    }
    sorted
        .into_iter()
        .enumerate()
        .map(|(rank, threshold)| Goal { threshold, tier: (rank + 1) as u8 })
        .collect()
}

/// Pure crossing decision: the goals whose threshold today's steps have now
/// reached and that have not yet been celebrated today. Returned ascending by
/// threshold, so a caller firing them in order lands the biggest goal last.
pub fn thresholds_to_celebrate(today_steps: i64, goals: &[Goal], already_celebrated: &HashSet<i64>) -> Vec<Goal> {
    let mut due: Vec<Goal> = goals
        .iter()
        .copied()
        .filter(|goal| today_steps >= goal.threshold && !already_celebrated.contains(&goal.threshold))
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
        assert_eq!(goals[0], Goal { threshold: 8000, tier: 1 });
        assert_eq!(goals[1], Goal { threshold: 10000, tier: 2 });
        assert_eq!(goals[2], Goal { threshold: 12000, tier: 3 });
    }

    #[test]
    fn assign_tiers_single_goal_is_tier_one() {
        assert_eq!(assign_tiers(&[8000]), vec![Goal { threshold: 8000, tier: 1 }]);
    }

    #[test]
    fn assign_tiers_two_goals_are_tiers_one_and_two() {
        let goals = assign_tiers(&[10000, 8000]);
        assert_eq!(goals, vec![Goal { threshold: 8000, tier: 1 }, Goal { threshold: 10000, tier: 2 }]);
    }

    #[test]
    fn exactly_at_threshold_counts_as_crossed() {
        // arr
        let goals = assign_tiers(&[8000, 10000, 12000]);
        // act
        let due = thresholds_to_celebrate(8000, &goals, &celebrated(&[]));
        // assert
        assert_eq!(due, vec![Goal { threshold: 8000, tier: 1 }]);
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
        assert_eq!(due, vec![Goal { threshold: 8000, tier: 1 }, Goal { threshold: 10000, tier: 2 }]);
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
        assert_eq!(due, vec![Goal { threshold: 12000, tier: 3 }]);
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
        assert_eq!(read_thresholds(&bad), None, "malformed JSON → None (caller uses defaults)");

        let empty = dir.join("empty.json");
        std::fs::write(&empty, r#"{ "goals": [] }"#).unwrap();
        assert_eq!(read_thresholds(&empty), None, "no usable thresholds → None");

        let missing = dir.join("does-not-exist.json");
        assert_eq!(read_thresholds(&missing), None, "missing file → None");

        std::fs::remove_file(&bad).ok();
        std::fs::remove_file(&empty).ok();
    }

    #[test]
    fn subset_of_goals_only_celebrates_configured_ones() {
        // Only two goals configured — 12k is not a goal, so 12100 steps still
        // only celebrates the configured 10k (8k already done).
        let goals = assign_tiers(&[8000, 10000]);
        let due = thresholds_to_celebrate(12100, &goals, &celebrated(&[8000]));
        assert_eq!(due, vec![Goal { threshold: 10000, tier: 2 }]);
    }
}
