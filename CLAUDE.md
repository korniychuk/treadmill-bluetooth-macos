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
- `src/control.rs` — FTMS Control Point (start/stop/speed).
- `src/presence.rs` — детекция присутствия: лента крутится, но шаги не растут → `AwayWhileRunning`.
- `src/store.rs` — SQLite (`~/Library/Application Support/treadmill-bluetooth-macos/treadmill.db`),
  дневная статистика (шаги/дистанция/время ходьбы), restart-safe дельта-накопление.
- `src/daemon.rs` — фоновый цикл (LaunchAgent): авто-скан/коннект/реконнект +
  presence + toast; на resume после паузы авто-восстанавливает pre-pause
  скорость ленты через `control.rs` (bounded BLE-write, см. `docs/tasks/012`).
- `src/power.rs` — детекция AC-питания (`pmset -g batt`); на батарее и без
  подключённой дорожки демон не сканирует, чтобы не сажать аккумулятор.
- `src/notify.rs` — нативные macOS-уведомления (`mac-notification-sys`,
  чистый Rust, без Swift в рантайме) с иконкой и именем "Treadmill";
  toast'ы presence/goal, компактный форматтер длительности `humanize_short`.
- `src/goals.rs` — дневные step-goal вехи: загрузка `config/goals.json`,
  присвоение tier'ов (1–3), чистая функция «какие пороги праздновать сейчас».
- `src/logger.rs` — сырой JSONL-лог телеметрии (source-of-truth параллельно с SQLite).

## Протокол

Большинство дорожек отдают стандартный GATT-профиль **FTMS** (Fitness Machine Service, `0x1826`).
Предполагаем его как основной путь. Возможен **vendor-specific** сервис Yesoul (как в их
мобильном приложении) — это ещё не реверс-инжинирилось; см. `docs/research/`.

Ключевые UUID:
- `0x1826` — Fitness Machine Service
- `0x2ACD` — Treadmill Data (notify)
- `0x2AD9` — Fitness Machine Control Point (write/indicate) — задел под управление
- `0x2ADA` — Fitness Machine Status (notify)

## Команды

```bash
cargo run             # = scan: перечислить BLE-устройства рядом (диагностика)
cargo run -- connect  # подключиться к первой FTMS-дорожке и стримить данные
cargo run -- daemon    # фоновый режим: авто-коннект + presence + toast (для интерактивной проверки)
cargo run -- stats     # статистика за сегодня; `stats --all` — за все дни
cargo run -- widget    # компактный TSV текущей тренировки для status-bar виджета; пусто если дорожка off (см. docs/tasks/009)
cargo run -- --help    # полный список команд
cargo test             # юнит-тесты
cargo clippy           # линт
RUST_LOG=debug cargo run  # подробные логи (env-filter)

scripts/install-daemon.sh    # собрать, подписать, поставить LaunchAgent (авто-старт при логине)
scripts/uninstall-daemon.sh  # снять LaunchAgent (данные в Application Support не трогает)
scripts/build-icon.sh        # перегенерировать macos/AppIcon.icns из SF Symbol (см. generate-icon.swift)
```

Короткий алиас `tm` — симлинк на release-бинарь в `~/.bin` (в `PATH`), чтобы
звать `tm stats` / `tm status` откуда угодно. Его **создаёт/обновляет
`install-daemon.sh` и снимает `uninstall-daemon.sh`** (переопределяется через
`LINK_DIR`/`LINK_NAME`, `LINK_NAME=""` — пропустить). Симлинк указывает на
артефакт сборки, поэтому подхватывает свежий бинарь после каждого rebuild.
Вручную (без демона): `ln -sfn "$PWD/target/release/treadmill-bluetooth-macos" ~/.bin/tm`.

## Конфиг целей (step goals)

Дневные цели по шагам (до 3) — **per-user**, конфиг живёт **не в этом репо**, а в
домашней директории: **`~/.config/treadmill-bluetooth-macos/goals.json`**
(`$HOME`-anchored, работает под launchd). Формат — см. `config/goals.example.json`:
`{ "goals": [8000, 10000, 12000] }`. Резолвинг: env `TREADMILL_GOALS_CONFIG`
(override пути) → `$HOME/.config/.../goals.json` → вшитые дефолты
`[8000,10000,12000]`. Нет файла — норма (INFO + дефолты); битый файл — WARN.
Каждый пользователь приносит свой файл (например, симлинком из личного dotfiles-
репо); правки активны после рестарта демона (`launchctl kickstart -k` или
переустановка). Tier (яркость toast'а) — из ранга по возрастанию: низший порог →
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
