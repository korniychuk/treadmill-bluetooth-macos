# Docs

Documentation-first workspace for `treadmill-bluetooth-macos`.

- `adr/` — Architecture Decision Records.
- `research/` — protocol reverse-engineering notes, BLE captures, findings.
  - [003](research/003-reliability-architecture-review.md) / [004](research/004-independent-reliability-review.md) — reliability plan; tasks **035–047 done**.
- `tasks/` — task specs (`000-name.md`); write before starting work.
  - Reliability `035`–`047` + live smoke [048](tasks/048-live-smoke-035-047.md).
  - Architecture wave `049`–`056`: module splits (store/CLI/daemon/zone_hold),
    scan auto-recover, typed config apply, session state extract, `CentiKmh`.
  - [057](tasks/057-cyan-configurable-values.md) cyan knobs; [058](tasks/058-led-strip-control.md)
    LED strip `tm led on|off`; [059](tasks/059-led-default-on-connect.md) strip
    off by default on connect (**done**, live-verified); [060](tasks/060-hygiene-broken-pipe-goals-split.md)
    hygiene: SIGPIPE panic on piped output + `goals.rs` split (**done**);
    [061](tasks/061-led-off-prime-after-power-cycle.md) LED `off` no-op after
    power-cycle → prime with `on` (**done**, pending live);
    [062](tasks/062-alacritty-zoom-while-walking.md) Alacritty font zoom while walking,
    `tm alacritty-zoom` (**done**, live-verified; facts: [research 008](research/008-alacritty-ipc-font-size.md));
    [063](tasks/063-keyboard-belt-control.md) `tm toggle` / `tm speed up|down` (**done**, live-verified).
  - External PRs by @huertin03 (finished in review): [066](tasks/066-prebuilt-release-packaging.md)
    release archive ships `install-prebuilt.sh` + archive check; [067](tasks/067-installer-path-guidance.md)
    `tm` alias in `~/.bin`/`~/.local/bin` + PATH hint; [068](tasks/068-json-cli-contract.md)
    `tm stats --json` / `tm samples` JSON export. `065` reserved for PR #4 (start with speed, draft).
- `backlog/` — not-yet-scheduled work. `004`–`011` done (see each file).
- `ideas/` — loose ideas / future directions.
