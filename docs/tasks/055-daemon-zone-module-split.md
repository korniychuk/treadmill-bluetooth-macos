# 055 — Сплит `src/daemon.rs` и `src/zone_hold.rs` на модульные директории

> **Статус: planned**
> **Источник:** [backlog/007](../backlog/007-split-god-modules.md) (остаток после 049/050) + [backlog/005](../backlog/005-session-state-extract.md) (предусловие Step 1)
> **Класс:** mechanical refactoring, поведение не меняется
> **Приоритет:** medium

## Предусловия (ЖЁСТКИЕ — без них не начинать)

Backlog 007 явно предупреждает: механический сплит `daemon.rs` **до** state extract
— это «переносим богов» (см. non-goals backlog 005). Эта задача идёт **последней**
в серии и стартует только когда в `main` смержены **все** параллельные работы:

| Задача | Что меняет в наших файлах | Проверка |
|---|---|---|
| 049 store split | `src/store.rs` → `src/store/` (импорты `crate::store::*` не меняются) | `test -d src/store` |
| 050 cli split | `main.rs` → `commands/` + `widget.rs`; callers `zone_hold::*`/`daemon::*` переезжают туда | `test -d src/commands` |
| 051 ble scan auto-recover | новый код recovery в `daemon.rs` (реакция на wedged scan, backlog 009) | grep по recovery-функциям в daemon |
| 052 typed config apply | typed apply в `daemon.rs`/`zone_hold.rs` (backlog 008) — меняет форму config-reload arm | `git log --oneline --grep 052` |
| 053 session state extract | ~20 `mut` locals `stream_with_presence` → структуры `HrSession` / `ZoneSession` / `AutoPause` с методами | `grep -n "struct HrSession" src/daemon.rs` |

Перед стартом: `git log --oneline -20` + `git branch -a` — убедиться, что ни одна
из веток 049–053 не висит несмерженной. Если хоть одна в полёте — **стоп**, задача
ждёт. Rebase этого сплита поверх их конфликтов дороже, чем подождать.

Из-за 051–053 код к моменту исполнения **сдвинется и частично переименуется**.
Поэтому весь план ниже адресует **имена функций/типов** (состояние на commit
`2e2bb1a`), а не номера строк. Правило маппинга для нового/переименованного
кода: единица переноса — «функция/тип + его тесты + его константы», целевой файл
выбирается по роли (см. таблицы). Всё, что 051 добавит про scan-recovery → в
`daemon/run_loop.rs`; всё, что 053 извлечёт в `HrSession`/`ZoneSession`/`AutoPause`
→ в `daemon/hr.rs`/`daemon/zone_tick.rs`/`daemon/auto_pause.rs` соответственно;
typed-apply код 052 → `daemon/config.rs` (daemon-сторона) и `zone_hold/config.rs`
(parsing-сторона).

## Контекст

`src/daemon.rs` — ~2750 строк (🔴 >1000 LOC), `src/zone_hold.rs` — ~1480 строк
(🔴). После 051–053 оба скорее вырастут, чем сожмутся. Это **чистый перенос**:
ни одна сигнатура (кроме visibility `pub(super)`/`pub(crate)` там, где перенос
этого требует), ни один log-message, ни один тест по смыслу не меняются.

Rust-механика, на которой держится сплит (та же, что в 049):

1. **Child-модули видят приватные item'ы родителя.** `impl`-блоки и функции можно
   раскидать по файлам директории; общие константы/типы остаются в `mod.rs` или
   получают `pub(super)` — никаких getter'ов не нужно.
2. **Re-export'ы в `mod.rs` сохраняют внешние пути.** `crate::daemon::run`,
   `crate::zone_hold::load_zone_hold_config` и т.д. остаются валидными — внешние
   callers (после 050 это `commands/*.rs`, `widget.rs` и остальной `src/`) **не
   меняются ни на байт**.

### Внешняя поверхность (проверено grep'ом на `2e2bb1a`, перепроверить после мержей)

- `crate::daemon`: `pub async fn run`, `pub(crate) const WATCHDOG_STALE_THRESHOLD`
  (используется в `main.rs`/будущем `commands/`). Всё остальное — приватное.
  (В doc-comment `main.rs` упомянут `daemon::CONTROL_EXEC_TIMEOUT` — если 051/052
  введут такую константу публично, добавить в re-exports.)
- `crate::zone_hold` (широкая, callers — `daemon.rs` + zone-CLI): типы
  `ZoneHoldConfig`, `ResolvedZone`, `ZoneDef`, `ZoneBounds`, `ZoneSelector`,
  `Method`, `Tracking`, `ZonePosition`, `ControllerParams`; функции
  `load_zone_hold_config`, `find_zone`, `slugify`, `default_zones`,
  `resolve_zone_bpm`, `hrmax_tanaka`, `safety_cap_bpm`, `next_speed`,
  `warmup_target_speed`, `safety_force_reduce_target`, `classify_position`,
  `config_path`, `upsert_zone_hold_keys`, `replace_zones`; константы
  `DEFAULT_TARGET_ZONE`, `DEFAULT_MIN_SPEED_KMH`, `DEFAULT_MAX_SPEED_KMH`,
  `MIN_SPEED_CHANGE_KMH` и прочие `DEFAULT_*`.

## Целевая структура: `src/zone_hold/` (меньший — идёт первым)

`src/zone_hold.rs` удаляется, вместо него:

| Файл | Содержимое (перенос по именам) | ~LOC (код+тесты) |
|---|---|---|
| `zone_hold/mod.rs` | module doc (текущий `//!`), `mod`-декларации, **re-exports** всей поверхности выше; доменные типы: `Method`, `Tracking`, `ZoneBounds`, `ZoneDef`, `ZoneSelector`, `ResolvedZone`, `ZonePosition`; `ZoneHoldConfig` + `impl` (`disabled_default`, `hrmax`, `resolve_target_zone`, `safety_cap_bpm`); чистые resolve-функции: `slugify`, `default_zones`, `find_zone`, `hrmax_tanaka`, `resolve_zone_bpm`, `safety_cap_bpm` (fn), `classify_position`; константы `DEFAULT_*`; тесты: `hrmax_tanaka_*`, `resolve_zone_bpm_*`, `resolve_target_zone_*`, `classify_position_*`, `default_zones_have_stable_slug_ids`, `safety_cap_bpm_*` | ~450 🟢/🟡 |
| `zone_hold/controller.rs` | closed-loop математика: `MIN_SPEED_CHANGE_KMH` (константа живёт с кодом, который её осмысляет; re-export из mod), `ControllerParams`, `next_speed`, `warmup_target_speed`, `safety_force_reduce_target`; тесты: `band_mode_*`, `center_mode_*`, `warmup_ramps_*`, `safety_force_reduce_*`, `band_mode_ignores_float_precision_gap_at_the_pin` + хелперы `band_params`/`center_params` | ~350 🟢 |
| `zone_hold/config.rs` | загрузка/парсинг: `config_path`, `load_zone_hold_config`, `parse_zone_hold_config`, `parse_zone_def`, `ABSOLUTE_BPM_MIN/MAX`, хелперы `bool_or`/`positive_float_or`/`non_negative_int_or`/`positive_int_or`; **сюда же** — что 052 сделает с parsing-стороной (typed apply); тесты: `parse_zone_hold_*`, `parse_zone_def_*`, `warmup_minutes_zero_is_allowed`, `load_zone_hold_config_absent_file_*` | ~500 🟡 (watch) |
| `zone_hold/cli_config.rs` | запись конфига для `tm zone` CLI: `upsert_zone_hold_keys`, `escape_toml_string`, `replace_zones`; тесты: `upsert_zone_hold_keys_appends_new_section`, `replace_zones_round_trips_through_parse` | ~200 🟢 |

Если `find_zone`+resolve-блок раздует `mod.rs` за ~500 — выделить
`zone_hold/zones.rs` (`slugify`, `default_zones`, `find_zone`, `resolve_zone_bpm`
+ их тесты), `mod.rs` оставить типам и re-export'ам. Решение на месте по факту LOC.

## Целевая структура: `src/daemon/`

`src/daemon.rs` удаляется, вместо него:

| Файл | Содержимое (перенос по именам) | ~LOC (код+тесты) |
|---|---|---|
| `daemon/mod.rs` | module doc (весь текущий `//!`-блок с историей инцидентов — сохранить verbatim), `mod`-декларации, re-exports (`pub use run_loop::run`, `pub(crate) use watchdog::WATCHDOG_STALE_THRESHOLD`, + что понадобится после 051/052); кросс-доменные константы, не прилипшие ни к одному файлу: `RETRY_DELAY`, `PERSIST_TICK_INTERVAL` | ~150 🟢 |
| `daemon/run_loop.rs` | `run()` целиком: power-idle цикл, scan/connect `select!`, teardown-последовательность после `stream_with_presence`; **сюда же** — весь scan-recovery код 051 (backlog 009); если 051 оформит recovery отдельными функциями — они едут сюда как есть | ~300–400 🟢 (после 051 — пересчитать) |
| `daemon/session.rs` | `stream_with_presence` (сам `select!`-event-loop — после 053 это thin wiring: arm → метод структуры → side-effect), `NOTIFICATION_TIMEOUT`, `celebrate_reached_goals`; session-scoped локалы, которые 053 НЕ извлёк (например `speed_history`/`last_walking_speed`/`pre_pause_speed`, если они не вошли в структуры); тесты: `telemetry_deadline_fires_despite_a_faster_sibling_arm`, `hr_silence_deadline_fires_despite_a_faster_sibling_arm` | ~450–550 🟡 (watch; см. риск ниже) |
| `daemon/hr.rs` | HR-линк: константы `HR_NOTIFICATION_TIMEOUT`, `HR_RECONNECT_INTERVAL`, `HR_CONNECT_ATTEMPT_DEADLINE`, `HR_BATTERY_*`, `ZH_BPM_MAX_AGE`; `HrNotificationStream`, `HrConnectOutcome`, `spawn_hr_connect_attempt`, `hr_battery_poll_interval`, `clear_hr_link_state`, `zh_bpm_if_fresh` (кормит zone, но по данным — HR-freshness; допустимо и в `zone_tick.rs`, решить по тому, куда 053 положит поле); **структура `HrSession` из 053 + её методы**; тесты: `hr_battery_poll_interval_*`, `hr_connect_latch_stale_*`, `clear_hr_link_state_*`, `zh_bpm_if_fresh_*` + unit-тесты методов `HrSession` | ~400 🟢/🟡 |
| `daemon/speed.rs` | восстановление/дефолт скорости: константы `SPEED_RESTORE_TIMEOUT` (`pub(super)` — переиспользуют commands/zone/auto_pause), `SPEED_RESTORE_EPSILON_KMH`, `SPEED_CRUISE_DECEL_SKIP`, `SPEED_CRUISE_FLOOR_KMH`, `DEFAULT_SPEED_APPLY_CEILING_KMH`, `SPEED_HISTORY_RETENTION`; `cruising_speed`, `speed_restore_target`, `try_restore_speed`, `restore_speed` (`pub(super)` — bounded-write примитив всего демона), `try_apply_default_speed`; тесты: `cruising_speed_*`, `speed_restore_target_*` | ~350 🟢 |
| `daemon/auto_pause.rs` | `AUTO_PAUSE_RETRY_COOLDOWN`, `away_duration`, `auto_pause_due`; **структура `AutoPause` из 053 + методы**; сам auto-pause блок из telemetry-arm (`execute_control_command(Stop, AutoPause)` round-trip), если 053 вынесет его в функцию; тесты: `auto_pause_due_*`, `away_duration_adds_the_confirmation_window` | ~200 🟢 |
| `daemon/zone_tick.rs` | daemon-сторона Zone Hold: `ZONE_HOLD_SAFETY_COOLDOWN`, `ZONE_HOLD_HARD_STOP_PERCENT`; `ZoneHoldPhase` + `label()`, `should_run_zone_hold`, `disengage_zone_hold`, `zone_hold_on_transition`, `zone_hold_tick`, `zh_persist_snapshot`, `apply_zone_hold_speed`; **структура `ZoneSession` из 053 + методы**; тесты: `should_run_zone_hold_*`, `disengage_zone_hold_*`, `zone_hold_on_transition_*`, `zone_hold_tick_skips_when_measured_speed_is_none` | ~500 🟡 (watch) |
| `daemon/commands.rs` | очередь управления: `CONTROL_POLL_INTERVAL`, `OPERATOR_OVERRIDE_WINDOW`, `ControlSource` + `as_str` (`pub(super)` — логируют speed/zone/auto_pause), `operator_override_active`, `process_control_commands`, `execute_control_command` (`pub(super)`); тесты: `operator_override_active_within_window` | ~200 🟢 |
| `daemon/config.rs` | `LiveConfig`; config-reload обработчик (`config_tick` arm телом — после 052 это typed apply; включая mid-session re-engage/disengage Zone Hold из reload-ветки, который дальше зовёт `zone_tick::*`) | ~150–250 🟢 (после 052 — пересчитать) |
| `daemon/state.rs` | `DaemonState` + `impl` (`new`, `set_config`, `set_power_mode`, `persist`), `power_mode_label`; тесты: `daemon_state_persist_roundtrips_and_touches_watchdog`, `set_power_mode_only_bumps_since_on_actual_change` (+ хелпер `memory_store` под `#[cfg(test)]` — нужен и другим файлам, положить в `mod.rs` `pub(super)`, как в 049) | ~250 🟢 |
| `daemon/watchdog.rs` | `Watchdog` + `impl` целиком, `WATCHDOG_STALE_THRESHOLD` (re-export через mod), `STREAMING_STALE_THRESHOLD`, `WATCHDOG_POLL_INTERVAL`, `WATCHDOG_EXIT_CODE`; тесты: `watchdog_uses_tighter_threshold_while_streaming`, `streaming_watchdog_ignores_non_telemetry_touches` | ~250 🟢 |

Ориентиры LOC даны от `2e2bb1a` + грубая поправка на 051–053; исполнитель
пересчитывает по факту (`cloc`/`grep -cve '^\s*(//|$)'`) и балансирует в рамках
правила: цель ≤500, допустимо ≤750 с пояснением, 🔴 >1000 — запрещено.

### Правила переноса

- **Verbatim**: код, doc-comments (включая русские комментарии с номерами задач
  — это архив решений) и тесты переезжают без правок. Единственные допустимые
  изменения: `use`-строки, visibility (`fn` → `pub(super) fn` где нужен
  cross-file доступ), и распил `mod tests` по файлам.
- **Константа живёт с кодом, который её осмысляет** (как `MIN_SPEED_CHANGE_KMH`
  в controller): длинный doc-comment константы — часть переноса.
- **Никакого попутного рефакторинга**: не переименовывать, не «улучшать»
  сигнатуры, не снимать `#[allow(clippy::too_many_arguments)]`, даже если после
  053 он стал лишним — это отдельный однострочный follow-up, не эта задача.
- **Watch-файлы** (`session.rs`, `zone_tick.rs`, `zone_hold/config.rs`): если по
  факту вылезают за ~750 — делить дальше по той же ролевой логике (например,
  `session.rs` → выделить telemetry-arm тело в `telemetry.rs`), а не просить
  exception.

## Порядок коммитов

Каждый шаг: перенос + его тесты, `cargo test` зелёный, коммит. Мелкие коммиты
без AI-футеров, с явным pathspec.

1. `refactor(zone_hold): extract controller.rs` — создать `src/zone_hold/`,
   `mod.rs` с re-exports, перенести controller.
2. `refactor(zone_hold): extract config.rs` (load/parse).
3. `refactor(zone_hold): extract cli_config.rs`; остаток `zone_hold.rs` → `mod.rs`,
   файл удалить.
4. `refactor(daemon): scaffold daemon/ with mod.rs` — директория, module doc,
   re-exports; `daemon.rs` пока жив.
5. `refactor(daemon): extract watchdog.rs`.
6. `refactor(daemon): extract state.rs`.
7. `refactor(daemon): extract commands.rs`.
8. `refactor(daemon): extract speed.rs`.
9. `refactor(daemon): extract auto_pause.rs`.
10. `refactor(daemon): extract hr.rs`.
11. `refactor(daemon): extract zone_tick.rs`.
12. `refactor(daemon): extract config.rs` (LiveConfig + reload).
13. `refactor(daemon): move run/session, drop daemon.rs` — `run_loop.rs` +
    `session.rs`, остаток растворяется, `src/daemon.rs` удалён.
14. Финал: `cargo fmt`, при необходимости `style: cargo fmt`.

Порядок внутри daemon — от листьев (watchdog/state ни от кого не зависят) к
корню (session/run зависят от всех), чтобы каждый промежуточный коммит
компилировался без forward-заглушек.

## Acceptance (из backlog 007 + домашнее правило)

- [ ] Предусловия выполнены: 049–053 в `main` до первого коммита этой задачи.
- [ ] Поведение не меняется: логика/сигнатуры/log-messages verbatim; `git diff`
      вне `src/daemon*`/`src/zone_hold*` пуст (кроме, возможно, `main.rs`/
      `commands/` — и только если 051/052 ввели новые публичные имена, которые
      re-export обязан сохранить).
- [ ] `src/daemon.rs` и `src/zone_hold.rs` удалены; вместо них директории по
      таблицам выше.
- [ ] **Ни одного файла в 🔴 (>1000 non-blank/non-comment LOC)** — без
      исключений; ни одного >750 без exception-note в отчёте.
- [ ] Внешние пути `crate::daemon::*` / `crate::zone_hold::*` сохранены
      re-export'ами; callers не изменены.
- [ ] `cargo test` зелёный (все существующие тесты пережили переезд, ни один не
      удалён/ослаблен), `cargo clippy` без новых warning'ов, `cargo fmt` чист.

## Non-goals / YAGNI

- Backlog 005 Step 2 (полный `Event`/`Effect` kernel) — нет.
- Поведенческие фиксы, найденные по дороге — в backlog, не в этот diff.
- Сплит чего-либо ещё (`scan.rs`, `ftms.rs` и т.д.) — вне scope.
- Переименование доменных понятий «под новую структуру» — нет.

## Риски

1. **Гонка с 051–053 (главный).** План писан по `2e2bb1a`; после мержей имена
   и границы кода сдвинутся. Митигация: маппинг по ролям (см. «Предусловия»),
   перед стартом — свежий проход по фактическому `daemon.rs` и корректировка
   таблиц прямо в этом доке (обновить док коммитом до начала переноса).
2. **`session.rs` останется толстым**, если 053 извлечёт меньше состояния, чем
   планирует (telemetry-arm — самый переплетённый кусок: presence match +
   restore/default speed + zone engage). Митигация: watch-правило выше — делить
   telemetry-arm тело дальше, а не просить exception.
3. **Скрытые связи через приватные item'ы** (`restore_speed`, `SPEED_RESTORE_TIMEOUT`,
   `ControlSource`, `execute_control_command` используются из 4+ мест).
   Митигация: они получают `pub(super)` в своём файле-владельце (speed/commands),
   остальные ходят через `super::` — компилятор ловит все пропуски.
4. **Тест-хелперы общего пользования** (`memory_store`): как в 049 — `#[cfg(test)]
   pub(super)` в `mod.rs`.
5. **Doc-comment ссылки** (`[`SPEED_RESTORE_TIMEOUT`]` intra-doc links) сломаются
   при переезде через границы файлов. Митигация: `cargo doc --no-deps` или clippy
   предупредит; чинить путём (`[`super::speed::SPEED_RESTORE_TIMEOUT`]`), не
   удалением ссылки.

## Затронутые файлы

- `src/daemon.rs` → `src/daemon/{mod,run_loop,session,hr,speed,auto_pause,zone_tick,commands,config,state,watchdog}.rs`
- `src/zone_hold.rs` → `src/zone_hold/{mod,controller,config,cli_config}.rs`
- Остальной `src/` — read-only (модуло re-export-совместимость, см. Acceptance).
- После завершения: обновить `CLAUDE.md` (раздел «Архитектура» — пути модулей) и
  backlog [007](../backlog/007-split-god-modules.md) (закрыть/отметить остаток).

## Связанное

- backlog [007](../backlog/007-split-god-modules.md), [005](../backlog/005-session-state-extract.md), [008](../backlog/008-typed-config-apply.md), [009](../backlog/009-btleplug-panic-wedges-ble-scan.md)
- задачи [049](049-store-module-split.md) (образец механики сплита), [050](050-cli-module-split.md), 051, 052, 053 (предусловия)
- research/003 Phase 5
