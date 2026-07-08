# treadmill-bluetooth-macos

BLE-коннектор для беговой дорожки **Yesoul** под **macOS**, на **Rust**.

## Что это

CLI-утилита, которая по Bluetooth Low Energy находит беговую дорожку, подключается
к ней и читает телеметрию (скорость, наклон, дистанция). Долгосрочная цель — двусторонний
контроль (старт/стоп, задание скорости и наклона) и стабильный коннектор поверх CoreBluetooth.

## Стек

- **Rust** (edition 2024, toolchain 1.95+).
- [`btleplug`](https://github.com/deviceplug/btleplug) — кросс-платформенный BLE; на macOS работает через **CoreBluetooth**.
- `tokio` — async runtime; `tracing` — логирование; `anyhow` — ошибки.

## Архитектура

- `src/main.rs` — точка входа и CLI (`scan` | `connect` | `daemon` | `stats` | ...).
- `src/scan.rs` — обнаружение адаптера, скан, подключение, подписка на нотификации.
- `src/ftms.rs` — константы Fitness Machine Service (`0x1826`) и парсинг Treadmill Data (`0x2ACD`).
- `src/hr.rs` — константы Heart Rate Service (`0x180D`) и парсинг Heart Rate
  Measurement (`0x2A37`, задача 025): u8/u16 bpm, sensor-contact флаги, RR-интервалы
  (задел под HRV, пока не используется). `bpm==0` (потеря контакта у H10) — DEBUG,
  не ошибка, кадр отбрасывается. Плюс Battery Service (`0x180F`/`0x2A19`,
  задача 026) — только Read (Polar не шлёт notify по заряду).
- `src/control.rs` — FTMS Control Point (start/stop/speed).
- `src/control_command.rs` — `ControlCommand` тип (`start`/`stop`/`speed:<kmh>`),
  парс/формат и staleness-проверка для очереди команд (задача 013).
- `src/presence.rs` — детекция присутствия: лента крутится, но шаги не растут →
  `AwayWhileRunning`. `observe(now, speed, steps)` — время инъектируется (демон
  даёт `Instant::now()`, replay — синтез из `ts_ms`), единый источник 10с-away-порога.
- `src/activity.rs` — общий движок presence+credit+сегменты (`ActivityAccumulator`,
  `credit_or_hold`), которым гоняют **и** живой демон, **и** replay (задача 015) —
  сегментация идентична by construction, не форкается.
- `src/default_speed.rs` — расчётная дефолтная скорость ленты на старте
  тренировки (задача 016): `trimmed_mean_speed` (чистая, 15%-trim сверху/снизу,
  floor) + `compute_default_speed` — берёт последнюю подходящую тренировку
  (`walking_time_s` ≥ 30 мин) за всю историю и её крейсерскую скорость из
  `raw_samples`. Демон применяет её на переходе в `Walking` без pre-pause
  скорости, только если лента на заводском crawl (`≤0.8`); read-time,
  переиспользует bounded BLE-write задачи 012.
- `src/recompute.rs` — команда `recompute-segments`: проигрывает `raw_samples`
  через тот же `ActivityAccumulator` (scratch in-memory `Store` переиспользует
  `advance_baseline`+`credit_activity` verbatim) и транзакционно/идемпотентно
  перестраивает `activity_segments` из ground-truth. `daily_stats`/`raw_samples`/
  `workouts` не трогает. Read-only по BLE (задача 015).
- `src/store.rs` — SQLite (`~/Library/Application Support/treadmill-bluetooth-macos/treadmill.db`),
  дневная статистика (шаги/дистанция/время ходьбы), restart-safe дельта-накопление.
  Тренировки хранятся как порог-независимые **сегменты** (`activity_segments`,
  задача 014) — непрерывное зачтённое шагание; отображаемые тренировки
  выводятся на **чтении** чистой `merge_segments(&[Segment], gap_minutes)`, так
  что `workout_gap_minutes` меняется ретроактивно без пересчёта. `daily_stats`
  — строго календарный, не тронут. Старая таблица `workouts` оставлена архивом
  (сид сегментов из неё одноразовый, ничто в неё больше не пишет).
- `src/daemon.rs` — фоновый цикл (LaunchAgent): авто-скан/коннект/реконнект +
  presence + toast; открывает/продлевает **сегмент** активности на зачтённом
  шаге и закрывает его (in-memory `current_segment=None`) в presence-переходе
  при уходе из `Walking` (задача 014); на resume после паузы авто-восстанавливает
  pre-pause скорость ленты через `control.rs` (bounded BLE-write, см. `docs/tasks/012`).
  Единственный владелец BLE-линка: команды управления (`tm speed`/`start`/`stop`)
  от CLI идут через SQLite-очередь `control_commands` и исполняются здесь на живом
  подключении (задача 013). CLI напрямую открывает BLE только если демон не держит линк.
  Авто-пауза простаивающей ленты (задача 020): если `AwayWhileRunning` длится
  дольше `auto_pause_minutes` (дефолт 5, `0` — выкл.), демон шлёт `Stop` (тот же
  bounded Control-Point round-trip), лента гаснет своим встроенным shutoff'ом;
  чистое решение `auto_pause_due`, одна попытка на away-spell + ретрай через
  cooldown при сбое.
  Пульс (задача 025): второй, независимый BLE-линк (HR-датчик, напр. Polar
  H10). Коннект/реконнект — best-effort на **отдельной spawned-таске**
  (`spawn_hr_connect_attempt`), чтобы скан (до 15с, нормальный исход когда
  датчик не надет) не блокировал телеметрию дорожки; результат приходит через
  `mpsc`-канал. Живой стрим `0x2A37` — отдельная ветка в том же `select!`,
  свой bounded timeout (10с) — пропажа датчика не роняет цикл дорожки.
  Сэмплы пишутся в `hr_samples`, снапшот (`hr_connected`+`last_bpm`+`last_bpm_ts`)
  — в `daemon_status` вместе с остальным heartbeat'ом.
  Заряд батареи датчика (задача 026): читается один раз сразу при коннекте
  (в том же spawned-таске) + адаптивно перечитывается — раз в 60 мин, раз в
  30 мин при заряде ≤20% (`hr_battery_poll_interval`, чистая функция). Опрос
  не про экономию батареи H10 (single-byte read ничтожен на фоне её ~400ч
  ресурса) — просто чтобы не делать бесполезную работу. Сбрасывается при
  потере HR-линка вместе с остальным HR-состоянием.
- `src/power.rs` — детекция AC-питания (`pmset -g batt`); на батарее и без
  подключённой дорожки демон не сканирует, чтобы не сажать аккумулятор.
- `src/notify.rs` — нативные macOS-уведомления (`mac-notification-sys`,
  чистый Rust, без Swift в рантайме) с иконкой и именем "Treadmill";
  toast'ы presence/goal, компактный форматтер длительности `humanize_short`.
- `src/goals.rs` — дневные step-goal вехи: загрузка `config.toml` (TOML, задача 023),
  присвоение tier'ов (1–3), чистая функция «какие пороги праздновать сейчас».
  Плюс `load_workout_gap_minutes()` — read-time порог склейки сегментов в
  тренировки из того же `goals.json` (задача 014, дефолт 15). Плюс
  `load_auto_pause()` — порог авто-паузы простаивающей ленты из того же файла
  (задача 020, дефолт 5 мин, `0` — выключено), `None` = выключено.
- `src/logger.rs` — сырой JSONL-лог телеметрии (source-of-truth параллельно с SQLite).
- `src/store.rs` (доп., задача 025) — `hr_samples` (индекс по `ts_ms`, не по
  `session_id` — агрегаты джойнят по временному окну тренировки/дня) +
  `hr_summary_for(from, to)`: `♥ avg/max` = trimmed-mean (переиспользует
  `default_speed::trimmed_mean_speed`) / p95 (устойчив к единичному спайку).
  `None` при < 10 сэмплов в окне. Плюс `hr_battery_pct` в `daemon_status`
  (задача 026, `Option<i64>`, ALTER-колонка).
- `src/zone_hold.rs` — **Zone Hold** (задача 027): HR-адаптивная подстройка
  скорости под целевую пульсовую зону. Чистый модуль, без BLE/времени внутри:
  `hrmax_tanaka`, `resolve_zone_bpm` (`hrmax`/`karvonen`, не смешиваются),
  `ZoneHoldConfig` (парсинг `[zone_hold]` из общего `config.toml`, тот же
  absent-тихо/invalid-WARN стиль, что `goals.rs`), контроллер `next_speed`
  (`band`/`center`, deadband, шаг, кламп — время и bpm инъекцией, юнит-тесты
  на синтетике), `warmup_target_speed` (линейный ramp), `safety_cap_bpm`,
  `classify_position` (below/in/above для виджета). `upsert_zone_hold_keys` —
  line-based апдейт секции для `tm zone` CLI, не трогает остальной файл.
  Демон (`daemon.rs`) держит per-session `ZoneHoldPhase`
  (`Ramp`→`Hold`, `Frozen`/`Grace` на сходе/возврате с ленты) и гоняет
  коррекцию тем же bounded speed-write (задача 012) на presence-тиках;
  safety-cap форсит уменьшение/`Stop` независимо от обычного цикла. Снапшот
  (`zone_hold_active`+`_phase`+`_target_lo/hi`+`_last_speed`+`_position`) —
  в `daemon_status`. CLI `tm zone on/off/setup/limits/target/mode` (без
  аргумента — статус); `on`/`setup` — интерактивный онбординг возраста.
  `tm widget` — поле `HR_ZONE` (below/in/above/пусто, красится только в
  `walking` при активном контроллере); `tm status` — строка Zone Hold.

## Протокол

Большинство дорожек отдают стандартный GATT-профиль **FTMS** (Fitness Machine Service, `0x1826`).
Предполагаем его как основной путь. Возможен **vendor-specific** сервис Yesoul (как в их
мобильном приложении) — это ещё не реверс-инжинирилось; см. `docs/research/`.

Ключевые UUID:
- `0x1826` — Fitness Machine Service
- `0x2ACD` — Treadmill Data (notify)
- `0x2AD9` — Fitness Machine Control Point (write/indicate) — задел под управление
- `0x2ADA` — Fitness Machine Status (notify)
- `0x180D` — Heart Rate Service (задача 025, напр. Polar H10)
- `0x2A37` — Heart Rate Measurement (notify)
- `0x180F` — Battery Service (задача 026)
- `0x2A19` — Battery Level (read)

## Команды

```bash
cargo run             # = scan: перечислить BLE-устройства рядом (диагностика)
cargo run -- connect  # подключиться к первой FTMS-дорожке и стримить данные
cargo run -- daemon    # фоновый режим: авто-коннект + presence + toast (для интерактивной проверки)
cargo run -- stats     # статистика за сегодня; `stats --all` — за все дни
cargo run -- widget    # компактный TSV текущей тренировки для status-bar виджета; пусто если дорожка off (см. docs/tasks/009)
cargo run -- recompute-segments  # пересобрать activity_segments из raw_samples (без BLE, идемпотентно; docs/tasks/015)
cargo run -- default-speed  # показать расчётную дефолтную скорость на старте тренировки (без BLE; docs/tasks/016)
cargo run -- hr        # диагностика: подключиться к HR-датчику, печатать заряд + live bpm (docs/tasks/025,026)
cargo run -- zone      # Zone Hold: статус (без аргумента) или on/off/setup/limits/target/mode (docs/tasks/027)
cargo run -- --help    # полный список команд
cargo test             # юнит-тесты
cargo clippy           # линт
RUST_LOG=debug cargo run  # подробные логи (env-filter)

scripts/install-daemon.sh    # собрать, подписать, поставить LaunchAgent (авто-старт при логине)
scripts/uninstall-daemon.sh  # снять LaunchAgent (данные в Application Support не трогает)
scripts/build-icon.sh        # перегенерировать macos/AppIcon.icns из SF Symbol (см. generate-icon.swift)
scripts/release.sh 0.2.0     # выпустить релиз: бамп версии + дата CHANGELOG + коммит + тег + пуш → Release-workflow (задача 024)
```

Короткий алиас `tm` — симлинк на release-бинарь в `~/.bin` (в `PATH`), чтобы
звать `tm stats` / `tm status` откуда угодно. Его **создаёт/обновляет
`install-daemon.sh` и снимает `uninstall-daemon.sh`** (переопределяется через
`LINK_DIR`/`LINK_NAME`, `LINK_NAME=""` — пропустить). Симлинк указывает на
артефакт сборки, поэтому подхватывает свежий бинарь после каждого rebuild.
Вручную (без демона): `ln -sfn "$PWD/target/release/treadmill-bluetooth-macos" ~/.bin/tm`.

## Конфиг (per-user)

Конфиг (цели, gap, авто-пауза, Zone Hold) — **per-user**, живёт **не в этом
репо**, а в домашней директории: **`~/.config/treadmill-bluetooth-macos/config.toml`**
(`$HOME`-anchored, работает под launchd). **TOML** (задача 023, был JSON
`config.json`/`goals.json`) — ради комментариев: дефолты в примере видны
закомментированными строками. Формат — см. `config/config.example.toml`:
`goals = [8000, 10000, 12000]` + опциональные `workout_gap_minutes` /
`auto_pause_minutes` / `[zone_hold]` (задача 027, см. `src/zone_hold.rs`
выше — секцию обычно пишет `tm zone on`/`setup`, не руки). Опциональный
`workout_gap_minutes` (задача 014, дефолт 15) —
read-time порог: соседние сегменты активности с разрывом ≤ него показываются
одной тренировкой; меняется ретроактивно (без пересчёта). Опциональный
`auto_pause_minutes` (задача 020, дефолт 5, `0` — выключено) — сколько лента может
крутиться `AwayWhileRunning` (человек сошёл) до того, как демон поставит её на
паузу; дальше лента гаснет своим встроенным механизмом. Отсутствует/битый ключ →
дефолт (absent — тихо, т.к. `widget` читает раз в 2 с; невалидное значение →
WARN). Резолвинг (задача 023, один путь): env `TREADMILL_CONFIG` →
`$HOME/.config/.../config.toml` → вшитые дефолты `[8000,10000,12000]` (JSON- и
legacy-env-фолбэки задачи 021 убраны). Нет файла — норма (INFO + дефолты); битый
файл — WARN. Каждый пользователь приносит свой файл (например, симлинком из
личного dotfiles-репо). Правки **подхватываются на лету без рестарта** (задача
017): демон следит за mtime `config.toml` и перечитывает цели **и** порог
авто-паузы при изменении
(≤5 с, только когда файл реально менялся). `workout_gap_minutes` и так read-time
(ретроактивен). Что **сейчас загружено в демоне** и когда он последний раз читал
файл — видно в `tm status` (задача 022): демон пишет снапшот загруженного
конфига (цели + авто-пауза + время чтения) в `daemon_status`, `status` его
печатает; `workout_gap_minutes` показывается отдельно как read-time. Мгновенно
применить всё равно можно рестартом (`launchctl kickstart -k` или переустановка). Tier (яркость toast'а) — из ранга по возрастанию: низший порог →
tier 1. Каждая цель празднуется ровно раз в день (local date, restart-safe через
таблицу `goal_celebrations`). См. `docs/tasks/011-...md`.

## Заметки по macOS

- Первый запуск запросит разрешение на Bluetooth (CoreBluetooth). Без него скан пуст.
- Адресов устройств на macOS нет — идентификатор это непрозрачный system UUID.

## Конвенции

- Комментарии в коде — только на английском.
- Логируем аномалии/edge cases, а не happy path.
- Держим файлы маленькими и однонаправленными; парсинг протокола отдельно от транспорта.
- Docs-first: перед задачей — заметка в `docs/tasks/`, после — обновить затронутые доки.
