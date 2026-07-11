# 052 — Typed config apply: `ConfigDelta` → session effects

**Статус:** сделано (2026-07-11)
**Источник:** backlog [008](../backlog/008-typed-config-apply.md) ← research [003](../research/003-reliability-architecture-review.md) Phase 3
**Зависимости:** задача [032](032-zone-hold-off-still-drives-speed.md) (ad-hoc фикс, который этот таск обобщает), [017](017-config-hot-reload.md) (mtime watch), [027](027-zone-hold.md) (Zone Hold), [037] (atomic TOML write — уже сделан, **не** этот таск).

## Проблема

Hot-reload сегодня (`daemon.rs`, ветка `config_tick.tick()`, ~строки 1050–1114;
номера могут дрейфовать) — это `stat` + три независимых **молчаливых field
copy** (`goals`, `auto_pause`, `zone_hold`) с двумя ad-hoc довесками для Zone
Hold (disengage при `enabled=false`, engage при `enabled=true` + Walking).
Задача 032 доказала, что field copy недостаточно для phase machines: фаза
переживает edit конфига, и живой `Ramp` продолжал писать в Control Point после
`enabled=false`. Фикс 032 закрыл ровно один случай (`enabled` ↓); остальные
mid-session правки (`target_zone`, `max_speed`, `warmup_minutes`, удаление
`age`) либо применяются неявно (следующий тик читает живой конфиг), либо
применяются **только при живой телеметрии** (age removed → disengage сидит в
telemetry-ветке, а не в reload-ветке), и ни одна не покрыта тестами как
политика.

Цель: типизированный конвейер

```text
reload_if_changed() -> Option<ConfigDelta>
apply_config(&mut LiveConfig, delta, session-context) -> Vec<ConfigEffect>
```

— ни одного молчаливого copy, каждая mid-session политика — явная строка в
матрице и table-driven тест.

## Дизайн

### Новый модуль `src/config_apply.rs` (чистый, без BLE/времени/IO в решениях)

Домом мог бы быть session-state из backlog 005, но 005 не сделан — маленький
отдельный модуль лучше, чем ещё +150 строк в `daemon.rs` (2750+ строк, 🔴).

Переносим туда `LiveConfig` из `daemon.rs` (это просто бандл трёх значений;
`daemon.rs` импортирует). Публичный API:

```rust
/// The three hot-reloadable config values (moved from daemon.rs verbatim).
pub struct LiveConfig {
    pub goals: Vec<Goal>,
    pub auto_pause: Option<Duration>,
    pub zone_hold: zone_hold::ZoneHoldConfig,
}

/// What actually changed on disk. `None` field = unchanged.
/// `auto_pause` is `Option<Option<Duration>>`: outer = "changed?",
/// inner = the new value (`None` = auto-pause disabled).
pub struct ConfigDelta {
    pub goals: Option<Vec<Goal>>,
    pub auto_pause: Option<Option<Duration>>,
    pub zone_hold: Option<zone_hold::ZoneHoldConfig>,
}
impl ConfigDelta {
    pub fn is_empty(&self) -> bool { /* all three None */ }
}

/// Pure diff old vs new (each field via PartialEq, as the reload branch does today).
pub fn diff(old: &LiveConfig, new: &LiveConfig) -> ConfigDelta;

/// stat + reload gate. Returns `None` when mtime hasn't moved (the common,
/// every-5s case — no file read, no parse, no logs). Returns `Some(delta)`
/// when the file WAS re-read — delta may still be empty (mtime moved, content
/// identical): the caller must refresh the `tm status` config snapshot
/// (задача 022) in that case too, exactly like today.
pub fn reload_if_changed(
    last_mtime: &mut Option<SystemTime>,
    current: &LiveConfig,
) -> Option<ConfigDelta>;
```

`reload_if_changed` — тонкая IO-обёртка: `goals::config_mtime()` +
`goals::load_goals()` + `goals::load_auto_pause()` +
`zone_hold::load_zone_hold_config()` + `diff`. Юнит-тестами покрываем `diff` и
`apply_config`; сам mtime-гейт — поведение задачи 017, переносится verbatim
(первый тик: `last_mtime = None` → форс-reconcile; `None↔Some` переход mtime —
тоже изменение).

### Контекст сессии и эффекты

`ZoneHoldPhase` остаётся в `daemon.rs` (несёт `Instant`'ы и завязан на
session-loop). В `config_apply.rs` — плоское зеркало без данных:

```rust
pub enum PhaseKind { Off, Ramp, Hold, Frozen, Grace }

pub struct SessionSnapshot {
    pub phase: PhaseKind,
    pub walking: bool, // accumulator.state() == PresenceState::Walking
}
```

В `daemon.rs` добавить крошечный `impl ZoneHoldPhase { fn kind(&self) -> PhaseKind }`
(рядом с существующим `label()`).

```rust
pub enum DisengageReason { DisabledInConfig, TargetUnresolvable }

pub enum ConfigEffect {
    /// goals list changed — executor logs old→new and uses the new list.
    GoalsChanged,
    /// auto-pause threshold changed — executor logs old→new.
    AutoPauseChanged,
    /// Zone Hold must stop driving the belt NOW (enabled ↓, or target zone
    /// no longer resolvable — e.g. age removed). Generalises the 032 ad-hoc fix.
    ZoneDisengage(DisengageReason),
    /// enabled ↑ mid-session while Walking — engage via the same
    /// zone_hold_on_transition path a fresh Walking entry uses (existing code).
    ZoneEngage,
    /// Zone-target-affecting keys changed while the controller is live
    /// (Hold/Grace/Frozen/Ramp): target_zone / zones / max_speed / method /
    /// age / resting_hr. Executor re-resolves ResolvedZone, refreshes the
    /// zone_hold_target_lo/hi snapshot, logs old→new bounds.
    ZoneReResolve,
    /// warmup_minutes changed mid-Ramp. POLICY: keep the ramp — do NOT restart.
    ZoneWarmupRetarget { old_minutes: i64, new_minutes: i64 },
}

/// Applies the delta to `config` (field updates) and decides session effects.
/// Pure: no IO, no clocks; logging of *applied effects* happens here or in the
/// executor — but every non-empty delta field must produce exactly one log line.
pub fn apply_config(
    config: &mut LiveConfig,
    delta: ConfigDelta,
    snap: &SessionSnapshot,
) -> Vec<ConfigEffect>;
```

Порядок эффектов детерминированный: disengage раньше всего остального
(safety-first), engage — последним.

### Матрица mid-session политик (нормативная)

| # | Изменение | Контекст (phase / walking) | Эффект | Примечание |
|---|---|---|---|---|
| 1 | `enabled` true→false | phase ≠ Off | `ZoneDisengage(DisabledInConfig)` | обобщает ad-hoc фикс 032 |
| 2 | `enabled` true→false | phase = Off | только field update + лог | disengage не нужен |
| 3 | `enabled` false→true | phase = Off, walking | `ZoneEngage` | существующий путь `zone_hold_on_transition(Unknown→Walking)` |
| 4 | `enabled` false→true | phase = Off, не walking | только field update + лог | engage случится на следующем presence-переходе |
| 5 | `target_zone` / per-zone `max_speed` / `zones` / `method` / `resting_hr` изменены, target разрешим | phase ∈ {Hold, Grace, Frozen, Ramp} | `ZoneReResolve` | mid-Hold re-resolve `ResolvedZone`; следующий тик уже корректирует к новым границам |
| 6 | то же | phase = Off | только field update + лог | нечего re-resolv'ить |
| 7 | `age` удалён (Some→None), либо новый `target_zone` не матчится ни одной зоне | phase ≠ Off | `ZoneDisengage(TargetUnresolvable)` | сегодня это срабатывает только в telemetry-ветке; теперь — сразу на reload |
| 8 | `age` удалён | phase = Off | только field update + лог | |
| 9 | `warmup_minutes` изменён | phase = Ramp | `ZoneWarmupRetarget` | **политика ниже** |
| 10 | `warmup_minutes` изменён | phase ≠ Ramp | только field update + лог | Hold/Grace/Frozen warmup не читают |
| 11 | `goals` изменены | любой | `GoalsChanged` | существующее поведение, но через delta + лог |
| 12 | `auto_pause_minutes` изменён | любой | `AutoPauseChanged` | существующее поведение, но через delta + лог |
| 13 | mtime сдвинулся, контент идентичен | любой | пустой delta, ноль эффектов, ноль логов про изменения | но снапшот `set_config` + `persist` обновить (задача 022 — "когда демон последний раз читал файл") |

Комбинации (например `enabled` ↓ **и** `warmup` изменён одним edit'ом): побеждает
disengage; retarget/re-resolve при выключенном контроллере не эмитятся.

#### Политика `warmup_minutes` mid-Ramp (пункт 9) — выбранная

**Пересчитать ramp от нового значения, НЕ рестартовать ramp с нуля.**
Конкретно: `started_at` / `start_speed_kmh` / `target_speed_kmh` в
`ZoneHoldPhase::Ramp` не трогаются; новый `warmup_minutes` просто читается
следующим `zone_hold_tick` (он и сегодня читает `config.warmup_minutes` живьём
— политика фиксирует и тестирует это поведение, а не изобретает новое).
Следствия, оба покрыть тестами:

- **Сокращение** ниже уже прошедшего elapsed (`elapsed >= new_warmup`) → ramp
  завершается на следующем тике, переход в `Hold` (существующая ветка
  `elapsed >= warmup`).
- **Удлинение** → наклон `warmup_target_speed(start, target, elapsed, new_warmup)`
  пересчитывается от того же `started_at`; цель ramp'а не меняется, скорость
  не дёргается назад (функция монотонна по elapsed при фиксированных краях).

Эффект `ZoneWarmupRetarget` при этом — **log-only** (executor ничего не мутирует
в фазе): его ценность — (а) явный INFO-лог с old→new, (б) тест-гарантия, что
никакой будущий рефакторинг не начнёт рестартовать ramp на этом delta.

### Исполнение эффектов (`daemon.rs`, только reload-ветка)

Ветка `config_tick.tick()` сжимается до:

```rust
if let Some(delta) = config_apply::reload_if_changed(&mut goals_mtime, &config) {
    let snap = config_apply::SessionSnapshot {
        phase: zh_phase.kind(),
        walking: accumulator.state() == PresenceState::Walking,
    };
    let effects = config_apply::apply_config(&mut config, delta, &snap);
    for effect in effects { /* small executor match */ }
    state.set_config(&config.goals, config.auto_pause);
    state.persist(store, watchdog)?;
}
```

Executor-match (может быть маленькой fn в `daemon.rs` рядом с
`disengage_zone_hold`):

- `ZoneDisengage(reason)` → `info!/warn!` (по reason) + существующий
  `disengage_zone_hold(&mut zh_phase, state)`.
- `ZoneEngage` → существующий блок engage (сегодняшние строки ~1090–1109:
  `last_walking_speed` seed, `compute_default_speed`, `zone_hold_on_transition`)
  — переносится внутрь executor'а verbatim.
- `ZoneReResolve` → `config.zone_hold.resolve_target_zone()`; `Some` →
  обновить `state.zone_hold_target_lo/hi` + INFO с новыми границами; `None`
  теоретически недостижим (apply_config эмитит Disengage вместо ReResolve),
  но на всякий случай — WARN + `disengage_zone_hold` (defense in depth).
- `ZoneWarmupRetarget { .. }` → INFO-лог, без мутаций.
- `GoalsChanged` / `AutoPauseChanged` → INFO-лог old→new (те же строки лога,
  что сегодня, допустимо переформулировать) — сами значения уже применены в
  `apply_config`.

**Рубежи 032 остаются как defense in depth, не трогать:** гейт
`should_run_zone_hold(enabled, phase)` на call site и ранний `return` при
`!config.enabled` в `zone_hold_tick`. Ветка telemetry-цикла «target zone no
longer resolvable → disengage» тоже остаётся (reload-путь её дублирует только
для момента edit'а; telemetry-путь ловит прочие деградации).

### Логирование

- Каждый применённый delta-эффект — ровно один INFO (disengage по
  `TargetUnresolvable` — WARN: конфиг сломали руками).
- Invalid-конфиг — как сегодня: WARN внутри существующих загрузчиков
  (`goals.rs` / `zone_hold.rs`), не дублировать.
- Пустой delta (пункт 13) — без логов об изменениях (не спамить каждые 5с
  после `touch`).

## Тесты (acceptance)

Table-driven, чистые, без BLE; время/фазы инъекцией:

1. **`apply_config` по всей матрице** — один тест с таблицей
   `(delta, phase, walking) → ожидаемые effects` минимум на пункты 1–12
   (+ комбинация «enabled ↓ + warmup changed» → только Disengage). Проверять и
   эффекты, и итоговое состояние `LiveConfig` (поля реально применены).
2. **`diff`** — unchanged → пустой delta; каждое поле по отдельности; все сразу.
3. **Warmup-политика (пункт 9)** — два теста на семантику:
   сокращение ниже elapsed → следующий тик завершает ramp (через существующую
   чистую логику: `warmup_target_speed` + правило `elapsed >= warmup`; если
   правило внутри `zone_hold_tick` не достаётся чисто — извлечь микро-хелпер,
   но не перестраивать `zone_hold_tick`); удлинение → target по
   `warmup_target_speed` с новым warmup от того же elapsed, без сброса к
   `start_speed_kmh`.
4. **Ни одного молчаливого copy:** в reload-ветке `daemon.rs` не остаётся
   прямых присваиваний `config.<field> = ...` мимо `apply_config` (ревью-чек,
   не автотест).
5. Существующие тесты (`should_run_zone_hold`, disengage, goals/zone parsing) —
   зелёные без правок семантики.
6. `cargo test` и `cargo clippy` — зелёные.

Инвариант производительности: mtime-дебаунс сохранён — `stat` раз в
`CONFIG_RELOAD_INTERVAL` (5с), чтение/парс файла только при сдвиге mtime
(поведение задачи 017 бит-в-бит).

## Scope

**Трогать:**
- `src/config_apply.rs` — новый модуль (LiveConfig, ConfigDelta, PhaseKind,
  SessionSnapshot, ConfigEffect, diff, reload_if_changed, apply_config + тесты).
- `src/daemon.rs` — ТОЛЬКО: ветка `config_tick.tick()`, executor эффектов,
  `ZoneHoldPhase::kind()`, импорт `LiveConfig` из нового модуля (и удаление
  локального определения). Ничего в scan/connect-цикле, HR-ветках, telemetry-ветке.
- `src/goals.rs`, `src/zone_hold.rs` — только если понадобится
  видимость/хелперы (например, сравнение zone-target-affecting полей);
  семантику загрузчиков не менять.
- `src/main.rs` — **исключение ровно на одну строку**: `mod config_apply;` в
  список модулей. Больше ни символа (параллельная задача пилит CLI из main.rs).

**Не трогать:** `src/store.rs`; весь остальной `daemon.rs` (в scan/connect-цикле
параллельно работает задача 051); atomic TOML write (сделан в 037);
`docs/README.md`.

## Риски

- Перенос `LiveConfig` — механический, но затрагивает сигнатуры
  `stream_with_presence`/`run` (тип тот же, меняется только путь импорта).
- Пункт 7 меняет наблюдаемое поведение (disengage на reload, а не на следующем
  telemetry-сэмпле) — это желаемое улучшение, зафиксировать в CHANGELOG-строке
  коммита не нужно, достаточно лога.
- Конфликт с задачей 051 в `daemon.rs` возможен только по соседству строк, не
  по семантике — области не пересекаются.
