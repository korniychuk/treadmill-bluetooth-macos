# 041 — Safety-cap force-reduce: no-op Control Point writes (030-класс на safety-пути)

> **Статус: done**
> **Источник:** [research/004](../research/004-independent-reliability-review.md) §3 N1
> **Класс:** `units` / deadband (тот же, что 030)
> **Приоритет:** high — живой воспроизводимый UX-баг, ~10 строк фикса

## Симптом

Лента прижата к `min_speed_kmh`, пульс выше `safety_cap` (80% HRmax), но не выше `hard_stop` (85%) → двойной бип ленты (RequestControl+SetSpeed) **каждый safety-cooldown (~5с), бесконечно**, без какого-либо изменения скорости. Ровно симптом 030, на пути, который фикс 030 не покрыл.

## Причина

`src/daemon.rs` ~1593–1602, ветка safety-cap «не hard-stop»:

```rust
let target = (measured_speed_kmh - config.max_step_kmh * 2.0)
    .max(config.min_speed_kmh);
warn!(bpm, safety_cap, target, "zone hold: safety cap exceeded — force-reducing speed");
apply_zone_hold_speed(peripheral, target).await;   // unconditional
```

Обычный контур давит no-op записи внутри `zone_hold::next_speed` (`MIN_SPEED_CHANGE_KMH`, фикс 030). Safety-путь зовёт `apply_zone_hold_speed` напрямую — deadband-гарда нет. На пине к `min_speed`: `target = max(min − 2·step, min) = min ≈ measured` → запись-пустышка.

`apply_zone_hold_speed` пишет безусловно **by design** — гард обязан жить у вызывающего (или переехать внутрь, см. Решение).

## Решение

1. Перед записью сравнить `target` с `measured_speed_kmh` через `MIN_SPEED_CHANGE_KMH` (тот же порог, что 030): разница меньше → **не писать** и не спамить WARN каждый cooldown (лог один раз на spell, DEBUG/INFO «safety cap active, already at floor»).
2. Предпочтительная форма — вытащить чистый helper рядом с существующими (`safety_force_reduce_target(measured, max_step, min_speed) -> Option<f32>`), возвращающий `None`, когда писать нечего; unit-тест на «пин к min → None».
3. Рассмотреть (не обязательно в этой задаче): перенести deadband-гард **внутрь** `apply_zone_hold_speed`, чтобы будущие пути записи не забывали его снова — это шаг к choke point из задачи [039](039-control-source-and-operator-override.md).

## Тесты

- Pure: `safety_force_reduce_target`: measured=min → `None`; measured=min+0.3 → `Some(min)`; measured велик → `Some(measured − 2·step)`.
- Regression: сценарий «пин к min, bpm между cap и hard_stop» не производит записи (если логика останется inline — тест на решение, не на BLE).

## Acceptance

- [x] Пин к `min_speed` при bpm ∈ (safety_cap, hard_stop] → ноль Control Point записей, ноль бипов
- [x] Реальный force-reduce (measured > min) работает как раньше
- [x] WARN не спамится каждый cooldown на «уже на полу»
- [x] Unit-тесты зелёные

## Затронутые файлы

- `src/daemon.rs` — safety-ветка `zone_hold_tick`
- возможно `src/zone_hold.rs` — чистый helper

## Связанное

- 030 — тот же класс на обычном контуре
- 039 — choke point для всех записей (стратегический фикс класса)
- research 004 §3 N1
