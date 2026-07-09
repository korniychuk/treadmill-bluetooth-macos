//! `recompute-hr` — delete frozen-bpm samples recorded from a strap that had
//! left the body (задача 034).
//!
//! Задача 033 taught the daemon that skin-contact loss is not link loss, but it
//! only looks forward. `hr_samples` still holds long runs of a removed H10's
//! last bpm, repeated ~1/s, which poison `hr_summary_for` (a whole workout once
//! read `♥ 111/111`).
//!
//! Deletion cannot key off a single frame: an RR-less frame is *normal* in
//! isolation (a worn H10 drops one now and then — 66 distinct bpm across the
//! "suspicious" frames in the live DB). Contact loss is a property of the
//! **sequence**, which is exactly what [`hr::ContactTracker`] decides. So this
//! command replays the ground truth (`hr_samples.raw_frame`) through the very
//! same production tracker — no forked rules, same spirit as
//! `recompute-segments` (задача 015).
//!
//! Read-only over BLE: no adapter is opened.

use std::collections::HashSet;

use anyhow::{Context, Result};
use tracing::warn;

use crate::hr::{self, Contact, ContactTracker};
use crate::store::{HrRow, Store};

/// A gap between consecutive samples above which the BLE link must have died,
/// so the tracker's *link-scoped* evidence (RR capability, the RR-less run) is
/// dropped — see [`ContactTracker::reset_link`].
///
/// Hand-kept mirror of `daemon::HR_NOTIFICATION_TIMEOUT` (private to
/// `daemon.rs`, same convention as `main.rs`'s copy of the watchdog threshold):
/// the daemon tears the link down after that much silence, so a stream denser
/// than this could not have survived a reconnect.
const HR_LINK_GAP_MS: i64 = 10_000;

/// Ids of samples that the contact tracker would have rejected — pure, no DB and
/// no clock. `rows` must be ordered by `ts_ms`.
///
/// **Wider than the live daemon, on purpose.** The daemon must wait out the
/// whole `CONTACT_FROZEN_BPM_MS` window before it can call a bpm frozen, so the
/// first minute of a removed strap's output is already on disk by the time it
/// knows. Replay does not have that handicap: once a constant-bpm run is
/// condemned, the *entire* run is garbage — including the samples that opened
/// it. Cost: up to ~16s of genuinely live samples (the longest constant-bpm run
/// ever observed from a worn strap) trimmed from the end of a real workout.
/// Worth it — the alternative is a permanent `♥ 111/111`.
///
/// Converges because the frozen-bpm clock ignores link gaps: deleting a run
/// *creates* `ts_ms` gaps between the survivors, and a clock that reset on those
/// gaps would grant each sparse fragment a fresh minute of presumed-live pulse,
/// leaving residue on every pass. See [`ContactTracker::reset_link`].
pub fn plan_contact_loss(rows: &[HrRow]) -> Vec<i64> {
    let mut tracker = ContactTracker::default();
    let mut previous_ts: Option<i64> = None;
    let mut doomed = Vec::new();
    // The run of samples sharing the current bpm, still presumed live.
    let mut run: Vec<i64> = Vec::new();
    let mut run_bpm: Option<u16> = None;

    for row in rows {
        // A gap means the link died — but the frozen-bpm clock survives it, or
        // the cleanup would not converge (see `ContactTracker::reset_link`).
        // The constant-bpm run survives for the same reason.
        if previous_ts.is_some_and(|prev| row.ts_ms - prev > HR_LINK_GAP_MS) {
            tracker.reset_link();
        }
        previous_ts = Some(row.ts_ms);

        // `insert_hr_sample` only ever ran on a successfully parsed frame, so an
        // undecodable row means the stored bytes are corrupt. Skip it without
        // feeding the tracker rather than guess at its contact state.
        let Some(measurement) = hr::parse_hr_measurement(&row.raw_frame) else {
            warn!(
                id = row.id,
                "hr sample has an undecodable raw frame — skipping"
            );
            continue;
        };

        if run_bpm != Some(measurement.bpm) {
            run.clear();
            run_bpm = Some(measurement.bpm);
        }
        run.push(row.id);

        if tracker.observe(&measurement, row.ts_ms) == Contact::Lost {
            if tracker.bpm_frozen_at(row.ts_ms) {
                // Condemned for a frozen bpm ⇒ the whole constant run was dead.
                doomed.append(&mut run);
            } else {
                // Condemned on RR evidence alone: only this frame. The two
                // tolerated RR-less frames before it stay — that tolerance is
                // the whole point of `CONTACT_LOST_FRAMES`.
                run.pop();
                doomed.push(row.id);
            }
        }
    }
    doomed
}

/// Safety bound on the fixpoint loop in [`plan_to_fixpoint`]. Each pass strictly
/// shrinks the row set, so termination is guaranteed regardless; this only caps
/// the pathological case where nearly every sample dies one pass at a time.
const MAX_FIXPOINT_PASSES: usize = 16;

/// Every id [`plan_contact_loss`] condemns, applied repeatedly until a pass finds
/// nothing new.
///
/// One pass is not enough, and not because the rule is unstable: deleting a
/// single RR-condemned frame **splices together** the constant-bpm runs that
/// flanked it. The merged run can exceed [`hr::CONTACT_FROZEN_BPM_MS`] where
/// neither half did, so a verdict only reachable after the deletion is a real
/// verdict, not an artefact. Iterating to a fixpoint is what makes
/// `recompute-hr` genuinely idempotent for its caller.
pub fn plan_to_fixpoint(rows: &[HrRow]) -> Vec<i64> {
    let mut surviving: Vec<HrRow> = rows.to_vec();
    let mut doomed: Vec<i64> = Vec::new();

    for pass in 1..=MAX_FIXPOINT_PASSES {
        let condemned = plan_contact_loss(&surviving);
        if condemned.is_empty() {
            return doomed;
        }
        let condemned_set: HashSet<i64> = condemned.iter().copied().collect();
        surviving.retain(|row| !condemned_set.contains(&row.id));
        doomed.extend(condemned);
        if pass == MAX_FIXPOINT_PASSES {
            warn!(
                passes = MAX_FIXPOINT_PASSES,
                "hr cleanup did not reach a fixpoint — re-run `recompute-hr` to continue"
            );
        }
    }
    doomed
}

/// Replay the whole HR history, drop the contact-lost samples, print a summary.
pub fn run(dry_run: bool) -> Result<()> {
    let mut store = Store::open()?;
    let rows = store
        .hr_samples_ordered()
        .context("read hr_samples for recompute")?;
    if rows.is_empty() {
        println!("recompute-hr: no hr_samples recorded — nothing to clean.");
        return Ok(());
    }

    let doomed = plan_to_fixpoint(&rows);
    if doomed.is_empty() {
        println!(
            "recompute-hr: replayed {} samples — no contact-lost frames found, history is clean.",
            rows.len()
        );
        return Ok(());
    }

    let kept = rows.len() - doomed.len();
    if dry_run {
        println!(
            "recompute-hr (dry run): would delete {} of {} samples ({} kept). Re-run without --dry-run to apply.",
            doomed.len(),
            rows.len(),
            kept
        );
        return Ok(());
    }

    store
        .delete_hr_samples(&doomed)
        .context("delete contact-lost hr samples")?;
    println!(
        "recompute-hr: deleted {} contact-lost samples of {} ({} kept). \
         `♥ avg/max` is computed at read time, so stats are already corrected.",
        doomed.len(),
        rows.len(),
        kept
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frames captured live in задача 033.
    const WORN: [u8; 6] = [0x10, 0x75, 0x72, 0x03, 0x27, 0x02];
    const REMOVED: [u8; 2] = [0x00, 0x6f];

    /// One row per second, starting at an arbitrary epoch.
    fn rows(frames: &[&[u8]]) -> Vec<HrRow> {
        frames
            .iter()
            .enumerate()
            .map(|(i, frame)| HrRow {
                id: i as i64 + 1,
                ts_ms: 1_000_000 + i as i64 * 1000,
                raw_frame: frame.to_vec(),
            })
            .collect()
    }

    #[test]
    fn a_worn_strap_loses_nothing() {
        let rows = rows(&[&WORN, &WORN, &WORN]);
        assert!(plan_contact_loss(&rows).is_empty());
    }

    #[test]
    fn only_the_frames_past_the_tolerance_are_doomed() {
        // Two RR-less frames are tolerated; the run only counts as contact loss
        // from the third onwards.
        let rows = rows(&[
            &WORN, &WORN, &REMOVED, &REMOVED, &REMOVED, &REMOVED, &REMOVED,
        ]);
        assert_eq!(plan_contact_loss(&rows), vec![5, 6, 7]);
    }

    #[test]
    fn a_link_gap_resets_the_rr_evidence_but_not_the_frozen_clock() {
        // Two RR-less frames, then a >10s gap, then two more: the RR-less run
        // cannot reach its threshold, because the second link starts fresh.
        let mut rows = rows(&[&WORN, &REMOVED, &REMOVED]);
        for (offset, frame) in [REMOVED, REMOVED].iter().enumerate() {
            rows.push(HrRow {
                id: 10 + offset as i64,
                ts_ms: rows[2].ts_ms + HR_LINK_GAP_MS + 1 + offset as i64 * 1000,
                raw_frame: frame.to_vec(),
            });
        }
        assert!(plan_contact_loss(&rows).is_empty());

        // But a bpm that has not moved across the gap is still frozen: a heart
        // does not reset with the BLE link. Without this, cleaning a run would
        // leave sparse survivors that look fresh forever.
        let far_future = rows.last().expect("non-empty").ts_ms + hr::CONTACT_FROZEN_BPM_MS;
        rows.push(HrRow {
            id: 99,
            ts_ms: far_future,
            raw_frame: REMOVED.to_vec(),
        });
        assert_eq!(plan_contact_loss(&rows), vec![2, 3, 10, 11, 99]);
    }

    /// The property the whole command rests on: applying the plan and replaying
    /// must produce an empty second plan, even though deleting the tail changes
    /// which rows sit next to each other.
    #[test]
    fn the_plan_is_idempotent() {
        let first_pass = rows(&[&WORN, &WORN, &REMOVED, &REMOVED, &REMOVED, &REMOVED]);
        let doomed = plan_contact_loss(&first_pass);
        assert!(!doomed.is_empty());

        let survivors: Vec<HrRow> = first_pass
            .into_iter()
            .filter(|r| !doomed.contains(&r.id))
            .collect();
        assert!(plan_contact_loss(&survivors).is_empty());
    }

    /// The 54 samples that survived the first (RR-only) cleanup: a strap on the
    /// desk interleaving RR-bearing frames with `00 6F`, bpm never moving.
    #[test]
    fn an_interleaved_rr_stream_with_a_frozen_bpm_is_doomed_past_the_freeze_window() {
        // `WORN` and `REMOVED` share bpm 0x6f here, mimicking the live capture.
        let worn_111: [u8; 4] = [0x10, 0x6f, 0xfb, 0x17];
        let frames: Vec<&[u8]> = (0..120)
            .map(|i| {
                if i % 3 == 0 {
                    &worn_111[..]
                } else {
                    &REMOVED[..]
                }
            })
            .collect();
        let rows = rows(&frames);
        let mut doomed = plan_contact_loss(&rows);
        doomed.sort_unstable();
        // The bpm never moves, so the whole run is garbage — including the
        // first minute the live daemon could not have known about.
        assert_eq!(doomed.len(), rows.len());
        assert_eq!(doomed[0], 1);
    }

    /// Offline replay condemns the *whole* constant-bpm run, unlike the daemon
    /// which necessarily keeps the first `CONTACT_FROZEN_BPM_MS` of it.
    #[test]
    fn a_frozen_run_is_deleted_from_its_very_first_sample() {
        // 20 live seconds at a moving bpm, then 90s frozen at the last value.
        let live: [u8; 4] = [0x10, 0x64, 0xfb, 0x17]; // bpm 100
        let live2: [u8; 4] = [0x10, 0x65, 0xfb, 0x17]; // bpm 101
        let frozen: [u8; 4] = [0x10, 0x65, 0xfb, 0x17]; // bpm 101, forever
        let mut frames: Vec<&[u8]> = Vec::new();
        for i in 0..20 {
            frames.push(if i % 2 == 0 { &live[..] } else { &live2[..] });
        }
        frames.extend(std::iter::repeat_n(&frozen[..], 90));

        let rows = rows(&frames);
        let doomed = plan_contact_loss(&rows);
        // The frozen run starts at index 19 (the last live2 frame flows into it,
        // since it carries the same bpm) — ids are 1-based.
        assert_eq!(doomed[0], 20);
        assert!(!doomed.contains(&19), "the moving-bpm prefix must survive");
        // Idempotent even with the widened verdict.
        let survivors: Vec<HrRow> = rows
            .into_iter()
            .filter(|r| !doomed.contains(&r.id))
            .collect();
        assert!(plan_contact_loss(&survivors).is_empty());
    }

    /// Why one pass is not enough: two constant-bpm fragments, each shorter than
    /// the freeze window, separated by a *different* bpm that is itself frozen.
    /// Deleting that middle run splices the flanks into one over-long run — a
    /// verdict only the second pass can reach.
    #[test]
    fn the_fixpoint_loop_catches_runs_that_only_merge_after_a_deletion() {
        let freeze_s = (hr::CONTACT_FROZEN_BPM_MS / 1000) as usize;
        let flank = freeze_s / 2 + 2; // each 111-fragment alone is under the window
        let bpm_111: [u8; 4] = [0x10, 0x6f, 0xfb, 0x17];
        let bpm_112: [u8; 4] = [0x10, 0x70, 0xfb, 0x17];

        let mut frames: Vec<&[u8]> = Vec::new();
        frames.extend(std::iter::repeat_n(&bpm_111[..], flank));
        frames.extend(std::iter::repeat_n(&bpm_112[..], freeze_s + 2)); // frozen on its own
        frames.extend(std::iter::repeat_n(&bpm_111[..], flank));
        let rows = rows(&frames);

        // Pass one condemns only the middle run.
        let single = plan_contact_loss(&rows);
        assert_eq!(single.len(), freeze_s + 2);

        // The fixpoint then sees the two 111-flanks merge into one frozen run.
        let full = plan_to_fixpoint(&rows);
        assert_eq!(full.len(), rows.len());

        // And it really is a fixpoint.
        let survivors: Vec<HrRow> = rows.into_iter().filter(|r| !full.contains(&r.id)).collect();
        assert!(plan_to_fixpoint(&survivors).is_empty());
    }

    #[test]
    fn a_sensor_that_never_sent_rr_is_left_alone() {
        let plain = [0x00u8, 90];
        let rows = rows(&[&plain, &plain, &plain, &plain, &plain]);
        assert!(plan_contact_loss(&rows).is_empty());
    }

    #[test]
    fn an_undecodable_frame_is_skipped_without_confusing_the_tracker() {
        let empty: [u8; 0] = [];
        let rows = rows(&[&WORN, &empty, &REMOVED, &REMOVED, &REMOVED]);
        // The corrupt row (id 2) is neither deleted nor counted as an RR-less frame.
        assert_eq!(plan_contact_loss(&rows), vec![5]);
    }
}
