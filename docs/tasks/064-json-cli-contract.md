# 064 — Local JSON CLI contracts

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
