//! Config hot-reload effect execution (задача 052).

use std::time::{Duration, Instant};

use tracing::{info, warn};

use super::state::DaemonState;
use crate::config_apply::{self, LiveConfig};
use crate::default_speed;
use crate::goals;
use crate::speed::CentiKmh;
use crate::store::Store;
use crate::zone_session::ZoneSession;

/// How often, during an active session, to check whether the goals config file
/// changed on disk and reload it without a daemon restart (задача 017). Only a
/// cheap `stat` per tick — the file is re-read/parsed only when its mtime moved.
/// 5s is a snappy pickup latency for a config edit while negligible in cost.
pub(super) const CONFIG_RELOAD_INTERVAL: Duration = Duration::from_secs(5);

/// Execute session effects from a config hot-reload (задача 052). Each effect
/// produces exactly one log line; field values are already applied by
/// [`config_apply::apply_config`]. Zone phase mutations go through [`ZoneSession`].
pub(super) fn execute_config_effects(
    effects: &[config_apply::ConfigEffect],
    config: &LiveConfig,
    zone: &mut ZoneSession,
    state: &mut DaemonState,
    last_walking_speed: Option<f32>,
    store: &Store,
) {
    use config_apply::{ConfigEffect, DisengageReason};

    for effect in effects {
        match effect {
            ConfigEffect::GoalsChanged => {
                info!(
                    goals = ?config.goals,
                    "goals config changed on disk — reloaded without a daemon restart"
                );
            }
            ConfigEffect::AutoPauseChanged => {
                info!(
                    auto_pause = ?config.auto_pause,
                    "auto-pause threshold changed on disk — reloaded without a daemon restart"
                );
            }
            ConfigEffect::ZoneDisengage(DisengageReason::DisabledInConfig) => {
                info!("zone hold: disabled in config — disengaging mid-session");
                zone.disengage(state);
            }
            ConfigEffect::ZoneDisengage(DisengageReason::TargetUnresolvable) => {
                warn!(
                    "zone hold: target zone no longer resolvable after config reload — disengaging"
                );
                zone.disengage(state);
            }
            ConfigEffect::ZoneEngage => {
                // Prefer last measured walking speed; min_speed only as
                // engage seed when we have *some* observation (задача 036
                // forbids inventing 0.0, not a known min floor).
                let zh_resumed = last_walking_speed
                    .and_then(CentiKmh::from_kmh_f32)
                    .or(Some(config.zone_hold.min_speed_kmh));
                let zh_default =
                    default_speed::compute_default_speed(store, goals::load_workout_gap_minutes())
                        .ok()
                        .flatten()
                        .and_then(|d| CentiKmh::from_kmh_f32(d.kmh))
                        .unwrap_or(config.zone_hold.min_speed_kmh);
                zone.on_config_engaged(&config.zone_hold, zh_resumed, zh_default, Instant::now());
            }
            ConfigEffect::ZoneReResolve => {
                match config.zone_hold.resolve_target_zone() {
                    Some(resolved) => {
                        info!(
                            lo = resolved.low_bpm,
                            hi = resolved.high_bpm,
                            zone = %resolved.name,
                            "zone hold: re-resolved target zone after config reload"
                        );
                        state.zone_hold_target_lo = Some(i64::from(resolved.low_bpm));
                        state.zone_hold_target_hi = Some(i64::from(resolved.high_bpm));
                    }
                    None => {
                        // apply_config emits Disengage instead of ReResolve when
                        // unresolvable; keep defense in depth.
                        warn!("zone hold: re-resolve failed after config reload — disengaging");
                        zone.disengage(state);
                    }
                }
            }
            ConfigEffect::ZoneWarmupRetarget {
                old_minutes,
                new_minutes,
            } => {
                info!(
                    old_minutes,
                    new_minutes,
                    "zone hold: warmup_minutes changed mid-ramp — retargeting without restart"
                );
            }
            ConfigEffect::ZoneConfigChanged { fields } => {
                info!(
                    ?fields,
                    "zone hold: config fields changed on disk — applied without a session phase effect"
                );
            }
        }
    }
}
