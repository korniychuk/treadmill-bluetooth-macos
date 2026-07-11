# 057 — Cyan highlight for configurable values in CLI output

## Проблема

Вывод read-команд (`tm status`, `tm zone`, `tm zone list`, `tm doctor`, …) —
плоский текст: статические подписи и значения выглядят одинаково. Оператор не
видит с первого взгляда, **что из напечатанного можно поменять в конфиге**
(`config.toml` / `tm ...`-сеттеры). Пример: `tm zone` печатает
`age 31, method hrmax, tracking band` — что здесь настраиваемое, а что вычислено,
неочевидно (было прямо озвучено оператором).

## Решение

Подсвечивать **голубым (cyan, ANSI 36)** ровно те токены, которые оператор
может изменить, редактируя `config.toml` или через `tm ...`-сеттер. Цвет =
строгий сигнал «это конфигурируемое значение».

### Семантика (согласовано)

- **Красим только значение**, не подпись и не описание. `tracking band` →
  cyan только `band`.
- **Не красим** живое/вычисленное состояние: `phase`, `active`, target-**bpm**
  (резолвится из HRmax в рантайме), power mode, computed default speed,
  presence, heartbeat-возраст, contact — всё это не конфиг.
- Граница: если для изменения токена оператор лезет в `config.toml` (или зовёт
  `tm zone …` / `tm speed-widget …`) — красим. Если токен выведен из живых
  данных / телеметрии — нет.

### Охват

Все read-команды **плюс** confirmation-строки write-команд, которые эхом
выводят только что записанное конфиг-значение (уточнение оператора).

## Механика раскраски

Новый хелпер в `src/commands/common.rs`:

```rust
use std::io::IsTerminal;
use std::sync::OnceLock;

/// Whether stdout should carry ANSI colour: it is a terminal AND the caller
/// hasn't opted out via the `NO_COLOR` convention (https://no-color.org).
/// Cached — the answer cannot change within one CLI invocation.
fn color_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
    })
}

/// Wrap a *configurable* value in cyan so it reads as "you can change this in
/// config.toml" (задача 057). No-op when colour is disabled (piped output,
/// `NO_COLOR`) so scripts/grep still get clean text.
pub(crate) fn highlight_config<T: std::fmt::Display>(value: T) -> String {
    if color_enabled() {
        format!("\x1b[36m{value}\x1b[0m")
    } else {
        value.to_string()
    }
}
```

- **TTY-гейт** обязателен: пайп в файл/`grep`/скрипт получает чистый текст (тот
  же принцип, что уже реализован в `stats::raw_hint`, `\x1b[2m` под
  `is_terminal()`).
- **`NO_COLOR`** уважаем (наличие переменной любой, даже пустой → выкл.).
- **Кэш** через `OnceLock` — TTY/env не меняются в пределах запуска.
- **Рефактор `raw_hint`** (stats.rs): прогнать через тот же `color_enabled()`
  вместо своей проверки `is_terminal()` — единый гейт (и `raw_hint` заодно
  начнёт уважать `NO_COLOR`). `raw_hint` красит faint (`\x1b[2m`), не cyan, —
  оставить faint, но взять решение «красить ли» из `color_enabled()`.

### Гейт корректности: выравнивание колонок

`zone list` печатает через padding (`{:<14}`, `{:<16}`). ANSI-байты входят в
подсчёт ширины `{:<N}` и **сломают выравнивание**. Правило: **паддинг сначала,
цвет поверх** — `highlight_config(format!("{:<14}", zone.name))`. Хвостовые
пробелы внутри cyan невидимы (нет глифа), выравнивание сохраняется.

### Гейт корректности: юнит-тесты

`format_doctor_report` — чистая функция, тесты проверяют `.contains(...)`. Тесты
идут в non-TTY (`cargo test`) → `color_enabled()` = `false` → чистый текст →
`contains` продолжает матчиться. Красить внутри неё безопасно. (Единственный
конфиг-токен там — `config enabled: {bool}`.)

## Точный список токенов

### `tm status` (`commands/status.rs::run_status`)
- Config-строка: `goals {goals_desc}` → cyan `goals_desc`; `auto-pause {auto_pause}`
  → cyan `auto_pause`.
- `workout gap: {N}m` → cyan `{N}m`.
- Zone-hold строка: `zone hold: off` / `zone hold: on (...)` → cyan `off` / `on`
  (это отражение конфиг-флага `enabled`). В ветке `active` (`zone hold: active,
  phase …, target …`) — **ничего** (всё живое).
- НЕ красим: presence, `since …`, power mode, heartbeat, target range, bpm.

### `tm doctor` (`commands/status.rs::format_doctor_report`)
- `config enabled:   {bool}` → cyan `{bool}`.
- Всё остальное — живое/inferred → без цвета.

### `tm zone` статус (`commands/zone.rs::print_zone_status`)
- `Zone Hold: on|off` → cyan `on|off` (флаг `enabled`).
- `age {age}` → cyan `age`; `method {method}` → cyan `method`;
  `tracking {tracking}` → cyan `tracking`.
- `target zone #{n} {id} ({name})` → cyan `#{n} {id} ({name})` (какая зона
  выбрана таргетом + её identity — всё конфиг).
- `{low}-{high} bpm` → **без цвета** (резолвится из HRmax, живое).
- `speed {min}-{max} km/h` → cyan `min` и `max` (это `min_speed_kmh` +
  `effective_max_speed_kmh` — конфиг).
- Строка `active now: …` → **без цвета** (живое).

### `tm zone list` (`commands/zone.rs::zone_list`)
- Заголовок `Configured zones ({method_label})` → cyan `method_label`.
- Каждая зона: `name` (cyan, pad-then-colour `{:<14}`), `id` (cyan, pad-then-colour
  `{:<16}`), `max {max_speed} km/h` → cyan (весь `max … km/h` кусок как значение).
- `range` (bpm/percent) → **без цвета** (резолв bpm — живое; percent-fallback
  оставляем без цвета ради консистентности).
- Маркер `*` таргета, префикс `id=` — подписи, без цвета.

### `tm stats`, `tm default-speed`
- **Конфиг-значений в выводе нет** (gap применяется молча; «15% trim», «≥30m» —
  константы, не конфиг; computed default speed выведен из истории). Изменений
  нет — команды проверены, красить нечего. (`raw_hint` рефакторится, но это про
  faint-hint, не про конфиг.)

### `tm speed-widget` статус (`widget.rs::run_speed_widget`)
- `Speed widget: on|off` → cyan `on|off` (отражение `show_speed`).

### Confirmation-строки write-команд
- `zone_limits`: `… max {m} km/h` / `min {m} km/h` → cyan числа `m`.
- `zone_target`: `target zone set to #{n} {id} ({name}).` → cyan `#{n} {id} ({name})`.
- `zone_mode`: `tracking mode set to \`{tracking}\`.` → cyan `{tracking}`.
- `zone_add`: `Added zone \`{id}\` ({name}).` → cyan `id`, `name`.
- `zone_edit`: `Updated zone \`{id}\`.` → cyan `id`.
- `zone_remove`: `Removed zone \`{id}\` ({name}).` → cyan `id`, `name`.
- `zone_on`: `Zone Hold enabled.` / `Off`: `Zone Hold disabled.` → cyan
  `enabled` / `disabled` (отражают записанный `enabled`-флаг).
- `set_show_speed`: `Speed widget enabled|disabled.` → cyan `enabled|disabled`.
- НЕ красим: `zone_onboarding_prompt` HRmax (`≈ … bpm` — вычислено из age),
  промпты (интерактив, не эхо записанного значения).

## Тесты

- Юнит-тест на `highlight_config`: под non-TTY (тестовое окружение) возвращает
  чистое значение без escape-байт (детерминированно, т.к. `cargo test` — не
  TTY). Проверить и `NO_COLOR`-ветку косвенно (в тесте stdout не TTY, так что
  `color_enabled()` = false в любом случае — достаточно assert «нет `\x1b`»).
- Существующие doctor-тесты должны продолжать проходить без изменений.
- `cargo test` + `cargo clippy` зелёные.

## Файлы

- `src/commands/common.rs` — `color_enabled()` + `highlight_config()` (+ юнит-тест).
- `src/commands/status.rs` — токены в `run_status` + `format_doctor_report`.
- `src/commands/zone.rs` — `print_zone_status`, `zone_list`, confirmation-строки
  (`zone_limits`/`zone_target`/`zone_mode`/`zone_add`/`zone_edit`/`zone_remove`/
  `zone_on`/off-ветка).
- `src/commands/stats.rs` — рефактор `raw_hint` на общий `color_enabled()`.
- `src/widget.rs` — `run_speed_widget` статус + `set_show_speed` confirmation.
- `CLAUDE.md` — короткая заметка про cyan-конвенцию (cyan = конфигурируемое).

## Статус: реализовано

Все пункты выполнены. Верификация через pty (`script -q /dev/null`):
`zone list` красит `method`/`name`/`id`/`max`, оставляя bpm-range plain,
выравнивание колонок цело (pad-then-colour); `doctor` красит `config enabled`;
piped-вывод и `NO_COLOR=1` — полностью чистый текст. `cargo build`/`clippy`
чисто, 192 теста зелёные.

## Не в scope

- `tm widget` (TSV/tmux-виджет) — машинный вывод, ANSI туда не идёт (свой tmux
  `#[fg=…]` формат).
- Раскраска не-конфиг значений (телеметрия, живое состояние) — сознательно нет.
- Смена самого цвета/тема — cyan зафиксирован.
