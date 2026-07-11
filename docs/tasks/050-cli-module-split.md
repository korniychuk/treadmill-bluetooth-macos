# 050 — CLI module split: разгрузить `src/main.rs` (частично backlog 007)

> **Статус: planned**
> **Источник:** [backlog/007](../backlog/007-split-god-modules.md) (строки `cli/`/`commands/` и `widget.rs`)
> **Класс:** mechanical refactor, **без изменения поведения**
> **Приоритет:** medium — `main.rs` в 🔴-зоне (2264 строк), дальше расти некуда

## Контекст

`src/main.rs` — god-module: помимо точки входа он содержит все CLI-хендлеры
(stats/status/doctor, весь `tm zone` с интерактивными промптами, control-очередь,
widget-форматтеры) и их тесты. Backlog 007 требует механического сплита. Части
про `store/` зависят от backlog 005 (state extract) и в эту задачу **не входят**
— здесь только CLI-слой + `widget.rs`, у которых зависимостей нет: это чистый
перенос кода из `main.rs` в новые модули.

Никакая логика вне `main.rs` не меняется: `store.rs`, `daemon.rs`, `zone_hold.rs`,
`goals.rs`, `scan.rs` и остальные существующие модули **не трогать**.

## Целевая структура

`src/main.rs` остаётся тонкой точкой входа: mod-декларации, clap-типы
(`Cli`/`Commands`/`ZoneAction`/`SpeedWidgetAction` — их `///`-доки это
load-bearing help-текст, они остаются как есть), `main()` с диспетчеризацией
и `init_tracing()`. Всё остальное разъезжается:

| Новый файл | Содержимое (функции из текущего `main.rs`) |
|---|---|
| `src/commands/mod.rs` | только `mod`-декларации + нужные `pub(crate) use` re-exports |
| `src/commands/common.rs` | shared-хелперы, нужные ≥2 командам: `WATCHDOG_STALE_THRESHOLD_S`, `HR_STALE_THRESHOLD_S`, `daemon_process_alive`, `daemon_status_fresh`, `refuse_if_daemon_live`, `zone_hold_config_path`, `describe_timestamp`, `format_local_time`, `humanize_ago`, `fmt_duration` |
| `src/commands/stats.rs` | `run_stats`, `print_day`, `print_workout_line` (pub(crate) — нужен и `status`), `fmt_hr_summary`, `day_hr_summary`, `day_bounds_rfc3339`, `workout_raw`, `raw_span_s`, `raw_hint`, `run_default_speed` (читает те же stats-агрегаты) |
| `src/commands/status.rs` | `run_status`, `run_doctor`, `format_doctor_report`, `age_secs_rfc3339`, `format_goal_list`, `format_secs_short` + doctor-тесты |
| `src/commands/zone.rs` | `run_zone`, `print_zone_status`, `zone_on`, `zone_onboarding_prompt`, `zone_limits`, `zone_target`, `zone_list`, `zone_add`, `zone_edit`, `zone_remove`, `zone_mode`, `parse_zone_selector`, `set_zone_hold_key` и все `prompt_*`-хелперы (`prompt_age`, `prompt_optional_resting_hr`, `prompt_line`, `prompt_f32`, `prompt_u16`, `prompt_zone_bounds`, `prompt_optional_max_speed`, `prompt_zone_id`) |
| `src/commands/belt.rs` | control-путь: `run_control`, `daemon_holds_link`, `enqueue_and_wait`, `describe_control_success`, `CONTROL_POLL_TIMEOUT`, `CONTROL_POLL_INTERVAL`, enum `Command`, `run_command`. Имя `belt`, а не `control` — чтобы не путаться с существующим `src/control.rs` (FTMS Control Point транспорт) |
| `src/commands/diag.rs` | одноразовые диагностические/reverse-engineering обёртки: `run_connect`, `run_hr`, `run_discover`, `run_sniff`, `run_daemon`, fitshow-probe/set-обёртки (сейчас инлайн в `match` — вынести как `run_fitshow_probe`/`run_fitshow_set`), `run_notify_test` |
| `src/widget.rs` | TSV-контракт виджета: `run_widget`, `widget_state`, `widget_hr_field`, `widget_hr_zone_field`, `widget_speed_field`, `widget_speed_value`, `format_speed_kmh`, `workout_is_live`, `widget_status_stale` + toggle `run_speed_widget`/`set_show_speed` (задача 029 — управляет именно виджетом) + widget-тесты |

Куда класть `ZoneAction`/`SpeedWidgetAction` — на усмотрение исполнителя:
либо остаются в `main.rs` рядом с `Commands` (проще), либо уезжают к своим
хендлерам; главное — лимиты LOC ниже.

### Golden test на TSV-контракт (backlog 007, Acceptance)

Сейчас `run_widget` собирает строку инлайн-`println!`. Вынести **чистый**
форматтер строки (например, `fn widget_line(...) -> String` или структура
с полями + `to_tsv()`), чтобы `run_widget` печатал её результат, и добавить
golden-тест:

- `assert_eq!(line.split('\t').count(), 12)` — число полей зафиксировано;
- порядок полей зафиксирован по документации `run_widget`:
  `state, workout_count, cur_walking_s, cur_steps, cur_distance_m,
  day_walking_s, day_steps, day_distance_m, hr_bpm, hr_battery_pct,
  hr_zone, speed_kmh`;
- пустые опциональные поля остаются пустыми строками (стабильный field
  count — контракт для tmux-консьюмера).

Вывод `tm widget` при этом **бит-в-бит** прежний.

## План

1. Создать `src/commands/` + `src/widget.rs`, перенести функции по таблице.
   Перенос **verbatim**: тела функций, комментарии (включая русские
   `задача NNN`-отсылки) и строки вывода не редактировать. Меняются только
   `use`-пути и видимость (`pub(crate)`/`pub(super)` по необходимости).
2. Тесты из `mod tests` в `main.rs` переезжают к своим функциям
   (doctor → `commands/status.rs`, widget/speed/workout_is_live → `widget.rs`).
3. Добавить golden-тест TSV (см. выше) — единственный **новый** код задачи.
4. `main.rs`: оставить mod-декларации, clap-типы, `main()` (диспетчеризация
   через вызовы `commands::*`/`widget::*`), `init_tracing`.
5. `cargo fmt`, `cargo test`, `cargo clippy` — зелёные.

Коммиты мелкие и осмысленные (например: skeleton `commands/`, перенос
stats/status, перенос zone, перенос belt/diag, выделение widget.rs + golden
test, финальная зачистка main.rs).

## Acceptance

- [ ] Поведение и текстовый вывод **каждой** CLI-команды не меняются ни на
      байт (включая help-текст `--help`, форматирование `stats`/`status`/
      `doctor`/`widget`, интерактивные промпты `zone`).
- [ ] Каждый новый файл ≤500 LOC (non-blank/non-comment); жёсткий потолок 750.
- [ ] `main.rs` ≤300 LOC (non-blank/non-comment; clap `///`-help не считается —
      это комментарии).
- [ ] Golden-тест: 12 TSV-полей + зафиксированный порядок.
- [ ] `cargo test` и `cargo clippy` зелёные; `cargo fmt` применён.
- [ ] Diff затрагивает **только** `src/main.rs`, новые `src/commands/*`,
      `src/widget.rs`. `store.rs`/`daemon.rs`/`zone_hold.rs`/`goals.rs`/
      `scan.rs`/прочие существующие модули — нетронуты.
- [ ] Комментарии в коде — только английские **новые**; существующие
      перенесены как есть.

## Non-goals

- Сплит `store.rs` / `daemon.rs` (остальная часть backlog 007 — ждёт 005).
- Schema snapshot test (строка `store/schema.rs` из backlog 007).
- Любые поведенческие изменения, переименования CLI-команд, рефакторинг
  логики хендлеров сверх переноса.

## Связанное

- backlog [007](../backlog/007-split-god-modules.md) — источник; после этой
  задачи в нём остаются только `store/*`-строки
- 009 / 025 / 026 / 027 / 029 — история TSV-полей виджета
- 038 — doctor; 043 — единый источник staleness-порогов
  (`WATCHDOG_STALE_THRESHOLD_S` остаётся производным от
  `daemon::WATCHDOG_STALE_THRESHOLD`, не литералом)
