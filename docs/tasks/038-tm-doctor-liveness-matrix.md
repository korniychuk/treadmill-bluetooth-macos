# 038 — `tm doctor`: матрица живости одним CLI-вызовом

> **Статус: done**  
> **Источник:** [research/003](../research/003-reliability-architecture-review.md) §3.10.10, Phase 0.4  
> **Класс:** observability / MTTR  
> **Приоритет:** high (дешёвый, режет время диагностики *следующего* инцидента)

## Проблема

Инциденты 031–033 диагностировались log archaeology + ad-hoc SQL по `daemon_status` / `raw_samples` / `hr_samples` / launchctl. Четыре источника, ручная сверка «что значит connected».

Нужен **один** read-only CLI-вывод, который печатает liveness matrix из 003 §3.2 — без BLE, без гонки с демоном (тот же контракт, что `tm status` / `tm widget`).

## Команда

```text
tm doctor
```

- `clap` subcommand рядом с `Status` / `Widget`
- **Никогда** не открывает BLE adapter
- Exit 0 всегда при успешном чтении DB (диагностика, не gate); ненулевой только на I/O/DB errors
- Human-readable stdout (не TSV) — для оператора в терминале; при желании позже `--json`

## Что печатать (минимум)

Возраст — секунды wall-clock now − timestamp; `n/a` если поля нет.

```text
daemon
  process:          alive | dead | unknown   (launchctl, same as status)
  heartbeat age:    Xs (updated_at)          WARN if > WATCHDOG_STALE_THRESHOLD_S while alive
  power:            ac_scanning | battery_idle | …

treadmill liveness
  connected flag:   true|false
  last 0x2ACD age:  Xs (from last_speed_ts)  or n/a
  presence:         Walking|…

hr liveness
  hr_connected:     true|false               (note: overloaded — contact-loss clears it)
  last HR frame age:Xs (last_bpm_ts)         or n/a
  last bpm:         N | n/a
  battery:          N% | n/a
  contact (inferred): live|stale|no-link     (derived: connected+fresh → live; connected+stale → stale; !connected → no-link)

zone hold
  config enabled:   true|false               (from config.toml)
  phase snapshot:   hold|ramp|off|…          (daemon_status.zone_hold_phase)
  active flag:      true|false
  mismatch:         WARN if enabled=false but phase not off / active=true
                    WARN if enabled=true, walking, phase off (informational)

control / streaming (best-effort)
  streaming:        not in DB today — print "n/a (in-process only)" OR skip
```

### Источники данных (без schema change в v1)

| Поле | Откуда |
|---|---|
| process alive | `daemon_process_alive()` (уже в main) |
| `updated_at`, `connected`, presence, HR, zone, speeds | `store.daemon_status()` |
| `config enabled` | `zone_hold::load_zone_hold_config()` |
| last 0x2ACD age | `last_speed_ts` (029) — proxy for telemetry age |
| last HR age | `last_bpm_ts` |

**Не** требовать новых колонок в v1. Если позже понадобится явный `last_telemetry_ts` / `streaming` — отдельная задача; doctor должен работать на текущей схеме.

### Liveness matrix (документировать в выводе или `--help`)

Краткий legend:

| Domain | Healthy when |
|---|---|
| loop | process alive + fresh `updated_at` |
| treadmill telemetry | `connected` + fresh `last_speed_ts` (when belt was moving) / or recent disconnect |
| hr link/contact | see note on `hr_connected` overload |
| config intent | `enabled` consistent with phase |

## UX notes

- WARN lines prefix `WARN:` so they grep cleanly.
- Keep &lt; ~25 lines; no essay.
- Reuse formatters from `run_status` where possible (`describe_timestamp`, thresholds) — don't fork magic numbers; share constants (`HR_STALE_THRESHOLD_S`, `WATCHDOG_STALE_THRESHOLD_S`).

## Тесты

- Unit: pure formatter from a fixture `DaemonStatus` + config flags → expected lines / WARN presence (table-driven).
- No BLE. Optional: `:memory:` store upsert + doctor format.

## Acceptance

- [x] `tm doctor` works with daemon down, daemon up+connected, HR worn, HR off, zone on/off mismatch
- [x] No BLE adapter open
- [x] Documents ages for last belt speed sample and last HR sample
- [x] Flags `enabled` vs `zone_hold_phase` / `zone_hold_active` mismatch
- [x] Mentioned in `CLAUDE.md` CLI list + research 003 links this task
- [ ] Smoke: run during a real walk and after treadmill power-off — output matches reality

## Затронутые файлы

- `src/main.rs` — subcommand + `run_doctor`
- optionally thin pure module for format/derive helpers (keeps main thinner)
- `CLAUDE.md` — one-liner in CLI/architecture

## Связанное

- 031–033 — incidents this would have shortened
- 006/009/022/029 — status/widget fields reused
- research 003 Phase 0.4 (was Phase 7 — promoted)
