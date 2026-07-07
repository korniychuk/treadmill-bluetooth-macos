# 021 — Переименование `goals.json` → `config.json` (с обратным фолбэком)

## Проблема

Файл `~/.config/treadmill-bluetooth-macos/goals.json` давно перерос имя: в нём
уже `goals` + `workout_gap_minutes` (задача 014) + `auto_pause_minutes` (задача
020). Это общий per-user конфиг, а имя говорит только про цели.

## Решение — `config.json` + backward-compat

Директория (`treadmill-bluetooth-macos/`) уже неймспейсит, поэтому внутри
достаточно **`config.json`**.

**Обратная совместимость обязательна** — миграция не должна ломать запущенные
инсталляции ни на секунду:
- Новый предпочтительный путь: `…/config.json`.
- Если `config.json` **нет**, а старый `goals.json` **есть** — читаем старый
  (тихо, без спама: `config_mtime`/`load_*` дёргаются на 2-с поллинге `widget`).
- Env-override: новый `TREADMILL_CONFIG`, со старым `TREADMILL_GOALS_CONFIG` как
  фолбэк (новый имеет приоритет).
- Ни того ни другого файла нет → резолвим в **новый** путь (`config.json`), чтобы
  «нет файла»-сообщения и дефолты указывали на новое имя.

Rust-модуль остаётся `goals.rs` (переименование модуля — отдельный, более
широкий рефактор импортов `crate::goals`; вне скоупа этой задачи). Он и так уже
не только про цели; трогать имя модуля сейчас — лишний риск без пользы.

## Резолвинг пути (новый `config_path`)

```
TREADMILL_CONFIG (env)               — если задан
  → TREADMILL_GOALS_CONFIG (env)     — legacy, если задан
  → $HOME/.config/.../config.json    — если существует
  → $HOME/.config/.../goals.json     — legacy, если существует
  → $HOME/.config/.../config.json    — дефолтная цель (даже если её нет)
```

Чистая, без логирования (логи остаются в `load_*`, как сейчас), чтобы не спамить
на hot-path `widget`.

## Затронутые файлы

- `src/goals.rs` — `CONFIG_ENV`/`CONFIG_ENV_LEGACY`, `HOME_CONFIG_RELPATH`/
  `…_LEGACY`, новый `config_path()` с фолбэком; тест на приоритет new-over-legacy.
- `config/goals.example.json` → **`config/config.example.json`** (git mv).
- `README.md`, `CLAUDE.md`, `CHANGELOG.md`, `scripts/install-daemon.sh`,
  `scripts/install-prebuilt.sh` — заменить `goals.json`→`config.json`,
  `goals.example.json`→`config.example.json`, упомянуть env `TREADMILL_CONFIG`
  (+ legacy-фолбэк одной строкой).

## Личная миграция оператора (вне репо)

- `ankor-dotfiles`: `git mv treadmill/goals.json treadmill/config.json`, коммит.
- Симлинк: `~/.config/treadmill-bluetooth-macos/goals.json` снять, создать
  `config.json` → на переименованный target.
- **Порядок:** сначала поставить новый бинарь (знает оба имени), потом
  переименовать файл — благодаря фолбэку окно миграции безопасно в любом порядке.

## Проверка

- Юнит: `config_path` предпочитает `config.json`, падает на `goals.json` только
  когда нового нет; env new > env legacy.
- На железе: после reinstall + переименования — `tm status`/лог показывают
  реальные цели оператора (не вшитые дефолты), hot-reload работает по новому файлу.
