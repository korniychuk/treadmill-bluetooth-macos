# 035 — HR silence: relative `timeout` в `select!` никогда не набегает

> **Статус: open**  
> **Источник:** [research/003](../research/003-reliability-architecture-review.md) §3.2, Phase 0.1; severity ↑ и расширение скоупа — [research/004](../research/004-independent-reliability-review.md) §2.1  
> **Класс:** `liveness` (тот же, что 031)  
> **Приоритет:** high — latent same-class bug; чинить первым

## ⚠ Severity выше, чем казалось (review 004)

Последствие зависшего таймаута — не только «reconnect мёртв, виджет спасёт». `zh_bpm` на входе Zone Hold (`daemon.rs:903`) гейтится **только** на `hr_connected`, свежесть `last_bpm_ts` не проверяется нигде в контуре управления (15с-порог живёт только в виджете). При «линк жив, notify молчит» `ContactTracker` тоже не спасает — он per-frame, а кадров нет. Итог: Zone Hold кормит контроллер замороженным bpm; в `Band`-режиме под-зонный frozen bpm даёт `+max_step` каждые `correction_interval` — **лента разгоняется до `effective_max_speed_kmh` по мёртвому датчику**.

Поэтому скоуп расширен двумя пунктами defense-in-depth (оба дёшевы, те же строки):

- **Freshness-гейт на `zh_bpm`**: bpm старше порога (константа, ~10–15с — согласовать с `HR_NOTIFICATION_TIMEOUT`) → `None`, контроллер молчит. Контур управления не должен зависеть от единственного детектора живости.
- **Чистка `last_bpm`/`last_bpm_ts` на link-loss ветках** (`Ok(None)` ~1100 / `Err`→silence ~1114): сейчас чистит только contact-Lost путь (~1091); эти ветки всё равно переписываются под sleep_until. Инвариант: `hr_connected=false` ⇒ `last_bpm=None` (остальная HR-гигиена — задача [042](042-hr-link-state-hygiene.md)).

## Симптом (ещё не в проде, но неизбежен)

Partial GATT death / OS stall: BLE-линк к HR-датчику жив, `hr_notifications = Some`, но notify-кадры `0x2A37` перестали приходить.

Ожидание: через ~10с (`HR_NOTIFICATION_TIMEOUT`) демон считает датчик lost → `hr_notifications = None` → reconnect tick может поднять линк заново.

Факт сегодня: deadline **может никогда не наступить**, пока крутятся соседние arms `select!` (`command_tick` 1с, treadmill frames ~1/s, `config_tick` 5с). `hr_connected` остаётся `true`, reconnect gated на `hr_notifications.is_none()` (~1132) — **датчик не вернётся до рестарта сессии**. Виджет спасёт `HR_STALE_THRESHOLD_S` (скрывает ♥), но reconnect мёртв.

## Почему сейчас маскируется

Снятый Polar H10 **продолжает** слать ~1 кадр/с (frozen bpm) — «тишины» нет, срабатывает `ContactTracker` (033), не silence path. Баг всплывёт только при «линк есть, notify молчит».

## Причина

`src/daemon.rs` ~1048–1050, внутри `tokio::select!`:

```rust
Some(stream) => tokio::time::timeout(HR_NOTIFICATION_TIMEOUT, stream.next()).await,
```

`select!` пересобирает future каждой ветки **каждый pass**. Relative `timeout` стартует с нуля, как только соседний arm выиграл. Ровно класс **031** (treadmill `NOTIFICATION_TIMEOUT`), который уже починили абсолютным дедлайном.

Ирония: над HR-веткой висит комментарий про *другую* ловушку `select!` (unwrap в теле future), а эту — ту же, что 031 — не заметили.

Шаблон фикса уже в том же файле:

- treadmill silence: `sleep_until(last_telemetry_at + NOTIFICATION_TIMEOUT)` (~688)
- ещё один sleep_until ~2124

## Решение

1. Завести `last_hr_at: tokio::time::Instant` (обновлять на каждом успешном HR-кадре / при установке stream).
2. HR silence arm:

   ```rust
   _ = tokio::time::sleep_until(last_hr_at + HR_NOTIFICATION_TIMEOUT), if hr_notifications.is_some() => {
       // same body as current timeout Err(_) path:
       // warn, hr_notifications = None, hr_connected = false, reset tracker/link-scoped state
   }
   ```

3. HR frame arm: `stream.next()` **без** обёртки `timeout` (silence вынесен в отдельный arm), либо оставить `next()` + отдельный sleep arm — как у treadmill.
4. Не трогать reconnect tick / contact tracker — они уже правильные; меняется только *когда* link считается lost по silence.

Инвариант (из 003 §3.2): **HR BLE link liveness** ≠ event-loop progress; clock = last `0x2A37` frame, не `persist()` / command tick.

## Тесты

Обязательно `tokio::time::pause` **с соседним быстрым arm'ом** — без sibling-arm тест невалиден (именно он сбрасывал relative timeout).

По аналогии с `telemetry_deadline_fires_despite_a_faster_sibling_arm` (031):

- pure helper или module-level test: absolute HR deadline fires after `HR_NOTIFICATION_TIMEOUT` even when a 1s sibling arm keeps completing;
- optional: deadline does **not** fire if frames keep arriving (advance clock < timeout between frames).

`tokio` `test-util` уже в `[dev-dependencies]` после 031.

## Acceptance

- [ ] Нет `tokio::time::timeout(HR_NOTIFICATION_TIMEOUT, stream.next())` внутри `select!`
- [ ] `sleep_until(last_hr_at + HR_NOTIFICATION_TIMEOUT)` (или эквивалент absolute Instant)
- [ ] `zh_bpm` с freshness-гейтом: устаревший `last_bpm_ts` → контроллер получает `None` (unit-тест)
- [ ] Link-loss ветки чистят `last_bpm`/`last_bpm_ts` (инвариант `hr_connected=false` ⇒ `last_bpm=None`)
- [ ] Regression test with paused clock + sibling arm green
- [ ] Existing HR contact / reconnect behaviour unchanged when frames keep flowing
- [ ] Smoke (live, optional): kill notify path / power-cycle strap mid-session → reconnect within timeout + reconnect interval

## Затронутые файлы

- `src/daemon.rs` — HR arm / `last_hr_at` / silence path
- tests рядом с 031 watchdog/deadline tests

## Связанное

- 031 — treadmill relative timeout (шаблон)
- 033/034 — contact ≠ link (silence path complementary)
- research 003 §3.2, §3.10.1, Phase 0.1
