# 005 — Session state extract (лёгкий kernel)

**Status:** Step 1 done (задача [053](../tasks/053-session-state-extract.md)); Step 2 still optional  
**Depends on:** Phase 0 tasks [035](../tasks/035-hr-relative-timeout-in-select.md)–[038](../tasks/038-tm-doctor-liveness-matrix.md) ideally landed first  
**Source:** [research/003](../research/003-reliability-architecture-review.md) Phase 1

## Goal

Свернуть ~20 `mut` locals в `stream_with_presence` в 3–4 структуры с методами + unit-тестами **без** полного `tick(Event) -> Vec<Effect>`.

## Step 1 (do)

```text
HrSession { link, contact, battery, last_frame_at, notifications_present, … }
ZoneSession { phase, resolved, last_write_at, override_until, … }
AutoPause { away_since, attempt_state, … }
```

- Methods: `on_hr_frame`, `on_silence`, `should_reconnect`, `on_config`, `disengage`, …
- `select!` stays a thin shell: arm → method → existing side-effect calls
- Nested awaits (`try_restore_speed`, `try_apply_default_speed`) may stay in daemon for now

**Acceptance:** zone/HR/auto-pause transitions unit-tested without btleplug; `stream_with_presence` reads as wiring.

## Step 2 (optional later)

Full `Event` / `Effect` only if Step 1 hits a wall. Do **not** block features on it.

## Non-goals

- Actors/ECS
- BLE in a separate process
- Mechanical file split of `daemon.rs` *before* state extract (see backlog 007)
