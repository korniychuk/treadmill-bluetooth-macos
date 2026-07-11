# 008 — Typed config apply (`ConfigDelta` → session effects)

**Status:** done → [052](../tasks/052-typed-config-apply.md)  
**Depends on:** [032](../tasks/032-zone-hold-off-still-drives-speed.md) (partial ad-hoc fix); [005](005-session-state-extract.md) for a home for `apply_config`  
**Source:** [research/003](../research/003-reliability-architecture-review.md) Phase 3

## Goal

Hot-reload today: `stat` + field copy. 032 proved that's not enough for phase machines.

```text
reload_if_changed() -> Option<ConfigDelta>
session.apply_config(delta) -> effects (disengage / re-engage / threshold change / ignore)
```

Cover mid-session policies explicitly:

- `enabled` ↓ / ↑
- `target_zone` / `max_speed` mid-Hold → re-resolve `ResolvedZone`
- `warmup_minutes` mid-Ramp — pick a policy and test it
- age removed → disengage

Atomic write of TOML is **not** this task — see [037](../tasks/037-atomic-config-toml-write.md).

## Acceptance

Table-driven tests for the matrix above; no silent field copy without apply.

Implemented in `src/config_apply.rs` + reload-branch executor in `daemon.rs` (задача 052).
