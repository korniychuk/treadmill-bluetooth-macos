# 032 — `tm zone off` не останавливает уже запущенный Zone Hold

> **Статус: сделано** (2026-07-09, `17ac179`). Реализованы все три рубежа:
> `should_run_zone_hold(enabled, phase)` (чистая, юнит-тест на 4 комбинации),
> `disengage_zone_hold(phase, state)` (общий сброс фазы + всего снапшота),
> явный disengage в ветке hot-reload и ранний `return` в `zone_hold_tick`.
> Проверено на живом демоне: после рестарта с `enabled=false` — ни одной записи
> `zone hold: applied speed correction` в лог.

**Тип:** баг, приоритет высокий (управляет физическим устройством против воли оператора)

## Симптом

Оператор выключил Zone Hold (`tm zone off` → `enabled = false` в `config.toml`),
вручную выставил на ленте 2.5 км/ч — через ~20 с скорость сама вернулась на 3.0.
Повторно. `tm status` при этом честно печатает `zone hold: off` (CLI читает файл,
а не живое состояние демона).

## Доказательство

`~/Library/Logs/treadmill-bluetooth-macos/daemon.log`:

```
13:09:29  zone_hold config changed on disk — reloaded without a daemon restart  enabled=false
13:09:47  zone hold: applied speed correction  target=3.079
13:10:28  zone hold: applied speed correction  target=3.066
13:10:49  zone hold: applied speed correction  target=3.059
```

Коррекции продолжаются **после** перечитывания конфига с `enabled=false`.
Убывающая цель (3.09 → 3.06 → 3.05) — это `ZoneHoldPhase::Ramp`, линейный ramp
к `default_kmh` (задача 016, ~3.0 км/ч), а не HR-коррекция: нагрудный датчик
снят, `bpm` в Hold-фазу не приходит, но Ramp пульс и не смотрит.

## Причина

`daemon.rs`, hot-reload (задача 017) обновляет **конфиг**, но не **фазу**:

```rust
let reloaded_zone_hold = zone_hold::load_zone_hold_config();
if reloaded_zone_hold != config.zone_hold {
    config.zone_hold = reloaded_zone_hold;   // ← zh_phase не тронут
}
```

а гейт исполнения смотрит только на фазу:

```rust
if zh_phase != ZoneHoldPhase::Off {   // ← config.zone_hold.enabled не проверяется
    zone_hold_tick(...).await;
}
```

`zone_hold_tick` тоже ничего не знает про `enabled`. Единственное место, где
`enabled == false` переводит фазу в `Off`, — `zone_hold_on_transition`, который
вызывается только на presence-переходе. Пока оператор идёт по ленте (`Walking`
без перерыва), перехода нет — контроллер живёт до схода с ленты или рестарта
демона.

Симметричный (и тоже реальный) случай: `enabled=false` + `resolve_target_zone()`
даёт `None` — второй инвариант, который сейчас проверяется только на переходе.

## Решение

Инвариант: **выключенный в конфиге Zone Hold не пишет в Control Point никогда**,
независимо от того, в какой фазе застал его edit.

1. `daemon.rs`, ветка hot-reload: если после reload `!config.zone_hold.enabled`
   и `zh_phase != Off` → `zh_phase = Off`, сбросить снапшот
   (`zone_hold_active/_phase/_position/_target_lo/_target_hi/_last_speed`),
   `info!` о разъединении. Реюз того же кода очистки, что в ветке
   «target zone no longer resolvable» — вынести в маленький хелпер
   `disengage_zone_hold(&mut zh_phase, state)`.
2. `daemon.rs`, гейт исполнения: `if config.zone_hold.enabled && zh_phase != Off`
   — defense in depth, чтобы любой будущий путь, оставивший фазу живой, не
   доехал до BLE-записи. `else if state.zone_hold_active` (существующая ветка
   очистки снапшота) остаётся и подхватывает такой случай.
3. `zone_hold_tick`: ранний `return`, если `!config.enabled`. Третий рубеж —
   функция асинхронная и пишет в устройство, ей положено самой проверять свой
   собственный enable-флаг (Information Expert).

Пункты 2 и 3 не дублируют 1: 1 — про корректный snapshot/лог, 2–3 — про то,
что «фаза жива при выключенном конфиге» не может привести к записи в железо.

### Обратная симметрия (`off` → `on` mid-session)

Уже реализована и не трогается: та же ветка hot-reload при `zh_phase == Off &&
state == Walking` вызывает `zone_hold_on_transition` (см. комментарий «`tm zone
on` is routinely run mid-session»). Наш пункт 1 — её недостающая пара.

## Тесты

Чистые, без BLE — вся логика в `zone_hold_on_transition` уже покрыта. Добавить:

- `zone_hold_tick` с `config.enabled == false` и `phase == Ramp` не меняет фазу
  и не зовёт запись (проверяется через `last_correction_at`, который остаётся
  `None`) — потребует либо вынести решение «слать ли коррекцию» в чистую
  функцию, либо тестировать хелпер `disengage_zone_hold` + гейт отдельно.
  Предпочтительно: чистая функция `should_run_zone_hold(enabled, phase) -> bool`,
  юнит-тест на 4 комбинации.
- `disengage_zone_hold` обнуляет весь снапшот и ставит `phase = "off"`.

## Обходной путь до фикса

`launchctl kickstart -k gui/$(id -u)/pro.korniychuk.treadmill-bluetooth-macos`
— при старте `zh_phase = Off`, а engage происходит только при `enabled = true`.

## Связанное

- Задача 027 — Zone Hold.
- Задача 017 — hot-reload конфига (тот же mtime-гейт).
- Задача 030 — холостые Control-Point записи на клампе (соседняя патология
  той же ветки: «пишем в ленту, когда не должны»).
