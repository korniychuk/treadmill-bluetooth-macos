# 006 — `SpeedKmh` / centi quantize at decode boundary

**Status:** backlog  
**Depends on:** [030](../tasks/030-zone-hold-noop-writes-at-clamp.md) (epsilon exists); better after state extract  
**Source:** [research/003](../research/003-reliability-architecture-review.md) Phase 4

## Goal

FTMS wire is `u16 * 0.01`. Config is TOML f32. Compare/clamp on raw f32 caused 030.

Introduce one type at the boundary:

```rust
struct CentiKmh(u16); // or SpeedKmh with from_wire / from_config / to_wire
```

- Quantize on decode **and** before compare/write
- Keep `MIN_SPEED_CHANGE_KMH` as **controller deadband**, not float glue

## Acceptance

- 030-style tests express quantize identity + deadband policy
- No new silent float compares on wire speeds

## Non-goals

- Rewriting all f32 in the codebase in one PR — boundary first (ftms decode, control set_speed, zone next_speed inputs)
