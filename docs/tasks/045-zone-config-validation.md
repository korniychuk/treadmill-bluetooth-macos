# 045 — Валидация zone-конфига: молчаливый drop зон сдвигает `target_zone`

> **Статус: done**
> **Источник:** [research/004](../research/004-independent-reliability-review.md) §3 N5
> **Класс:** config semantics / silent fallback (родственно 040)
> **Приоритет:** medium

## Находки (все в `src/zone_hold.rs`)

### 1. Битая зона молча выпадает и перенумеровывает остальные (главное)

`parse_zone_def` (~570–611) собирается через `filter_map` (~547): зона с `name`, но опечатанной/отсутствующей границей (`min_percent` есть, `max_percent` нет; `min_bpm` без `max_bpm`) — дропается **без WARN**. `default_zones` включается только когда выпали **все** (~548). Одна битая зона → список сжался → 1-based `target_zone = 3` теперь указывает на бывшую зону 4. Молча, load-bearing. CLI-канонические id-таргеты — митигация, но hand-edited числовые остаются уязвимы.

### 2. Absolute bpm: `i64 as u16` wrap + нет `min < max`

~583–590: `min_bpm: min_bpm as u16` — `70000` заворачивается, отрицательное — в огромное; range-гарда нет (в отличие от `age`/`resting_hr`). Нет проверки `min ≤ max` ни для `Absolute`, ни для `Percent`. Инвертированная зона: Band-режим корректирует вечно (в зону не попасть), Center — молча никогда (`half_width ≤ 0` → `None`).

### 3. `find_zone` substring-тир берёт первый попавшийся

~171–175: `"recovery"` при зонах `"Recovery"`/`"Recovery walk"` резолвится порядко-зависимо и молча. Exact-тиры обычно спасают.

## Решение

1. **WARN на каждую невалидную зону** (имя + причина), не тихий `filter_map`. Если хоть одна зона дропнута **и** `target_zone` — `Number` → дополнительный WARN «zone numbering shifted, target may point at the wrong zone» (или отказ резолвить таргет — решить при имплементации; WARN минимум).
2. Range-валидация bpm-границ (разумные физиологические рамки, стиль `age`-гарда) + `min < max` для обоих режимов; невалидная → та же WARN-ветка из п.1.
3. Substring-матч: `debug!`/`info!` лог «target matched by substring», чтобы порядко-зависимость была видима.
4. Заодно (мелочь из N7, тот же файл): решить политику `warmup_minutes = 0` — сейчас `positive_int_or` (~644) запрещает легитимный «без прогрева» (0 → WARN + дефолт 5), при том что `warmup_target_speed` имеет защиту от деления на такой 0. Либо разрешить 0 как «skip warmup», либо задокументировать запрет.

**Не в скоупе:** Karvonen без `resting_hr` — это [040](040-karvonen-missing-resting-hr-warn.md).

## Тесты

- Table-driven на парсер: битая зона → WARN + остальные живы; wrap-значения → отклонены; min>max → отклонена; все битые → дефолтные 5.
- `find_zone`: substring-неоднозначность — документирующий тест текущего «первый выигрывает».

## Acceptance

- [x] Ни один невалидный `[[zone_hold.zones]]` не исчезает без WARN
- [x] Дроп зоны при числовом `target_zone` даёт явный WARN о сдвиге нумерации
- [x] bpm-границы валидируются (range + ordering)
- [x] Политика `warmup_minutes=0` выбрана и протестирована (`0` = skip warmup)
- [x] Существующие zone-тесты зелёные

## Затронутые файлы

- `src/zone_hold.rs` — `parse_zone_def`, `find_zone`, `positive_int_or`

## Связанное

- 027 — zone hold; 040 — Karvonen (та же семья «config должен WARN»)
- research 004 §3 N5
