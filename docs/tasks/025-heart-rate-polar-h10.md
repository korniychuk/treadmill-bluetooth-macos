# 025 — Heart-rate: нагрудный датчик (Polar H10) → сэмплы, статистика, widget

**Статус:** planned (не начато)
**Дата:** 2026-07-08
**Research:** `docs/research/002-heart-rate-polar-h10.md`
**Зависит от:** 005 (демон/presence), 009 (widget-контракт), 014 (сегменты),
016 (`trimmed_mean` — переиспользуем), 022 (`daemon_status` снапшот)

## Цель

Автоматически (zero-touch, как с дорожкой) подхватывать нагрудный HR-датчик
**Polar H10** по BLE, писать **непрерывные сэмплы пульса** в БД рядом с шагами,
показывать **живой пульс** в tmux-виджете (только когда датчик надет) и **сводку
пульса** в `tm stats` — компактно, не раздувая вывод.

Пульс — это отдельный BLE-peripheral. Демон становится владельцем **двух**
параллельных линков (дорожка + HR), оставаясь единственным, кто держит BLE.

---

## Архитектура и разделение слоёв (важно)

Пульс встраивается в **существующую** трёхслойную модель, ничего нового не
изобретаем:

```
   BLE (CoreBluetooth)          SQLite (IPC-граница)         презентация
┌─────────────────────┐      ┌──────────────────────┐    ┌──────────────────┐
│ demon (единственный │ ───▶ │ hr_samples (time-     │ ◀─ │ tm stats  (CLI)  │
│ владелец обоих      │ ───▶ │   series, durable)    │ ◀─ │ tm widget (CLI)  │ ◀─ tmux-скрипт
│ линков: 0x1826+0x180D)     │ daemon_status.last_bpm│ ◀─ │ tm status (CLI)  │
└─────────────────────┘      │   + hr_connected (snap)│    └──────────────────┘
                              └──────────────────────┘
```

**Три инварианта, которые задача обязана сохранить:**

1. **Только демон открывает HR-линк в проде.** CLI-команды (`stats`/`widget`/
   `status`) — read-only из `Store`, BLE не трогают (как `status`/`widget`
   сегодня). Исключение — диагностический `tm hr` (§CLI), по образцу `connect`.
2. **tmux-виджет НЕ читает БД.** Он зовёт `tm widget`; стабильный контракт —
   **вывод команды**, а не схема БД (задача 009). Пульс добавляется как новое
   TSV-поле, скрипт `treadmill-widget.sh` обновляется вместе с контрактом.
3. **Согласованность demon↔CLI — через снапшот, а не через живой BLE.** Демон на
   каждый HR-сэмпл/heartbeat пишет в single-row `daemon_status` (id=0): `last_bpm`,
   `last_bpm_ts`, `hr_connected`. CLI читает снапшот — точно так же, как сейчас
   читает `connected`/`presence_state`. Свежесть виджета/статуса определяется тем
   же staleness-порогом, что и heartbeat дорожки. Никаких гонок: один писатель,
   атомарный upsert строки.

**Почему `daemon_status`, а не «последний `hr_samples`»:** widget поллится раз в
2 с и должен знать не только bpm, но и **надет ли датчик прямо сейчас**
(`hr_connected` + свежесть `last_bpm_ts`). Это состояние живёт в снапшоте демона,
как и `connected` дорожки. `hr_samples` — durable-история для `stats`, не для
«прямо сейчас».

---

## Протокол (кратко; детали — research 002)

- **Heart Rate Service `0x180D`** — H10 рекламирует его → фильтруется в скане как
  дорожка (`0x1826`). Открытое подключение, без pairing/bonding → zero-touch.
- **Heart Rate Measurement `0x2A37`** (notify): байт флагов, затем bpm (`u8`/`u16`
  по bit0), опц. energy (bit3), опц. RR-интервалы (bit4). H10 — обычно `u8` bpm +
  sensor-contact + RR.
- H10 включается от контакта с кожей, рекламируется только когда надет; снят →
  реклама пропадает (естественный «датчик off»). До 2 BLE-слотов (наш + телефон).

---

## Изменения по файлам

### Новое: `src/hr.rs` (протокол HR, отдельно от транспорта)
- Константы UUID `HEART_RATE_SERVICE 0x180D`, `HEART_RATE_MEASUREMENT 0x2A37`
  (паттерн `Uuid::from_u128(...)` как в `ftms.rs:14`).
- `struct HrMeasurement { bpm: u16, contact: Option<bool>, rr_ms: Vec<u16> }`.
- `parse_hr_measurement(payload: &[u8]) -> Option<HrMeasurement>` — калька с
  `parse_treadmill_data` (`ftms.rs:114`): флаги → курсор → условные поля через
  те же `read_u8/u16` хелперы. **Отбрасывает bpm==0** (H10 шлёт 0 при потере
  контакта) — логируем как edge case (DEBUG, не WARN: это норма при снятии).
- Юнит-тесты на реальные кадры (u8/u16, с RR и без, contact-флаги).

### `src/scan.rs` — скан + коннект HR-peripheral
- `connect_hr(adapter) -> Result<Peripheral>` рядом с `connect_treadmill`
  (`scan.rs:140`): скан по `ScanFilter{services:[HEART_RATE_SERVICE]}`, матч
  `services.contains(0x180D)`, `connect` + `discover_services` под теми же
  timeout'ами. **Отдельный скан-проход** (CoreBluetooth фильтрует по service-UUID
  рекламы — нельзя переиспользовать `0x1826`-фильтр).
- `subscribe_hr(peripheral) -> bool` рядом с `subscribe_treadmill_data`
  (`scan.rs:199`): подписка на `0x2A37`, best-effort (нет char → `false`, WARN).
- `is_hr_sensor(peripheral)` — по образцу `is_treadmill` (`scan.rs:186`).

### `src/store.rs` — схема + инсертер + агрегаты
- Новая таблица в `migrate()`-батч, сразу после `raw_samples`+индекса (`store.rs:217`):
  ```sql
  CREATE TABLE IF NOT EXISTS hr_samples (
    id         INTEGER PRIMARY KEY,
    session_id INTEGER REFERENCES sessions(id),
    ts_ms      INTEGER NOT NULL,
    bpm        INTEGER NOT NULL,
    rr_ms      BLOB,            -- опц. RR-интервалы (HRV-задел); парсинг можно отложить
    raw_frame  BLOB NOT NULL
  );
  CREATE INDEX IF NOT EXISTS idx_hr_samples_ts ON hr_samples(ts_ms);
  ```
  Индекс по `ts_ms` (не `session_id`): агрегаты в `stats` джойнят по **временному
  окну** тренировки/дня, а не по session (см. ниже — робастнее).
- `insert_hr_sample(session_id, ts_ms, m: &HrMeasurement, raw_frame)` — по образцу
  `insert_raw_sample` (`store.rs:480`). `rr_ms` пока `NULL` (задел).
- `hr_summary_for(from_ms, to_ms) -> Option<HrSummary>` — агрегат по временному
  окну: тянет `bpm` из `hr_samples WHERE ts_ms BETWEEN ... ORDER BY bpm`, считает
  сводку (§Статистика). `None` если сэмплов < порога (напр. < 10 → не показываем).
- ALTER-колонки в `daemon_status` (идемпотентно, `add_column_if_missing`
  `store.rs:280`, около `store.rs:269`): `last_bpm INTEGER`, `last_bpm_ts INTEGER`,
  `hr_connected INTEGER DEFAULT 0`. Поля в `DaemonStatus` struct (`store.rs:93`) +
  upsert/read (`store.rs:760`).

### `src/daemon.rs` — второй стрим в цикле
- В `stream_with_presence` (`daemon.rs:383`) после подписки на дорожку
  (`daemon.rs:393`): **best-effort** `scan::connect_hr` + `subscribe_hr`. Нет
  датчика → продолжаем без пульса (`hr_connected=false`), это норма — датчик
  надевают не всегда. Отдельный `.notifications()`-стрим HR-peripheral.
- Новая ветка в `tokio::select!` около `daemon.rs:485`, с собственным
  `timeout` (по образцу `NOTIFICATION_TIMEOUT`): парс `0x2A37` →
  `store.insert_hr_sample(session_id, now, &m, &frame)` → обновить `last_bpm`/
  `last_bpm_ts` в `DaemonState` (`daemon.rs:995`). Пропавший датчик (timeout) →
  `hr_connected=false`, reconnect-петля для HR **не должна ронять** основной цикл
  дорожки.
- Lifecycle: HR-линк рвём в тех же exit-путях, что и дорожку
  (`disconnect_best_effort` `daemon.rs:364`) — no dangling peripheral.
- `state.persist` (`daemon.rs:654`) пишет `hr_connected`+`last_bpm`+`last_bpm_ts`
  в `daemon_status` вместе с heartbeat.

### `src/main.rs` — CLI (stats / widget / status)
- **`stats`** (`run_stats`/`render` `main.rs:319-388`): к per-workout строке
  (`main.rs:378`) и per-day header (`main.rs:340`) дописать HR-сводку из
  `store.hr_summary_for(span)`, **опускать когда сэмплов нет** (тот же стиль
  «omit when nothing extra», что `dist_hint`/`time_hint`).
- **`widget`** (`run_widget` `main.rs:573`, TSV `main.rs:628`): добавить 9-е поле
  `HR_BPM` — **пусто когда датчик не надет/устарел** (гейт: `hr_connected` +
  свежесть `last_bpm_ts`), иначе live bpm. Поле присутствует всегда (стабильный
  порядок), пустота — сигнал скрыть сердечко. Обновить докстринг «8→9 полей»
  (`main.rs:563`) и контракт задачи 009.
- **`status`** (`run_status` `main.rs:444`): строка `heart rate: Polar H10
  connected, 118 bpm` / `heart rate: no sensor` — рядом с `treadmill: ...`
  (`main.rs:466`).

### `scripts/tmux/treadmill-widget.sh` + `README.md`
- Парсить 9-е поле; рисовать `♥ NNN` только если поле непустое (датчик надет).
  Глиф/цвет сердечка — переменной сверху (тюнинг под тему). Цвет опц. по зоне —
  future (см. §Config).

---

## Статистика: как показать пульс компактно

Текущий формат:
```
2026-07-08: 8234 steps, 5.12 km, 41m walking
  #1  09:12 → 09:54   3200 steps, 2.10 km, 36m27s
```

**Решение (по умолчанию): `♥ avg/max`** — два числа на строку:
```
2026-07-08: 8234 steps, 5.12 km, 41m walking   ♥ 116/141
  #1  09:12 → 09:54   3200 steps, 2.10 km, 36m27s   ♥ 118/134
```

- **avg** = **trimmed mean** пульса за окно тренировки/дня — переиспользуем
  паттерн `trimmed_mean_speed` из `default_speed.rs` (15%-trim сверху/снизу).
  Trim + отброс `bpm==0` убивает артефакты H10 (потеря контакта, спайки) — это
  и есть аккуратный аналог твоей идеи «откинуть хвосты».
- **max** = **p95**, а не сырой максимум: единичный спайк датчика не должен
  раздувать «максимум». Физиологически это «пиковое усилие».

**Почему пара avg/max, а не тройка/пятёрка (твои идеи взвешены):**
- Твой хард-constraint — «не раздувать вывод». Два числа + один глиф — минимум.
- avg + max — это ровно то, чем Apple Fitness / Garmin резюмируют сессию: типичное
  усилие + пик. Для **ходьбы** (не бег) этого достаточно.
- «Минимум/resting» на прогулке доминируется разминкой/простоем и малоинформативен
  → в дефолт не берём.
- Средние верхних/нижних 10% и «min–avg–max из 80%» — полезны, но это **детальный**
  разрез, а не строка-сводка.

**Опционально (не в первом milestone): `tm stats --hr`** — развёрнутый разрез
твоей полной идеи, по тренировке:
```
  #1  ...  ♥ trimmed 102–118–134 (80%)  ·  low10% 96  ·  high10% 139  ·  n=2148
```
Легко включается поверх `hr_summary_for` (перцентили уже посчитаны). Если после
железа окажется полезнее тройка по умолчанию — это **однострочная** смена
форматтера, схему не трогает.

> Сердечко-глиф `♥` — и в `stats`, и в `widget`, единый визуальный маркер пульса.

---

## CLI-команды

Прод-поверхность расширяется **без новых команд** — `stats`/`widget`/`status`
получают пульс. Плюс **один диагностический** для bring-up на железе:

- **`tm hr`** (read-only live-стрим bpm, по образцу `connect` `main.rs:207`):
  подключиться к H10, печатать `bpm` в stdout, `Ctrl-C` — отключиться. Только для
  проверки датчика/парсера на железе (milestone 1). Открывает BLE напрямую —
  допустимо: H10 держит 2 слота, диагностика не мешает демону. В `--help` помечен
  как диагностический.

Диагностика скана нового команды не требует: `tm scan` (`scan_and_list`
`scan.rs:47`) уже перечисляет все BLE-устройства без фильтра — H10 там виден.

---

## Config (per-user, `config.toml`) — задел на будущее, не в milestone 1

- Опциональный `hr_max` (или `age` → `220-age`) для будущих **HR-зон** и
  раскраски сердечка в виджете по зоне (зелёный/жёлтый/красный). Резолвинг — тем
  же путём, что `goals`/`auto_pause_minutes` (задача 023), hot-reload (017).
- **Вне scope сейчас** (YAGNI): зоны, `% max HR`, HRV из RR-интервалов.

---

## Milestones

**M1 — данные (без widget/stats-презентации):**
- `hr.rs` (UUID, парсер, тесты), `connect_hr`/`subscribe_hr`, `hr_samples` +
  инсертер, HR-ветка в демоне (best-effort, lifecycle), `tm hr` диагностика.
- Проверка на железе: скан видит H10, коннект, парсинг реальных кадров, запись
  сэмплов, поведение при снятии/надевании датчика в середине тренировки.

**M2 — презентация:**
- `daemon_status` HR-колонки + снапшот из демона.
- `tm status` HR-строка; `tm stats` `♥ avg/max`; `tm widget` 9-е поле +
  `treadmill-widget.sh` рисует сердечко только при надетом датчике.
- Обновить контракт задачи 009 (8→9 полей), README виджета, `CLAUDE.md`
  (команды/архитектура), CHANGELOG.

**M3 (опц., по результату использования):** `tm stats --hr` разбор; HR-зоны из
config; RR/HRV.

---

## Тесты

- `hr.rs`: парсер на табличных кадрах (u8/u16 bpm, RR present/absent, contact-биты,
  `bpm==0` → `None`).
- `store`: `hr_summary_for` на синтетике (trimmed avg, p95, порог «мало сэмплов
  → None», отброс нулей).
- `main`: `widget_state`/форматтер — HR-поле пустое без датчика, непустое с ним;
  свежесть-гейт (`last_bpm_ts` устарел → пусто).
- Presence/replay не затрагиваются (HR — отдельная ось, не влияет на сегментацию).

---

## Риски / что проверить на железе (из research 002)

- Рекламирует ли H10 `0x180D` в CoreBluetooth-скане на macOS (ожидаемо да).
- Два `connect` (дорожка + HR): не мешают ли под раздельными скан-проходами;
  порядок подключения.
- Снятие датчика в середине тренировки → reconnect-петля HR не ломает цикл дорожки.
- Второй BLE-слот H10 занят телефоном Polar Flow → наш `connect` может не пройти →
  WARN, деградируем без пульса (`hr_connected=false`).
- `cargo build` ломает нотификации демона — после сборки прогнать
  `install-daemon.sh` (см. память проекта).

---

## Решения (согласовано с оператором)

- **Дефолтный формат HR в `stats` — `♥ avg/max`** (trimmed avg / p95). Зафиксировано
  2026-07-08. Диапазон-тройка `♥ low–avg–high` отклонён как дефолт (остаётся
  однострочной сменой форматтера, если понадобится).
- Полный разрез (min–avg–max из 80%, средние top/bottom-10%, n) — опциональный
  `tm stats --hr`, не в дефолт (M3).
