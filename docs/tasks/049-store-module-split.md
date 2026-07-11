# 049 — Сплит `src/store.rs` на модульную директорию `src/store/`

> **Статус: planned**
> **Источник:** [backlog/007](../backlog/007-split-god-modules.md) (частичная реализация — только store; `cli/`/`widget.rs` не в scope)
> **Класс:** mechanical refactoring, поведение не меняется
> **Приоритет:** medium

## Контекст

`src/store.rs` — 2021 строк (🔴 по домашнему правилу >1000 LOC): schema
migration, DTO, sample-inserts, HR-агрегаты, activity segments + merge,
daemon_status snapshot, control-command queue и ~635 строк тестов — всё в одном
файле. Backlog 007 предписывает механический сплит на `src/store/` с
однонаправленными файлами. Это **чистый перенос**: ни одна SQL-строка, ни одна
сигнатура, ни один тест по смыслу не меняются. Единственное новое поведение —
**schema snapshot test** (см. ниже).

Ключевой инвариант: **публичный API `crate::store` идентичен** — все внешние
callers (`daemon.rs`, `main.rs`, `activity.rs`, `recompute.rs`,
`recompute_hr.rs`, `default_speed.rs`) **не трогаются вообще**. Это достигается
re-export'ами в `store/mod.rs`.

Текущая внешняя поверхность (проверено grep'ом по `use crate::store` и
`store::`):

- Типы: `Store`, `DailyStats`, `Segment`, `Workout`, `DaemonStatus`,
  `RawDeltas`, `RawSample`, `HrRow`, `HrSummary`, `QueuedControlCommand`.
- Функция: `merge_segments`.
- `pub(crate)`: `Store::open_at`, `Store::all_segments_asc`.
- Все `pub`-методы `Store` (sessions, baseline, credit, inserts, stats,
  workouts, daemon_status, celebrations, control queue).

## Целевая структура

Rust-специфика, на которой держится весь сплит: `impl Store`-блоки можно
раскидать по child-модулям (`impl Store { ... }` в каждом файле), и child-модули
видят приватные item'ы родителя — поле `conn` остаётся приватным в
`store/mod.rs`, а `store/samples.rs` и т.д. свободно пишут `self.conn`.
Никаких `pub(crate) conn` / getters не нужно.

| Файл | Содержимое (перенос из текущего `store.rs`) |
|---|---|
| `store/mod.rs` | module doc (текущий `//!`-блок), `mod`-декларации, **re-exports** (`pub use`) всей поверхности выше; `struct Store { conn }`, `open()`, `open_at()`, `db_path()`; `#[cfg(test)] pub(super) fn memory_store()` — общий тест-хелпер (сейчас в `mod tests`, нужен нескольким файлам) |
| `store/schema.rs` | `migrate()` (все CREATE TABLE / CREATE INDEX), `add_column_if_missing()`, `prune_status_events()` + `STATUS_EVENTS_RETENTION` (prune вызывается из migrate — living рядом), **НОВЫЙ schema snapshot test** |
| `store/samples.rs` | sessions (`start_session`/`end_session` — существуют, чтобы тегировать sample-строки), `insert_raw_sample`, `insert_hr_sample`, `insert_status_event`, `hr_summary_for` (+ `MIN_HR_SAMPLES_FOR_SUMMARY`, `HR_TRIM_FRACTION`, `percentile_95`, `HrSummary`), `raw_distance_m`, `walking_speeds_in_window`, `raw_samples_ordered` (+ `RawSample`), `hr_samples_ordered` / `delete_hr_samples` (+ `HrRow`) |
| `store/activity.rs` | `Segment`, `Workout`, `DailyStats`, `RawDeltas`; `advance_baseline` (+ `delta_since`), `credit_activity`, `today_stats`/`stats_for`/`all_stats`, `all_segments_asc` (+ `segment_from_row`), `workouts_for`, `latest_workout`, `replace_activity_segments`, `merge_segments` (+ `push_or_extend_new`, `parse_end`), goal celebrations (`celebrated_thresholds`/`mark_goal_celebrated` — daily-goal семантика, ближе всего к daily_stats) |
| `store/status.rs` | `DaemonStatus` DTO, `upsert_daemon_status`, `daemon_status()` |
| `store/control_queue.rs` | `QueuedControlCommand`, `CONTROL_COMMAND_RETENTION`, `CONTROL_STATUS_*`, `enqueue_control_command`, `prune_control_commands`, `next_pending_control_command`, `mark_control_command_done`/`_failed`, `control_command_outcome` |

Файл `src/store.rs` удаляется (заменяется директорией `src/store/`).

### Тесты

Существующий `mod tests` разрезается по тому же принципу — каждый тест едет в
файл, чей код он проверяет:

- `delta_since`-тесты, `credit_activity_*`, `merge_segments_*`,
  `goal_celebrations_*` → `activity.rs`;
- `hr_summary_*`, `prune_status_events_*` (проверяет prune — но prune живёт в
  `schema.rs`, тест едет туда же), `insert_hr` хелпер → `samples.rs` /
  `schema.rs` соответственно;
- `daemon_status_upsert_roundtrips` → `status.rs`;
- `control_command_*`, `enqueue_prunes_*` → `control_queue.rs`.

Тесты, лезущие в `store.conn` напрямую (poison-row inject и т.п.), продолжают
работать: child-модули видят приватное поле. Хелперы `memory_store()` (общий) —
в `mod.rs` под `#[cfg(test)]`, `credit`/`seg` (только activity) — в
`activity.rs`, `insert_hr` (только samples/hr) — рядом со своими тестами.

### Schema snapshot test (новый, `schema.rs`)

Единственная **добавляемая** функциональность. Смысл: дрейф схемы (кто-то
добавил колонку/таблицу/индекс и забыл про migration-путь или про этот тест)
должен ронять `cargo test` с читаемым diff'ом.

Реализация — без новых зависимостей, обычный `assert_eq!` строк:

1. Открыть `memory_store()` (прогоняет `migrate()` на пустой БД).
2. Сдампить фактическую схему детерминированной строкой:
   `SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name`,
   затем для каждой таблицы `PRAGMA table_info(<t>)` → строки вида
   `table.column:TYPE:notnull:default` в порядке `cid`. Индексы туда же:
   `type='index'` из `sqlite_master` (только именованные `idx_*`, autoindex'ы
   SQLite пропустить), отсортированно.
3. Сравнить с эталонной константой (inline `&str` в тесте, multi-line). Эталон
   пишется один раз с фактического дампа и дальше правится **сознательно**
   вместе с каждой миграцией.

Формат дампа — на усмотрение исполнителя, требования: детерминирован,
покрывает **все** таблицы + колонки (+ типы/NOT NULL) + именованные индексы,
diff при падении читается человеком.

## План

1. `mkdir src/store/`; создать `mod.rs` с module doc, `Store`/`open`/`open_at`/
   `db_path` и пустыми `mod`-декларациями — крупный перенос по одному файлу за
   коммит (schema → samples → activity → status → control_queue), `store.rs`
   тает по мере переноса, в конце удаляется.
2. Каждый шаг: перенос кода + его тестов verbatim (комментарии сохранить как
   есть), `pub use` в `mod.rs`, `cargo test` зелёный.
3. Отдельным коммитом — schema snapshot test.
4. Финал: `cargo test`, `cargo clippy` (ноль новых warning'ов), `cargo fmt`.

Коммиты мелкие и осмысленные, без AI-футеров.

## Acceptance

- [ ] Поведение не меняется: ни одна SQL-строка/сигнатура/логика не тронута,
      только перенос (плюс snapshot test).
- [ ] Публичный API `crate::store` идентичен: `daemon.rs`, `main.rs`,
      `activity.rs`, `recompute.rs`, `recompute_hr.rs`, `default_speed.rs`
      **не изменены ни на байт** (`git diff` их не показывает).
- [ ] `src/store.rs` удалён, вместо него `src/store/{mod,schema,samples,activity,status,control_queue}.rs`.
- [ ] Каждый новый файл ≤500 LOC (non-blank/non-comment); допустимый максимум
      ≤750 с пояснением в отчёте, 🔴 >1000 — нет.
- [ ] `cargo test` зелёный (все существующие тесты пережили переезд), `cargo
      clippy` без новых warning'ов.
- [ ] Schema snapshot test существует, покрывает все таблицы/колонки/индексы и
      падает при дрейфе (проверить, временно закомментировав один
      `add_column_if_missing`, — и вернуть обратно).

## Explicitly YAGNI (из backlog 007)

- **НИКАКИХ** versioned migrations / `schema_version`-таблицы / down
  migrations. Single-user sole-writer SQLite + `recompute-*` уже восстанавливают
  truth; snapshot test + `add_column_if_missing` — достаточно.
- Не в scope: `cli/`-сплит `main.rs`, `widget.rs`, backlog 005 (Event/Effect
  kernel), любые изменения схемы или запросов.

## Затронутые файлы

- `src/store.rs` → `src/store/*.rs` (только это; остальной `src/` — read-only)

## Связанное

- backlog [007](../backlog/007-split-god-modules.md), research/003 Phase 5
- задачи 014/015/025/034/044/046 — происхождение переносимых кусков
