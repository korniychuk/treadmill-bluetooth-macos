# 051 — Авто-восстановление после btleplug-паники, которая навсегда ломает BLE-скан

> **Статус: planned**
> **Источник:** backlog [009](../backlog/009-btleplug-panic-wedges-ble-scan.md); live-инцидент 2026-07-11 ([048](048-live-smoke-035-047.md))
> **Класс:** liveness / third-party panic / MTTR
> **Scope-файлы (ЖЁСТКО):** `src/daemon.rs`, `src/scan.rs`. **НЕ трогать** `src/main.rs`, `src/store.rs`, `tm doctor` (`daemon_status`-колонки) — эти файлы параллельно рефакторятся другими агентами. LaunchAgent plist (`scripts/install-daemon.sh`) — правка **не требуется** (см. «KeepAlive-семантика» ниже), не трогать без причины.

## Контекст (self-sufficient)

Live 2026-07-11 (smoke 048): после `connect` к дорожке **btleplug 0.12** паникует
на **фоновом CoreBluetooth-callback-потоке** (не в tokio-таске):

```
Got descriptors for a characteristic we don't know about
  at btleplug-0.12.0/src/corebluetooth/internal.rs:282
```

Последствия:

1. Процесс **не умирает** — паника на unnamed-потоке, дефолтный panic hook
   печатает backtrace и разматывает только этот поток.
2. `CBCentralManager` внутри btleplug остаётся в сломанном состоянии: каждый
   последующий цикл `daemon::run` падает мгновенно с
   `err=start filtered BLE scan` (это `.context("start filtered BLE scan")`
   на `adapter.start_scan(...)` в `scan::connect_treadmill`).
3. launchd `KeepAlive` не помогает — процесс жив, exit'а не было.
4. `tm status` показывает «daemon alive + AC scanning», виджет пуст. Единственное
   лечение — ручной `launchctl kickstart -k`.

Это фиксы **1 + 2** из backlog 009 (fail-fast + adapter recycle). Фикс 4
(`ble_scan_broken` в `daemon_status`/`tm doctor`) — **вне scope** этой задачи,
никаких новых колонок и правок doctor.

## KeepAlive-семантика (проверено, правка plist НЕ нужна)

`scripts/install-daemon.sh` генерит plist с **булевым** `KeepAlive`:

```xml
<key>KeepAlive</key>
<true/>
```

Булевый `KeepAlive=true` — launchd рестартует job при **любом** exit'е (нулевом
и ненулевом), с дефолтным throttle ~10 с (`ThrottleInterval`). Watchdog уже
опирается на это: `std::process::exit(WATCHDOG_EXIT_CODE /* 86 */)` в
`Watchdog::spawn_monitor` (`daemon.rs`) → launchd поднимает процесс заново.
`SuccessfulExit=false` не нужен (он бы наоборот сузил рестарты до ненулевых
exit-кодов). Вывод: чтобы launchd лечил панику, достаточно **довести панику до
`process::exit`** — что и делает часть (a).

## Дизайн

### (a) Fail-fast panic hook — только в daemon-режиме

В начале `daemon::run()` (`src/daemon.rs`; **не** в `main.rs` — one-shot CLI
команды hook получать не должны, и `main.rs` трогать нельзя):

```rust
/// Exit code for the fail-fast panic hook — matches Rust's own exit code for
/// a panicking main thread, distinct from WATCHDOG_EXIT_CODE (86) and
/// SCAN_WEDGED_EXIT_CODE (87) for log/`launchctl print` forensics.
const PANIC_EXIT_CODE: i32 = 101;
```

```rust
let default_hook = std::panic::take_hook();
std::panic::set_hook(Box::new(move |info| {
    // payload: downcast &str / String, fallback "<non-string panic payload>"
    // location: info.location() -> "file:line"
    error!(
        target: "panic_fail_fast",
        payload = %payload, location = %location, exit_code = PANIC_EXIT_CODE,
        "panic detected — exiting so launchd KeepAlive restarts the daemon (backlog 009)"
    );
    default_hook(info); // keep the default backtrace print (goes to daemon.log via StandardErrorPath)
    std::process::exit(PANIC_EXIT_CODE);
}));
```

Требования:

- **Стабильный тег** для grep'а по логам: `panic_fail_fast` (как `target:` или
  отдельное поле — главное, чтобы `rg 'panic_fail_fast' daemon.log` находил).
- **Полезность backtrace сохранить**: сначала свой `error!` (структурный payload
  + location), затем вызвать **сохранённый дефолтный hook** (он печатает
  message + backtrace в stderr → тот же `daemon.log`), затем `exit`.
- Hook глобальный на процесс, но ставится только на входе в `daemon::run` —
  one-shot команды (`tm scan`/`connect`/`stats`/...) его не получают, т.к.
  `daemon::run` у них не вызывается.
- Побочный эффект (осознанный): паника в **любом** потоке/таске демона
  (включая HR spawned-таску и btleplug-потоки) теперь роняет процесс →
  launchd-рестарт. Это и есть цель: замолчавшая паника хуже рестарта.
  `catch_unwind` в кодовой базе не используется (проверено), ломать нечего.
- Вынести чистый хелпер форматирования (например,
  `fn describe_panic(payload_str: Option<&str>, location: Option<String>) -> String`
  или аналог) — паникующий `PanicHookInfo` в юнит-тесте не сконструировать,
  а извлечение payload'а (`&str` vs `String` vs прочее) — тестируемая логика.

### (b) Adapter recycle на streak scan-start-отказов

**Классификация ошибки.** `scan::connect_treadmill` сейчас возвращает
`anyhow::Error`, и «сканер сломан» (`start filtered BLE scan`) неотличим без
string-match'а от здорового исхода «дорожка выключена»
(`no FTMS treadmill found within 15s`) и от connect/discover-ошибок. Ввести в
`src/scan.rs` типизированный маркер:

```rust
/// Marker context for a failed `start_scan` so callers can classify the
/// failure without string-matching (backlog 009: a wedged CBCentralManager
/// fails scan starts instantly and forever).
#[derive(Debug)]
pub struct ScanStartFailed;
// impl Display ("start filtered BLE scan") + std::error::Error
```

и вешать его через `.context(ScanStartFailed)` на `start_scan` в
`connect_treadmill` (человекочитаемый текст ошибки в логах сохранить — можно
двойным `.context(...)` или Display самого маркера). В daemon'е классификация:
`err.downcast_ref::<scan::ScanStartFailed>().is_some()` (anyhow умеет
downcast по context-типам). `connect_hr` можно пометить тем же маркером, но
streak считается **только** по главному циклу дорожки (HR — best-effort в
spawned-таске, в recycle-решение не входит).

**Чистая streak-логика** (юнит-тестируемая, без IO/времени — по образцу
`presence.rs`: решения чистые, side effects снаружи) в `src/daemon.rs`:

```rust
/// Consecutive `start_scan` failures before recycling the adapter (~15s at
/// RETRY_DELAY=5s — fast enough for MTTR, wide enough to skip a one-off blip).
const SCAN_START_RECYCLE_THRESHOLD: u32 = 3;
/// Adapter recycles (without a successful scan start in between) before
/// giving up and exiting for a launchd restart.
const SCAN_RECYCLE_MAX: u32 = 2;
/// Exit code when scanning stays wedged after SCAN_RECYCLE_MAX recycles.
const SCAN_WEDGED_EXIT_CODE: i32 = 87;

enum ScanRecoveryAction { Retry, RecycleAdapter, Exit }

struct ScanRecovery { scan_start_streak: u32, recycles: u32 }
impl ScanRecovery {
    fn on_connect_failure(&mut self, is_scan_start_failure: bool) -> ScanRecoveryAction;
    fn on_scan_ok(&mut self); // any non-scan-start outcome resets BOTH counters
}
```

Семантика:

- `is_scan_start_failure == false` (дорожка не найдена — норма; connect/discover
  ошибки) → `on_scan_ok()`-эквивалент: скан **стартовал**, адаптер жив, оба
  счётчика в ноль, действие `Retry`.
- streak из `SCAN_START_RECYCLE_THRESHOLD` подряд scan-start-отказов →
  `RecycleAdapter`, streak в ноль, `recycles += 1`.
- Если `recycles` уже достиг `SCAN_RECYCLE_MAX` и новый streak снова добрался
  до порога → `Exit`.
- Успешный connect (`Ok`) → `on_scan_ok()`.

**Механика recycle в `daemon::run`.** `run` получает `adapter: &Adapter`
(заимствован из `main.rs`, который трогать нельзя), поэтому замена — через
локальный владеющий override:

```rust
let mut recycled_adapter: Option<Adapter> = None;
// в каждой итерации цикла:
let active_adapter = recycled_adapter.as_ref().unwrap_or(adapter);
// select! { ... result = scan::connect_treadmill(active_adapter) => ... }
```

`active_adapter` передаётся и в `stream_with_presence` (там он клонируется для
HR spawned-таски — `Adapter: Clone`; старые клоны в умерших тасках просто
дропнутся). На `RecycleAdapter`:

1. `WARN` со стабильным тегом, streak и номером recycle, например:
   `warn!(target: "scan_recovery", streak, recycle = recycles, "start_scan failure streak — recycling BLE adapter (backlog 009)")`.
2. Best-effort `active_adapter.stop_scan()` под bounded timeout
   (`scan::CONNECT_TIMEOUT`, конвенция задачи 007 — ни одного unbounded BLE
   await); ошибку/timeout — WARN, не фатально.
3. `recycled_adapter = None` (дроп старого owned, если был), затем
   `scan::first_adapter().await` → `recycled_adapter = Some(new)`. Свежий
   `Manager::new()` внутри `first_adapter` создаёт новый `CBCentralManager` —
   это и есть гипотеза лечения (wedged-инстанс выбрасываем).
4. Если сам `first_adapter` упал — `ERROR` (тот же стабильный тег) +
   `std::process::exit(SCAN_WEDGED_EXIT_CODE)`: Manager мёртв, чинить внутри
   процесса больше нечем, launchd перезапустит.

На `Exit`: `ERROR` со стабильным тегом (`scan_recovery`), счётчиками и
`exit_code = SCAN_WEDGED_EXIT_CODE`, затем `std::process::exit(...)` — тот же
путь к launchd-рестарту, что у watchdog'а (86) и panic hook'а (101).

**Тайминг** (соответствует acceptance «~1 мин»): отказ каждые ~5 с
(`RETRY_DELAY`): 3 отказа ≈ 15 с → recycle #1 → 15 с → recycle #2 → 15 с →
exit(87) ≈ 45–50 с; launchd throttle ~10 с → свежий процесс в пределах минуты.
Если recycle реально лечит CBCentralManager — восстановление за ~15–20 с вообще
без рестарта процесса.

**Не сломать здоровые пути:**

- «Дорожка выключена» (`no FTMS treadmill found`) — самый частый исход в жизни
  демона: он **сбрасывает** счётчики и никогда не ведёт к recycle/exit.
- Обычный reconnect после потери линка (`stream_with_presence` вернулся →
  disconnect → новый цикл) не проходит через failure-ветку — не затронут.
- Watchdog (`WATCHDOG_STALE_THRESHOLD` 120 с) не трипается: каждый цикл
  по-прежнему завершает `select!`-arm и жмёт `persist()` → `touch()`;
  recycle-операции bounded.
- Idle-on-battery ветка не затронута (скан там вообще не зовётся).

## План

1. `src/scan.rs`: маркер `ScanStartFailed` + повесить на `start_scan` в
   `connect_treadmill` (и, опционально, `connect_hr` — без участия в streak'е).
2. `src/daemon.rs`: константы (`PANIC_EXIT_CODE=101`, `SCAN_WEDGED_EXIT_CODE=87`,
   `SCAN_START_RECYCLE_THRESHOLD=3`, `SCAN_RECYCLE_MAX=2`), чистые
   `ScanRecovery`/`ScanRecoveryAction`, panic hook в начале `run()`,
   `recycled_adapter`-override + обработка `RecycleAdapter`/`Exit` в
   `Err`-ветке connect-select'а, чистый panic-описатель.
3. Юнит-тесты (чистые, без BLE/времени):
   - не-scan-start отказы (N раз подряд) → всегда `Retry`, счётчики не растут;
   - 3 подряд scan-start отказа → `RecycleAdapter`; streak обнулён;
   - успех/не-scan-start отказ после частичного streak'а → полный сброс
     (включая `recycles`);
   - 2 recycle без успешного скана между ними + третий streak → `Exit`;
   - успешный скан после recycle → `recycles` сброшен, следующий streak снова
     ведёт к recycle, не к exit;
   - panic-описатель: payload `&str` / `String` / нестроковый, с/без location.
4. `cargo test`, `cargo clippy` — зелёные. `cargo fmt`.
5. Обновить doc-комментарии в шапке `daemon.rs` (module docs упоминают watchdog —
   дописать panic hook + scan recovery) и этот файл (статус → done).

## Вне scope

- `src/main.rs`, `src/store.rs` — параллельный рефакторинг другими агентами.
- Новые колонки `daemon_status`, правки `tm doctor` (фикс 4 из backlog 009).
- Upgrade/патч btleplug (фикс 3) — отдельная работа.
- Правка plist/`install-daemon.sh` — не нужна (KeepAlive=true уже рестартует
  любой exit).
- `docs/README.md` — не трогать.

## Acceptance (из backlog 009)

- [ ] После симулированного streak'а scan-start-отказов (или реальной
      discover-паники) демон восстанавливается сам, без `kickstart`, за ~1 мин:
      либо adapter recycle (~15 с), либо exit(87/101) → launchd-рестарт.
- [ ] Логи делают класс отказа очевидным: `panic_fail_fast` /
      `scan_recovery` теги, streak/recycle-счётчики, exit-код.
- [ ] Здоровый путь не тронут: «no FTMS treadmill found» сбрасывает счётчики,
      обычный connect/reconnect работает, ложных рестартов нет.
- [ ] Юнит-тесты на чистую streak-логику и panic-описатель зелёные;
      `cargo clippy` чистый.

## Связанное

- backlog [009](../backlog/009-btleplug-panic-wedges-ble-scan.md) — симптом,
  workaround, варианты фиксов
- task [048](048-live-smoke-035-047.md) — live-инцидент 2026-07-11
- task [007](007-daemon-hang-unbounded-ble-awaits.md) — конвенция bounded BLE
  awaits + watchdog exit(86)
- task [018](018-streaming-watchdog-fast-reconnect.md) — streaming-порог
  watchdog'а
