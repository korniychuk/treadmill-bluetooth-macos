# 003 — Reliability & Architecture Review

> **Дата:** 2026-07-09  
> **Тип:** research / code review  
> **Скоуп:** весь репозиторий `treadmill-bluetooth-macos`  
> **Триггер:** кластер багов 030–034 + рост `fix(*)` после Zone Hold / HR  
> **Цель:** понять *почему* баги множатся, где архитектура не держит инварианты, и дать компактный план, как сделать систему надёжной.  
> **Review:** 2026-07-09 — Opus 4.8 code-checked review принят; скорректированы claim'ы §3.4/§3.5/§3.8, Phase 1–2/5/7, метрики §8 и запрет §6.  
> **Independent review:** 2026-07-10 — [research/004](004-independent-reliability-review.md) (четыре независимых ревьюера без доступа к этому доку): диагноз и план подтверждены; **две поправки** — severity 035 ↑ (Zone Hold без freshness-гейта на bpm), премиза 036 неверна (путь недостижим, invariant hardening); **семь новых находок** → задачи 041–047; Phase 0 пересмотрен (004 §5).

---

## 1. Карта системы (as-is)

```
CLI (main.rs) ──config.toml──► daemon hot-reload
     │                              │
     ├── control_commands (SQLite) ──┤
     └── daemon_status / stats  ◄────┤
                                    ▼
              ┌──────── stream_with_presence (select!) ────────┐
              │  0x2ACD treadmill   │  0x2A37 HR (opt)         │
              │  Control Point out  │  power events            │
              │  presence/activity  │  zone hold / auto-pause  │
              └────────────┬───────────────────────────────────┘
                           ▼
              store.rs (SQLite) + logger.rs (JSONL)
```

| Модуль | LOC | Роль | Чистота |
|---|---:|---|---|
| `daemon.rs` | **2365** | event loop, session, watchdogs, Zone Hold wiring, HR, control queue | impure god |
| `main.rs` | **1991** | CLI surface, widget/status formatters, zone UX | mixed |
| `store.rs` | **1914** | schema, migrations, all SQLite | impure + pure merges |
| `zone_hold.rs` | **1254** | config parse + controller math + TOML patch | mostly pure + I/O |
| `goals.rs` | 700 | config keys, goals, gaps | pure + file I/O |
| `hr.rs` | 410 | parse + `ContactTracker` | pure |
| `activity.rs` / `presence.rs` | 270 / 172 | segmentation engine (shared live+replay) | pure |
| остальные | <400 | BLE scan/control, power, notify, recompute | mixed |

Четыре файла в 🔴-зоне (>1000 LOC): `daemon` / `main` / `store` / `zone_hold` — суммарно > половины из ~12k LOC проекта.

**Что уже сделано правильно (не ломать):**

1. **Replay = prod engine** — `ActivityAccumulator` / `ContactTracker` / `recompute-*` (015, 034).  
2. **Время инъекцией** — `presence`, `ContactTracker::observe(&m, ts_ms)`, `zone_hold::next_speed`.  
3. **Daemon sole owner of BLE** — CLI → `control_commands` (013).  
4. **Bounded BLE awaits + process-exit watchdog** (007, 018, 031).  
5. **Документация-first** + task docs с root-cause, не только симптомами.  
6. **Treadmill silence уже на `sleep_until`** (daemon ~688, ~2124) — шаблон есть; HR ещё нет.

---

## 2. Последние 10 коммитов и 5 задач — сигнал, не шум

### 2.1. Коммиты (HEAD →)

| SHA | Суть |
|---|---|
| `bb0e53e` | docs 033/034 — frozen-bpm + fixpoint |
| `3f50281` | **feat 034** `recompute-hr` |
| `6914ec6` | **fix 033** frozen bpm (RR alone insufficient) |
| `406d5f5` | docs 032/033 |
| `a9513bd` | **fix 033** contact ≠ link |
| `17ac179` | **fix 032** `tm zone off` mid-session |
| `c2b1239` | docs 032/033 |
| `4f61d9d` | **fix 031** stuck `connected` |
| `8bf55ea` | docs 031 |
| `3bc8f4a` | **fix** tmux session filter |

Перед этим: `11c1e14`/`e3f5b6a` (030 float clamp spam), `578544c` (stale ramp start speed), `7fb4e64` (zone on mid-session), `479ced2` (select! unwrap crash), `7098005` (hang/watchdog), `d32b752` (decel tail restore).

### 2.2. Задачи 030–034 — один класс проблем

| # | Симптом | Корневая ошибка | Слой |
|---|---|---|---|
| **030** | двойной бип на клампе | float bit-equality + `unwrap_or(0.0)` на missing speed | domain math + call-site defaults |
| **031** | `connected=1` 19 мин без телеметрии | «loop alive» ≡ «0x2ACD flows»; relative `timeout` в `select!`; `touch()` на любом `persist()` | liveness model |
| **032** | `tm zone off` не останавливает ramp | hot-reload обновляет **config**, не **phase**; gate смотрел только на phase | config vs session state |
| **033** | снятый H10 → `♥ 111` + мусор в `hr_samples` | link ≡ contact; silent frames ≠ meaningful samples | sensor semantics |
| **034** | data repair | live detection опоздала; один pass ≠ fixpoint | recovery / truth |

**Мета-паттерн:** каждый фикс *добавлял* новое различение (telemetry touch, contact tracker, disengage helper, epsilon), но **не вводил общий каркас** для таких различений. Следующий feature снова смешает соседние понятия. Баги живут в **склейке**, не в чистых модулях.

---

## 3. Системные дефекты архитектуры

### 3.1. God-session в `stream_with_presence`

Один `loop { select! { … } }` + **~20 локальных мутабельных переменных** (presence, speed history, zone phase, auto-pause, HR link/contact/battery, config mtime, …) и **~10 arms**.

Последствия:

- Нет единого `Session` / `SessionEvent` — нельзя unit-тестить orchestration без BLE.
- Любой новый arm (HR, battery, zone, control) **увеличивает комбинаторику** сбросов таймеров и side-effects.
- Invariants размазаны по веткам и комментариям, не по типам.

**Коррекция приоритета:** полный `tick(Event) -> Vec<Effect>` — правильное направление, но **завышенная форма** на первом шаге. Await-эффекты сейчас вложены прямо в `match next_state` (`try_restore_speed`, `try_apply_default_speed` внутри presence-ветки) — вытаскивать всё сразу рискованно. ~90% тестируемости даёт дешевле: свернуть ~20 `mut` в 3–4 структуры состояния (`HrSession`, `ZoneSession`, `AutoPause`, …) с методами + unit-тестами, оставив `select!` тонкой оболочкой. `Event`/`Effect` — потом, если реально понадобится.

### 3.2. Conflated liveness (главный источник 031-класса)

В системе **как минимум 5 «живостей»**, которые исторически смешивались:

| Сигнал | Что значит | Должен кормить |
|---|---|---|
| Event-loop progress | `persist()` / любой arm | `Watchdog::touch` (hang) |
| Treadmill telemetry | decoded `0x2ACD` | `connected`, `touch_telemetry`, widget belt fields |
| HR BLE link | stream open | reconnect / battery |
| HR body contact | meaningful bpm | `hr_samples`, widget ♥, zone bpm input |
| Config intent | `enabled` flags | phase machines (zone, auto-pause) |

**031** смешал 1↔2. **033** смешал 3↔4. **032** смешал 5↔ phase.

После фиксов в коде **ещё живёт relative timeout для HR** в том же `select!`:

```rust
// daemon.rs ~1048–1050
Some(stream) => tokio::time::timeout(HR_NOTIFICATION_TIMEOUT, stream.next()).await,
```

`command_tick` (1s), treadmill frames (~1/s), `config_tick` (5s) пересоздают future **каждый pass** → 10s HR silence deadline **может никогда не наступить**, пока крутится соседний arm. Тот же класс, что 031, только для strap. Ирония: прямо над этой строкой — подробный комментарий про *другую* ловушку `select!` (unwrap в теле future), а эту не заметили.

**Маскировка сегодня:** снятый H10 продолжает слать кадры → «тишины» не бывает. Всплывёт при partial GATT death (линк жив, notify молчит): `hr_notifications` останется `Some` → reconnect не запустится (gate на `hr_notifications.is_none()`, ~1132), `hr_connected` останется `true`. Виджет спасёт `HR_STALE_THRESHOLD_S`, но датчик не вернётся до рестарта сессии.

**Фикс:** `sleep_until(last_hr_at + HR_NOTIFICATION_TIMEOUT)` — как уже для treadmill на ~688 и ~2124. Регрессия: `tokio::time::pause` **обязательно с соседним быстрым arm'ом**, иначе тест ничего не доказывает.

### 3.3. Config reload ≠ state machine apply

Hot-reload (017) — `stat` + field-by-field assignment. **032** доказал: недостаточно скопировать struct.

Нужен явный контракт:

```text
ConfigDelta → apply(session): Vec<Effect>
```

где `Effect` = disengage zone / re-engage / change auto-pause threshold / ignore. Сейчас ad-hoc `if !enabled && phase != Off` + `if Off && Walking` — хрупко, неполно (например, смена `target_zone` / `max_speed` mid-Hold не пересчитывает `ResolvedZone` до следующего tick-path; смена `correction_interval` ок, а `warmup_minutes` mid-Ramp — нет явной политики).

### 3.4. Ownership скорости, не «гонка» Control Point

Пишут в ленту из нескольких *логических* источников:

- `process_control_commands` (CLI queue) — daemon ~1768
- Zone Hold ramp/hold/safety — daemon ~1298
- auto-pause Stop
- resume speed restore
- default-speed on walk start

**Коррекция:** параллельной гонки **нет**. Обе основные точки (`controller.set_speed` zone + CLI queue) живут **внутри одного `select!` в одном таске**. Auto-pause Stop и zone safety Stop физически не могут пересечься. Bounded timeout'ы на write — хорошо.

Реальная проблема — **отсутствие модели владения intent'ом**:

- CLI `tm speed 4.0` mid-Hold → zone через ≤20s молча перебьёт; оператор не поймёт почему.
- Нет `control_source` в логах записи.
- Нет окна подавления zone после явной CLI-команды.

Лечится **не** пятиуровневым арбитром, а ~50 строками:

1. `control_source=zone|cli|auto_pause|restore` на каждом write-логе.
2. Operator-override window: N секунд после CLI speed zone не пишет.

Полный priority-arbiter — YAGNI, пока не появится реальная параллельность писателей или больше конфликтующих политик.

Физическое устройство + beep = пользователь сразу видит конфликт (030) — это про float/clamp, не про race.

### 3.5. Defaults, которые врут (неравномерно)

Повторяющийся антипаттерн: `Option` → `unwrap_or(0.0)` / `unwrap_or(min)` — **но не везде одинаково опасен**.

**Реальные баги / latent (speed):**

- 030: missing speed → 0.0 → «лента встала» — починили внутри `zone_hold_tick`, **engage-пути остались**.
- `zh_effective_speed_kmh = data.speed_kmh.unwrap_or(0.0)` — daemon ~755; ещё два `unwrap_or(0.0)` на ~771 и ~797 (`resumed_speed`). MORE_DATA frame на transition edge может стартовать Ramp с 0. На engage: `None` обязан **пропускать тик**, а не изображать остановленную ленту.

**Karvonen — драма завышена:**

- `resting_hr.unwrap_or(0)` (`zone_hold.rs` ~297) **не** «тихо ломает зоны».
- При `resting = 0` формула Карвонена алгебраически вырождается **ровно в HRmax-проценты**.
- Оператор просит Karvonen → получает HrMax: молча, но **не мусор**.
- Фикс: `WARN` + явный fallback на ~10 строк, не класс 030.

Правило для **физических измерений** (speed, live bpm): **отсутствие измерения ≠ ноль**. `None` обязан short-circuit.  
Правило для **конфиг-параметров зон**: silent algebraic fallback → явный WARN + documented fallback method.

### 3.6. Float domain без wire-квантования

FTMS wire: `u16 * 0.01`. Config: TOML f32. Compare/clamp на raw f32 → 030.

Нужен **один** тип `SpeedKmh` / quantize-to-centi при decode **и** перед compare/write.  
`MIN_SPEED_CHANGE_KMH` остаётся, но уже как **честный deadband контроллера**, а не заплатка на float-сравнение.

### 3.7. God modules + schema accretion

- `daemon_status` раздут ALTER-колонками (goals, auto-pause, hr_*, zone_hold_*, last_speed_*). Каждый feature = новый snapshot field + widget field + status line.
- `store.migrate` = `CREATE IF NOT EXISTS` + `add_column_if_missing`.
- `main.rs` держит CLI + formatting + zone interactive UX + one-shot BLE paths.

**Versioned migrations — YAGNI** (см. Phase 5): однопользовательская локальная SQLite, единственный писатель, `recompute-*` как ground-truth восстановление. `add_column_if_missing` + snapshot-тест схемы закрывают риск; `schema_version` / down-миграции — церемония без потребителя. **Разбиение `store.rs` на файлы — делать.**

### 3.8. Dual sources of truth + non-atomic config write

| Данные | Writer | Reader |
|---|---|---|
| `raw_samples` | daemon | recompute-segments, default-speed |
| JSONL workouts | daemon | human / external |
| `hr_samples` | daemon | stats/widget aggregates, recompute-hr |
| `daemon_status` | daemon | widget/status (2s poll) |
| `config.toml` | CLI line-patch | daemon mtime + CLI status |

JSONL и SQLite не сверяются — приемлемо (разные потребители).

**Config write — реальный риск не half-read:**

- `std::fs::write` в `zone_hold.rs` (~682, ~704, ~769) и `goals.rs` (~335).
- Окно «демон прочитает полуфайл» микросекундное (poll 5s) — **не** главный риск.
- `fs::write` сначала **truncate**: падение между truncate и write **уничтожает конфиг оператора насовсем**.
- Фикс: write-temp + rename — ~3 строки. Делать в Phase 0.

### 3.9. Тестовый перекос

| Слой | Покрытие |
|---|---|
| pure math (zone, hr contact, presence, merge) | хорошо |
| select! / session orchestration | почти нет (2 watchdog tests после 031) |
| multi-writer Control Point | нет (и «multi-writer» — логический, не concurrent; тесты ownership/override) |
| config apply mid-session | unit на helpers, не на full apply |
| BLE integration | live-only |

Баги 031–033 **не ловились** pure unit-тестами до инцидента — они жили в wiring.

### 3.10. Что ещё может сломаться (latent)

1. **HR relative timeout** в `select!` (см. 3.2) — **high**, чинить первым.  
2. **Zone vs CLI speed ownership** (silent override, no log) — medium, UX surprise; не race.  
3. **MORE_DATA speed=None на presence/engage** → ramp start 0 (`unwrap_or(0.0)` ×3) — medium.  
4. **`hr_connected` overload**: contact-loss sets `hr_connected=false` while BLE up — status text «no sensor» vs reality «sensor linked, no contact»; reconnect tick gated on `hr_notifications.is_none()` (ok), но семантика флага перегружена.  
5. **Frozen-bpm 60s window** — live always writes up to ~60s garbage before Lost (034 replay smarter); **осознанный tradeoff**, SLA = «≤60с мусора на снятие», не «~0».  
6. **Non-atomic config write** (truncate risk) — cheap, do in Phase 0.  
7. **Watchdog streaming flag** если `set_streaming(false)` пропущен на error path — check all returns from session.  
8. **Widget TSV contract** (12 fields) — fragile string protocol; already broken once by IFS (029).  
9. **No integration test that daemon_status.connected tracks last 0x2ACD**, only unit on sleep_until.  
10. **Нет `tm doctor`** — диагностика следующего инцидента = log archaeology по 4 источникам.

---

## 4. Принципы целевой архитектуры

1. **One concept → one type / one clock.** Не переиспользовать `connected`, `touch()`, `enabled` для разных смыслов.  
2. **Pure core, thin shell.** Decision functions pure; BLE/SQLite/notify = effects (эффекты можно выносить постепенно).  
3. **Config changes produce explicit effects**, not silent field copies.  
4. **Intent ownership for Control Point** — log `control_source` + operator-override window; full arbiter only if concurrency/policies demand it.  
5. **No silent zero defaults** for physical measurements; config-method fallbacks must WARN.  
6. **Quantized physical units** at the boundary; deadband is control policy, not float glue.  
7. **God modules** — split after state is extracted; mechanical file splits alone don't buy invariants.  
8. **Every incident class gets a regression that would have failed pre-fix.**  
9. **Repair tools are temporary debt signals** — prefer preventing write of bad data; recompute stays for history.  
10. **Cheap diagnostics first** — `tm doctor` / status liveness lines reduce MTTR more than most refactors.

---

## 5. План надёжности (по leverage)

### Phase 0 — Stop the bleeding (1–2 сессии) ⭐ half the value

**Цель:** закрыть known same-class latent bugs + дешёвую диагностику **без** большого refactor. Половина ценности документа — здесь (~100–150 строк + doctor).

| # | Действие | Task | Acceptance |
|---|---|---|---|
| 0.1 | HR silence: `sleep_until(last_hr_at + HR_NOTIFICATION_TIMEOUT)` как у treadmill (031) | [035](../tasks/035-hr-relative-timeout-in-select.md) | unit test: `tokio::time::pause` **+ соседний быстрый arm** (без sibling-arm тест невалиден) |
| 0.2 | Убрать `unwrap_or(0.0)` на **трёх engage-путях** speed (~755, ~771, ~797); `None` → skip | [036](../tasks/036-zone-engage-zero-speed-defaults.md) | MORE_DATA без speed не стартует Ramp с 0; unit/regression |
| 0.3 | Atomic config write: temp + rename в `zone_hold` / `goals` TOML writers | [037](../tasks/037-atomic-config-toml-write.md) | crash mid-write не оставляет empty/truncated config |
| 0.4 | **`tm doctor`**: матрица живости live | [038](../tasks/038-tm-doctor-liveness-matrix.md) | один CLI-вызов заменяет 4 log sources; MTTR↓ |
| 0.5 | Document liveness matrix (§3.2) in `CLAUDE.md` as invariant | (в 038 / 035 PR) | reviewers check new code against matrix |
| 0.6 | Karvonen: `WARN` + explicit fallback to HrMax when `resting_hr` missing | [040](../tasks/040-karvonen-missing-resting-hr-warn.md) | no silent method switch without log |
| 0.7 | Smoke: power-off treadmill with HR worn; remove H10; `tm zone off` mid-ramp | checklist in 035/036/038 | already fixed paths stay green |

`tm doctor` раньше был Phase 7 — **задвинут зря**: единственный пункт, который прямо режет время диагностики *следующего* инцидента, стоит один вечер.

### Phase 1 — Session state extract (лёгкая версия kernel)

**Цель:** тестируемость orchestration без полного Event/Effect rewrite.  
**Backlog:** [005](../backlog/005-session-state-extract.md)

Шаг 1 (делать):

```
// свернуть ~20 mut locals в структуры + методы
HrSession { link, contact, battery, last_frame_at, notifications, … }
ZoneSession { phase, resolved, last_write_at, … }
AutoPause { … }
// (+ Presence/Activity уже частично выделены)
```

- Методы: `on_hr_frame`, `on_silence`, `should_reconnect`, `on_config`, `disengage`, …
- Unit-тесты на transitions **без** btleplug.
- `select!` остаётся оболочкой: arm → method → side-effect calls.

Шаг 2 (опционально, позже — если Step 1 упрётся):

```
src/session/
  mod.rs           // Session, optional tick(Event) -> Vec<Effect>
  liveness.rs
  zone_session.rs
  hr_session.rs
  …
```

Полный `Event`/`Effect` требует вытащить nested awaits (`try_restore_speed`, `try_apply_default_speed` в presence-ветке) — **большая и рискованная** операция. Не блокировать фичи ради неё.

**Acceptance (Step 1):** zone/HR/auto-pause transitions unit-tested; `stream_with_presence` читается как wiring, не god-body of ad-hoc muts.  
**Acceptance (Step 2, if ever):** body ≤ ~300 LOC orchestration + effect executor.

### Phase 2 — Control intent (не arbiter)

**Task:** [039](../tasks/039-control-source-and-operator-override.md)

**Не** пятиуровневый priority scheduler. **Да:**

1. Log every Control Point write with `control_source=zone|cli|auto_pause|restore`.  
2. Operator-override window: после CLI speed zone не пишет N секунд (конфиг/const).  
3. (Optional later) coalesce writes within quantize/deadband.

**Acceptance:** CLI speed mid-Hold либо держится N секунд, либо override **виден в логе**; unit tests на window + source tags.  
Полный arbiter — только если появятся concurrent writers или >2 конфликтующих политик, которые window не закрывает.

### Phase 3 — Typed config apply

**Backlog:** [008](../backlog/008-typed-config-apply.md)

- `DaemonConfig` single struct (goals + auto_pause + zone_hold + …).  
- `reload_if_changed() -> Option<ConfigDelta>`.  
- `session.apply_config(delta)` owns 032-class logic (живёт на `ZoneSession` / peers из Phase 1).  
- Atomic write уже из Phase 0.

**Acceptance:** table-driven tests: enabled↓, enabled↑ mid-walk, target zone change, age removed → disengage, etc.

### Phase 4 — Physical units & parsing boundary

**Backlog:** [006](../backlog/006-speed-quantize-newtype.md)

- `struct CentiKmh(u16)` or newtype `Speed` with `from_wire` / `from_config` / `to_wire`.  
- Compare and clamp only in that type.  
- `MIN_SPEED_CHANGE_KMH` = deadband контроллера, не float-patch.

**Acceptance:** 030-style test is about quantize identity + deadband policy, not magic 0.05 alone.

### Phase 5 — Split store / CLI / status contract

**Backlog:** [007](../backlog/007-split-god-modules.md)

| Split | Content |
|---|---|
| `store/schema.rs` | CREATE + `add_column_if_missing` + **schema snapshot test** |
| `store/samples.rs` | raw/hr inserts |
| `store/activity.rs` | segments, credit, merge |
| `store/status.rs` | daemon_status DTO |
| `store/control_queue.rs` | commands |
| `cli/` or `commands/` | one file per command group |
| `widget.rs` | TSV contract + field formatters |

**Не делать versioned migrations / down / `schema_version` table** — YAGNI для single-user sole-writer SQLite + `recompute-*`. Snapshot-тест схемы + `add_column_if_missing` достаточно.

Widget: version field **или** fixed-width documented schema test (`assert_eq!(fields.len(), 12)` + golden lines).

Разбиение файлов — **механическое, после** извлечения состояния (Phase 1 Step 1); иначе просто перемещаем god.

### Phase 6 — Test strategy that matches failure modes

| Kind | Tool |
|---|---|
| Pure domain | existing unit tests — keep |
| Time/select | `tokio::time::pause` (031) — expand to HR (**+ sibling arm**), override window |
| Session state structs | fake clocks + method-level scripts (Phase 1) |
| Store | `:memory:` (already) + schema snapshot; golden old DB only if migrate breaks |
| Contract | widget TSV / status golden |
| Live soak | scripted checklist (power off, strap off, zone off, AC unplug) in `docs/tasks/` — not CI |

**Definition of done for any future bugfix:**  
(1) pure regression if domain; (2) session-method / select-script regression if wiring; (3) task doc root-cause class tagged (`liveness` / `config-apply` / `control-intent` / `units` / …).

### Phase 7 — Observability (continuous, not a dump)

- Structured fields already good (`tracing`); add **stable tags**: `liveness_domain=telemetry|hr_link|hr_contact|loop`, `control_source=…` (часть уже из Phase 2).  
- Rate-limit + transition-only logs (already for contact Lost) as pattern for all state machines.  
- `tm doctor` — **уже Phase 0.4**, не ждать «obs phase».

---

## 6. Порядок внедрения (рекомендуемый)

```
Phase 0  (latent bugs + doctor + atomic write)  ──► ship immediately; max leverage/$
Phase 1  (state structs, light kernel)          ──► before large new features; not full Effect rewrite
Phase 2  (control_source + override window)     ──► before more zone/auto features
Phase 3  (config apply)                         ──► with / after state structs
Phase 4  (units)                                ──► can parallel with 2–3 once state extracted
Phase 5  (file splits + schema snapshot)        ──► mechanical after 1; no versioned migrations
Phase 6–7 (tests/obs tags)                      ──► continuous, not a big-bang
```

**Правило для новых arms в `select!` (смягчённое):**  
не жёсткий запрет «никаких arms до Phase 1» — мы всё равно нарушим при первой фиче.  
Вместо этого: **никаких новых arms без явного ответа** в PR/task:

1. Какой это **liveness domain** (таблица §3.2)?  
2. Кто **владеет скоростью** / пишет в Control Point?  
3. Есть ли regression (pause clock / state-method test)?

---

## 7. Что *не* делать

- Не «переписать на actors/ECS» — overkill.  
- Не полный `Event`/`Effect` kernel **до** извлечения state structs — риск без пропорциональной отдачи.  
- Не пятиуровневый Control Point arbiter, пока нет concurrent writers — `control_source` + override window.  
- Не versioned / down migrations для local sole-writer SQLite.  
- Не выносить BLE в отдельный процесс — sole-owner model (013) правильный.  
- Не дублировать detection в `main`/widget — daemon is single writer of truth (033 lesson).  
- Не удалять `recompute-*` — ground-truth repair; но каждый новый recompute = smell, что live path отстаёт.  
- Не смешивать Life OS (ADR 0002) — public contracts only.  
- Не ставить цель «0 bad HR samples forever» — противоречит 60s frozen-bpm detector SLA.

---

## 8. Метрики «стало надёжнее»

| Метрика | Сейчас (сигнал) | Цель |
|---|---|---|
| `fix(*)` commits / week touching daemon wiring | высокий (030–033 cluster) | спад; новые fix — в pure / state modules |
| LOC `daemon.rs` god-body | 2365 🔴 | state extracted; wiring readable; file split after |
| Same-class bug recurrence | 031-style still latent in HR | 0 open same-class (HR sleep_until shipped) |
| Incident → regression test | partial | 100% for wiring bugs (pause + sibling arm where relevant) |
| Time-to-diagnose | log archaeology | `tm doctor` / status liveness lines |
| Bad HR samples on strap-off | up to ~60s (detector window) + historical 1749 deleted (034) | **≤60s garbage per removal** (SLA = detector window), not ~0 |

---

## 9. Краткий вердикт

Проект **не «плохой код»** — pure islands (`presence`, `activity`, `hr::ContactTracker`, `zone_hold::next_speed`) и docs-first дисциплина сильные.  
Хрупкость — в **склейке**: один раздутый session-loop, перегруженные флаги живости, config-without-apply, **intent ownership** скорости (не race), float/Option defaults на engage.

Кластер 030–034 — не «неудачная неделя», а **системный feedback**: каждый новый sensor/feature без явной модели времени, intent и ownership ломает соседний инвариант.

**Главный рычаг (пересмотрен):**  
**Phase 0** (HR `sleep_until` + engage zero-defaults + atomic config write + **`tm doctor`**) даёт половину ценности за сотню строк.  
Дальше — control intent logs/window, лёгкое извлечение state structs, quantize на decode boundary.  
Полный Session kernel + arbiter + versioned migrations — правильные *направления* с **завышенной формой**; не блокировать ими дешёвые фиксы.

---

## 10. Review errata (что изменилось vs draft)

| Claim в draft | Verdict review | Решение в этом файле |
|---|---|---|
| Control Point «гонка» auto-pause vs zone | **Нет гонки** — один `select!` task | §3.4 → ownership/intent; Phase 2 → logs + override window |
| Karvonen `resting=0` «ломает зоны» | Алгебраически = HrMax, не мусор | §3.5 + Phase 0.6 WARN fallback |
| Bad samples → ~0 | Противоречит 60s frozen-bpm | §8 → ≤60s SLA |
| Versioned migrations Phase 5 | YAGNI sole-writer SQLite | Phase 5 → split + schema snapshot only |
| Full `tick(Event)->Effect` Phase 1 | 90% value from state structs | Phase 1 Step 1/2 |
| `tm doctor` Phase 7 | Занижен; MTTR lever | Phase 0.4 |
| Hard ban new select! arms | Всё равно нарушим | §6 → explicit liveness/owner questions |
| Half-file TOML read risk | Реальный risk = truncate wipe | §3.8 + Phase 0.3 atomic write |

Подтверждено буквально: HR relative timeout (§3.2), engage `unwrap_or(0.0)` ×3, god-module sizes, `std::fs::write` config paths, meta-диагноз «баги в склейке».

---

## 11. Связанные артефакты

- **Reliability tasks (from this review):**  
  - open: [035](../tasks/035-hr-relative-timeout-in-select.md) HR silence, [036](../tasks/036-zone-engage-zero-speed-defaults.md) engage zeros, [037](../tasks/037-atomic-config-toml-write.md) atomic config, [038](../tasks/038-tm-doctor-liveness-matrix.md) doctor, [039](../tasks/039-control-source-and-operator-override.md) control intent, [040](../tasks/040-karvonen-missing-resting-hr-warn.md) Karvonen WARN  
  - backlog: [005](../backlog/005-session-state-extract.md) state extract, [006](../backlog/006-speed-quantize-newtype.md) quantize, [007](../backlog/007-split-god-modules.md) file splits, [008](../backlog/008-typed-config-apply.md) config apply  
- Prior tasks: `030`–`034`, also `007`, `013`, `015`, `017`, `018`, `020`, `025`–`029`  
- ADR: `0001` (no OS pair), `0002` (Life OS boundary)  
- Research: `001` protocol, `002` Polar H10  
- Code anchors:  
  - `src/daemon.rs` — `stream_with_presence`, HR `timeout` ~1050, engage unwraps ~755/771/797, treadmill `sleep_until` ~688/2124, zone set_speed ~1298, CLI queue ~1768, reconnect gate ~1132  
  - `src/hr.rs`, `src/zone_hold.rs` (resting_hr ~297, fs::write ~682+), `src/goals.rs` (~335), `src/activity.rs`, `src/store.rs`, `src/main.rs`
