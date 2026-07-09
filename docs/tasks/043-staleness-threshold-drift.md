# 043 — Staleness-константы: дрейф main↔daemon уже случился

> **Статус: done**
> **Источник:** [research/004](../research/004-independent-reliability-review.md) §3 N2
> **Класс:** contract drift / duplicated constants
> **Приоритет:** medium-high (дёшево; влияет на routing control-команд)

## Факт дрейфа

```rust
// main.rs:1271-1275
/// Duplicated from `daemon::WATCHDOG_STALE_THRESHOLD` (private to
/// `daemon.rs`) ... keep in sync by hand.
const WATCHDOG_STALE_THRESHOLD_S: i64 = 15 + 20 + 60;   // = 95

// daemon.rs
const WATCHDOG_STALE_THRESHOLD: Duration = Duration::from_secs(120);
```

Комментарий утверждает «duplicated», значения — разные. Расхождение **уже произошло**, «keep in sync by hand» не сработал.

## Последствия (95 < 120: CLI считает демона мёртвым раньше, чем он сам)

Порог гейтит три реальных поведения CLI (`main.rs`):

1. `run_status` (~1402) — ложный «possible silent hang» WARNING при легитимном медленном scan/reconnect;
2. `widget_status_stale` (~1632) — виджет прячется при живом демоне;
3. `daemon_status_fresh` (~1806) → `daemon_holds_link` — **`tm start/stop/speed` уходит в прямое BLE-подключение**, которое может законтендить с in-flight reconnect'ом демона за единственный слот CoreBluetooth.

## Бонус-баг: `widget_speed_field` использует не тот порог

`main.rs:1574-1593`: doc-коммент обещает «same freshness threshold as `widget_hr_field`» (= `HR_STALE_THRESHOLD_S`, 15с), код использует `WATCHDOG_STALE_THRESHOLD_S` (95с) → застывшая скорость может показываться на ~80с дольше обещанного. Тестов на `widget_speed_field` нет вообще (у `widget_hr_field` — два). Реализация честно следовала самопротиворечивому спеку задачи 029 (:54).

## Решение

1. **Один источник истины**: экспортировать константу из `daemon` (сделать `pub(crate)`) или вынести в маленький общий модуль (`src/status_contract.rs` — кандидат на будущий дом контрактов status/widget из backlog [007](../backlog/007-split-god-modules.md)); `main.rs` импортирует, комментарий-враньё удалить.
   - Решить **какое** значение верное: 120с (даёмон-сторона) выглядит правильной базой; если CLI нужен свой запас — выразить как `WATCHDOG_STALE_THRESHOLD + margin`, а не независимым литералом.
2. `widget_speed_field`: привести к `HR_STALE_THRESHOLD_S` (намерение задачи 029 — «как у HR») + тесты (fresh → показывается; stale 16с → пусто; kmh=0 → пусто).
3. Существующий `const { assert!(HR_STALE_THRESHOLD_S < WATCHDOG_STALE_THRESHOLD_S) }` (~1937) сохранить/адаптировать.

## Acceptance

- [x] Ровно одно определение watchdog-порога в кодовой базе (или производное с явной формулой)
- [x] `tm status`/`tm widget`/`daemon_holds_link` согласованы с реальным порогом демона
- [x] `widget_speed_field` использует HR-порог и покрыт тестами
- [x] Нет комментариев «keep in sync by hand» на константах

## Затронутые файлы

- `src/main.rs`, `src/daemon.rs` (± новый крошечный общий модуль)

## Связанное

- 018/031 — происхождение порогов
- 029 — самопротиворечивый спек speed-поля
- 038 — `tm doctor` печатает те же пороги (должен взять shared константу)
- research 004 §3 N2
