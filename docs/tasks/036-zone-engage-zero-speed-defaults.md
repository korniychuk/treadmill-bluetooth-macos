# 036 — Engage-пути Zone Hold: `unwrap_or(0.0)` — скрытый инвариант, не живой баг

> **Статус: done**  
> **Источник:** [research/003](../research/003-reliability-architecture-review.md) §3.5, Phase 0.2; **премиза скорректирована** [research/004](../research/004-independent-reliability-review.md) §2.2  
> **Класс:** invariant hardening + regression gap (хвост 030)  
> **Приоритет:** ~~high~~ → **medium** — путь сегодня недостижим (см. ниже), из Phase 0 выведена

## ⚠ Поправка премизы (review 004)

Утверждение «MORE_DATA frame (`speed=None`) на transition может стартовать Ramp с 0» — **неверно для текущего кода**. `presence.rs:75-93`: `observe` при `speed_kmh = None` возвращает `None` (state не меняется → нет transition), значит transition-блок `daemon.rs:747` выполняется только с `Some(speed)` — все три `unwrap_or(0.0)` недостижимы с `None`. Проверено двумя независимыми ревьюерами и вручную.

**Но задача остаётся**: безопасность :755/:771/:797 — **скрытый кросс-модульный инвариант**, живущий в одной строке `presence.rs:86`. Если presence когда-нибудь научится давать transition без скорости (например, по шагам), три `unwrap_or(0.0)` молча оживут ровно с описанными ниже последствиями. Фикс тот же — протащить `Option` и убрать fabricated 0.0; меняется только срочность.

## Плюс: незакрытый regression-пробел 030-part-B

Инвентаризация тестов (004 §4) показала: `zone_hold_tick` skip-on-`None` (второй фикс 030, `daemon.rs` ~1527) — **единственный фикс кластера 030–034 без регрессионного теста** (ноль call sites в `mod tests`). Закрыть здесь же — та же тема Option-speed.

## Потенциальный симптом (если инвариант сломается)

Engage-пути подставляют `0.0` → контроллер / restore / default-speed читают «лента остановилась» на живой ходьбе → Ramp может стартовать с 0, default-speed ceiling check врёт, restore toast врёт.

## Контекст: 030 починил не всё

В 030 `zone_hold_tick` уже принимает `Option<f32>` и **пропускает тик** на `None` — правильный паттерн.

Engage-пути в `stream_with_presence` **остались** (достижимы только через transition, см. поправку выше):

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

- [x] Нет `data.speed_kmh.unwrap_or(0.0)` на engage/resume/default путях
- [x] Гипотетический transition с `speed=None` не даёт target/ramp seed 0.0 (защита не зависит от `presence.rs:86`)
- [x] `try_apply_default_speed` / restore не получают fake 0.0
- [x] `zone_hold_tick` path (030) остаётся skip-on-None **и получает regression-тест** (пробел 030-part-B закрыт)
- [x] Regression tests green

## Затронутые файлы

- `src/daemon.rs` — presence match ~754–850
- возможно thin pure helper рядом с zone/default_speed

## Связанное

- 030 — tick path fixed; this is the engage-path remainder
- 016 — default speed on walk start
- 012 — pause/resume restore
- 028 — ramp stale start speed
- research 003 §3.5, Phase 0.2
