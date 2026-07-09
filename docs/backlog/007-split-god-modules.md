# 007 — Split god modules (`store` / `daemon` wiring / `main` CLI)

**Status:** backlog  
**Depends on:** [005](005-session-state-extract.md) Step 1 (state extract first — otherwise we only move gods)  
**Source:** [research/003](../research/003-reliability-architecture-review.md) Phase 5

## Goal

Mechanical splits after state is extracted:

| Target | Content |
|---|---|
| `store/schema.rs` | CREATE + `add_column_if_missing` + **schema snapshot test** |
| `store/samples.rs` | raw/hr inserts |
| `store/activity.rs` | segments, credit, merge |
| `store/status.rs` | daemon_status DTO |
| `store/control_queue.rs` | commands |
| `cli/` or `commands/` | one file per command group |
| `widget.rs` | TSV contract + field formatters |

## Explicitly YAGNI

- Versioned migrations / `schema_version` table / down migrations  
  Single-user sole-writer SQLite + `recompute-*` already recover truth. Snapshot test + `add_column_if_missing` is enough.

## Acceptance

- No file stays in 🔴 (&gt;1000 LOC) without an explicit exception note
- Schema snapshot test fails if columns drift
- Widget field count golden (`assert_eq!(fields.len(), 12)` or versioned contract)

## Non-goals

- Behaviour changes
- Full Event/Effect kernel (backlog 005 Step 2)
