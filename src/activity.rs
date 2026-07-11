//! Shared presence + credit/segment engine (задача 015).
//!
//! The rule that turns raw per-sample device deltas into credited walking and
//! open/close activity segments must live in exactly one place, because the
//! *only* way the historical replay (`crate::recompute`) can reproduce the
//! live daemon's segmentation byte-for-byte is to run the same code. That code
//! is [`ActivityAccumulator`] here, driven identically by:
//! - the live daemon loop (`crate::daemon::stream_with_presence`), fed
//!   `Instant::now()` and `Utc::now()`; and
//! - the offline replay, fed instants/timestamps synthesized from
//!   `raw_samples.ts_ms`.
//!
//! The buffering subtlety (why distance/time can't be credited the instant a
//! sample arrives) is documented on [`credit_or_hold`].

use std::time::Instant;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::presence::{PresenceState, PresenceTracker};
use crate::speed::CentiKmh;
use crate::store::{RawDeltas, Store};

/// Distance/time accrued since the last confirmed step, held back from
/// `daily_stats`/`activity_segments` until either a new step confirms it was
/// real walking, or the operator is confirmed away and it gets discarded.
#[derive(Default)]
pub struct PendingCredit {
    distance_m: i64,
    elapsed_s: i64,
}

/// The live daemon loop and the offline replay share this single accumulator so
/// their segmentation is identical by construction (задача 015). It owns the
/// three pieces of per-session state the daemon used to hold inline: the
/// [`PresenceTracker`], the [`PendingCredit`] buffer, and the in-memory
/// open-segment id.
///
/// A fresh accumulator per device session mirrors `stream_with_presence`
/// creating these locals fresh on every reconnect — see `docs/tasks/014`
/// (restart safety) and `docs/tasks/015` (replay session boundaries).
pub struct ActivityAccumulator {
    presence: PresenceTracker,
    pending: PendingCredit,
    /// Open activity segment handle `(id, started_at)`, or `None` when closed.
    /// Identity includes `started_at` so a renumbered table after
    /// `recompute-segments` cannot extend a different historical row (задача 044).
    /// Closed (set to `None`) on the presence transition leaving `Walking`.
    current_segment: Option<(i64, String)>,
}

impl ActivityAccumulator {
    pub fn new() -> Self {
        Self {
            presence: PresenceTracker::new(),
            pending: PendingCredit::default(),
            current_segment: None,
        }
    }

    /// The current presence state (after the most recent [`Self::observe`]).
    pub fn state(&self) -> PresenceState {
        self.presence.state()
    }

    /// Feed one sample's presence inputs. On a transition *leaving* `Walking`
    /// (a pause `speed=0` or a step-away `AwayWhileRunning`) the open segment
    /// is closed here — the next credited step opens a fresh one, and read-time
    /// `merge_segments` regroups by gap (задача 014). Returns the presence
    /// transition (if any) so the live caller can fire its toasts / speed
    /// restore; the replay ignores the return value.
    pub fn observe(
        &mut self,
        now: Instant,
        speed: Option<CentiKmh>,
        steps: Option<u32>,
    ) -> Option<PresenceState> {
        let transition = self.presence.observe(now, speed, steps);
        if let Some(next) = transition
            && next != PresenceState::Walking
        {
            self.current_segment = None;
        }
        transition
    }

    /// Credit this sample's raw deltas into `store` at wall-clock time `now`,
    /// buffering distance/time until a step confirms them (see
    /// [`credit_or_hold`]). `now` is the sample's real timestamp — `Utc::now()`
    /// live, or the `ts_ms`-derived instant on replay — so segment start/end
    /// and the local-date attribution match the live daemon exactly.
    pub fn credit(
        &mut self,
        store: &mut Store,
        now: DateTime<Utc>,
        deltas: RawDeltas,
    ) -> Result<()> {
        credit_or_hold(
            store,
            &mut self.pending,
            &mut self.current_segment,
            self.presence.state(),
            now,
            deltas,
        )
    }
}

impl Default for ActivityAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

/// Decide what to do with this sample's raw deltas.
///
/// Steps only ever advance when a step is genuinely registered, so a
/// non-zero `deltas.steps` is itself the confirmation signal — crediting it
/// immediately is always correct. Distance/time are different: the belt
/// keeps moving and `elapsed_s` keeps ticking during the up-to-
/// `presence::AWAY_THRESHOLD` window before an absence is confirmed, so they
/// are held in `pending` and only flushed to `daily_stats`/`activity_segments`
/// alongside a confirming step. If the away threshold fires first, `pending`
/// is dropped instead of committed — otherwise every departure would silently
/// credit an extra `AWAY_THRESHOLD` worth of phantom distance/time.
///
/// `current_segment` is the caller's in-memory open-segment handle
/// `(id, started_at)` (задачи 014/044): each confirming step extends it (or
/// opens one if `None`), and the returned handle is stored back. The segment is
/// *closed* elsewhere — on the presence transition leaving `Walking` (see
/// [`ActivityAccumulator::observe`]), which clears it to `None` so the next
/// step opens a fresh segment.
///
/// `now` is the sample's timestamp, threaded through to `store::credit_activity`
/// so cross-segment gaps match wall-clock precisely (live) or the replayed
/// `ts_ms` (offline) rather than processing time.
pub fn credit_or_hold(
    store: &mut Store,
    pending: &mut PendingCredit,
    current_segment: &mut Option<(i64, String)>,
    state: PresenceState,
    now: DateTime<Utc>,
    deltas: RawDeltas,
) -> Result<()> {
    match state {
        PresenceState::Walking => {
            pending.distance_m += deltas.distance_m;
            pending.elapsed_s += deltas.elapsed_s;
            if deltas.steps > 0 {
                let handle = store.credit_activity(
                    deltas.steps,
                    pending.distance_m,
                    pending.elapsed_s,
                    now,
                    current_segment.clone(),
                )?;
                *current_segment = Some(handle);
                *pending = PendingCredit::default();
            }
        }
        PresenceState::AwayWhileRunning | PresenceState::Paused | PresenceState::Unknown => {
            *pending = PendingCredit::default();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn memory_store() -> Store {
        Store::open_at(Path::new(":memory:")).expect("open in-memory store")
    }

    #[test]
    fn confirmed_step_flushes_pending_distance_and_time() {
        let mut store = memory_store();
        let mut pending = PendingCredit::default();
        let mut segment: Option<(i64, String)> = None;
        let now = Utc::now();

        // Ambiguous gap: belt moved 3m/1s but no step registered yet.
        credit_or_hold(
            &mut store,
            &mut pending,
            &mut segment,
            PresenceState::Walking,
            now,
            RawDeltas {
                steps: 0,
                distance_m: 3,
                elapsed_s: 1,
            },
        )
        .unwrap();
        assert_eq!(pending.distance_m, 3);
        assert!(
            segment.is_none(),
            "no segment opened until a step confirms walking"
        );

        // A step now confirms the whole gap was real walking — flush it.
        credit_or_hold(
            &mut store,
            &mut pending,
            &mut segment,
            PresenceState::Walking,
            now,
            RawDeltas {
                steps: 1,
                distance_m: 1,
                elapsed_s: 1,
            },
        )
        .unwrap();
        assert_eq!(pending.distance_m, 0);
        assert!(segment.is_some(), "the confirming step opens the segment");
        let today = store.today_stats().unwrap();
        assert_eq!(today.distance_m, 4);
        assert_eq!(today.steps, 1);
        assert_eq!(today.walking_time_s, 2);
    }

    #[test]
    fn confirmed_away_discards_pending_instead_of_crediting_it() {
        let mut store = memory_store();
        let mut pending = PendingCredit::default();
        let mut segment: Option<(i64, String)> = None;
        let now = Utc::now();

        // The belt kept moving for the whole confirmation window before the
        // tracker flips to AwayWhileRunning — this must never reach daily_stats.
        for _ in 0..10 {
            credit_or_hold(
                &mut store,
                &mut pending,
                &mut segment,
                PresenceState::Walking,
                now,
                RawDeltas {
                    steps: 0,
                    distance_m: 1,
                    elapsed_s: 1,
                },
            )
            .unwrap();
        }
        assert_eq!(pending.distance_m, 10);

        credit_or_hold(
            &mut store,
            &mut pending,
            &mut segment,
            PresenceState::AwayWhileRunning,
            now,
            RawDeltas {
                steps: 0,
                distance_m: 1,
                elapsed_s: 1,
            },
        )
        .unwrap();

        assert_eq!(pending.distance_m, 0);
        let today = store.today_stats().unwrap();
        assert_eq!(today.distance_m, 0);
        assert_eq!(today.walking_time_s, 0);
    }
}
