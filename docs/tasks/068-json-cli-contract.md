# 068 — Local JSON CLI contracts

Status: **done** (external PR #3 by @huertin03, finished in review).

Add `stats --json [--all]` and `samples --after-id N --limit N` for external
consumers of local data. No networking or outbound integration is added.
Both commands open SQLite read-only, without migrations or retention pruning.
Missing databases produce an empty, versioned response and are not created.

Stats retain existing local-day and workout-gap semantics. Samples are ordered
by increasing row ID, not timestamp (clock adjustments must not skip rows).
The exclusive cursor is returned as `next_after_id`; empty pages preserve it.
`has_more` is determined using one extra row in the same query. Limits are
1–10000. IDs belong to one database lifetime; a consumer must reset its cursor
when the source database is replaced. Optional wire fields stay JSON null.
Diagnostics go to stderr; stdout is one complete JSON document.

Validation covers empty and populated stats, null fields, exclusive pagination,
clock rollback, cursor exhaustion, and prevention of writes on read-only opens.

## Layering (review follow-up)

`Store::raw_samples_page` returns typed `SampleExportRow`s (`src/store/export.rs`);
the JSON wire contract — field names, `schema_version`, null handling — lives only
in `src/commands/json.rs`, so persistence refactors cannot change the documents.
Page limits are the shared constant `SAMPLES_PAGE_MAX`, enforced by both clap and
the store.

Stats fields: `days[]` with `date` (local `YYYY-MM-DD`), `steps`, `distance_m`,
`walking_time_s`, `workouts[]` (`id`, RFC3339 `started_at`/`ended_at`, same
totals). Workouts use the configured gap and are attributed to their start day,
so their sum may differ from calendar totals across midnight.
Sample fields: `id`, `session_id`, UTC `ts_ms`, `speed_centikmh`,
`avg_speed_centikmh`, `distance_m`, `energy_kcal`, `elapsed_s`, `steps` (null
when the frame lacked the field). Persist a cursor only after processing its
page; consumers must tolerate retries.
