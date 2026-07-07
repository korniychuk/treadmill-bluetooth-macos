# 023 — Миграция конфига с JSON на TOML

## Зачем

JSON не поддерживает комментарии. Оператор хочет, чтобы дефолты в конфиге можно
было **документировать закомментированными строками** («вот дефолтный ключ и его
значение — раскомментируй, чтобы поменять»). TOML это даёт из коробки + чище
читается/правится руками. Формат — под ключ: код, доки, пример, личный конфиг.

## Ключевой факт

`serde_json` в проекте используется **только** для парсинга конфига (`goals.rs`,
3 функции — `read_thresholds`, `read_workout_gap_minutes`, `read_auto_pause_minutes`).
Больше нигде (JSONL-логгер форматирует строки сам). Значит миграция = **замена
зависимости** `serde_json` → `toml`, без нового «лишнего» крейта.

## Решение — TOML-only, чистый разрыв

Формат один — TOML. Файл: **`config.toml`**. Парсим через `toml::Value` (без
derive), зеркально текущей `serde_json::Value`-логике:
`toml::from_str::<toml::Value>` → `.get(key)` → `.as_integer()` (TOML native i64) /
`.as_array()` / `.as_str()`. Трёхкейсовые `GapSetting`/`AutoPauseSetting`
(Configured/Invalid/Unset) сохраняются без изменений — меняется только парсер.

**Убираем** транзитивную обратную совместимость задачи 021 (JSON-фолбэки
`config.json`/`goals.json` и legacy-env `TREADMILL_GOALS_CONFIG`): формат теперь
один, single-user tool, миграция под ключ. Остаётся `TREADMILL_CONFIG` (env
override пути) + дефолт `$HOME/.config/treadmill-bluetooth-macos/config.toml`.
`resolve_config_path` упрощается до одного пути (фолбэк-хелпер 021 удаляется).

### Зависимости

- `Cargo.toml`: убрать `serde_json`, добавить `toml = "0.9"` (последняя стабильная;
  сверить актуальную минорную на момент правки). `toml` тянет `serde` транзитивно,
  derive не используем — только `toml::Value`.

### Пример конфига `config/config.example.toml`

Дефолты — закомментированными строками (вот ради чего всё):
```toml
# treadmill-bluetooth-macos — per-user config.
# All keys are optional; commented lines show the built-in defaults.

# Daily step goals (up to 3), each celebrated once per day with a toast.
goals = [8000, 10000, 12000]

# Merge activity segments closer than this many minutes into one displayed
# workout. Read-time & retroactive — no recompute needed.
# workout_gap_minutes = 15

# Auto-pause the belt after it runs this many minutes with nobody walking
# (you stepped off). 0 disables. The machine's own shutoff then powers it down.
# auto_pause_minutes = 5
```

## Затронутые файлы

- `Cargo.toml` / `Cargo.lock` — `serde_json` → `toml`.
- `src/goals.rs` — 3 парсера на `toml::Value`; `HOME_CONFIG_RELPATH` →
  `config.toml`; удалить legacy-env/fallback (`CONFIG_ENV_LEGACY`,
  `HOME_CONFIG_RELPATH_LEGACY`, `resolve_config_path`), `config_path` упрощается;
  тесты пишут TOML вместо JSON.
- `config/config.example.json` → **`config/config.example.toml`** (git rm + new),
  с комментариями-дефолтами.
- `README.md`, `CLAUDE.md`, `CHANGELOG.md`, `scripts/install-daemon.sh`,
  `scripts/install-prebuilt.sh` — `config.json`→`config.toml`, пример-блок в TOML,
  убрать упоминания JSON-фолбэка/legacy-env.

## Личная миграция оператора (dotfiles)

- `ankor-dotfiles`: `config.json` → `config.toml`, содержимое в TOML, **сохранив
  текущие значения** (goals `[8500,10750,13000]`, gap 15, auto-pause **3**).
  `git rm config.json` + новый `config.toml`, коммит.
- Симлинк `~/.config/treadmill-bluetooth-macos/config.json` → снять; создать
  `config.toml` → на новый target.
- Порядок: сначала новый бинарь (читает config.toml), потом переключить файл.

## Проверка

- Юнит: `read_*` парсят TOML (configured/unset/invalid/disabled), `resolve`/
  `config_path` резолвит `config.toml`.
- `cargo build`/`clippy -D warnings`/`fmt`/`test` — зелёные; `serde_json` исчез
  из `cargo tree`.
- На железе: reinstall + миграция → лог `loaded config … goals=[8500,10750,13000]
  auto_pause=Some(180s)` (реальные значения из TOML, 3 мин), `tm status` их
  показывает; hot-reload по `config.toml` работает.

## Заметки / риски

- Чистый разрыв: старый `config.json`/`goals.json` больше не читаются. Для
  single-user tool ок; в CHANGELOG отметить как breaking (pre-release).
- TOML целые — i64 native (в JSON были `as_i64` c filter); поведение эквивалентно.
