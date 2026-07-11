# 053 — Session state extract: ~20 mut-locals `stream_with_presence` → 4 структуры

> **Статус:** запланировано (реализация — в отдельной сессии, см. §Sequencing)
> **Источник:** backlog [005](../backlog/005-session-state-extract.md), research [003](../research/003-reliability-architecture-review.md) Phase 1
> **Скоуп кода:** `src/daemon.rs` (2754 строки на момент планирования, anchor-коммит `2e2bb1a`) + 4 новых файла
> **Non-goal:** полный `tick(Event) -> Vec<Effect>` кернел (Step 2 backlog 005) — НЕ делать

## Цель

Свернуть ~20 `mut` locals тела `stream_with_presence` в 4 структуры состояния с
методами и unit-тестами, чтобы:

- zone / HR / auto-pause переходы тестировались **без btleplug** (fake clock +
  method-level скрипты);
- `stream_with_presence` читалось как wiring: arm → метод структуры → существующие
  side-effect вызовы;
- каждый liveness-домен (матрица в `CLAUDE.md`) имел **своего** владельца-структуру
  и свои часы — новая фича не сможет молча переиспользовать чужой сброс таймера
  (мета-паттерн багов 030–034).

Поведение **бит-в-бит идентично**: это перенос состояния и решений, не изменение
логики. Ни одного нового лога/порога/ветки, кроме оговорённых в §Snapshot-ordering.

## Sequencing — ЖЁСТКОЕ требование

Реализация стартует **только после мержа в main** двух параллельных задач:

1. **051 — BLE scan auto-recover** (panic hook + adapter recycle в scan-цикле
   `run()`): добавляет streak-счётчик неудачных сканов. Это состояние живёт в
   scope `run()`, не `stream_with_presence` — 053 его **не трогает**, но дизайн
   `TreadmillLink` оставляет место (см. §TreadmillLink, замечание о scan health):
   если 051 оформит streak как маленькую структуру — оставить как есть; если как
   голые locals в `run()` — свернуть их в `TreadmillLink`/`ScanHealth` отдельным
   коммитом в хвосте этой задачи.
2. **052 — Typed config apply** (`ConfigDelta` / `apply_config` в reload-ветке
   `config_tick`): владелец `goals_mtime` и всей reload-механики. Методы
   `on_config`/`apply` структур 053 (см. ниже) — **приёмники** дельты 052:
   сигнатуры проектируются под неё заранее. Если типы 052 к моменту реализации
   отличаются от эскизов здесь — адаптировать эскиз, не 052.

Причина: обе задачи меняют те же регионы `daemon.rs`; extract поверх их диффа
дешевле, чем rebase их поверх extract'а. Все line-anchors ниже — по `2e2bb1a`,
после мержа 051/052 строки уедут — ориентироваться по именам locals.

## Inventory: local → struct.field

Полный список `mut` locals тела `stream_with_presence` (строки 621–705 @ `2e2bb1a`)
и их судьба. «Shell» = остаётся голым local в `stream_with_presence`.

| # | Local (строка) | Тип | Куда |
|---|---|---|---|
| 1 | `last_telemetry_at` (702) | `tokio::time::Instant` | `TreadmillLink.last_telemetry_at` |
| 2 | `speed_history` (648) | `VecDeque<(Instant, f32)>` | `TreadmillLink.speed_history` |
| 3 | `last_walking_speed` (649) | `Option<f32>` | `TreadmillLink.last_walking_speed` |
| 4 | `pre_pause_speed` (650) | `Option<f32>` | `TreadmillLink.pre_pause_speed` |
| 5 | `paused_since` (633) | `Option<Instant>` | `TreadmillLink.paused_since` |
| 6 | `default_speed_applied` (654) | `bool` | `TreadmillLink.default_speed_applied` |
| 7 | `away_since` (632) | `Option<Instant>` | `AutoPause.away_since` |
| 8 | `auto_pause_fired` (639) | `bool` | `AutoPause.fired` |
| 9 | `auto_pause_last_attempt` (640) | `Option<Instant>` | `AutoPause.last_attempt` |
| 10 | `zh_phase` (658) | `ZoneHoldPhase` | `ZoneSession.phase` |
| 11 | `zh_last_correction_at` (659) | `Option<Instant>` | `ZoneSession.last_correction_at` |
| 12 | `zh_last_safety_write_at` (660) | `Option<Instant>` | `ZoneSession.last_safety_write_at` |
| 13 | `operator_override_until` (662) | `Option<Instant>` | `ZoneSession.operator_override_until` |
| 14 | `last_hr_at` (705) | `tokio::time::Instant` | `HrSession.last_hr_at` |
| 15 | `hr_contact_tracker` (697) | `hr::ContactTracker` | `HrSession.contact_tracker` |
| 16 | `hr_contact` (698) | `hr::Contact` | `HrSession.contact` |
| 17 | `hr_battery_pct` (690) | `Option<u8>` | `HrSession.battery_pct` |
| 18 | `hr_battery_last_read` (691) | `Option<Instant>` | `HrSession.battery_last_read` |
| 19 | `hr_connect_in_flight` (681) | `bool` | `HrSession.connect_in_flight` |
| 20 | `hr_connect_started_at` (682) | `Instant` | `HrSession.connect_started_at` |
| — | `hr_notifications` (680) | `Option<HrNotificationStream>` | **Shell** (BLE handle); `HrSession.link_up: bool` — зеркало `is_some()` |
| — | `hr_peripheral` (679) | `Option<Peripheral>` | **Shell** (BLE handle) |
| — | `goals_mtime` (670) | `Option<SystemTime>` | **052** (reload-механика владеет mtime) |
| — | `command_tick` / `config_tick` / `hr_reconnect_tick` / `hr_battery_check_tick` | intervals | **Shell** (arm-механика `select!`) |
| — | `hr_tx`/`hr_rx` (678) | mpsc channel | **Shell** |
| — | `accumulator` (628) | `ActivityAccumulator` | **Shell** (уже извлечённый engine, задача 015 — не трогать) |
| — | `logger` (622), `session_id` (621) | | **Shell** |
| — | `notifications` (606) | stream дорожки | **Shell** (BLE handle) |

Итого: **20 mut-locals → 4 структуры**; в shell остаются только BLE handles,
intervals, канал, engine и I/O.

`zh_effective_speed_kmh` (817) — внутренний local одной ветки, не session state,
остаётся на месте.

## Liveness matrix → владельцы (сверка с CLAUDE.md)

| Сигнал | Часы | Владелец после extract |
|---|---|---|
| Event-loop progress | `Watchdog::touch` через `persist()` | **Shell** + `Watchdog` (уже извлечён, не трогать) |
| Treadmill telemetry | `last_telemetry_at` (+ `touch_telemetry` на call site) | `TreadmillLink` |
| HR BLE link | `last_hr_at`, `link_up`, connect latch, battery | `HrSession` |
| HR body contact | `contact_tracker` / `contact` | `HrSession` (отдельные поля — link ≠ contact, задача 033) |
| Config intent | `enabled`-флаги | `ZoneSession::apply_config` / `AutoPause` (threshold прокидывается параметром) — приёмники `ConfigDelta` из 052 |

Инварианты, которые структуры обязаны держать типом/методом, а не комментарием:

- `hr_connected=false ⇒ last_bpm=None` — единственная точка записи в snapshot
  `DaemonState` для HR — методы `HrSession` (сегодня рассыпано:
  `clear_hr_link_state` + две inline-ветки contact).
- Абсолютные `sleep_until`-дедлайны — только `silence_deadline()` у
  `TreadmillLink`/`HrSession`; сырые поля наружу не торчат.
- Zone Hold пишет в Control Point только при `enabled && phase != Off` — гейт
  внутри `ZoneSession::tick` (сегодня три рубежа задачи 032; они сохраняются,
  просто два из них становятся методами одной структуры).

## Два типа часов — принято как есть

`last_telemetry_at`/`last_hr_at` — `tokio::time::Instant` (нужен `sleep_until` +
`tokio::time::pause` в тестах); всё остальное — `std::time::Instant` (арифметика
c `cruising_speed`, cooldown'ами). **Не унифицировать** в этой задаче: методы
принимают время параметром (`now: Instant` / `tokio_now: tokio::time::Instant`),
внутри методов `*::now()` не зовётся — тот же приём, что `presence.rs` и
`hr::ContactTracker::observe(&m, ts_ms)`. Где нужны оба (телеметрия-тик) —
передаются оба, это честнее скрытой конверсии.

## Целевые структуры

Каждая — свой новый файл (плоско в `src/`, как остальные модули; каталог
`src/session/` — только если дойдёт до Step 2, не сейчас). Логи (`info!`/`warn!`)
переезжают внутрь методов вместе с решениями — tracing не мешает тестируемости.
Методы, пишущие snapshot, принимают `&mut DaemonState` (чистая in-memory
структура, для тестов — `DaemonState::new(true)`).

### 1. `src/auto_pause.rs` — `AutoPause`

Самая маленькая и независимая. Поглощает `auto_pause_due` + `away_duration`
(вместе с их тестами).

```rust
pub struct AutoPause {
    away_since: Option<Instant>,
    fired: bool,
    last_attempt: Option<Instant>,
}

impl AutoPause {
    pub fn new() -> Self;
    /// Presence → AwayWhileRunning: армит свежий spell (сброс fired/last_attempt).
    pub fn on_away(&mut self, now: Instant);
    /// Возврат Walking: отдаёт длительность для toast'а (back-dated на
    /// presence::AWAY_THRESHOLD — бывший away_duration) и гасит spell.
    pub fn on_return(&mut self, now: Instant) -> Option<Duration>;
    /// Честное «сколько лента крутится без меня» для решения/лога.
    pub fn away_for(&self, now: Instant) -> Option<Duration>;
    /// Бывший auto_pause_due: threshold из config (None = выключено).
    pub fn due(&self, threshold: Option<Duration>, now: Instant) -> bool;
    pub fn on_pause_ok(&mut self);              // fired = true
    pub fn on_pause_failed(&mut self, now: Instant); // last_attempt = Some(now)
    /// Для подавления generic "Paused" toast (строка 893).
    pub fn fired(&self) -> bool;
}
```

Shell-ветка сохраняет сам bounded `execute_control_command(..., AutoPause)` await.

### 2. `src/treadmill_link.rs` — `TreadmillLink`

Владелец телеметрических часов и speed-памяти. Поглощает `cruising_speed`
(+ тесты; `speed_restore_target` остаётся при `try_restore_speed` — это политика
restore, не состояние линка).

```rust
pub struct TreadmillLink {
    last_telemetry_at: tokio::time::Instant,
    speed_history: VecDeque<(Instant, f32)>,
    last_walking_speed: Option<f32>,
    pre_pause_speed: Option<f32>,
    paused_since: Option<Instant>,
    default_speed_applied: bool,
}

impl TreadmillLink {
    pub fn new(tokio_now: tokio::time::Instant) -> Self;
    /// Каждый декодированный 0x2ACD: двигает silence-якорь, ведёт history
    /// (push + prune по SPEED_HISTORY_RETENTION) и last_walking_speed.
    /// touch_telemetry() остаётся на call site — часы watchdog'а не наши.
    pub fn on_telemetry(&mut self, speed_kmh: Option<f32>,
                        now: Instant, tokio_now: tokio::time::Instant);
    /// last_telemetry_at + NOTIFICATION_TIMEOUT — для sleep_until arm.
    pub fn silence_deadline(&self) -> tokio::time::Instant;
    /// Walking→Paused: paused_since = now, pre_pause = cruising_speed(...)
    ///   .or(last_walking_speed).
    pub fn on_pause(&mut self, now: Instant);
    /// Paused→Walking: (paused_for, pre_pause) одним изъятием — двойного
    /// take() по разным веткам, как сейчас (832/835/858), больше нет.
    pub fn on_resume(&mut self, now: Instant) -> ResumeSnapshot; // {paused_for, pre_pause_speed}
    pub fn last_walking_speed(&self) -> Option<f32>;
    /// Для try_apply_default_speed (см. §Shell): бывший &mut bool.
    pub fn default_speed_applied(&self) -> bool;
    pub fn mark_default_speed_applied(&mut self);
}
```

Замечание про 051: streak-счётчик scan-фейлов живёт в `run()`-scope. Если после
мержа 051 он — голые locals, свернуть их сюда (или в крошечный `ScanHealth`)
финальным коммитом; если 051 уже дал структуру — не дублировать.

### 3. `src/zone_session.rs` — `ZoneSession`

Самый большой перенос. Переезжают: `ZoneHoldPhase` (enum + `label`),
`should_run_zone_hold`, `zone_hold_on_transition`, decision-часть
`zone_hold_tick`, `disengage_zone_hold`, `zh_persist_snapshot`,
`operator_override_active`, `zh_bpm_if_fresh` (+ все их тесты). Ключевой ход —
**decision/effect split** `zone_hold_tick`: сегодня функция await'ит BLE прямо
из match'а; после extract `tick` — чистый, возвращает команду, shell исполняет.

```rust
pub enum ZoneWrite {
    SetSpeed { target_kmh: f32 },
    /// Подавлено operator-override окном (задача 039) — shell логирует, не пишет.
    Suppressed { target_kmh: f32 },
    /// Safety hard-stop — НЕ подавляется override (текущее поведение, 1777).
    Stop,
}

pub struct ZoneSession {
    phase: ZoneHoldPhase,
    last_correction_at: Option<Instant>,
    last_safety_write_at: Option<Instant>,
    operator_override_until: Option<Instant>,
}

impl ZoneSession {
    pub fn new() -> Self; // phase = Off
    /// Бывший zone_hold_on_transition (engage/freeze/grace) — уже почти чистый.
    pub fn on_presence_transition(&mut self, prev: PresenceState, next: PresenceState,
                                  cfg: &ZoneHoldConfig,
                                  resumed_kmh: Option<f32>, default_kmh: f32,
                                  now: Instant);
    /// Бывший zone_hold_tick минус BLE: ramp/grace таймеры, safety-cap,
    /// closed-loop коррекция. Внутри — оба гейта задачи 032 (enabled + phase)
    /// и override-check; None speed → None (задача 036/030).
    pub fn tick(&mut self, cfg: &ZoneHoldConfig, resolved: &ResolvedZone,
                measured_speed_kmh: Option<f32>, bpm: Option<u16>,
                now: Instant) -> Option<ZoneWrite>;
    /// Бывший disengage_zone_hold: phase → Off + полная очистка snapshot.
    pub fn disengage(&mut self, state: &mut DaemonState);
    /// Бывший should_run_zone_hold — гейт call site'а (второй рубеж 032).
    pub fn should_run(&self, enabled: bool) -> bool;
    /// Приёмник 052: enabled=false при живой фазе → disengage; сюда же
    /// engage-catchup mid-session `tm zone on` (ветка 1090) как метод
    /// on_config_engaged(...) — сигнатуру согласовать с ConfigDelta из 052.
    pub fn apply_config(&mut self, cfg: &ZoneHoldConfig, /* delta 052 */
                        state: &mut DaemonState, ...);
    /// Успешный CLI tm speed → открыть override-окно (задача 039).
    pub fn note_cli_speed(&mut self, now: Instant);
    /// Бывший zh_persist_snapshot.
    pub fn persist_snapshot(&self, state: &mut DaemonState, resolved: &ResolvedZone,
                            bpm: Option<u16>, measured_speed_kmh: f32);
}
```

`zh_bpm_if_fresh` — ассоциированная чистая функция там же (вход контроллера,
freshness-гейт задачи 035).

**Snapshot-ordering (единственное осознанное микро-отличие):** сегодня
`zh_persist_snapshot` зовётся *после* await-записи (или перед `return` в
safety-ветке). После split shell делает `let w = zone.tick(...); 
zone.persist_snapshot(...); if let Some(w) { /* await */ }` — snapshot ложится
до записи. Все snapshot-поля (phase/target/position/measured speed) от исхода
записи не зависят, разница ненаблюдаема; зафиксировать это в комментарии и в
тесте на состав snapshot'а.

### 4. `src/hr_session.rs` — `HrSession`

Владелец HR-link + contact + battery + connect-latch. Поглощает
`clear_hr_link_state` и `hr_battery_poll_interval` (+ тесты).

```rust
/// Что shell должен сделать с распарсенным кадром — решение чистое, I/O его.
pub enum HrFrameAction {
    /// Contact Live: записать hr_sample (snapshot уже обновлён методом).
    Store { ts_ms: i64 },
    /// Contact Lost (переход уже залогирован/snapshot почищен) или Lost-стабильно.
    Drop,
}

pub struct HrSession {
    link_up: bool,               // зеркало hr_notifications.is_some()
    last_hr_at: tokio::time::Instant,
    contact_tracker: hr::ContactTracker,
    contact: hr::Contact,
    battery_pct: Option<u8>,
    battery_last_read: Option<Instant>,
    connect_in_flight: bool,
    connect_started_at: Instant,
}

impl HrSession {
    /// Стартовое состояние сессии: connect уже spawned (как сегодня, 681–683).
    pub fn new_connecting(now: Instant, tokio_now: tokio::time::Instant) -> Self;
    /// HrConnectOutcome::Connected: fresh tracker/contact, battery seed,
    /// link_up=true, last_hr_at=tokio_now, state.hr_connected=true.
    pub fn on_connected(&mut self, battery_pct: Option<u8>,
                        now: Instant, tokio_now: tokio::time::Instant,
                        state: &mut DaemonState);
    pub fn on_connect_finished(&mut self); // любой Outcome: in_flight=false
    /// Кадр 0x2A37: двигает last_hr_at, гоняет ContactTracker, обновляет
    /// snapshot (Live: bpm/ts; Lost-переход: hr_connected=false + None) —
    /// инвариант hr_connected=false ⇒ last_bpm=None живёт ЗДЕСЬ.
    pub fn on_frame(&mut self, m: &hr::HrMeasurement, ts_ms: i64,
                    tokio_now: tokio::time::Instant,
                    state: &mut DaemonState) -> HrFrameAction;
    /// Не-HR uuid по линку: только двигает last_hr_at (строка 1206).
    pub fn on_link_activity(&mut self, tokio_now: tokio::time::Instant);
    /// Stream end / silence: бывший clear_hr_link_state (link_up=false, contact
    /// reset, battery reset, snapshot чистится). Peripheral сбрасывает shell.
    pub fn on_link_lost(&mut self, state: &mut DaemonState);
    pub fn silence_deadline(&self) -> tokio::time::Instant; // last_hr_at + HR_NOTIFICATION_TIMEOUT
    pub fn link_up(&self) -> bool; // гейт silence-arm и reconnect-arm
    /// Reconnect-tick (задача 042): Skip (in-flight свежий) | Spawn
    /// (нет линка / latch протух за HR_CONNECT_ATTEMPT_DEADLINE — WARN внутри).
    pub fn reconnect_decision(&mut self, now: Instant) -> HrReconnect;
    /// Адаптивный battery-poll (бывший hr_battery_poll_interval + due-check).
    pub fn battery_read_due(&self, now: Instant) -> bool;
    pub fn on_battery_read(&mut self, pct: Option<u8>, now: Instant,
                           state: &mut DaemonState);
}
```

`spawn_hr_connect_attempt`, канал, `hr_peripheral`, `disconnect_best_effort` —
shell. Инвариант синхронизации: `link_up` меняется только в `on_connected`/
`on_link_lost`, и shell в тех же ветках ставит/сбрасывает `hr_notifications` —
это единственная пара, которую нельзя рассинхронизировать; закрепить комментарием
на обоих концах (типом не выразить, пока stream живёт в shell — осознанно).

## Что остаётся в `select!`-шелле

- Все arms как есть (никаких новых/удалённых arms — правило §6 research 003).
- **Nested awaits остаются в daemon.rs**: `try_restore_speed`,
  `try_apply_default_speed` (принимает `&mut TreadmillLink` вместо `&mut bool`),
  `apply_zone_hold_speed`, `execute_control_command`, `process_control_commands`,
  `restore_speed`, `scan::read_hr_battery`, `disconnect_best_effort`.
- `ControlSource`, `Watchdog`, `DaemonState`, `LiveConfig`, power-ветки,
  `celebrate_reached_goals` — не трогать (кроме прокидывания структур).
- `state.last_speed_kmh/ts` (задача 029) — пишется в телеметрической ветке shell
  как сейчас (это snapshot-зеркало, не session-решение).
- `#[allow(clippy::too_many_arguments)]` на `zone_hold_tick` исчезает вместе с
  функцией; на `stream_with_presence` — останется (сигнатура не меняется).

## План коммитов (одна структура = один коммит, тесты в том же коммите)

Порядок — от наименьшего риска к наибольшему; после каждого коммита
`cargo test && cargo clippy` зелёные, демон собирается.

1. **`refactor(daemon): extract AutoPause session struct`** — `src/auto_pause.rs`,
   перенос `auto_pause_due`/`away_duration` + их 4 тестов, новые тесты spell-цикла
   (on_away → due → on_pause_ok → fired; failed → cooldown; on_return отдаёт
   back-dated duration). Wiring: ветки presence-transition + auto-pause-check.
2. **`refactor(daemon): extract TreadmillLink session struct`** —
   `src/treadmill_link.rs`, перенос `cruising_speed` + тестов; новые тесты:
   history prune, on_pause берёт cruising или fallback, on_resume одним изъятием,
   silence_deadline арифметика, default_speed once-per-session. Wiring:
   телеметрическая ветка, silence-arm, `try_apply_default_speed` сигнатура.
3. **`refactor(daemon): extract HrSession struct`** — `src/hr_session.rs`,
   перенос `clear_hr_link_state`/`hr_battery_poll_interval` + их тестов; новые
   тесты: on_frame Live→Store + snapshot, Live→Lost переход чистит bpm, но не
   battery, on_link_lost чистит всё, reconnect_decision latch-recovery (042),
   battery due-адаптивность, инвариант `hr_connected=false ⇒ last_bpm=None`
   на всех путях. Wiring: hr_rx-ветка, hr-frame-ветка, hr-silence-arm,
   reconnect-tick, battery-tick.
4. **`refactor(daemon): extract ZoneSession struct (decision/effect split)`** —
   `src/zone_session.rs`, перенос `ZoneHoldPhase`/transition/tick-решений/
   disengage/snapshot/override/`zh_bpm_if_fresh` + их тестов; новые тесты:
   Ramp→Hold по warmup, Grace→Hold, safety-cap → `Stop`/`SetSpeed` c cooldown,
   override → `Suppressed` (но `Stop` не подавляется), None speed/bpm → None,
   оба гейта 032 (enabled=false при живой фазе — ни одного `ZoneWrite`),
   catchup-engage mid-session. Wiring: обе zone-ветки телеметрии + reload-ветка
   (совместно с `apply_config` из 052) + `note_cli_speed` на обоих
   `process_control_commands` call sites.
5. **`refactor(daemon): stream_with_presence reads as wiring`** — финальная
   чистка: убрать отработавшие helpers, свериться с inventory-таблицей (в теле
   не осталось session-`mut` кроме shell-списка), обновить module docs
   `daemon.rs`, `CLAUDE.md` (архитектурная секция: 4 новых модуля + владельцы
   liveness-строк), этот док → done. Сюда же — 051 scan-streak sweep, если
   актуально (см. §TreadmillLink).

Каждый коммит — с explicit pathspec. Никаких `fix`-примесей: найденный по дороге
баг = отдельная задача/коммит.

## Тестовая стратегия

- Все существующие тесты `daemon.rs` переезжают в модули структур без ослабления
  (их 20+, включая `tokio::time::pause` sibling-arm тесты — те остаются про
  дедлайны и мигрируют на `silence_deadline()`).
- Новые тесты — method-level скрипты с инъекцией времени (`Instant::now() + N`),
  без runtime, кроме pause-тестов дедлайнов.
- `DaemonState` как проверяемый snapshot-выход (`DaemonState::new(true)` +
  ассерты полей) — так уже делают `clear_hr_link_state`/`disengage` тесты.
- Регрессии инцидентов обязаны сохраниться поимённо: 030 (deadband/None-speed),
  031/035 (absolute deadlines), 032 (оба гейта), 033 (link ≠ contact),
  036 (no zero-speed engage), 039 (override window), 041 (no-op safety write),
  042 (latch recovery).

## Acceptance (из backlog 005)

- [ ] Zone / HR / auto-pause переходы unit-тестятся **без btleplug** (ни одного
      `Peripheral`/`Adapter` в тестах структур).
- [ ] `stream_with_presence` читается как wiring: в теле не осталось session-`mut`
      locals кроме shell-списка (BLE handles, intervals, канал, engine, I/O).
- [ ] Поведение идентично: полный существующий test suite зелёный, live smoke
      по чек-листу [048](048-live-smoke-035-047.md) (connect + walk + HR +
      zone on/off mid-session + auto-pause) без регрессий.
- [ ] Liveness matrix в `CLAUDE.md` дополнена колонкой владельца-структуры.
- [ ] `daemon.rs` заметно похудел (структуры + их тесты уехали; точный LOC —
      не гейт, файловый split — backlog 007, после этой задачи).

## Риски

1. **Конфликты с 051/052** — закрыт sequencing'ом (реализация после их мержа);
   line-anchors в этом доке протухнут — ориентироваться по именам.
2. **Snapshot-ordering в ZoneSession** (см. §выше) — единственное место, где
   порядок side-effects меняется; обосновано, зафиксировать тестом.
3. **Рассинхронизация `link_up` ↔ `hr_notifications`** — пара живёт в двух
   владельцах; менять только в парных ветках, комментарии на обоих концах.
   Если по ходу окажется дешёвым держать stream в `HrSession` за трейтом — НЕ
   делать (это шаг к Step 2, YAGNI сейчас).
4. **Borrow checker**: методы берут `&mut self` + `&mut DaemonState`, shell
   параллельно держит `&Store`/`&Peripheral` — дизъюнктно, но в телеметрической
   ветке следить, чтобы `zone.tick(...)` не звался, пока жив заём от
   `zone.persist_snapshot` — порядок вызовов в эскизе это уже учитывает.
5. **Соблазн «починить заодно»** (например, перегруженную семантику
   `hr_connected`, latent 3.10.4 research 003) — не в этой задаче; refactor
   должен быть бесповеденческим, иначе live smoke неинтерпретируем.
6. **Два типа Instant** — принято (§выше); не конвертировать втихую внутри
   методов, только параметры.

## Non-goals (повтор backlog 005 + research 003 §7)

- Полный `Event`/`Effect` кернел, `src/session/` каталог — Step 2, отдельное
  решение, только если Step 1 упрётся.
- Actors/ECS, BLE в отдельном процессе.
- Механический file-split `daemon.rs` сверх выноса четырёх структур —
  backlog [007](../backlog/007-split-god-modules.md), после.
- Вынос nested awaits из presence-ветки.
- Изменение логики/порогов/логов (кроме §Snapshot-ordering).
