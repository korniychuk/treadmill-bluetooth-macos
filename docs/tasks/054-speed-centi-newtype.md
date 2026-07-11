# 054 — `CentiKmh` newtype: квантизация скорости на FTMS wire boundary

**Статус:** done
**Источник:** backlog [006](../backlog/006-speed-quantize-newtype.md), research [003](../research/003-reliability-architecture-review.md) Phase 4
**Первопричина:** задача [030](030-zone-hold-noop-writes-at-clamp.md) (холостые Control Point записи на клампе)

## Контекст и первопричина

FTMS wire кодирует скорость как `u16` в единицах **0.01 km/h** (little-endian).
Внутри кодовой базы она живёт как `f32`, и одно и то же номинальное значение
приходит по **двум разным float-путям**:

- телеметрия: `ftms.rs` декодирует `raw as f32 * 0.01` → `320 * 0.01 = 3.1999998`
  (0.01 не представим точно в binary32);
- конфиг/target: TOML-парсинг `3.2` → `3.200000047683716`.

Оба значения означают одну скорость, но **никогда не равны побитово**. Задача 030:
`next_speed` на клампе слал тот же target каждые ~20с вечно (RequestControl +
SetSpeed = двойной бип ленты, ноль реального изменения). Фикс 030 — epsilon
`MIN_SPEED_CHANGE_KMH = 0.05` — это **float-glue**: он маскирует representation
gap вместо того, чтобы устранить его в типе. Каждое новое сравнение wire-скоростей
на f32 — потенциальный рецидив (уже понадобились ещё два epsilon-а:
`SPEED_RESTORE_EPSILON_KMH` в `daemon.rs` и deadband в
`safety_force_reduce_target`, задача 041).

Дополнительный сигнал, что centi — правильный домен: `store.rs` **уже** хранит
`raw_samples.speed_centikmh INTEGER` (wire scale) и делает ad-hoc обратную
квантизацию `(v * 100.0).round() as i64` в `insert_raw_sample` — т.е. третье,
рукописное место с той же логикой.

## Цель

Один тип на границе: **`CentiKmh(u16)`** — скорость в wire-единицах (0.01 km/h).

- Квантизация происходит **на decode** (`ftms.rs`) и **на входе конфига/CLI**
  (`from_kmh_f32`, half-up) — дальше по compare/clamp-путям ходит только целое.
- Все сравнения и клампы wire-скоростей — **точная integer-арифметика**
  (`Eq`/`Ord` derive), representation gap исчезает by construction.
- `MIN_SPEED_CHANGE_KMH` **остаётся**, но переосмысляется: это **controller
  deadband** (политика «не дёргай ленту из-за мелочи», поглощает и настоящий
  сенсорный jitter, если он есть у какой-то модели), а не заплатка на float.
  В centi-единицах: `MIN_SPEED_CHANGE: CentiKmh = CentiKmh(5)`.

## Дизайн newtype

Новый модуль **`src/speed.rs`** (маленький, single-purpose, без BLE/IO —
конвенция репо «парсинг протокола отдельно от транспорта»):

```rust
/// Belt speed in FTMS wire units (0.01 km/h). The only type in which
/// wire speeds are compared or clamped — comparisons are exact integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CentiKmh(u16);

impl CentiKmh {
    pub const ZERO: Self = Self(0);
    /// Sane command ceiling (25 km/h), mirrors the old range check in
    /// `control.rs::set_speed`.
    pub const MAX_SANE: Self = Self(2500);

    /// Lossless: the wire u16 *is* the value.
    pub const fn from_wire(raw: u16) -> Self;
    pub const fn to_wire(self) -> u16;

    /// Quantize a config/CLI float, half-up: `(kmh * 100.0).round()`.
    /// `None` on NaN / negative / overflow past u16 — caller решает,
    /// это WARN-and-default (config) или hard error (CLI arg).
    pub fn from_kmh_f32(kmh: f32) -> Option<Self>;

    /// Единственная точка обратного преобразования для display/статистики.
    pub fn to_kmh_f32(self) -> f32; // raw as f32 * 0.01

    /// Integer clamp/шаги для контроллера (без выхода в f32).
    pub fn clamp(self, min: Self, max: Self) -> Self; // via Ord
    pub fn abs_diff(self, other: Self) -> u16;
    pub fn saturating_add_centi(self, delta: u16) -> Self;
    pub fn saturating_sub_centi(self, delta: u16) -> Self;
}

/// "3.2", "3.05", "0" — trimmed decimal, совместим с текстовым wire-форматом
/// очереди `control_commands` (`speed:2.5`) и человекочитаем в логах.
impl fmt::Display for CentiKmh;
```

Инварианты (юнит-тесты в `speed.rs`):

1. **Quantize identity** (ядро acceptance backlog 006): для всех `raw: u16`
   `CentiKmh::from_kmh_f32(CentiKmh::from_wire(raw).to_kmh_f32()) == Some(from_wire(raw))`
   — прогнать полный диапазон `0..=u16::MAX` циклом (65k итераций — мгновенно,
   proptest не нужен).
2. Repro бага 030 на типе: `from_kmh_f32(320f32 * 0.01)` (телеметрия
   `3.1999998`) `== from_kmh_f32(3.2)` (конфиг) `== Some(CentiKmh(320))`.
3. Half-up: `from_kmh_f32(3.145) → 315` (учесть, что сам литерал 3.145 в f32 ≈
   3.1449998 — тест фиксирует фактическую политику `.round()` на честных
   примерах, напр. `0.045`/`0.055`), NaN/`-0.1`/`700.0` → `None`.
4. `Display` round-trip через parse-путь `ControlCommand` (см. §Инвентаризация,
   `control_command.rs`).

## Границы применения — boundary first

Принцип: тип живёт **на границе wire ↔ логика**. Внутренности, которые считают
статистику или рисуют текст, остаются на f32/f64 через `to_kmh_f32` — это
compute/display, не compare.

| Граница | Было | Станет |
|---|---|---|
| **decode** `ftms.rs::parse_treadmill_data` | `speed_kmh: Option<f32>` = `raw as f32 * 0.01` | `speed: Option<CentiKmh>` = `from_wire(raw)`; то же для `avg_speed` |
| **encode** `control.rs::Controller::set_speed` | `set_speed(kmh: f32)`, `(kmh * 100.0).round() as u16` | `set_speed(speed: CentiKmh)`, `speed.to_wire().to_le_bytes()`; range check `<= MAX_SANE` |
| **controller** `zone_hold.rs::next_speed` | f32 clamp + `abs() > MIN_SPEED_CHANGE_KMH` | вход/выход `CentiKmh`, integer clamp, `abs_diff > MIN_SPEED_CHANGE.to_wire()` |
| **config/CLI input** | TOML f32 / clap f32 сравниваются с телеметрией напрямую | квантизация `from_kmh_f32` в момент построения `ControllerParams` / парсинга CLI-аргумента |

## Инвентаризация call sites (полная, по функциям)

> ⚠️ **Указаны функции, не номера строк** — параллельные задачи 052 (typed
> config apply, трогает `zone_hold.rs`) и 053 (session state extract, трогает
> `daemon.rs`) сдвинут код. Перед началом — заново `grep -n "speed" src/*.rs`
> и сверить с этой таблицей.

### Меняются на `CentiKmh` (compare/clamp на wire-скоростях)

| Файл | Функция / место | Сейчас | План |
|---|---|---|---|
| `src/ftms.rs` | `parse_treadmill_data` (ветки instantaneous + avg speed) | `raw as f32 * 0.01` ×2 | `CentiKmh::from_wire(raw)`; поля `TreadmillData.speed`/`avg_speed: Option<CentiKmh>`. Тесты — точный `assert_eq!` вместо `(avg - 2.17).abs() < 1e-4` |
| `src/control.rs` | `Controller::set_speed` | `f32`, `(0.0..=25.0).contains`, ручная квантизация | `CentiKmh`, `<= MAX_SANE`, `to_wire()` |
| `src/zone_hold.rs` | `MIN_SPEED_CHANGE_KMH: f32 = 0.05` | float epsilon | `MIN_SPEED_CHANGE: CentiKmh = CentiKmh(5)` — с комментарием «controller deadband, не float glue»; история 030 сохраняется в докстринге |
| `src/zone_hold.rs` | `ControllerParams { max_step_kmh, min_speed_kmh, max_speed_kmh }` | f32 | `CentiKmh` (шаг — `u16` centi). Квантизация — один раз при построении params из конфига (после 052 — из typed config), не в контроллере |
| `src/zone_hold.rs` | `next_speed` | f32 `clamp` + epsilon-compare | `CentiKmh` вход/выход; `current ± step` через `saturating_*`, `clamp(min, max)`, `(target.abs_diff(current) > deadband).then_some(target)` |
| `src/zone_hold.rs` | `safety_force_reduce_target` | f32 `.max` + epsilon | `CentiKmh`: `saturating_sub_centi(2 * step)`, `.max(min)`, `abs_diff`-deadband |
| `src/zone_hold.rs` | `resolve_target_zone` → `ResolvedZone.effective_max_speed_kmh`; `ZoneDef.max_speed_kmh` | f32 | `CentiKmh` (per-zone override `Option<CentiKmh>`); квантизация на парсинге зоны |
| `src/zone_hold.rs` | `warmup_target_speed` | f32 интерполяция | **остаётся f32 внутри** (линейный ramp — compute, не compare); сигнатура принимает/возвращает `CentiKmh`, интерполяция через `to_kmh_f32`, выход — `from_kmh_f32(...)` (квантизация до compare/write) |
| `src/daemon.rs` | `SPEED_RESTORE_EPSILON_KMH: f32 = 0.05` | float epsilon | `SPEED_RESTORE_EPSILON: CentiKmh = CentiKmh(5)` (или общая константа с zone_hold — решить на месте: семантика одна, владельцы разные) |
| `src/daemon.rs` | `speed_restore_target` | `pre > resumed + eps` | `pre_pause.to_wire() > resumed.to_wire() + eps.to_wire()` на `CentiKmh` |
| `src/daemon.rs` | `zone_hold_tick` | `measured_speed_kmh: Option<f32>`; Ramp-ветка `(target - measured).abs() > eps`; hard-stop `measured <= min + eps` | `Option<CentiKmh>`; `abs_diff > eps`; `measured <= min.saturating_add_centi(eps)` |
| `src/daemon.rs` | `zone_hold_on_transition` | `default_kmh.clamp(config.min, config.max)` | integer `clamp` на `CentiKmh` (default приходит из `default_speed` как f32 → `from_kmh_f32` на входе) |
| `src/daemon.rs` | `try_apply_default_speed` | `resumed_kmh > DEFAULT_SPEED_APPLY_CEILING_KMH (0.8)` | `resumed > CentiKmh(80)` — crawl-порог это wire-сравнение |
| `src/daemon.rs` | `try_restore_speed` / `restore_speed` / `apply_zone_hold_speed` | `f32` таргеты насквозь до `set_speed` | `CentiKmh` насквозь (конверсия в f32 только в `info!`/toast) |
| `src/presence.rs` | `PresenceTracker::observe(speed_kmh: Option<f32>)`, сравнения `speed <= 0.0` | f32 zero-check | `Option<CentiKmh>`, `speed == CentiKmh::ZERO` — точный «belt stopped» |
| `src/activity.rs` | `ActivityAccumulator::observe(speed_kmh: Option<f32>, ...)` | f32 pass-through в presence | `Option<CentiKmh>` pass-through |
| `src/recompute.rs` | replay-ряд: `speed_centikmh.map(...)` → `observe` | через `store` decode `c as f32 / 100.0` | `CentiKmh::from_wire(c as u16)` — **бонус**: replay гоняет точные wire-значения, ноль float round-trip |
| `src/store.rs` | `insert_raw_sample` | ad-hoc `(v * 100.0).round() as i64` | `speed.to_wire() as i64` — дубль квантизации исчезает |
| `src/store.rs` | `raw_samples_ordered` (`speed_kmh: c as f32 / 100.0`) | float decode | отдать `CentiKmh` (потребитель — recompute) |
| `src/control_command.rs` | `ControlCommand::Speed(f32)`, `to_wire` `speed:{kmh}`, `parse` | f32 carrier | `Speed(CentiKmh)`; текстовый формат очереди **не меняется** (`speed:2.5` через `Display`), `parse` = `f32::parse` → `from_kmh_f32` (ошибка — loud, как сейчас). Round-trip тест `to_wire→parse` обновить |
| `src/main.rs` | `Commands::Speed { kmh: f32 }` → `run_control` | f32 из clap | квантизация на границе CLI: `from_kmh_f32` сразу после парсинга аргумента, невалидное → user-facing error |

### Остаются f32/f64 через `to_kmh_f32` (compute/display, НЕ compare) — явные non-goals внутри PR

| Файл | Место | Почему остаётся |
|---|---|---|
| `src/default_speed.rs` | `trimmed_mean_speed`, `round_to_tenth`, `compute_default_speed` | статистика (mean/trim/sort) — честный f32-домен; floor-фильтр `>= WALKING_FLOOR_KMH` сравнивает **исторические** значения с константой, gap-бага тут нет. Выход применяется к ленте через `from_kmh_f32` на write-границе |
| `src/daemon.rs` | `speed_history`, `cruising_speed`, `last_walking_speed`, `SPEED_CRUISE_FLOOR_KMH` | тот же статистический estimate; порог 0.8 — эвристика, не wire-равенство. Опционально мигрировать в 053+ |
| `src/store.rs` | `daemon_status.last_speed_kmh REAL` (f64), `walking_speeds_in_window → Vec<f32>` | display-снапшот виджета и вход статистики; кормится `speed.to_kmh_f32() as f64` |
| `src/main.rs` | `format_speed_kmh(f64)`, `widget_speed_field`, `tm status`/`tm zone` печать | чистый display |
| `src/notify.rs` | `SpeedRestore { from_kmh, to_kmh }`, `default_speed_applied` | текст toast'ов |
| `src/logger.rs` | JSONL `speed_kmh`/`avg_speed_kmh` | человекочитаемый лог; точность гарантирует соседний `raw_frame` |
| `src/zone_hold.rs` | парсинг TOML (`positive_float_or` и т.п.), `upsert_zone_hold_keys`, CLI-промпты | вход/выход конфиг-файла — числа в km/h для человека; квантизация происходит на построении params. ⚠️ после 052 этот слой станет typed config — сверить точку квантизации с его дизайном |
| `src/fitshow.rs` | `set_speed_incline` | vendor-протокол (не FTMS), свой framing — вне scope |
| `src/presence.rs` / прочее | пороги bpm, incline (`raw as f32 * 0.1`) | incline — отдельный wire-домен (deci-percent); осознанно не трогаем (YAGNI: по incline нет write-compare путей) |

## Что НЕ делаем в этом PR (non-goals)

- **Не** переписываем весь f32 в кодовой базе — boundary first (backlog 006).
- **Не** трогаем схему SQLite (в `raw_samples` уже centi; `last_speed_kmh REAL`
  остаётся display-снапшотом).
- **Не** вводим `DeciPercent` для incline — нет compare-путей, YAGNI.
- **Не** меняем текстовый формат очереди `control_commands` и TOML-конфига —
  человекочитаемые km/h снаружи, centi внутри.
- **Не** трогаем `fitshow.rs`.

## План (milestones)

1. **`src/speed.rs`** — newtype + полный тест-набор инвариантов (§Дизайн).
   Чистый, компилируется независимо.
2. **`ftms.rs`** — поля `TreadmillData` → `Option<CentiKmh>`; тесты decode на
   точное равенство. Это ломает компиляцию потребителей — дальше по цепочке.
3. **`control.rs`** — `set_speed(CentiKmh)`.
4. **`zone_hold.rs`** — `ControllerParams`/`next_speed`/
   `safety_force_reduce_target`/`warmup_target_speed`/`ResolvedZone` (по
   таблице). Портировать тест 030
   (`band_mode_ignores_float_precision_gap_at_the_pin`) в два теста:
   *quantize identity* (телеметрия-vs-конфиг дают один `CentiKmh` → pinned на
   клампе = `None` **точно**, без epsilon) и *deadband policy* (diff ≤ 5 centi
   → `None`; diff > 5 → `Some`; настоящая коррекция `max_step` не глушится).
5. **`presence.rs` / `activity.rs` / `recompute.rs`** — `Option<CentiKmh>`
   pass-through; recompute читает `from_wire` из store.
6. **`daemon.rs`** — compare-сайты по таблице; статистика (`speed_history`,
   cruising) остаётся f32 через `to_kmh_f32`; снапшоты/toast'ы — display-конверсия.
7. **`store.rs`** — `insert_raw_sample` через `to_wire`; `raw_samples_ordered`
   отдаёт `CentiKmh`.
8. **`control_command.rs` + `main.rs`** — `Speed(CentiKmh)`, CLI-квантизация,
   round-trip тесты wire-формата.
9. **Сверка полноты**: `grep -rn "speed" src/ | grep -i "f32\|f64"` — каждое
   оставшееся вхождение должно попадать в таблицу «остаются f32» (display/
   статистика). Новых молчаливых f32-compare на wire-скоростях — ноль.
10. `cargo test` + `cargo clippy` зелёные; `cargo fmt`.

Ожидаемый профиль диффа: почти весь — механическая замена типов по цепочке
компилятора; содержательные решения только в п.1 и п.4.

## Acceptance (из backlog 006, конкретизировано)

- [x] Тесты в стиле 030 выражают **quantize identity + deadband policy**, а не
      «магический 0.05» сам по себе (п.4 плана + инварианты `speed.rs`).
- [x] Ни одного нового молчаливого f32-compare на wire-скоростях; existing
      compare-сайты из таблицы либо на `CentiKmh`, либо явно в списке
      «display/статистика».
- [x] `MIN_SPEED_CHANGE` документирован как controller deadband.
- [x] `cargo test`, `cargo clippy` — зелёные.
- [ ] Поведенческий смок: `tm speed 3.2` → одна команда, лента на 3.2; Zone
      Hold pinned на клампе → тишина (ни бипов, ни Control Point записей).
      (live — requires hardware; unit coverage of pin/deadband in place)

## Sequencing — зависимость от 052/053

**Реализация стартует ПОСЛЕ мержа задачи 052 (typed config apply)** — она
перерабатывает конфиг-слой `zone_hold.rs`, т.е. ровно ту точку, где этот план
квантизует `min/max/step` при построении `ControllerParams`. Делать
одновременно — гарантированный конфликт по одним и тем же функциям.

**Желательно также после 053 (session state extract)** — она выносит
per-session state из `daemon.rs` (`speed_history`, `pre_pause_speed`,
`ZoneHoldPhase` и т.д.); наша таблица call sites в `daemon.rs` тогда слегка
переедет в новый модуль. План намеренно ссылается на **функции, а не строки** —
перед стартом повторить инвентаризацию grep'ом и свериться с фактическим
расположением.

## Риски

1. **Rebase-churn с 052/053** — снят sequencing'ом выше; таблица по функциям.
2. **Wire-формат очереди `control_commands`**: `Display` для `CentiKmh` обязан
   давать строку, которую `parse` читает обратно (`speed:2.5`, не `speed:250`).
   Round-trip тест обязателен. Строки, поставленные старым бинарём во время
   апгрейда, читаются тем же float-парсом → `from_kmh_f32` — совместимо;
   плюс staleness-гард (30с) и так отбрасывает старьё.
3. **Поведенческий сдвиг deadband'а**: раньше сравнение шло на f32 с epsilon
   0.05 — после квантизации значения, различавшиеся на <0.005 km/h, становятся
   равными. Практических изменений нет (wire-гранулярность и есть 0.01), но
   тесты п.4 фиксируют новую семантику явно.
4. **`warmup_target_speed` через f32-интерполяцию**: квантизация выхода может
   давать одинаковый `CentiKmh` на соседних тиках ramp'а — это **желаемое**
   поведение (нет изменения — нет записи), но проверить, что ramp при
   `max_step`-величинах шага всё же прогрессирует (юнит-тест на монотонность
   ramp-таргета по elapsed).
5. **Паника/overflow на арифметике u16**: только `saturating_*` в контроллере;
   `from_kmh_f32` отвергает NaN/negative/overflow — от кривого TOML или CLI
   не паникуем (тесты).
6. **Полнота инвентаризации**: сегодняшний grep покрыл `ftms/control/zone_hold/
   daemon/presence/activity/recompute/store/main/control_command/notify/logger/
   default_speed/fitshow`; шаг 9 плана — обязательная повторная сверка на
   актуальном main после 052/053.
