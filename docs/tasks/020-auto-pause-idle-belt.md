# 020 — Авто-пауза ленты при долгом бездействии (сошёл с дорожки)

## Проблема

Если оператор сходит с ленты, она продолжает крутиться — сама по таймеру не
останавливается достаточно быстро (у Yesoul нет удобного короткого idle-таймаута).
Это трата ресурса ленты и лишний шум. Хочется: **лента крутится «вхолостую»
дольше N минут → ставим её на паузу**. После паузы дорожка сама выключится через
свой встроенный механизм — нам это делать не нужно.

«Выключить» здесь = **поставить на паузу** (`FTMS STOP_PAUSE`, opcode `0x08`,
param `0x01`), тот же путь, что `tm stop` (см. `control.rs`).

## Сигнал у нас уже есть

`PresenceState::AwayWhileRunning` (см. `presence.rs`) — лента бежит (`speed>0`),
но step-counter не растёт `AWAY_THRESHOLD`(10 с). Это ровно «человек сошёл, лента
крутится». Демон уже:
- держит `away_since: Option<Instant>` (момент перехода в away);
- считает честную длительность отсутствия `away_duration()` (прибавляет
  `AWAY_THRESHOLD`, т.к. реальный уход на 10 с раньше подтверждения);
- умеет слать bounded Control-Point write (`execute_control_command`,
  `SPEED_RESTORE_TIMEOUT` = 15 с, конвенция watchdog задачи 007).

Значит фича — тонкая **политика** поверх этого, без нового BLE-кода.

## Решение

### Конфиг — `auto_pause_minutes` в `goals.json`

Тот же per-user `~/.config/treadmill-bluetooth-macos/goals.json`, что и цели.
Новый ключ `auto_pause_minutes`:
- **absent** → дефолт **5 минут** (норма для старых конфигов, тихо);
- `0` → **выключено** (осознанный опт-аут);
- `n > 0` → `n` минут;
- present-but-invalid (не-целое / отрицательное / битый JSON) → дефолт 5 мин + WARN.

`goals::load_auto_pause() -> Option<Duration>` (`None` = выключено). Парсинг —
трёхкейсовый `AutoPauseSetting { Configured(i64) | Invalid | Unset }`, как
`GapSetting` для `workout_gap_minutes` (чистый, юнит-тестируемый).

**Hot-reload** (задача 017): порог держим сессионной переменной
`auto_pause_threshold: Option<Duration>`, обновляем в уже существующем
`config_tick` рядом с `goals` — по mtime-гейту, без лишних `stat`/чтений.

### Триггер и антидребезг

В `stream_with_presence`, на каждом telemetry-сэмпле, **после** presence-блока,
если текущее состояние `AwayWhileRunning`:

- честная длительность отсутствия = `away_duration(away_since)`;
- чистое решение `auto_pause_due(threshold, away_for, fired, since_last_attempt)`:
  - `threshold=None` (выключено) → `false`;
  - `fired` (уже успешно поставили на паузу в этот away-spell) → `false`;
  - `away_for < threshold` → `false`;
  - неудачная попытка была < `AUTO_PAUSE_RETRY_COOLDOWN`(15 с) назад → `false`
    (не долбим Control Point каждую секунду при сбое);
  - иначе → `true`.

Пер-spell состояние: `auto_pause_fired: bool` + `auto_pause_last_attempt:
Option<Instant>`, **сбрасываются при любом выходе из `AwayWhileRunning`** (человек
вернулся / поставил на паузу сам / дисконнект). Так каждый новый уход получает
свежий отсчёт и одну гарантированную попытку.

### Исполнение паузы

Переиспользуем `execute_control_command(peripheral, ControlCommand::Stop)` внутри
`tokio::time::timeout(SPEED_RESTORE_TIMEOUT, …)` — тот же bounded round-trip, что
у очереди команд и restore-скорости:
- успех → `auto_pause_fired = true`, INFO, тост `notify::auto_paused(away_for)`;
- сбой/таймаут → WARN, `auto_pause_last_attempt = now` (ретрай через cooldown),
  сессия живёт (авто-пауза best-effort, никогда не роняет стрим).

### Тосты (без двойного «Paused»)

Новый `notify::auto_paused(away: Duration)` — например «Auto-paused — belt idle
{humanize_short(away)}». После успешного Stop лента за ~2-3 с тормозит до 0 →
presence переходит `AwayWhileRunning → Paused`, что штатно шлёт `treadmill_paused()`.
Чтобы не дублировать, в Paused-arm **подавляем** generic-тост, если
`auto_pause_fired`. `pre_pause_speed` при этом **захватываем как обычно**: если
оператор вернётся до авто-выключения и возобновит — скорость восстановится
(задача 012).

## Затронутые файлы

- `src/goals.rs` — `AutoPauseSetting`, `read_auto_pause_minutes` (чистый + тесты),
  `load_auto_pause() -> Option<Duration>`, `DEFAULT_AUTO_PAUSE_MINUTES = 5`.
- `src/daemon.rs` — `AUTO_PAUSE_RETRY_COOLDOWN`, `auto_pause_due(...)` (+ тесты);
  сессионные `auto_pause_threshold` / `auto_pause_fired` / `auto_pause_last_attempt`;
  проверка после presence-блока; reset при выходе из away; hot-reload в `config_tick`;
  подавление двойного тоста в Paused-arm.
- `src/notify.rs` — `auto_paused(away: Duration)`.
- `config/goals.example.json` — добавить `auto_pause_minutes: 5`.
- `CLAUDE.md` — описать ключ `auto_pause_minutes` и поведение авто-паузы.

## Проверка

- Юнит: `auto_pause_due` (выключено / рано / готово / cooldown / уже-fired),
  `read_auto_pause_minutes` (configured / unset / disabled=0 / invalid).
- На железе: `auto_pause_minutes: 1`, встать на ленту, сойти, не двигаться →
  через ~1 мин лента встаёт на паузу, один тост, дальше лента гаснет сама.
  Вернуться до срабатывания → отсчёт сбрасывается, паузы нет.
