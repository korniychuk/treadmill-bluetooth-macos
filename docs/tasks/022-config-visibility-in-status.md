# 022 — Видимость загруженного конфига в `tm status`

## Проблема

Демон перечитывает конфиг сам (mtime-watch, задачи 017/020), но из CLI не видно
**что именно сейчас загружено в демоне** и **когда он последний раз читал файл**.
Из лога видно, из `tm status` — нет. Оператор просил: показать текущие значения /
форс-перечитать / показать время последнего чтения.

## Решение — снапшот загруженного конфига в `daemon_status` + вывод в `status`

`daemon_status` (одна строка, uпsert на каждом переходе) — правильное место:
`status` уже читает её, не трогая BLE. Демон пишет туда то, что реально держит в
памяти, плюс время последнего чтения. Это честнее, чем отдельный `tm config`,
который читал бы файл сам (показал бы «что будет загружено», а не «что в демоне» —
расходятся до 5 с). Форс-команда не нужна: авто-подхват ≤5 с, а мгновенно — `touch`
файла или рестарт.

`workout_gap_minutes` в снапшот НЕ кладём: он read-time (задача 014), демон его не
держит — CLI резолвит его сам на чтении и показывает отдельной строкой как
«read-time».

### Схема (`store.rs`)

Три новых nullable-колонки в `daemon_status` (через `ALTER TABLE ADD COLUMN`,
идемпотентно — игнор «duplicate column name», чтобы существующая БД оператора
мигрировала на месте):
- `config_goals TEXT` — пороги через запятую, напр. `8500,10750,13000`.
- `config_auto_pause_secs INTEGER` — секунды, `NULL` = авто-пауза выключена.
- `config_loaded_at TEXT` — RFC3339 момента последнего (пере)чтения.

Старые строки (демон до этого апдейта) → колонки `NULL` → `status` просто не
печатает config-строку, пока демон не перезапишет (после reinstall).

### Демон (`daemon.rs`)

- `DaemonState` +3 поля + метод `set_config(&goals, auto_pause)` (обновляет снапшот
  и `config_loaded_at = now`).
- `auto_pause_threshold` грузится теперь в `run()` (рядом с `step_goals`) и
  прокидывается в `stream_with_presence` как `&mut Option<Duration>` (по образцу
  `step_goals`), чтобы был единый снапшот.
- `set_config` вызывается: в `run()` при инициализации и в `config_tick` при
  реальном изменении файла (внутри mtime-гейта) → затем `persist`, чтобы
  `config_loaded_at` обновился.

### CLI (`main.rs run_status`)

После power-mode блока:
```
config (in daemon): goals 8500 / 10750 / 13000 · auto-pause 5m · read 2026-07-07 20:52 (3m ago)
workout gap:        15m (read-time, applied on read)
```
`auto-pause` = `off` при `NULL`. Переиспользуем `describe_timestamp` для «read …».
Строка печатается только если `config_loaded_at` есть (демон новый).

## Затронутые файлы

- `src/store.rs` — миграция ADD COLUMN (+ helper), `DaemonStatus` +3 поля,
  upsert/read SQL; тест roundtrip с новыми полями.
- `src/daemon.rs` — `DaemonState` снапшот + `set_config`; загрузка/проводка
  `auto_pause_threshold` из `run()`; вызовы `set_config`.
- `src/main.rs` — config-блок в `run_status` + формат минут/целей.
- `CLAUDE.md` — упомянуть, что `tm status` показывает загруженный конфиг и время.

## Проверка

- Юнит: store roundtrip с `config_*`; миграция ADD COLUMN идемпотентна.
- На железе: `tm status` показывает реальные цели/порог/время; правка файла →
  через ≤5 с `read …` и значения обновляются.
