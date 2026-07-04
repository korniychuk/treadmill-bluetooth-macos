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

- `src/main.rs` — точка входа и CLI (`scan` | `connect`).
- `src/scan.rs` — обнаружение адаптера, скан, подключение, подписка на нотификации.
- `src/ftms.rs` — константы Fitness Machine Service (`0x1826`) и парсинг Treadmill Data (`0x2ACD`).

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
cargo test            # юнит-тесты (парсинг FTMS)
cargo clippy          # линт
RUST_LOG=debug cargo run  # подробные логи (env-filter)
```

## Заметки по macOS

- Первый запуск запросит разрешение на Bluetooth (CoreBluetooth). Без него скан пуст.
- Адресов устройств на macOS нет — идентификатор это непрозрачный system UUID.

## Конвенции

- Комментарии в коде — только на английском.
- Логируем аномалии/edge cases, а не happy path.
- Держим файлы маленькими и однонаправленными; парсинг протокола отдельно от транспорта.
- Docs-first: перед задачей — заметка в `docs/tasks/`, после — обновить затронутые доки.
