# 005 — Presence-aware daemon: авто-подключение, детекция ухода, дневная статистика

**Status:** in progress
**Depends on:** [003](003-yesoul-w2-pro-controller.md) (FTMS телеметрия), [002](002-macos-bluetooth-permission.md).

## Goal

Фоновый сервис на macOS (LaunchAgent), который:
1. Постоянно сканирует и авто-подключается к дорожке, как только она включается.
2. Управление скоростью/паузой остаётся за оператором (Bluetooth-пульт, RF,
   вне зоны видимости этого приложения) — приложение только слушает.
3. Детектирует **присутствие**: лента крутится, но `steps` не растёт ⇒ оператор
   сошёл с дорожки. Отдельно от паузы (speed=0).
4. Копит **за день**: пройденную дистанцию, шаги, время реальной ходьбы —
   только пока присутствие подтверждено (см. "Развилка" ниже).
5. Шлёт нативные toast-уведомления (без Swift — через `osascript`) на 6 событий:
   обнаружена / потеряна / ушёл (away) / вернулся (resumed) / поставлена на
   паузу / снята с паузы.
6. Переживает рестарт демона без потери статистики (SQLite, инкрементальные
   апдейты, дельта-накопление с ребейзлайном при сбросе счётчика устройства).
7. CLI-команда `stats` — посмотреть накопленное.

## Де-риск (сделано 2026-07-05, до реализации остального)

Полный план по совету advisor'а: сначала подтвердить два риска, которые могли
похоронить всю фичу, потом строить стек.

### Риск 1 — видит ли Bluetooth демон под launchd

Ранее (задача 002) TCC-грант на Bluetooth ушёл на **Alacritty** (responsible
process), не на бинарник — под launchd родителя-терминала нет, был риск
пустого скана без `.app`-bundle.

**Тест:** временный `LaunchAgent` (`~/Library/LaunchAgents/com.korniychuk.treadmill-scan-test.plist`),
запускающий `treadmill-bluetooth-macos scan` в лог, `RunAtLoad`.
**Результат: PASS.** Лог показал полный скан с `YS_W2PRO_02395 ftms=true` —
голый бинарник видит Bluetooth под launchd без `.app`-bundle. Тестовый
LaunchAgent выгружен и удалён после проверки.

### Риск 2 — надёжность тиков `steps`

Presence-детекция целиком висит на vendor-поле steps (flag-13 слот 0x2ACD).
Нужно было измерить: интервал нотификаций и максимальный разрыв между
приростами steps при реальной ходьбе.

**Тест:** `connect` в фоне ~50 с, оператор шёл по дорожке (скорость 0.5 км/ч —
минимальная и самая "дёрганая" по шагу).

**Результат:**
- Нотификации приходят ~2/сек (каждые 0.4–0.6 с), но `elapsed_s` и `steps`
  реально обновляются **раз в секунду**, без пропусков в `elapsed_s`.
- `steps` на минимальной скорости иногда не растёт 1 полный тик (~1–2 с между
  приростами); двух гэпов подряд не наблюдалось.
- На более высоких скоростях (реальный темп ходьбы 3–6 км/ч) шаг будет ещё
  плотнее — 0.5 км/ч это худший случай.

**Вывод:** порог away-детекции **10 секунд** без прироста steps при
`speed_kmh > 0` даёт ~5-кратный запас от худшего наблюдаемого естественного
гэпа. Оба риска сняты — реализуем весь стек.

## Развилка: как считать дистанцию/шаги во время "away"

Пользователь просил вычитать из **времени** периоды без шагов. Про дистанцию
буквально сказано только "трекать пройденную дистанцию". Но если лента
крутится без человека — эта дистанция фантомна (крутится лента, не ноги).

**Решение (зафиксировано, не переспрашивалось отдельно):** все три метрики —
дистанция, шаги, время — копятся **только в состоянии Walking**. Если
понадобится "сырая" дистанция ленты независимо от присутствия — это отдельный
столбец на будущее, сейчас не реализуется.

## Архитектура

```
src/
  store.rs      — SQLite (rusqlite, bundled): sessions, daily_stats,
                  device_baseline (last_device_steps/distance для delta-накопления)
  presence.rs   — state machine: Walking | AwayWhileRunning | Paused
  notify.rs     — osascript-обёртка (display notification), 4 события
  daemon.rs     — вечный цикл: scan → found → notify → connect → stream
                  (presence + store + notify) → disconnect → notify → rescan
  main.rs       — новые режимы: `daemon`, `stats [--date|--range]`
```

### `store.rs` — схема

```sql
CREATE TABLE sessions (
  id INTEGER PRIMARY KEY,
  started_at TEXT NOT NULL,   -- RFC3339
  ended_at   TEXT             -- NULL пока сессия активна
);

CREATE TABLE daily_stats (
  date TEXT PRIMARY KEY,      -- YYYY-MM-DD, локальная дата
  distance_m INTEGER NOT NULL DEFAULT 0,
  steps INTEGER NOT NULL DEFAULT 0,
  walking_time_s INTEGER NOT NULL DEFAULT 0
);

-- restart-safety: последнее увиденное сырое значение счётчика устройства,
-- чтобы после рестарта демона продолжать дельты без двойного счёта и без
-- потери прогресса между последним сэмплом и рестартом.
CREATE TABLE device_baseline (
  id INTEGER PRIMARY KEY CHECK (id = 0),
  last_steps INTEGER,
  last_distance_m INTEGER
);
```

Дельта-накопление: `delta = new - last_seen`; если `new < last_seen` (счётчик
устройства сбросился при power-cycle) — ребейзлайн (`delta = new`, без ухода в
минус). Дельта прибавляется в `daily_stats` только пока состояние = Walking.

### `presence.rs` — state machine

```rust
enum PresenceState { Unknown, Walking, AwayWhileRunning, Paused }
```

- `speed_kmh == 0` → `Paused` (лента стоит; не "away", это штатная пауза).
- `speed_kmh > 0` и steps растут (за скользящее окно) → `Walking`.
- `speed_kmh > 0` и steps не растут ≥ `AWAY_THRESHOLD` (10 с) → `AwayWhileRunning`.
- Переходы `* → AwayWhileRunning` и `AwayWhileRunning → Walking` триггерят toast.

### `notify.rs`

`Command::new("osascript").arg("-e").arg(script)` — без shell, инъекций нет;
экранирование `"`/`\` в тексте уведомления. 4 события: обнаружена / потеряна /
away / walking-resumed.

### `daemon.rs` + LaunchAgent

- **LaunchAgent (не LaunchDaemon)** — обязательно: `osascript`-нотификации и
  Bluetooth-prompt работают только в Aqua-сессии пользователя; LaunchDaemon
  (system context) их не покажет.
- `KeepAlive` + `RunAtLoad`, лог в `~/Library/Logs/treadmill-bluetooth-macos/`.
- Cкрипты `scripts/install-daemon.sh` / `scripts/uninstall-daemon.sh`.
- Caveat: пока демон держит BLE-соединение, телефонное приложение Yesoul к
  дорожке не подключится (peripheral отдаёт один central) — управление
  оператора идёт через независимый RF-пульт, поэтому не мешает.
- Caveat: "потеряна" детектится с лагом (BLE supervision timeout, секунды–
  десятки секунд после реального выключения) — это ожидаемо, не баг.

## Storage insurance

Существующий JSONL `WorkoutLogger` (см. 003) остаётся как raw append-only
source-of-truth параллельно с SQLite-агрегацией — даже баг в агрегации не
потеряет сырые данные.

## incline

Как установлено в 003 — не читается и не пишется по BLE на этом железе.
Колонка в схеме не заводится (уже подтверждено: `incline_percent` в
`TreadmillData` всегда `None`).

## Presence-confirmation window: не засчитывать хвост перед away

Обнаружено при живом e2e-тесте (замечено оператором): наивная реализация
"копим distance/time, пока `state == Walking`" пропускает баг — сам
`AWAY_THRESHOLD` (10 с) выполняется как задержка **до** перехода в
`AwayWhileRunning`, всё это время состояние формально остаётся `Walking`, и
крутящаяся без оператора лента успевает "накрутить" ~10 с фантомного времени
и дистанции в daily_stats на каждый уход.

**Фикс:** дистанция/время не пишутся в SQLite немедленно — копятся в памяти
(`daemon::PendingCredit`) с момента последнего подтверждённого шага. Шаг
(steps delta > 0) — самоподтверждающийся сигнал, коммитится сразу вместе с
накопленным буфером. Если вместо этого наступает подтверждённый away —
буфер сбрасывается **без записи в БД**. `store.rs` разделён на
`advance_baseline` (persist-safe дельты, restart-safety) и `credit_daily`
(явное зачисление уже принятого решения) — само решение "считать за ходьбу
или нет" целиком переехало в `daemon.rs`. Тесты
`daemon::tests::confirmed_away_discards_pending_instead_of_crediting_it` и
`confirmed_step_flushes_pending_distance_and_time` фиксируют оба сценария.

## Progress log

- 2026-07-05: оба де-риска подтверждены на живом железе (LaunchAgent видит
  BLE; steps-каданс устойчив, worst-case gap ~2с на мин. скорости) →
  AWAY_THRESHOLD = 10s. Развилка по distance/steps/time при away зафиксирована
  (все три — только в Walking).
- 2026-07-05: реализованы store (SQLite), presence state machine, notify
  (osascript toast), daemon loop, `scripts/install-daemon.sh` /
  `uninstall-daemon.sh` (LaunchAgent), CLI `stats`. 13 unit-тестов.
- 2026-07-05: живой e2e-тест поймал баг с фантомным хвостом перед away
  (см. выше) — исправлено буферизацией; ещё +2 теста (15 всего). LaunchAgent
  переустановлен с фиксом, живая проверка на реальном железе: presence
  корректно переключается Walking ⇄ AwayWhileRunning, `stats` показывает
  только подтверждённую ходьбу.
- 2026-07-05: добавлены toast на паузу/снятие с паузы (оператор просил).
  Заодно нашли и исправили смежный баг: `last_step_change` не сбрасывался
  во время паузы, поэтому снятие с паузы дольше `AWAY_THRESHOLD` (обычное
  дело) немедленно читалось бы как `AwayWhileRunning` вместо `Walking`.
  Фикс — "приколачивать" `last_step_change` к текущему моменту на каждом
  paused-сэмпле, так что таймер стартует заново ровно с момента возобновления.
  Toast на паузу не шлётся на самом первом сэмпле после коннекта (если
  дорожка уже стояла) — только на реальном переходе. 16 тестов, LaunchAgent
  переустановлен.
