# 012 — Длительность паузы + авто-восстановление скорости на resume

Расширение задачи 010 (её «бонусная» pause-часть вынесена сюда и развита).
Обе части касаются перехода **Paused → Walking** (guard `prev_state == Paused`;
не применяется к away→walking и к первому коннекту).

## Task C — длительность паузы в toast «Resumed» (многострочный)

- Трекаем `paused_since: Option<Instant>` в состоянии цикла `stream_with_presence`
  (тот же паттерн, что `away_since` в 010).
- На resume считаем длительность паузы и показываем её в toast.
- Toast **многострочный** (`mac-notification-sys` умеет subtitle + message):
  - `subtitle`: `Paused for 2m15s` (когда известно);
  - `message`: строка про восстановление скорости (Task D) или просто `Resumed`.

## Task D — авто-восстановление скорости перед паузой (функционал)

**Проблема:** пауза на пульте + resume сбрасывают скорость ленты в ~0.5 км/ч;
оператор ходит на ~2.5 и каждый раз доводит вручную.

**Фикс:**
- Запоминаем последнюю ненулевую `speed_kmh` (это и есть скорость ходьбы) в
  `last_walking_speed` на каждом сэмпле.
- На переходе в `Paused` снимаем снапшот `pre_pause_speed = last_walking_speed`.
- На `Paused → Walking` шлём FTMS set-speed через `control::Controller`
  (переиспользуем — не изобретаем Control Point заново) и восстанавливаем
  pre-pause скорость.

### Выбор цели восстановления — чистая функция

`speed_restore_target(pre_pause_kmh, resumed_kmh) -> Option<f32>`: возвращает
`Some(pre_pause)` только если лента реально замедлилась
(`pre_pause > resumed + SPEED_RESTORE_EPSILON_KMH`), иначе `None` (нечего чинить).
Юнит-тест: типовой сброс 2.5→0.5, равные скорости, resume быстрее, в пределах
эпсилона.

### BLE-запись — bounded (конвенция watchdog, задача 007)

`restore_speed(peripheral, target)` = `Controller::take_control` + `set_speed`.
Весь round-trip обёрнут в `tokio::time::timeout(SPEED_RESTORE_TIMEOUT = 15s)` —
ни один BLE-await не остаётся неограниченным, 15s ≪ `WATCHDOG_STALE_THRESHOLD`
(120s), так что медленное-но-легитимное восстановление watchdog не роняет.

`peripheral` уже держится в цикле стрима — прокинут в обработчик перехода.
Второй `notifications()`-стрим (внутри `Controller::execute`) безопасен: на
CoreBluetooth это **broadcast**-канал (`.subscribe()`), каждый подписчик получает
свою копию, стрим демона не «обкрадывается».

### Toast: вторая строка про восстановление

`Speed restored 0.5 → 2.5 km/h` — «from» = скорость, до которой дорожка
сбросилась на resume (текущий сэмпл, ~0.5), «to» = восстановленная pre-pause.

## Edge cases (все логируются, WARN)

- Нет снятой pre-pause скорости (демон стартовал уже в паузе / пауза до ходьбы)
  → пропускаем restore, строку скорости не показываем, toast = «Resumed» +
  длительность паузы. WARN.
- Ошибка write set-speed → WARN, toast без строки восстановления, **демон не
  падает**.
- Timeout → WARN (возможный CoreBluetooth-hang), toast без строки.

Это легитимное BLE-управление (разрешено), НЕ прошивочное изменение.

## Затронутые файлы (сверх 010/011)

- `src/notify.rs` — `SpeedRestore`, `treadmill_resumed(Option<Duration>,
  Option<SpeedRestore>)`, subtitle в `toast_full`.
- `src/daemon.rs` — трекинг `last_walking_speed`/`pre_pause_speed`,
  `speed_restore_target` (pure + тест), `try_restore_speed`/`restore_speed`
  (bounded), проводка в переходе Paused→Walking.
- `src/main.rs` — обновлён smoke-test toast для новой сигнатуры.
</content>
