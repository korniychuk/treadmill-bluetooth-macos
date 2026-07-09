# 046 — `raw_samples`: индекс по `ts_ms` + решение по retention

> **Статус: done**
> **Источник:** [research/004](../research/004-independent-reliability-review.md) §3 N6
> **Класс:** storage growth / query performance
> **Приоритет:** medium-low (деградация линейная, не обрыв)

## Проблема

1. **Нет индекса по `ts_ms`.** `raw_samples` имеет только `idx_raw_samples_session(session_id)` (`store.rs` ~270–282), а горячие чтения фильтруют/сортируют по `ts_ms`:
   - `raw_distance_m` (~804) — на каждую тренировку в `tm stats`;
   - `walking_speeds_in_window` (~825) — default-speed;
   - `raw_samples_ordered` (~856) — весь `recompute-segments`.
   При 1–2 Гц телеметрии и годах истории — full scan, линейно медленнее с каждым днём. `hr_samples` индекс по `ts_ms` уже имеет (задача 025) — `raw_samples` просто отстал.
2. **Нет retention.** `raw_samples` и `status_events` растут бесконечно (у `control_commands` retention есть — `store.rs` ~36). Правило дома: «bound anything that accumulates».

## Решение

1. `CREATE INDEX IF NOT EXISTS idx_raw_samples_ts ON raw_samples(ts_ms)` в `migrate` — одна строка, немедленно.
2. Retention — **решение, не рефлекс**: `raw_samples` — ground truth для `recompute-segments`/`default-speed`, стирать нельзя бездумно.
   - Сначала измерить: размер БД сейчас, скорость роста (байт/день ходьбы).
   - Кандидат-политика: `status_events` — простой age-based prune (как control_commands); `raw_samples` — либо age-based с большим горизонтом (например, ≥ 12 месяцев, за пределами которого recompute уже не пересчитывают), либо явное «не трогаем, дёшево» с записанным обоснованием и измеренной цифрой.
   - Решение зафиксировать в этом доке при имплементации.

## Тесты

- Store-тест: prune-политика (какая бы ни была выбрана) удаляет старое, не трогает окно.
- Косвенно: существующие recompute/default-speed тесты зелёные с индексом.

## Acceptance

- [x] Индекс `raw_samples(ts_ms)` создаётся миграцией
- [x] `status_events` ограничен retention'ом (90 days)
- [x] Политика по `raw_samples` выбрана, обоснована измерением и записана здесь
- [x] `tm stats --all` / `recompute-segments` не полносканят по `ts_ms`

## Retention decision (implementation)

- **Measured (2026-07-10):** live `treadmill.db` ≈ **8 MB** after months of use —
  cheap; recompute/default-speed still need long history.
- **`raw_samples`:** **no prune** in v1. Ground truth for `recompute-segments` and
  `default-speed`; growth is linear and currently small.
- **`status_events`:** prune rows older than **90 days** on migrate/open.

## Затронутые файлы

- `src/store.rs` — `migrate`, prune

## Связанное

- 015/016 — читатели `raw_samples`
- research 004 §3 N6
