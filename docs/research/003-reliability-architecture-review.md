# 003 — Reliability & Architecture Review

> **Дата:** 2026-07-09  
> **Тип:** research / code review  
> **Скоуп:** весь репозиторий `treadmill-bluetooth-macos`  
> **Триггер:** кластер багов 030–034 + рост `fix(*)` после Zone Hold / HR  
> **Цель:** понять *почему* баги множатся, где архитектура не держит инварианты, и дать компактный план, как сделать систему надёжной.

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

**Что уже сделано правильно (не ломать):**

1. **Replay = prod engine** — `ActivityAccumulator` / `ContactTracker` / `recompute-*` (015, 034).  
2. **Время инъекцией** — `presence`, `ContactTracker::observe(&m, ts_ms)`, `zone_hold::next_speed`.  
3. **Daemon sole owner of BLE** — CLI → `control_commands` (013).  
4. **Bounded BLE awaits + process-exit watchdog** (007, 018, 031).  
5. **Документация-first** + task docs с root-cause, не только симптомами.

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

**Мета-паттерн:** каждый фикс *добавлял* новое различение (telemetry touch, contact tracker, disengage helper, epsilon), но **не вводил общий каркас** для таких различений. Следующий feature снова смешает соседние понятия.

---

## 3. Системные дефекты архитектуры

### 3.1. God-session в `stream_with_presence`

Один `loop { select! { … } }` + **~20 локальных мутабельных переменных** (presence, speed history, zone phase, auto-pause, HR link/contact/battery, config mtime, …) и **~10 arms**.

Последствия:

- Нет единого `Session` / `SessionEvent` — нельзя unit-тестить orchestration без BLE.
- Любой новый arm (HR, battery, zone, control) **увеличивает комбинаторику** сбросов таймеров и side-effects.
- Invariants размазаны по веткам и комментариям, не по типам.

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

`command_tick` (1s), treadmill frames (~1/s), `config_tick` (5s) пересоздают future **каждый pass** → 10s HR silence deadline **может никогда не наступить**, пока крутится соседний arm. Тот же класс, что 031, только для strap. Сейчас маскируется: H10 off-body *продолжает* слать кадры; баг всплывёт при «линк есть, notify молчит» (partial GATT death / OS stall).

### 3.3. Config reload ≠ state machine apply

Hot-reload (017) — `stat` + field-by-field assignment. **032** доказал: недостаточно скопировать struct.

Нужен явный контракт:

```text
ConfigDelta → apply(session): Vec<Effect>
```

где `Effect` = disengage zone / re-engage / change auto-pause threshold / ignore. Сейчас ad-hoc `if !enabled && phase != Off` + `if Off && Walking` — хрупко, неполно (например, смена `target_zone` / `max_speed` mid-Hold не пересчитывает `ResolvedZone` до следующего tick-path; смена `correction_interval` ок, а `warmup_minutes` mid-Ramp — нет явной политики).

### 3.4. Много писателей в Control Point, нет арбитра

Пишут в ленту независимо:

- `process_control_commands` (CLI queue)
- Zone Hold ramp/hold/safety
- auto-pause Stop
- resume speed restore
- default-speed on walk start

Каждый bounded timeout'ом (хорошо), но **нет единого write-scheduler**:

- CLI `tm speed 4.0` mid-Hold → zone через ≤20s перебьёт.
- auto-pause Stop vs zone safety Stop — гонка.
- Нет «intent owner» (кто сейчас владеет скоростью: operator / zone / restore).

Физическое устройство + beep = пользователь сразу видит конфликт (030).

### 3.5. Defaults, которые врут

Повторяющийся антипаттерн: `Option` → `unwrap_or(0.0)` / `unwrap_or(min)`.

- 030: missing speed → 0.0 → «лента встала».
- zone transition: `data.speed_kmh.unwrap_or(0.0)` всё ещё на engage path (`zh_effective_speed_kmh`) — MORE_DATA frame на transition edge может стартовать Ramp с 0.
- Karvonen: `resting_hr.unwrap_or(0)` тихо ломает зоны, если resting не задан, но method=karvonen.

Правило: **отсутствие измерения ≠ ноль**. `None` обязан short-circuit.

### 3.6. Float domain без wire-квантования

FTMS wire: `u16 * 0.01`. Config: TOML f32. Compare/clamp на raw f32 → 030.

Нужен **один** тип `SpeedKmh` / quantize-to-centi при decode **и** перед compare/write. Epsilon `MIN_SPEED_CHANGE_KMH` — patch, не модель.

### 3.7. God modules + schema accretion

- `daemon_status` раздут ALTER-колонками (goals, auto-pause, hr_*, zone_hold_*, last_speed_*). Каждый feature = новый snapshot field + widget field + status line.
- `store.migrate` = `CREATE IF NOT EXISTS` + `add_column_if_missing`. Нет versioned migrations, нет down, нет schema snapshot tests.
- `main.rs` держит CLI + formatting + zone interactive UX + one-shot BLE paths.

### 3.8. Dual sources of truth

| Данные | Writer | Reader |
|---|---|---|
| `raw_samples` | daemon | recompute-segments, default-speed |
| JSONL workouts | daemon | human / external |
| `hr_samples` | daemon | stats/widget aggregates, recompute-hr |
| `daemon_status` | daemon | widget/status (2s poll) |
| `config.toml` | CLI line-patch | daemon mtime + CLI status |

JSONL и SQLite не сверяются. Line-based TOML upsert (`upsert_zone_hold_keys`, `upsert_top_level_key`, `replace_zones`) гоняется с ручным edit и hot-reload — риск битого TOML mid-write (нет atomic rename-everywhere policy явно).

### 3.9. Тестовый перекос

| Слой | Покрытие |
|---|---|
| pure math (zone, hr contact, presence, merge) | хорошо |
| select! / session orchestration | почти нет (2 watchdog tests после 031) |
| multi-writer Control Point | нет |
| config apply mid-session | unit на helpers, не на full apply |
| BLE integration | live-only |

Баги 031–033 **не ловились** pure unit-тестами до инцидента — они жили в wiring.

### 3.10. Что ещё может сломаться (latent)

1. **HR relative timeout** в `select!` (см. 3.2) — high.  
2. **Zone vs CLI speed ownership** — medium, UX surprise.  
3. **MORE_DATA speed=None на presence transition** → ramp start 0 — low/medium.  
4. **`hr_connected` overload**: contact-loss sets `hr_connected=false` while BLE up — status text «no sensor» vs reality «sensor linked, no contact»; reconnect tick gated on `hr_notifications.is_none()` (ok), но семантика флага перегружена.  
5. **Frozen-bpm 60s window** — live still writes up to ~60s garbage before Lost (034 replay smarter than live); acceptable tradeoff, document as SLA.  
6. **Watchdog streaming flag** если `set_streaming(false)` пропущен на error path — check all returns from session.  
7. **Widget TSV contract** (12 fields) — fragile string protocol; already broken once by IFS (029).  
8. **No integration test that daemon_status.connected tracks last 0x2ACD**, only unit on sleep_until.

---

## 4. Принципы целевой архитектуры

1. **One concept → one type / one clock.** Не переиспользовать `connected`, `touch()`, `enabled` для разных смыслов.  
2. **Pure core, thin shell.** Decision functions pure; BLE/SQLite/notify = effects.  
3. **Config changes produce explicit effects**, not silent field copies.  
4. **Single Control Point arbiter** with priority + last-intent.  
5. **No silent zero defaults** for physical quantities.  
6. **Quantized physical units** at the boundary.  
7. **God modules forbidden** by the project size rule (🔴 >1000 LOC) — split now.  
8. **Every incident class gets a regression that would have failed pre-fix.**  
9. **Repair tools are temporary debt signals** — prefer preventing write of bad data; recompute stays for history.

---

## 5. План надёжности (по leverage)

### Phase 0 — Stop the bleeding (1–2 сессии)

**Цель:** закрыть known same-class latent bugs без большого refactor.

| # | Действие | Acceptance |
|---|---|---|
| 0.1 | HR silence: `sleep_until(last_hr_at + HR_NOTIFICATION_TIMEOUT)` как у treadmill (031) | unit test с paused clock + 1s sibling arm |
| 0.2 | Audit all `unwrap_or(0.0)` on speed/bpm paths in daemon | none left on measurement paths; `None` skips |
| 0.3 | Document liveness matrix (table §3.2) in `CLAUDE.md` as invariant | reviewers check new code against matrix |
| 0.4 | Smoke: power-off treadmill with HR worn; remove H10; `tm zone off` mid-ramp | already fixed paths stay green |

### Phase 1 — Session kernel extract (architecture backbone)

**Цель:** вынуть orchestration из 2.3kLOC `daemon.rs`.

```
src/session/
  mod.rs           // Session struct, tick(Event) -> Vec<Effect>
  liveness.rs      // TelemetryClock, LoopClock, HrLinkClock, HrContact
  presence_wire.rs // glue ActivityAccumulator + toasts intents
  zone_session.rs  // ZoneHoldPhase + should_run + disengage + apply_config
  hr_session.rs    // link/contact/battery state machine
  control_arbiter.rs
```

- `Event` = `TreadmillFrame | HrFrame | ConfigReloaded | Power | CommandTick | …`
- `Effect` = `BleWrite(…) | PersistStatus | Toast | InsertSample | ExitSession`
- `daemon.rs` becomes: open BLE → loop select! → `session.handle` → execute effects.

**Acceptance:** `stream_with_presence` body ≤ ~300 LOC; zone/HR/auto-pause transitions unit-tested without btleplug.

### Phase 2 — Control Point arbiter

**Приоритеты (пример):**

1. Safety (zone hard-stop / safety cap)  
2. Operator explicit (`control_commands` start/stop/speed)  
3. Auto-pause stop  
4. Zone hold correction  
5. Restore / default-speed (one-shot)

- Coalesce writes within ε / quantize.  
- Log owner of every write (`source=zone|cli|auto_pause|restore`).  
- Optional: suppress zone for N seconds after CLI speed (operator override window).

**Acceptance:** no double-beep on clamp (already); CLI speed not immediately overwritten without log; unit tests on priority.

### Phase 3 — Typed config apply

- `DaemonConfig` single struct (goals + auto_pause + zone_hold + …).  
- `reload_if_changed() -> Option<ConfigDelta>`.  
- `session.apply_config(delta)` owns 032-class logic.  
- Atomic write helper for TOML (`write temp + rename`).

**Acceptance:** table-driven tests: enabled↓, enabled↑ mid-walk, target zone change, age removed → disengage, etc.

### Phase 4 — Physical units & parsing boundary

- `struct CentiKmh(u16)` or newtype `Speed` with `from_wire` / `from_config` / `to_wire`.  
- Compare and clamp only in that type.  
- Remove ad-hoc epsilons where quantization supersedes them (keep min-step for control loop separately).

**Acceptance:** 030-style test is about quantize identity, not magic 0.05 alone.

### Phase 5 — Split store / CLI / status contract

| Split | Content |
|---|---|
| `store/schema.rs` | migrations, version |
| `store/samples.rs` | raw/hr inserts |
| `store/activity.rs` | segments, credit, merge |
| `store/status.rs` | daemon_status DTO |
| `store/control_queue.rs` | commands |
| `cli/` or `commands/` | one file per command group |
| `widget.rs` | TSV contract + field formatters |

- Versioned migrations (`schema_version` table) instead of only `ADD COLUMN` soup.  
- Widget: consider version field or fixed-width documented schema test (`assert_eq!(fields.len(), 12)` + golden lines).

### Phase 6 — Test strategy that matches failure modes

| Kind | Tool |
|---|---|
| Pure domain | existing unit tests — keep |
| Time/select | `tokio::time::pause` (already in 031) — expand to HR, config apply |
| Session kernel | fake clocks + recorded Event scripts |
| Store | `:memory:` (already) + migration from golden old DB fixture |
| Contract | widget TSV / status golden |
| Live soak | scripted checklist (power off, strap off, zone off, AC unplug) in `docs/tasks/` or CONTRIBUTING — not CI |

**Definition of done for any future bugfix:**  
(1) pure regression if domain; (2) session-script regression if wiring; (3) task doc root-cause class tagged (`liveness` / `config-apply` / `write-arbiter` / `units` / …).

### Phase 7 — Observability for the next incident

- Structured fields already good (`tracing`); add **stable tags**: `liveness_domain=telemetry|hr_link|hr_contact|loop`, `control_source=…`.  
- Rate-limit + transition-only logs (already for contact Lost) as pattern for all state machines.  
- Optional: `tm doctor` — print liveness matrix live (last 0x2ACD age, last HR frame age, contact, streaming flag, zh_phase vs enabled). Faster than reading 4 log sources.

---

## 6. Порядок внедрения (рекомендуемый)

```
Phase 0  (latent same-class)     ──► ship immediately
Phase 1  (session kernel)        ──► unlocks all else; do before new features
Phase 2  (write arbiter)         ──► before more zone/auto features
Phase 3  (config apply)          ──► with kernel
Phase 4  (units)                 ──► can parallel with 2–3 once kernel exists
Phase 5  (file splits)           ──► mechanical after 1
Phase 6–7 (tests/obs)            ──► continuous, not a big-bang
```

**Запрет до Phase 1:** новые feature-ветки в `daemon.rs` select! (ещё HR modes, incline, LED, multi-device) — каждый +arm без kernel ≈ +1 incident class.

---

## 7. Что *не* делать

- Не «переписать на actors/ECS» — overkill; достаточно `Session::handle` + effects.  
- Не выносить BLE в отдельный процесс — sole-owner model (013) правильный.  
- Не дублировать detection в `main`/widget — daemon is single writer of truth (033 lesson).  
- Не удалять `recompute-*` — они ground-truth repair; но каждый новый recompute = smell, что live path отстаёт.  
- Не смешивать Life OS (ADR 0002) — public contracts only.

---

## 8. Метрики «стало надёжнее»

| Метрика | Сейчас (сигнал) | Цель |
|---|---|---|
| `fix(*)` commits / week touching daemon wiring | высокий (030–033 cluster) | спад; новые fix — в pure modules |
| LOC `daemon.rs` | 2365 🔴 | <500 orchestration + session/* |
| Same-class bug recurrence | 031-style still latent in HR | 0 open same-class |
| Incident → regression test | partial | 100% for wiring bugs |
| Time-to-diagnose (status/doctor) | log archaeology | `tm doctor` / status liveness lines |
| Bad samples written then repaired | 1749 hr rows deleted (034) | ~0 going forward |

---

## 9. Краткий вердикт

Проект **не «плохой код»** — pure islands (`presence`, `activity`, `hr::ContactTracker`, `zone_hold::next_speed`) и docs-first дисциплина сильные.  
Хрупкость — в **склейке**: один раздутый session-loop, перегруженные флаги живости, config-without-apply, многоголосый Control Point, float/Option defaults.

Кластер 030–034 — не «неудачная неделя», а **системный feedback**: каждый новый sensor/feature без явной модели времени, intent и ownership ломает соседний инвариант.

**Главный рычаг:** Phase 0 (HR deadline + kill zero-defaults) + Phase 1 (Session kernel). Всё остальное — следствие.

---

## 10. Связанные артефакты

- Tasks: `030`–`034`, также `007`, `013`, `015`, `017`, `018`, `020`, `025`–`029`  
- ADR: `0001` (no OS pair), `0002` (Life OS boundary)  
- Research: `001` protocol, `002` Polar H10  
- Code anchors: `src/daemon.rs` (`stream_with_presence`, `Watchdog`, `ZoneHoldPhase`), `src/hr.rs`, `src/zone_hold.rs`, `src/activity.rs`, `src/store.rs`, `src/main.rs`
