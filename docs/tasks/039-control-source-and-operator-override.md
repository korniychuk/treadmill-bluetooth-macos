# 039 — Control Point intent: `control_source` + operator-override window

> **Статус: done**  
> **Источник:** [research/003](../research/003-reliability-architecture-review.md) §3.4, Phase 2  
> **Класс:** `control-intent`  
> **Приоритет:** medium (после 035–038; до новых zone/auto фич)

## Проблема (не «гонка»)

Несколько **логических** писателей в Control Point, **один** `select!` task — concurrent race **нет**:

- Zone Hold (`apply_zone_hold_speed` ~1667)
- CLI queue (`process_control_commands` ~1719 / `execute_control_command` ~1763)
- auto-pause Stop
- resume restore / default-speed (`try_restore_speed` / `try_apply_default_speed`)

Реальный UX-баг: `tm speed 4.0` mid-Hold → zone через ≤`correction_interval` (~20с) **молча** перебьёт. Оператор не видит в логе *кто* владеет скоростью.

## Решение (не arbiter)

YAGNI пятиуровневый priority scheduler. Два куска (~50 LOC):

### 1. `control_source` на каждом write-логе

Стабильный tag / field:

```text
control_source=zone|cli|auto_pause|restore|default_speed
```

Все success/fail/timeout логи Control Point writes:

| Path | source |
|---|---|
| `apply_zone_hold_speed` | `zone` |
| `execute_control_command` (start/stop/speed) | `cli` |
| auto-pause Stop | `auto_pause` |
| `try_restore_speed` | `restore` |
| `try_apply_default_speed` | `default_speed` |

Предпочтительно один thin wrapper:

```rust
async fn write_speed(peripheral, kmh, source: ControlSource) { … log with source … }
async fn write_stop(peripheral, source: ControlSource) { … }
```

чтобы нельзя было забыть tag.

### 2. Operator-override window

После **успешного** CLI `Speed(kmh)` (и, по желанию, CLI `Start` с implicit speed — out of scope unless trivial):

- запомнить `operator_override_until = now + OVERRIDE_WINDOW` (const, e.g. 60s, or config later);
- `zone_hold_tick` / `apply_zone_hold_speed`: if `Instant::now() < operator_override_until` → **skip zone speed writes** (log once per window at DEBUG/INFO: `zone hold: suppressed, operator override active`);
- safety hard-stops (if any future «must stop belt») **не** глотать — сегодня zone safety = speed clamp inside controller, not a separate Stop; auto-pause Stop **не** suppress'ить override window (auto-pause — safety/UX idle, не zone).

Scope v1:

- suppress **only zone speed corrections** after CLI speed;
- do **not** build full priority matrix;
- window not persisted across daemon restart (in-memory OK).

## Тесты

Pure:

- `override_active(now, until) -> bool`
- `should_zone_write(override_until, now) -> bool`

Optional clock-inject on a small gate function used by `zone_hold_tick`.

No BLE.

## Acceptance

- [x] Every Control Point write log line includes `control_source=…`
- [x] After `tm speed X` while Zone Hold active, zone does not write for N seconds
- [x] After window elapses, zone corrections resume
- [x] auto-pause / restore paths still work; tagged in logs
- [ ] Unit tests on override gate
- [ ] No full arbiter / priority queue introduced

## Затронутые файлы

- `src/daemon.rs` — write sites, override Instant, zone gate
- maybe `ControlSource` enum in `control.rs` / `control_command.rs`

## Non-goals

- Multi-writer concurrent scheduler
- Persisted override across restart
- Suppressing auto-pause
- Changing FTMS wire / beep behaviour (030)

## Связанное

- 013 — control queue
- 012/016 — restore/default writers
- 020 — auto-pause Stop
- 027/030/032 — zone writes
- research 003 §3.4, Phase 2 (demoted from «arbiter»)
