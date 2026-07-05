# 007 — Тихое зависание демона на unbounded BLE-await'ах + watchdog-рестарт

## Инцидент (2026-07-05, ~13:50 WITA)

Оператор выключил/включил дорожку — демон её не задетектил, тоста не было.
Диагностика на живом процессе (PID 96856):

- `pmset` — AC power, процесс жив, `status` CLI: `daemon_status` не обновлялся
  4755 с при живом процессе → watchdog-ветка задачи D сработала *в CLI*, но не
  в самом демоне.
- Лог: последняя строка `13:50:42 ERROR notification stream ended` — и полная
  тишина, ни одного ре-скана.
- `sample 96856`: **все** tokio-потоки запаркованы (`pthread_cond_wait`),
  главная future висит на pending-await; при этом CoreBluetooth-делегат
  продолжает получать `didDiscoverPeripheral` — BLE-стек жив, застрял наш код.

## Root cause

После возврата из `stream_with_presence` `run()` вызывает
`peripheral.disconnect().await` (daemon.rs:233) — **без таймаута**. Задача D
обернула `connect()`/`discover_services()`, но не `disconnect()`. Для жёстко
обесточенной дорожки CoreBluetooth не завершает disconnect неопределённо долго
(инцидент 2026-07-04 18:51 → 04:54: disconnect «завис» на ~10 часов и
завершился только когда дорожку снова включили).

Каскад следствий:

1. `notify::treadmill_lost()` и `state.persist()` стоят **после** disconnect →
   ни тоста «lost», ни обновления `daemon_status` (status врал «connected»).
2. Watchdog задачи D — ветка того же `tokio::select!` в `run()`; пока
   управление внутри *тела* другой ветки (этот самый `disconnect().await`),
   тик watchdog'а не выполняется. Сторож заперт в одной задаче с тем, кого
   сторожит → за 79 минут ни одного WARN.

## Фикс

1. **Таймауты на все BLE-await'ы.** Экспортировать `CONNECT_TIMEOUT` из
   `scan.rs` (убрать дублированные `ASSUMED_SCAN_TIMEOUT`/локальный
   `CONNECT_TIMEOUT` из `daemon.rs` — там же лежал TODO об этом) и обернуть:
   - `peripheral.disconnect()` в `run()` — таймаут + `warn!`, идём дальше;
   - `subscribe_treadmill_data`/`subscribe_treadmill_status`/
     `peripheral.notifications()` в `stream_with_presence` — тоже unbounded
     CoreBluetooth-вызовы того же класса.
2. **Тост и persist — до disconnect.** «Treadmill lost» и обновление
   `daemon_status` не должны зависеть от исхода BLE-вызова.
3. **Watchdog → отдельный spawned tokio-таск + рестарт процесса.**
   `Watchdog` переводится на interior mutability (`Arc` + `AtomicU64` от
   якорного `Instant`), `touch()` остаётся на каждом persist'e; отдельный
   `tokio::spawn`-таск раз в 30 с проверяет staleness и при превышении
   порога логирует `ERROR` и завершает процесс ненулевым кодом.
   `KeepAlive=true` в LaunchAgent-plist гарантирует авто-перезапуск launchd'ом.

   **Пересмотр решения задачи D** («только сигнал, не самолечиться»):
   инцидент показал, что hang живёт *внутри* btleplug/CoreBluetooth и из
   процесса не лечится — «только сигнал» означает гарантированно упущенный
   трекинг до ручного `kickstart`. Оператор явно потребовал «не рисковать
   упущенным трекингом каждый раз». Рестарт процесса безопасен: SQLite
   коммитит на каждой операции, JSONL-лог пишется построчно, а раз демон
   завис — он и так ничего не трекает; хуже сделать нельзя.
   Порог остаётся щедрым (≫ худшего легитимного цикла scan+connect), чтобы
   не убить себя не по делу. `Instant` на macOS не тикает во сне
   (mach_absolute_time) → ложных срабатываний после wake нет.

## Верификация

- `cargo test` (существующие + юнит на staleness-детекцию), `cargo clippy`.
- Пересобрать/переустановить демона (`scripts/install-daemon.sh`).
- Live: выключить дорожку → в пределах ~20 с тост «lost» и корректный
  `status`; демон продолжает ре-скан (или перезапускается watchdog'ом, если
  что-то снова виснет); включить → тост «found».

## Лог

- 2026-07-05: инцидент задиагностирован (sample + status + daemon.log),
  root cause подтверждён, план согласован с требованием оператора о
  надёжности. Демон вручную перезапущен `launchctl kickstart -k` —
  трекинг восстановлен, дорожка найдена сразу после рестарта.
