# 004 — Независимый reliability review (проверка 003 свежим взглядом)

> **Дата:** 2026-07-10
> **Тип:** research / independent verification
> **Скоуп:** весь репозиторий; проверка выводов [research/003](003-reliability-architecture-review.md) + собственные находки
> **Метод:** четыре независимых ревьюера (daemon.rs; чистые модули+конфиг; CLI/store/транспорт; тесты+git-история) **без доступа к 003**, чтобы исключить анкоринг; затем cross-check их находок против 003 и ручная верификация всех load-bearing утверждений по коду.

---

## 1. Вердикт по 003: диагноз подтверждён, две поправки, семь новых находок

**Подтверждено независимо** (ревьюеры пришли к тем же выводам, не видя 003):

- Мета-диагноз «баги живут в склейке, не в чистых модулях» — подтверждён **данными**: из 13 `fix(*)`-коммитов последнего окна **8 (61.5%) трогали `daemon.rs`**, и ровно 1 — чистый модуль в одиночку (`e3f5b6a`, zone_hold). При этом 150 юнит-тестов зелёные, clippy `-D warnings` чистый: тестами покрыто то, что и так не ломается.
- **HR relative timeout** (задача 035) — найден независимо как находка №1 по severity (см. §2.1 — severity выше, чем в 003).
- Матрица живости §3.2, atomic config write (037), `tm doctor` (038), Karvonen WARN (040), control intent вместо арбитра (039), «state structs, не полный Event/Effect» (backlog 005) — всё подтверждено.
- «Что сделано хорошо» — подтверждено и расширено: dual-watchdog split, bounded BLE awaits, replay=prod engine, время инъекцией, `ZonePosition::wire()` (осознанная развязка wire-формата от `Debug`), poison-pill tolerance в control queue, `disengage_zone_hold` как единая точка сброса.

**Поправки к 003** — §2. **Новые находки** (в 003 отсутствуют) — §3.

---

## 2. Поправки к 003

### 2.1. Задача 035: severity ВЫШЕ, чем оценивал 003

003 (§3.2) описывает последствие зависшего HR-таймаута как «reconnect не запустится, виджет спасёт `HR_STALE_THRESHOLD_S`». Проверка по коду показывает хуже:

```rust
// daemon.rs:903 — вход Zone Hold
let zh_bpm = state.hr_connected.then_some(state.last_bpm).flatten()...
```

`zh_bpm` гейтится **только** на `hr_connected` — **никакой проверки свежести `last_bpm_ts`**. 15-секундный стейл-порог существует только в виджете (`main.rs`), не в контуре управления. Значит при «линк жив, notify молчит» (partial GATT death — silence-путь, который relative timeout никогда не закрывает):

1. `hr_connected` залипает `true`, `last_bpm` заморожен;
2. `ContactTracker` (033) **не спасает** — он работает per-frame, а кадров нет;
3. Zone Hold продолжает кормить контроллер мёртвым bpm; в `Band`-режиме замороженный под-зонный bpm даёт `+max_step` каждые `correction_interval` — **лента разгоняется до `effective_max_speed_kmh` по мёртвому датчику** без человека у пульта.

**Следствие для 035:** фикс silence-arm обязателен (как в 003), но добавляется **defense-in-depth**: freshness-гейт на `zh_bpm` (bpm старше N секунд → `None`), чтобы контур управления не зависел от единственного детектора живости. Задача 035 обновлена.

### 2.2. Задача 036: премиза неверна — путь сегодня недостижим

003 §3.5 / задача 036 утверждают: «MORE_DATA frame (`speed=None`) на transition может стартовать Ramp с 0». **Оба независимых ревьюера** (и ручная проверка) опровергают:

```rust
// presence.rs:75-93
let next = match speed_kmh {
    ...
    None => self.state,   // no speed → state unchanged
};
if next == self.state { return None; }   // → no transition
```

`observe` при `speed=None` возвращает `None` → **transition-блок в `daemon.rs:747` не выполняется вовсе** → все три `unwrap_or(0.0)` (:755, :771, :797) сегодня недостижимы с `None`.

Это **не** отменяет задачу, но меняет её класс: не «latent bug, чинить в Phase 0», а «скрытый кросс-модульный инвариант» — безопасность :755 живёт в `presence.rs:86` и молча сломается, если presence когда-нибудь научится давать transition по одним шагам. Приоритет ↓ (из Phase 0 → обычный), фикс тот же (протащить `Option`), плюс сюда же — единственный незакрытый регрессионный пробел кластера 030–034: **030-part-B (`zone_hold_tick` skip-on-`None`) не имеет теста** (проверено: ноль call sites в `mod tests`). Задача 036 переписана.

---

## 3. Новые находки (нет в 003)

| # | Находка | Severity | Задача |
|---|---|---|---|
| N1 | Safety-cap force-reduce пишет в Control Point **без** `MIN_SPEED_CHANGE`-гарда — 030-класс на непочиненном пути | **high** (живой UX-баг) | [041](../tasks/041-zone-safety-cap-noop-writes.md) |
| N2 | `WATCHDOG_STALE_THRESHOLD_S` (main.rs, 95с) **уже разъехался** с `daemon::WATCHDOG_STALE_THRESHOLD` (120с) при комментарии «duplicated, keep in sync by hand»; `widget_speed_field` использует не тот порог, противореча собственному doc-комменту | medium | [043](../tasks/043-staleness-threshold-drift.md) |
| N3 | `recompute-segments` при живом демоне: DELETE+reinsert переназначает id с 1, демон держит id открытого сегмента в памяти → `credit_activity` (UPDATE WHERE id) молча дописывает живые шаги в **чужой исторический сегмент** | medium (тихая порча данных) | [044](../tasks/044-recompute-vs-live-daemon-segment-id.md) |
| N4 | HR link-state: ветки link-loss (`Ok(None)`/`Err` — daemon.rs:1100/:1114) не чистят `last_bpm`/`last_bpm_ts` (contact-Lost путь :1091 — чистит); `hr_connect_in_flight` может залипнуть `true` навсегда (нет deadline) → HR reconnect мёртв до конца сессии | medium | [042](../tasks/042-hr-link-state-hygiene.md) + часть в 035 |
| N5 | Zone-конфиг без валидации: битая зона молча выпадает из `filter_map` и **сдвигает 1-based `target_zone`**; `i64 as u16` wrap для абсолютных bpm-границ; нет проверки `min < max`; substring-матч `find_zone` берёт первый попавшийся без лога | medium | [045](../tasks/045-zone-config-validation.md) |
| N6 | `raw_samples`: нет индекса по `ts_ms` (все горячие чтения — full scan) и нет retention вообще (`status_events` тоже) — unbounded growth при 1–2 Гц телеметрии | medium-low | [046](../tasks/046-raw-samples-index-retention.md) |
| N7 | Гигиена (low, пачкой): `tm notify-test` не покрывает `auto_paused`-тост; `presence_state` — stringly-typed `Debug`-контракт с молчаливым `_ => "unknown"` без WARN; `fitshow`/`discover`/`sniff` — живые CLI-команды без тестов и без упоминания в CLAUDE.md; `compute_default_speed` (DB-запрос) на каждом presence-transition даже при выключенном Zone Hold; `goals.rs` читает+парсит один файл до 4× за widget-tick (3 клона Setting-энумов); `partial_cmp().expect()` на float'ах → `total_cmp`; `warmup_minutes = 0` невозможно сконфигурировать (parse запрещает легитимный «без прогрева») | low | [047](../tasks/047-hygiene-sweep.md) |

### N1 подробнее — единственный найденный **живой** баг

```rust
// daemon.rs:1593-1602 — safety-cap, ветка "не hard-stop"
let target = (measured_speed_kmh - config.max_step_kmh * 2.0)
    .max(config.min_speed_kmh);
...
apply_zone_hold_speed(peripheral, target).await;   // безусловно, без deadband
```

Обычный контур давит no-op записи через `next_speed`/`MIN_SPEED_CHANGE_KMH` (фикс 030). Safety-путь — **нет**. Сценарий: лента на `min_speed_kmh`, bpm выше `safety_cap`, но ниже `hard_stop` → `target = max(min - 2·step, min) = min` = текущая скорость → RequestControl+SetSpeed (двойной бип) **каждый safety-cooldown (~5с), бесконечно**, ровно когда оператору и так тяжело (пульс высокий). Тот самый класс 030, «закрытый» два релиза назад — доказательство тезиса 003 §3.4 про единый choke point для записей: пока гард живёт на call-site'ах, каждый новый путь записи забывает его заново.

### N3 подробнее — тихая порча истории

`replace_activity_segments` (store.rs:940) = `DELETE FROM activity_segments` + reinsert с детерминированными id с 1 (scratch-replay). Демон кэширует id открытого сегмента; `credit_activity` (store.rs:541) делает `UPDATE ... WHERE id = ?5`, а страховка ловит только `rows == 0` («id не найден»). После recompute id почти наверняка существует, но указывает на **другой** (закрытый исторический) сегмент → живой зачёт дописывается туда молча. Ни task 015, ни доки не требуют «останови демона перед recompute».

---

## 4. Данные: тесты и история (независимая инвентаризация)

- **150 тестов, 0 failed; clippy `-D warnings` чистый; CI (fmt+clippy+build+test) есть.**
- Покрытие по слоям: pure math — отлично (zone_hold 30, store 25, hr 16, goals 15...); **async-оркестрация — 1 (один!) `#[tokio::test]`** (`telemetry_deadline_fires_despite_a_faster_sibling_arm`, 031). Перекос 003 §3.9 подтверждён числом.
- Регрессии кластера 030–034: 5 из 6 фиксов покрыты; пробел — 030-part-B (`zone_hold_tick` skip-on-`None`) → закрывается в 036.
- Re-fix'ы (027, 030, 033 — по два fix-коммита на задачу) — все задокументированы в task-доках как staged discoveries, не тихие регрессии. Дисциплина docs-first работает.
- `daemon.rs` = 61.5% фиксов при нулевом покрытии своего цикла → главный аргумент за backlog [005](../backlog/005-session-state-extract.md) (state extract) остаётся в силе.

---

## 5. Пересмотренный Phase 0 (замена §5 Phase 0 из 003)

| Приоритет | Задача | Почему |
|---|---|---|
| 1 | [035](../tasks/035-hr-relative-timeout-in-select.md) HR `sleep_until` **+ freshness-гейт на `zh_bpm` + чистка `last_bpm` на link-loss** | severity ↑ (§2.1): контур управления лентой на мёртвом bpm |
| 2 | [041](../tasks/041-zone-safety-cap-noop-writes.md) safety-cap no-op writes | живой воспроизводимый баг, 030-класс, ~10 строк |
| 3 | [043](../tasks/043-staleness-threshold-drift.md) staleness-константы | дрейф уже случился; влияет на routing control-команд (риск contention за BLE) |
| 4 | [037](../tasks/037-atomic-config-toml-write.md) atomic config write | + новое следствие: гонка partial-write ↔ hot-reload = mid-session disengage (см. обновление 037) |
| 5 | [038](../tasks/038-tm-doctor-liveness-matrix.md) `tm doctor` | без изменений |
| 6 | [040](../tasks/040-karvonen-missing-resting-hr-warn.md) Karvonen WARN | без изменений; уточнение: fallback-зоны **ниже** задуманных — тренировка ниже целевой аэробной зоны, не «эквивалентный» результат |

**Выбывает из Phase 0:** 036 (премиза неверна, §2.2) — остаётся открытой как invariant-hardening, приоритет medium.

**Дальше (порядок как в 003, с добавками):** 039 → 042 → 044 → 045 → 046 → backlog 005–008 (без изменений) → 047 оппортунистически.

---

## 6. Что 003 предлагает правильно и что подтверждаем без изменений

- Phase 1 «state structs, не Event/Effect» — подтверждено; независимый ревьюер daemon.rs предложил буквально то же (`HrLink` struct с `on_frame/on_lost/reset` — killer для класса «одна ветка забыла поле», доказан N4; общий `Liveness {last_seen, timeout}` для обоих стримов — killer для класса 031/035).
- Phase 2 control intent (039) — подтверждено; N1 усиливает аргумент за единый write-through choke point с deadband-гардом внутри (не только тег + окно).
- YAGNI-запреты §7 (актёры, versioned migrations, полный арбитр, вынос BLE) — подтверждены.
- Разбиение god-модулей (backlog 007) — подтверждено; независимая карта швов `main.rs`/`store.rs` совпала с 003 и уточнена: zone-CLI ≈540 строк — самый крупный связный кусок `main.rs` (−27% одним извлечением); `store.rs` режется по `impl Store`-блокам в подмодули без изменения API.

## 7. Связанные артефакты

- Обновлены: задачи [035](../tasks/035-hr-relative-timeout-in-select.md), [036](../tasks/036-zone-engage-zero-speed-defaults.md), [037](../tasks/037-atomic-config-toml-write.md), [040](../tasks/040-karvonen-missing-resting-hr-warn.md); research [003](003-reliability-architecture-review.md) (эррата-ссылка).
- Созданы: задачи [041](../tasks/041-zone-safety-cap-noop-writes.md)–[047](../tasks/047-hygiene-sweep.md).
- **Implementation (2026-07-10/11):** 035–047 shipped in code; live smoke [048](../tasks/048-live-smoke-035-047.md) partial green.
- Backlog 005–008 — без изменений (подтверждены).
- **New backlog after smoke:** [009](../backlog/009-btleplug-panic-wedges-ble-scan.md) — btleplug descriptor panic leaves process alive with permanent `start filtered BLE scan` failure.
