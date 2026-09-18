# 065 — `tm start --speed <kmh>`: start the belt at an explicit speed

> **Статус: planned** (2026-09-18). **Класс:** feature · **Приоритет:** medium.
> **Источник:** external PR [#4](https://github.com/korniychuk/treadmill-bluetooth-macos/pull/4)
> by @huertin03 (draft, left open). Its idea is taken, its sequencing is not (see
> «Live facts»). Builds on [012](012-pause-resume-speed-restore.md) (bounded speed
> write), [013](013-control-commands-via-daemon-queue.md) (daemon queue),
> [016](016-default-speed-on-workout-start.md) (computed default speed), [039](039-control-source-and-operator-override.md)
> (control source / Zone Hold override), [054](054-speed-centi-newtype.md) (`CentiKmh`),
> [063](063-keyboard-belt-control.md) (`BeltIntent`, relative commands).

## Goal

`tm start --speed 3.5` starts the belt and, **as soon as the console countdown
ends**, sets the target speed to 3.5 km/h, **instead of** the computed default
(016) or a pre-pause restore (012). A bare `tm start` does not change: same
behaviour, same queue wire text `start`.

## Live facts (W2 Pro, 2026-09-18, current `main`)

Experiment: `tm start`, then `tm speed 3.5` right away, then `tm speed 3.5` again
~10 s later. Daemon log, in time since `start`:

| t | Event |
|---|---|
| +0.46 s | Start acknowledged (`op 0x07`) |
| +0.5 s | status event `StartedOrResumedByUser` (0x04) |
| +0.9 s | `tm speed 3.5` → **`op 0x02 rejected: result 4` (Operation Failed)**, because the countdown is running |
| +3.7 s | status event `TargetSpeedChanged` (0x05), and presence `Paused → Walking` (the belt moves at the 0.5 crawl; the operator was standing on the side rails) |
| +4.0 s | **`try_apply_default_speed` succeeded**: 0.5 → 2.7 |
| +9.9 s | `tm speed 3.5` accepted |

Conclusions:
1. The firmware rejects Set Target Speed during the ~3 s countdown. PR #4 does
   Start + Speed back to back and Stops on failure, so on this device it would
   stop the belt every time.
2. The daemon **already has a hook at the exact moment the countdown ends**: the
   presence resume branch in `src/daemon/session.rs` (≈ l.280–325,
   `pre_pause` restore → `try_apply_default_speed`). The explicit speed must
   plug into that hook. Do not add a separate wait, retry or Stop pipeline.

## Design

1. **CLI.** `tm start` gets an optional `--speed <kmh>`. Parse it with the same
   code as `tm speed <kmh>` (`CentiKmh::from_kmh_f32`, clamp/validation as for
   the absolute speed command). A value out of range is a CLI error before
   anything is enqueued.
2. **Queue wire.** Add a new `ControlCommand::StartWithSpeed(CentiKmh)` with the
   wire text `start_speed:<kmh>` (underscore, like `speed_step:*`), round-trip
   tested in `src/control_command.rs`. An older daemon fails the row as
   unparseable, which is safe. Upgrade the CLI and daemon together
   (`install-daemon.sh`).
3. **Daemon: execute.** Send the normal Start (the same code path as `start`).
   On ack:
   - `link.set_pending_start_speed(target, now)` on `TreadmillLink`;
   - `intent.note_run(...)` exactly as for `start` (063);
   - Zone Hold: open the operator-override window exactly as an operator speed
     command does today (039).

   The queue row resolves right away: CLI output
   `belt started; 3.5 km/h after the countdown`. This matches the queue's
   request/ack model, because the speed write happens later on a telemetry event.
4. **Daemon: apply.** In the presence resume branch, the precedence is:
   **pending start speed (fresh)** → pre-pause restore → computed default.
   - Take the pending target once, via `take_pending_start_speed(now)`. It
     returns `None` when older than `PENDING_START_SPEED_TTL` (15 s from the
     Start ack: countdown ~3.3 s plus generous slack) and logs a `WARN` on expiry.
   - Write it through the existing bounded `restore_speed` /
     `SPEED_RESTORE_TIMEOUT` path, with `ControlSource` = the CLI/operator
     source.
   - Then call `link.mark_default_speed_applied()`, so 016 never overrides it
     later in the session.
   - Call `intent.note_speed(target, ...)` (063) and feed `zh_effective` like
     the other two branches.
   - Toast: reuse `notify::default_speed_applied`-style wording, or a small
     sibling such as "Started at 3.5 km/h". Pick whichever is less code.

   A failed or timed-out write: `WARN`, and the belt stays at the crawl.
   **No Stop**, because the Start was an ordinary operator start. The pending
   target is consumed either way (one attempt, like 016).
5. **Other pending-target cleanup.** Clear the pending target on:
   - a Stop of any origin (CLI, auto-pause, safety, a console-stop status event,
     see 063's `note_safety_stop` site);
   - session end / disconnect (`TreadmillLink` is session-scoped already).

   A later ordinary `tm speed X` issued before the countdown ends is out of
   scope. It is rejected by the firmware exactly as today.
6. **Scenario to check in the design:** the belt is already running when
   `start --speed` arrives. Start is then a no-op for the belt, and presence may
   never go through a resume transition. In that case, if live speed is > 0 at
   execute time, write the speed immediately instead of arming the pending
   target (the countdown is not running). The pure decision function must cover
   this case.

## Out of scope

- Everything else PR #4 added: the Supported Speed Range (0x2AD4) read, the
  Stop-on-failure pipeline, and the 19 s / 25 s execution/CLI budgets. The
  firmware's own range is already enforced by the existing `tm speed` clamp.
- Incline.

## Implementation notes

- Keep the decision pure and time-injected (house style: `presence.rs`,
  `belt_intent.rs`, `auto_pause.rs`). Pending-target TTL, precedence and the
  already-running case are unit tests without BLE.
- Constants are named. No inline magic numbers (PR #4 review finding).
- Update `CLAUDE.md` (the `control_command.rs` / `treadmill_link.rs` /
  `daemon/speed.rs` entries plus the Команды block), README (Commands list: one
  line), `CHANGELOG.md` `[Unreleased]` (credit @huertin03 for the idea, PR #4),
  and the `065` entry in `docs/README.md` (flip «planned» to the final status).

## Gates

`cargo fmt --all --check` → `cargo clippy --all-targets -- -D warnings` →
`cargo build` → `cargo test`, and `bash scripts/test-cli-link.sh` (CI mirror,
see `docs/ci.md`).

## Live acceptance (orchestrating session only, needs the treadmill)

After `scripts/install-daemon.sh`:
1. From a stopped belt, `tm start --speed 3.5`. Expect: countdown, then the
   console shows **3.5 directly** (no 2.7 step). The log shows the pending start
   speed applied, and no `op 0x02 rejected`.
2. Right after 1, `tm speed up`. Expect: 3.6 (063 intent picked up the target).
3. Stop; plain `tm start`. Expect: the computed default as before (016 is not
   regressed).
4. With the belt already running, `tm start --speed 3.0`. Expect: an immediate
   switch to 3.0.

## After landing

Comment on PR #4 with thanks, the live table above, and the landed commit SHAs,
then close it. The operator decides when. The session that prepared this task
integrates the change and cuts the single release `v0.5.0`.
