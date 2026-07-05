# 013 — Route control commands through the daemon via a SQLite queue

## Problem

`tm speed <kmh>` / `tm start` / `tm stop` currently open their **own** BLE
adapter and scan for the treadmill's FTMS advertisement
(`scan::connect_treadmill`). But the always-on daemon (a LaunchAgent) holds the
BLE connection, so the treadmill no longer advertises, and the CLI — a separate
process with its own CoreBluetooth central — can't find it:
`no FTMS treadmill found within 15s`.

Two processes can't co-own the link. Verified: the treadmill UUID is not
persisted, and `connect_by_id` also relies on a fresh scan, so it can't help
across processes either.

## Fix

The daemon is the single BLE owner. The CLI **enqueues** a command into a
SQLite queue; the daemon **executes** it on its live connection using the
existing `control::Controller` (same take-control + set-speed path the
pause-resume speed-restore already uses — `daemon.rs::restore_speed`), bounded
by `SPEED_RESTORE_TIMEOUT`.

### Store (`src/store.rs`)

New table:

```sql
CREATE TABLE control_commands (
    id          INTEGER PRIMARY KEY,
    created_at  TEXT NOT NULL,   -- RFC3339 UTC
    command     TEXT NOT NULL,   -- wire form: "start" | "stop" | "speed:2.5"
    status      TEXT NOT NULL,   -- pending | done | failed
    executed_at TEXT,
    error       TEXT
);
CREATE INDEX idx_control_commands_status ON control_commands(status, id);
```

Methods (follow existing migration/insert/query patterns):
- `enqueue_control_command(&ControlCommand) -> i64` — inserts pending, prunes
  old rows (bound growth), returns the new id.
- `next_pending_control_command() -> Option<QueuedControlCommand>` — oldest
  pending row, parsed.
- `mark_control_command_done(id)` / `mark_control_command_failed(id, error)`.
- `control_command_outcome(id) -> Option<(String status, Option<String> error)>`
  — for CLI polling.

**Pruning (bound what accumulates):** on every enqueue, delete rows older than
5 minutes. Safe against the ≤8s CLI poll and the 30s staleness guard (a 5-min
row is long resolved). Keeps the table tiny with no unbounded growth.

### Command type (`src/control_command.rs`, new small module)

Pure, unit-tested — shared by CLI (enqueue), store (persist), daemon (execute):

```rust
enum ControlCommand { Start, Stop, Speed(f32) }
```
- `to_wire()` / `parse()` round-trip: `start`, `stop`, `speed:2.5`.
- `is_stale(created_at, now)` — pure staleness decision, threshold
  `CONTROL_STALE_THRESHOLD` = **30s**.

### Daemon (`src/daemon.rs`)

In the streaming `select!` loop:
- Call `process_control_commands` **at the end of the telemetry-branch
  handler** (after `state.persist`) — telemetry arrives ~1/s while connected,
  so this bounds latency to **≤1s** during an active session.
- **Also** add a `tokio::time::interval(~1s)` backstop arm so commands still
  run during quiet stretches. `process_control_commands` is a single indexed
  `SELECT`, **silent on the empty path** (runs ~1/s — no happy-path log).
- Drain **one command per call** so a burst can't block the loop for
  N×`SPEED_RESTORE_TIMEOUT`.
- **Staleness guard:** a command older than 30s is marked `failed`
  ("stale, not executed") and never executed — prevents a surprise belt-speed
  change when the daemon reconnects/restarts long after a command was queued.
- Execute via `Controller::take_control` + the matching call, wrapped in a
  bounded `tokio::time::timeout(SPEED_RESTORE_TIMEOUT, ...)`. Success → `done`;
  BLE write failure/timeout → `failed` + WARN. **Never crash the daemon over a
  failed control write.** (DB errors still propagate, matching the rest of the
  loop.)
- Only the streaming loop (which holds the `Peripheral`) executes; when
  disconnected, commands stay pending until the staleness guard fails them.

### CLI (`src/main.rs`)

`Start` / `Stop` / `Speed` handled **before** the BLE adapter is opened (like
`status` / `widget`), dual-path:
1. If the daemon is **alive** (`launchctl` probe) **AND** connected **AND**
   fresh (`WATCHDOG_STALE_THRESHOLD_S`) → **enqueue**, poll the row up to ~8s
   for `done`/`failed`, print the result. Never opens the BLE adapter.
   Including the `launchctl` liveness probe (not just connected+fresh) closes
   the "daemon died <95s ago" window where the row still reads connected+fresh
   but the direct fallback would actually work.
2. Else → **fallback** to the existing direct-BLE path
   (`scan::connect_treadmill` + `control`), so the CLI still works when the
   daemon is off. Only this path touches BLE.

`Incline` stays direct-only (out of scope; the device rejects incline anyway)
— it remains unavailable while the daemon holds the link.

## Latency & thresholds

| Knob | Value | Why |
| --- | --- | --- |
| daemon command poll | end-of-telemetry (~1/s) + 1s interval backstop | ≤1s during a session |
| execution timeout | `SPEED_RESTORE_TIMEOUT` (15s) | reuse; well under watchdog 120s |
| staleness guard | 30s | > poll latency, < CLI 8s giving-up; blocks surprise fires |
| CLI poll | 500ms × ~8s | enough for one execution round-trip |
| prune | rows older than 5 min | bound growth; safe vs poll/staleness |

## Tests

Pure/unit-testable (no BLE mocking — hardware only):
- `ControlCommand` wire round-trip (`start`/`stop`/`speed`) + parse errors.
- `is_stale` decision (created_at + now → execute vs skip).
- Queue status transitions against an in-memory `Store` (`memory_store()`):
  enqueue → pending → done/failed; `next_pending_control_command` ordering;
  prune bounds the table.

## Risks for operator review

- **Second `notifications()` stream:** `take_control` opens its own indication
  stream while the daemon holds the main telemetry stream. Validated by задача
  012's `restore_speed`, which already does exactly this in the same loop — but
  worth a live hardware check under load.
- **Arbitration vs pause-resume restore:** a queued `speed` and an automatic
  pause-resume restore can't overlap within the single select task, but
  whichever runs last wins semantically. Low risk, called out.
</content>
