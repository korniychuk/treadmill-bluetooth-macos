# 065 — `tm start --speed <kmh>`: start the belt at an explicit speed

> **Статус: planned** (2026-09-18). **Класс:** feature · **Приоритет:** medium.
> Fable design finished 2026-09-18 (in-session, effort high) — the pending target
> moved from `TreadmillLink` to `BeltIntent` (stop-cleanup by construction), the
> already-running case became an execute-time resolution to a plain `speed:`, and
> the CLI got an explicit range check (the draft's "existing clamp" did not exist).
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
   presence resume arm in `src/daemon/session.rs`
   (`PresenceState::Walking if prev_state == PresenceState::Paused`, ≈ l.283–329:
   `pre_pause` restore → `try_apply_default_speed`). The explicit speed plugs
   into that hook. No separate wait, retry or Stop pipeline.

**Not measured:** what the firmware answers to Start (`op 0x07`) on an already
moving belt. The design below never sends it (see Design 3), so it does not
need to be known.

## Code facts the design rests on (verified 2026-09-18)

- `BeltIntent` (`src/belt_intent.rs`) and `TreadmillLink` are both created per
  session in `src/daemon/session.rs:110–112`, so both are session-scoped.
- **Every Stop the daemon sends or observes already goes through `BeltIntent`:**
  CLI stop / toggle→stop → `record_cli_intent` → `note_run(RunIntent::Stop)`
  (`src/daemon/commands.rs:192–202`); auto-pause (`session.rs:424`), Zone Hold
  safety stop (`src/daemon/zone_write.rs:36`) and FTMS stop status events
  0x02/0x03 (`session.rs:199`) → `note_safety_stop`. `zone_write.rs` has no
  access to `TreadmillLink` at all.
- `tm speed <kmh>` is **not** range-checked beyond `CentiKmh::from_kmh_f32`
  (finite, ≥ 0, fits `u16`; `src/commands/belt.rs:37–54`). `SPEED_MIN` (0.50) /
  `SPEED_MAX` (6.10) in `src/belt_intent.rs:20–23` only clamp relative steps.
  Today an out-of-range `tm speed 9` fails at once with the firmware's reject;
  for `start --speed` the same reject would arrive ~4 s *after* the CLI printed
  success — hence the explicit CLI check below.
- The queue row carries only `status` + `error`
  (`Store::control_command_outcome`, `src/store/control_queue.rs:132`): a `done`
  row has no payload, so the CLI cannot learn which path the daemon took.
- `process_control_commands` (`src/daemon/commands.rs:63–157`) resolves relative
  commands first (`resolve_relative_command` → `RelativeResolve::Ready(cmd)`),
  executes the resolved command, then `record_cli_intent(intent, executed, now)`.
  It returns `was_speed` (today computed from `queued.command`); both call sites
  in `session.rs` (≈ l.508–519) open the Zone Hold operator-override window on
  `true` (`zone.note_cli_speed`, 60 s, `src/zone_session.rs:131`).
- Presence leaves `Unknown` on the first sample, so a Start from a stopped belt
  always reaches the `Paused → Walking` arm; the `Unknown → Walking` arm (fresh
  connect, belt already moving) needs no change.
- `ControlSource` is logging-only (`execute_control_command` ignores it).

## Design

1. **CLI.** `Commands::Start` becomes `Start { #[arg(long)] speed: Option<CentiKmh> }`
   (`src/main.rs:141`, dispatch at `:358`, the no-adapter arm at `:403`).
   - Value parser `parse_start_speed(&str) -> Result<CentiKmh, String>` in
     `src/commands/belt.rs`. It shares the float→`CentiKmh` step with
     `SpeedTarget::from_str` (extract that step into one private helper; do not
     duplicate it) and then requires `belt_intent::is_supported_target(speed)`.
     Out of range is a clap error before anything is enqueued:
     `speed 9 km/h is outside the belt's range 0.5–6.1 km/h`.
   - `pub fn is_supported_target(speed: CentiKmh) -> bool` lives in
     `src/belt_intent.rs` next to `SPEED_MIN`/`SPEED_MAX` (`SPEED_MIN..=SPEED_MAX`).
     `tm speed <kmh>` keeps its current validation — not this task.
   - `Some(speed)` → `ControlCommand::StartWithSpeed(speed)`, `None` →
     `ControlCommand::Start` (unchanged).
2. **Queue wire.** New `ControlCommand::StartWithSpeed(CentiKmh)`, wire text
   `start_speed:<kmh>` (underscore, like `speed_step:*`; `<kmh>` is the
   `CentiKmh` `Display`, same as `speed:`). `parse` stays range-agnostic like
   `speed:` (`from_kmh_f32` only). An older daemon fails the row as unparseable,
   which is safe; CLI and daemon upgrade together (`install-daemon.sh`).
   - `requires_daemon_intent()` returns `true` for it: without the daemon there
     is no telemetry to tell when the countdown ends, and a fixed sleep in the
     direct-BLE path is exactly the fragile sequencing this task avoids. The
     existing `bail!` in `run_control` covers it; the direct-BLE `mapped` match
     puts it into the `unreachable!` arm with the relative commands.
3. **Daemon: resolve at execute time** (pure, in `BeltIntent`):

   ```rust
   pub enum StartSpeedPlan { SetSpeedNow, StartThenSpeed }
   pub fn resolve_start_speed(&self, live: Option<CentiKmh>, now: Instant) -> StartSpeedPlan
   ```

   - a Stop (CLI or safety) inside `INTENT_WINDOW` → `StartThenSpeed` (the belt
     is decelerating; a bare speed write must never re-accelerate it — same rule
     as `resolve_step`'s `RecentStop`);
   - else `live > 0` → `SetSpeedNow` (no countdown is running);
   - else (`live == 0` or unknown) → `StartThenSpeed`.

   `resolve_relative_command` maps `SetSpeedNow` → `Ready(ControlCommand::Speed(target))`:
   the row then runs as an ordinary `speed:` write — **no Start is sent to a
   moving belt**, `record_cli_intent` notes the speed, and the override window
   opens. `StartThenSpeed` → `Ready(StartWithSpeed(target))`.
   To make the override follow the *executed* command, compute `was_speed` as
   `matches!(executed, ControlCommand::Speed(_))` (equivalent for every existing
   command: `SpeedStep` always resolves to `Speed`, `Toggle` to `Start`/`Stop`).
4. **Daemon: execute `StartWithSpeed`.** `execute_control_command` sends the
   normal `controller.start()`. On ack, `record_cli_intent` does
   `intent.note_run(RunIntent::Start, now)` + `intent.arm_start_speed(target, now)`.
   A rejected/timed-out Start fails the row as today and arms nothing. The row
   resolves right away (fits `CONTROL_POLL_TIMEOUT` 8 s); the speed write happens
   later on a telemetry event. `was_speed` is `false` here — the override window
   opens when the speed is actually written (step 6).
5. **Pending target — owned by `BeltIntent`** (operator intent, time-injected,
   session-scoped; `TreadmillLink` stays telemetry-only):
   - field `pending_start_speed: Option<(CentiKmh, Instant)>`;
   - `arm_start_speed(target, now)`;
   - `take_pending_start_speed(now) -> Option<CentiKmh>`: always clears; returns
     `None` and logs one `WARN` (target, age, TTL) when older than
     `PENDING_START_SPEED_TTL` = 15 s (countdown ≈ 3.3 s + slack; guards
     against "Start acked, belt never moved, operator starts from the console a
     minute later and gets a surprise 3.5");
   - **cleared inside `note_run(RunIntent::Stop, _)` and `note_safety_stop(_)`**.
     That covers CLI stop, toggle→stop, auto-pause, Zone Hold safety stop and
     console/safety-key stop events with zero new call sites. `note_run(Start)`
     does not clear it. Session end drops the whole struct.
6. **Daemon: apply.** In the `Paused → Walking` arm, right after
   `link.on_resume(...)`, the precedence becomes **pending start speed (fresh)
   → pre-pause restore → computed default**:

   ```rust
   if let Some(target) = intent.take_pending_start_speed(Instant::now()) {
       match try_apply_start_speed(peripheral, target, &mut link, &mut intent).await {
           Some(applied) => {
               zh_effective = Some(applied);
               zone.note_cli_speed(Instant::now());
               notify::start_speed_applied(applied.to_kmh_f32());
           }
           None => notify::treadmill_resumed(resume.paused_for, None),
       }
   } else if let Some(resumed_speed) = data.speed {
       // existing restore/default block, unchanged
   ```

   The write does not need the measured speed, so it sits *before* the
   `data.speed` check. Keep the arm this small — `session.rs` is already in the
   🟡 size zone; the logic goes to `src/daemon/speed.rs`:

   `try_apply_start_speed(peripheral, target, link, intent) -> Option<CentiKmh>`,
   modelled on the tail of `try_apply_default_speed`:
   `link.mark_default_speed_applied()` **before** the write (one attempt either
   way; the operator owns this session's speed, 016 must not override it later),
   then the bounded `timeout(SPEED_RESTORE_TIMEOUT, restore_speed(..))` with
   `ControlSource::Cli` in the log fields (no new variant — it *is* the
   operator's CLI command, deferred). Success → `INFO` + `intent.note_speed` →
   `Some(target)`. Error / timeout → `WARN`, `None`, the belt stays at the crawl.
   **No Stop** — the Start was an ordinary operator start.
   - Zone Hold: an explicit start speed is an operator override like `tm speed`
     — 60 s window, then the controller resumes (a fresh Ramp starts from
     `zh_effective` = the applied speed). Making it the Ramp *target* is out of
     scope.
   - Toast: new `notify::start_speed_applied(to_kmh)` → `"Started at 3.5 km/h"`
     (sibling of `default_speed_applied`, whose "your usual pace" wording would
     be wrong here).
7. **CLI wording.** `describe_control_success(StartWithSpeed(s))` →
   `belt starting — {s} km/h once the countdown ends (right away if already moving)`.
   One neutral line, because a `done` row cannot say which plan ran. The speed
   value is an operator-set value → `highlight_config` (cyan rule, задача 057),
   same as wherever the neighbouring `speed set to …` line does or does not —
   follow the neighbour, do not introduce colour if it has none.

Accepted edges (no code): a second `start --speed` or bare `tm speed X` issued
*during* the countdown is rejected by the firmware exactly as today; if the
belt never goes through `Paused → Walking` inside the TTL, the target silently
expires (the `WARN` fires on the next take).

## Out of scope

- Everything else PR #4 added: the Supported Speed Range (0x2AD4) read, the
  Stop-on-failure pipeline, and the 19 s / 25 s execution/CLI budgets.
- Range validation of `tm speed <kmh>`; incline; start speed as Zone Hold Ramp
  target; `start --speed` without the daemon.

## Tests (all pure, no BLE)

- `belt_intent.rs`: arm → take inside TTL returns the target exactly once;
  expired → `None`; `note_run(Stop)` clears; `note_safety_stop` clears;
  `note_run(Start)` keeps; `resolve_start_speed` — live > 0 → `SetSpeedNow`,
  live 0 → `StartThenSpeed`, live unknown → `StartThenSpeed`, recent CLI Stop
  and recent safety stop with live > 0 → `StartThenSpeed`; `is_supported_target`
  at 0.49 / 0.50 / 6.10 / 6.11.
- `control_command.rs`: the new variant in `wire_round_trips_every_variant`;
  literal wire text `start_speed:3.5`; garbage (`start_speed:`, `start_speed:abc`)
  rejected; `requires_daemon_intent` is `true`.
- `commands/belt.rs`: `parse_start_speed` accepts `3.5`, rejects `0.4`, `6.2`,
  `abc`; the success wording.
- `daemon/commands.rs`: `resolve_relative_command` is pure — add a small test
  module: `StartWithSpeed` + live 3.0 → `Speed(target)`; + live 0 →
  `StartWithSpeed(target)`; `record_cli_intent(StartWithSpeed)` arms the target
  and notes Start.

## Execution

One Codex executor run (`codex-agent.sh`, name `start-speed`) implements Design
1–7 + Tests + the docs below. Owned files: `src/main.rs`, `src/commands/belt.rs`,
`src/control_command.rs`, `src/belt_intent.rs`, `src/daemon/commands.rs`,
`src/daemon/speed.rs`, `src/daemon/session.rs` (the resume arm only),
`src/notify.rs`, `CLAUDE.md`, `README.md`, `CHANGELOG.md`. Not touched:
`src/treadmill_link.rs`, `src/zone_session.rs`, `src/store/`, `scripts/`,
this task doc and `docs/README.md` (the orchestrating session flips the status
after live acceptance). `cargo fmt` changes, if any, as a separate commit.

Docs the run updates: `CLAUDE.md` (the `control_command.rs`, `belt_intent.rs`,
`daemon/` speed entries + the Команды block), `README.md` (the `tm start | tm stop
| tm toggle` line, ≈ l.141), `CHANGELOG.md` `[Unreleased]` → Added (credit
@huertin03 for the idea, PR #4).

- Constants are named, no inline magic numbers (PR #4 review finding).
- Code comments in English; log the abnormal paths (`WARN`), not the happy path
  beyond the one `INFO` per applied start speed (neighbours do the same).

## Gates

`cargo fmt --all --check` → `cargo clippy --all-targets -- -D warnings` →
`cargo build` → `cargo test`, and `bash scripts/test-cli-link.sh` (CI mirror,
see `docs/ci.md`).

## Live acceptance (orchestrating session only, needs the treadmill)

After `scripts/install-daemon.sh`:
1. From a stopped belt, `tm start --speed 3.5`. Expect: countdown, then the
   console shows **3.5 directly** (no 2.7 step). The log shows the start speed
   applied, and no `op 0x02 rejected`.
2. Right after 1, `tm speed up`. Expect: 3.6 (063 intent picked up the target).
3. Stop; plain `tm start`. Expect: the computed default as before (016 is not
   regressed — note that within one daemon session 016 applies once, so check
   this after a reconnect or accept the pre-pause restore to 3.6 as correct).
4. With the belt already running, `tm start --speed 3.0`. Expect: an immediate
   switch to 3.0, and the log shows `resolved = speed:3` with **no** Start write.
5. `tm start --speed 9` → CLI error, nothing enqueued.

## After landing

Comment on PR #4 with thanks, the live table above, and the landed commit SHAs,
then close it. The operator decides when. The session that prepared this task
integrates the change and cuts the single release `v0.5.0`.
