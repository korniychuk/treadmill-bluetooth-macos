# 007 — Split god modules (`store` / `daemon` wiring / `main` CLI)

**Status:** done — `store` → [049](../tasks/049-store-module-split.md),
`main`/CLI → [050](../tasks/050-cli-module-split.md),
`daemon` wiring + `zone_hold` → [055](../tasks/055-daemon-zone-module-split.md)  
**Depends on:** [005](005-session-state-extract.md) Step 1 (state extract first — otherwise we only move gods)  
**Source:** [research/003](../research/003-reliability-architecture-review.md) Phase 5

## Goal
Mechanical splits after state is extracted:

| Target | Content | Done |
|---|---|---|
| `store/schema.rs` | CREATE + `add_column_if_missing` + **schema snapshot test** | 049 |
| `store/samples.rs` | raw/hr inserts | 049 |
| `store/activity.rs` | segments, credit, merge | 049 |
| `store/status.rs` | daemon_status DTO | 049 |
| `store/control_queue.rs` | commands | 049 |
| `cli/` or `commands/` | one file per command group | 050 |
| `widget.rs` | TSV contract + field formatters | 050 |
| `daemon/` | run_loop / session / state / watchdog / … | 055 |
| `zone_hold/` | controller / config / cli_config | 055 |

## Explicitly YAGNI

- Versioned migrations / `schema_version` table / down migrations  
  Single-user sole-writer SQLite + `recompute-*` already recover truth. Snapshot test + `add_column_if_missing` is enough.

## Acceptance

- [x] No file stays in 🔴 (&gt;1000 LOC) without an explicit exception note
- [x] Schema snapshot test fails if columns drift
- [x] Widget field count golden (`assert_eq!(fields.len(), 12)` or versioned contract)

## Non-goals

- Behaviour changes
- Full Event/Effect kernel (backlog 005 Step 2)
