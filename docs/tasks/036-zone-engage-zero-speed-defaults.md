# 036 — Engage-пути Zone Hold: `unwrap_or(0.0)` на missing speed

> **Статус: open**  
> **Источник:** [research/003](../research/003-reliability-architecture-review.md) §3.5, Phase 0.2  
> **Класс:** `units` / call-site defaults (хвост 030)  
> **Приоритет:** high

## Симптом (latent)

На presence-transition / engage Zone Hold кадр FTMS Treadmill Data может нести `speed_kmh = None` (флаг `MORE_DATA` — скорость в следующем кадре, см. `src/ftms.rs`).

Сейчас engage-пути подставляют `0.0` → контроллер / restore / default-speed читают «лента остановилась» на живой ходьбе → Ramp может стартовать с 0, default-speed ceiling check врёт, restore toast врёт.

## Контекст: 030 починил не всё

В 030 `zone_hold_tick` уже принимает `Option<f32>` и **пропускает тик** на `None` — правильный паттерн.

Engage-пути в `stream_with_presence` **остались**:

| Строка (≈) | Код | Использование |
|---:|---|---|
| 755 | `zh_effective_speed_kmh = data.speed_kmh.unwrap_or(0.0)` | seed для zone engage + snapshot |
| 771 | `resumed_speed = data.speed_kmh.unwrap_or(0.0)` | resume after pause → restore / default |
| 797 | `resumed_speed = data.speed_kmh.unwrap_or(0.0)` | Unknown→Walking → default speed |

`zh_effective_speed_kmh` дальше кормит `zone_hold_on_transition` / ramp start speed (комментарий ~836–840 прямо говорит «use effective, not raw sample»).

## Правило

**Отсутствие измерения ≠ ноль.** Для физической скорости `None` обязан short-circuit:

- не стартовать Ramp с 0;
- не считать belt «crawl» (`≤0.8`) при unknown speed;
- не писать в Control Point на основании выдуманной скорости.

Следующий telemetry frame приходит &lt;1с — пропуск одного transition-tick безопаснее, чем ложный 0.

## Решение

1. На transition-ветках presence: работать с `Option<f32>` до последнего момента.
2. Если `data.speed_kmh` is `None` **и** speed нужна для решения (default ceiling, restore compare, ramp seed):
   - **не** вызывать `try_apply_default_speed` / `try_restore_speed` с 0.0;
   - **не** engage Zone Hold ramp с seed 0.0;
   - допустимо: defer engage до кадра со speed, **или** engage с phase, но без speed-write до первого measured sample (предпочтительно: skip speed-dependent side-effects, presence state всё равно обновлять).
3. Конкретный минимальный фикс (предпочтительный, KISS):

   ```rust
   let measured = data.speed_kmh; // Option<f32>
   let mut zh_effective_speed_kmh = measured;
   // restore/default only when measured.is_some()
   // zone_hold_on_transition gets Option or only called when Some
   ```

4. Пройтись `rg 'unwrap_or\(0\.0\)'` по `daemon.rs` speed/bpm paths — на measurement paths не должно остаться; config floors — отдельно.

**Не в скоупе:** Karvonen `resting_hr.unwrap_or(0)` — это задача 040 (не measurement, algebraic fallback).

## Тесты

- Unit / pure: helper «effective engage speed» — `None` → no seed / skip; `Some(3.2)` → 3.2; after restore applied → restored value.
- Если логику оставить inline: regression test на `zone_hold_on_transition` / default-speed guard с `None` measured (не вызывает write, не стартует ramp @ 0).
- Не нужен BLE.

## Acceptance

- [ ] Нет `data.speed_kmh.unwrap_or(0.0)` на engage/resume/default путях
- [ ] MORE_DATA frame (`speed=None`) на Walking transition не даёт target/ramp seed 0.0
- [ ] `try_apply_default_speed` / restore не получают fake 0.0
- [ ] `zone_hold_tick` path (030) остаётся skip-on-None
- [ ] Regression tests green

## Затронутые файлы

- `src/daemon.rs` — presence match ~754–850
- возможно thin pure helper рядом с zone/default_speed

## Связанное

- 030 — tick path fixed; this is the engage-path remainder
- 016 — default speed on walk start
- 012 — pause/resume restore
- 028 — ramp stale start speed
- research 003 §3.5, Phase 0.2
