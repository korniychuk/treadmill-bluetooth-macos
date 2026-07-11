# 055 — Сплит `src/daemon.rs` и `src/zone_hold.rs` на модульные директории

> **Статус: done** (2026-07-11)
> **Источник:** [backlog/007](../backlog/007-split-god-modules.md) (остаток после 049/050) + [backlog/005](../backlog/005-session-state-extract.md) (предусловие Step 1)
> **Класс:** mechanical refactoring, поведение не меняется
> **Приоритет:** medium

## Итог (фактическая разбивка)

Предусловия 049–054 выполнены до старта. После 053 session-state уже жил в
`auto_pause` / `treadmill_link` / `hr_session` / `zone_session` (crate-root),
typed config apply — в `config_apply.rs`, `CentiKmh` — в `speed.rs`. Целевые
таблицы task-doc пересмотрены под эту структуру: **не** переносили уже
извлечённые session-структуры внутрь `daemon/`.

### `src/zone_hold/` (было 1549 raw → директория)

| Файл | Роль | raw / code-ish LOC |
|---|---|---|
| `mod.rs` | module doc, типы/константы/resolve, re-exports | 536 / ~393 🟢 |
| `controller.rs` | `next_speed` / warmup / safety_force_reduce + тесты | 289 / ~229 🟢 |
| `config.rs` | load/parse `[zone_hold]` + тесты | 564 / ~505 🟡 (watch) |
| `cli_config.rs` | `upsert_zone_hold_keys` / `replace_zones` + тесты | 209 / ~160 🟢 |

### `src/daemon/` (было 2437 raw → директория)

| Файл | Роль | raw / code-ish LOC |
|---|---|---|
| `mod.rs` | module doc, re-exports (`run`, `DaemonState`, `WATCHDOG_STALE_THRESHOLD`), shared `SPEED_RESTORE_TIMEOUT` | 111 / ~14 🟢 |
| `run_loop.rs` | `run()`, ScanRecovery, panic fail-fast hook + тесты | 632 / ~476 🟡 |
| `session.rs` | `stream_with_presence` thin wiring + goal toast | 682 / ~495 🟡 |
| `state.rs` | `DaemonState`, `tolerate_db_write` / `persist_daemon_status` + тесты | 276 / ~202 🟢 |
| `watchdog.rs` | `Watchdog` + thresholds + тесты | 215 / ~109 🟢 |
| `commands.rs` | `ControlSource`, control-queue drain | 136 / ~98 🟢 |
| `speed.rs` | pause-resume / default-speed BLE writes + тесты | 198 / ~139 🟢 |
| `hr.rs` | background HR connect (`spawn_hr_connect_attempt`) | 87 / ~53 🟢 |
| `config.rs` | `execute_config_effects` (hot-reload) | 111 / ~92 🟢 |
| `zone_write.rs` | `ZoneWrite` → BLE side-effects | 73 / ~61 🟢 |

Silence-deadline `select!` regression tests переехали к владельцам часов:
`treadmill_link` / `hr_session` (не daemon shell).

**Ни одного файла 🔴 (>1000).** Все ≤750 raw; code-ish ≤505.
Публичные пути `crate::daemon::*` / `crate::zone_hold::*` сохранены re-export'ами.

## Acceptance

- [x] Предусловия 049–053 (+ 051/052/054) в истории до старта
- [x] Поведение verbatim; callers снаружи `daemon*`/`zone_hold*` не менялись (кроме переноса двух silence-тестов в `treadmill_link`/`hr_session`)
- [x] `src/daemon.rs` и `src/zone_hold.rs` удалены
- [x] Нет 🔴 файлов; нет >750 без note
- [x] Re-exports сохраняют внешние пути
- [x] `cargo test` / `cargo clippy --all-targets -- -D warnings` / `cargo fmt` зелёные

## Non-goals (не делалось)

- Event/Effect kernel (backlog 005 Step 2)
- Поведенческие фиксы
- Сплит `scan.rs` / `ftms.rs` и т.п.

## Связанное

- backlog [007](../backlog/007-split-god-modules.md) — **done** через 049/050/055
- задачи 049 (store), 050 (CLI), 051–054 (предусловия)
