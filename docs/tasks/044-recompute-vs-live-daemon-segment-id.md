# 044 — `recompute-segments` при живом демоне: id-коллизия портит чужой сегмент

> **Статус: done**
> **Источник:** [research/004](../research/004-independent-reliability-review.md) §3 N3
> **Класс:** data integrity / dual-writer window
> **Приоритет:** medium (тихая порча истории; проявляется только при recompute во время ходьбы)

## Сценарий

1. Демон идёт по тренировке, держит **id открытого сегмента** в памяти (`current_segment`), продлевает его `credit_activity` → `UPDATE activity_segments ... WHERE id = ?5` (`store.rs` ~541).
2. Оператор запускает `tm recompute-segments`. `replace_activity_segments` (`store.rs` ~940) делает `DELETE FROM activity_segments` + reinsert с детерминированными id **с 1** (scratch-replay, `recompute.rs`).
3. Кэшированный демоном id почти наверняка существует снова — но теперь указывает на **другой, закрытый исторический** сегмент.
4. Следующий зачтённый шаг: `UPDATE` успешен (`rows == 1`) → живые шаги/дистанция молча дописываются в чужую строку. Страховка в `credit_activity` (~528) ловит только `rows == 0` («wiped DB») — этот случай не её.

Ни `docs/tasks/015`, ни доки команды не требуют останавливать демона перед recompute.

## Решение (два слоя, оба дешёвые)

1. **Guard в CLI** (`recompute-segments`, и заодно `recompute-hr` — тому это не критично, но консистентно): если демон-процесс жив и heartbeat свеж (та же проверка, что `tm status` / `daemon_holds_link`) — отказ с сообщением «stop the daemon first (`launchctl …`) or wait for idle»; `--force` не делать (YAGNI).
   - Вариант мягче: отказывать только когда `presence_state == Walking` / открытый сегмент существует. Начать с простого «демон жив → отказ»: recompute — редкая ремонтная операция.
2. **Identity-check в `credit_activity`**: продление сегмента матчит не голый `id`, а `id + started_at_ms` (кэшировать вместе с id). Не совпало → как `rows == 0`: открыть новый сегмент, WARN «cached segment id no longer matches — reopened». Закрывает и будущие переписыватели таблицы, не только recompute.

Слой 2 — настоящий фикс (инвариант в data layer), слой 1 — UX, чтобы не терять открытый сегмент без нужды.

## Тесты

- Store-тест: открыть сегмент, `replace_activity_segments` с переназначенными id, снова `credit_activity` со старым (id, started_at) → новый сегмент открыт, историческая строка не изменилась, WARN.
- Существующие `credit_activity_*` тесты зелёные.

## Acceptance

- [x] `credit_activity` не может дописать в сегмент с совпавшим id, но другим `started_at`
- [x] `tm recompute-segments` при живом демоне отказывает с понятным сообщением
- [x] task 015 док + CLAUDE.md упоминают предусловие/поведение
- [x] Regression-тест на id-переназначение

## Затронутые файлы

- `src/store.rs` — `credit_activity` identity-check
- `src/daemon.rs` — кэшировать `(id, started_at_ms)`
- `src/main.rs` / `src/recompute.rs` — guard
- `docs/tasks/015-...md` — предусловие

## Связанное

- 015 — recompute-segments
- 034 — recompute-hr (только delete, id-коллизии не подвержен — сверить и зафиксировать в доке)
- research 004 §3 N3
