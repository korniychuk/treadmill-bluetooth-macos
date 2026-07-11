# treadmill-bluetooth-macos

BLE-коннектор для беговой дорожки **Yesoul** под **macOS**, на **Rust**.

## Что это

CLI-утилита, которая по Bluetooth Low Energy находит беговую дорожку, подключается
к ней и читает телеметрию (скорость, наклон, дистанция). Долгосрочная цель — двусторонний
контроль (старт/стоп, задание скорости и наклона) и стабильный коннектор поверх CoreBluetooth.

## Стек

- **Rust** (edition 2024, toolchain 1.95+).
- [`btleplug`](https://github.com/deviceplug/btleplug) — кросс-платформенный BLE; на macOS работает через **CoreBluetooth**.
- `tokio` — async runtime; `tracing` — логирование; `anyhow` — ошибки.

## Delegating to the Grok subagent

This file is read by two different agents. Follow the branch that matches who you are.

**If you are a Grok subagent** — launched via the global wrapper (`GROK_AGENT_DEPTH` is set in your env); your working directory is a `…_grok-<name>` worktree (or the main repo in read-only research mode): do the assigned task **directly and completely**, then commit on your branch (research mode: just report). Your own in-session subagents (`spawn_subagent`) are allowed and encouraged; the one thing you must **never** do is shell out to `grok-agent.sh` — that's a fork loop, and the wrapper hard-refuses re-entry anyway. The rest of this section is not for you.

**If you are the orchestrating Claude Code session**: **Grok delegation is the DEFAULT in this repo** — this overrides the global CLAUDE.md's explicit-opt-in rule; no explicit ask is needed here.
- **Delegate:** internet research (`~/.claude/tools/grok-agent.sh --ro <name> "<question>"`) and self-contained, well-scoped implementation (`~/.claude/tools/grok-agent.sh <name> "<task>"`) — run from inside this repo. Grok is fast and produces good, well-split commits.
- **Keep in the main thread:** task decomposition, design decisions, writing the task doc, reviewing Grok's diff, and integrating its `grok/<name>` branch.
- **Rust builds needing crate fetches:** add `--net crates.io --net static.crates.io`.
- **Trust the diff, not the prose:** verify the actual commit on `grok/<name>`. An aborted run exits 2 with `stopReason=Cancelled` — continue it with `--resume <name>` rather than restarting.
- Full doc: `ankor-dotfiles/docs/grok-agent.md`.

## Архитектура

- `src/main.rs` — точка входа и CLI (`scan` | `connect` | `daemon` | `stats` | ...).
- `src/scan.rs` — обнаружение адаптера, скан, подключение, подписка на нотификации.
- `src/ftms.rs` — константы Fitness Machine Service (`0x1826`) и парсинг Treadmill Data (`0x2ACD`).
- `src/hr.rs` — константы Heart Rate Service (`0x180D`) и парсинг Heart Rate
  Measurement (`0x2A37`, задача 025): u8/u16 bpm, sensor-contact флаги, RR-интервалы
  (задел под HRV, пока не используется). `bpm==0` (потеря контакта у H10) — DEBUG,
  не ошибка, кадр отбрасывается. Плюс Battery Service (`0x180F`/`0x2A19`,
  задача 026) — только Read (Polar не шлёт notify по заряду).
  `ContactTracker`/`Contact` (задача 033) — **контакт с телом ≠ BLE-линк**:
  снятый H10 держит линк и продолжает слать ~1 кадр/с с **замороженным**
  последним bpm, без `bpm==0` и без contact-битов. `observe(&m, ts_ms)` (время
  инъекцией, как `presence.rs`) даёт `Lost` по трём сигналам, в порядке
  надёжности: (1) `contact == Some(false)` — сразу; (2) **bpm не менялся
  бит-в-бит** `CONTACT_FROZEN_BPM_MS` (60с) — решающий; (3) `CONTACT_LOST_FRAMES`
  (3) подряд RR-less кадров от датчика, который RR когда-либо слал (capability
  учится из потока, не хардкодится по вендору) — быстрый, ~3с.
  RR-правила **недостаточно**: снятый H10 перемежает RR-несущие кадры
  (`10 6F FB 17`) с `00 6F` и вечно обнуляет счётчик; поэтому frozen-bpm стоит
  **выше** RR в `observe` — присутствие RR не ручается за показание,
  не менявшееся минуту. Порог 60с выбран по данным: самая длинная константная
  серия у надетого страпа — 16с, у снятого — 26 мин.
  `reset_link()` (задача 034) чистит только link-scoped улики (RR-capability,
  счётчик), но **не** часы заморозки — сердце не перезапускается вместе с
  BLE-линком. Датчик без RR и без contact-битов, но с живым (дрожащим) bpm
  детектировать нечем — остаётся `Live` до линк-таймаута (честная деградация).
  Чистый, без BLE.
- `src/recompute_hr.rs` — команда `recompute-hr [--dry-run]` (задача 034):
  проигрывает `hr_samples.raw_frame` через тот же продовый `ContactTracker` и
  удаляет сэмплы, записанные со снятого датчика (они травили `hr_summary_for` —
  наблюдалось `♥ 111/111` на целой тренировке). Шире живого демона: тот обязан
  выждать 60с окна, а replay видит будущее и хоронит **весь** константный
  прогон, включая его начало. `plan_to_fixpoint` крутит план до пустого прохода:
  удаление замороженного прогона **смыкает** соседние прогоны другого bpm, и
  объединённый может пробить порог там, где ни одна половина не пробивала (на
  живой базе сошлось за 3 прохода). Read-only по BLE, трогает только
  `hr_samples`.
- `src/speed.rs` — `CentiKmh(u16)` newtype: скорость в FTMS wire-единицах
  (0.01 km/h). Квантизация на decode/конфиг/CLI (`from_wire` / `from_kmh_f32`);
  compare/clamp — integer (`Eq`/`Ord`). Display — человекочитаемые km/h
  (`"3.2"`). Задача 054 / backlog 006; устраняет float-gap задачи 030.
- `src/control.rs` — FTMS Control Point (start/stop/speed); `set_speed(CentiKmh)`.
- `src/control_command.rs` — `ControlCommand` тип (`start`/`stop`/`speed:<kmh>`),
  `Speed(CentiKmh)`; текстовый wire-формат очереди без изменений (задача 013/054).
- `src/presence.rs` — детекция присутствия: лента крутится, но шаги не растут →
  `AwayWhileRunning`. `observe(now, speed: Option<CentiKmh>, steps)` — время
  инъектируется (демон даёт `Instant::now()`, replay — синтез из `ts_ms`),
  единый источник 10с-away-порога; belt stopped = `CentiKmh::ZERO`.
- `src/activity.rs` — общий движок presence+credit+сегменты (`ActivityAccumulator`,
  `credit_or_hold`), которым гоняют **и** живой демон, **и** replay (задача 015) —
  сегментация идентична by construction, не форкается.
- `src/default_speed.rs` — расчётная дефолтная скорость ленты на старте
  тренировки (задача 016): `trimmed_mean_speed` (чистая, 15%-trim сверху/снизу,
  floor) + `compute_default_speed` — берёт последнюю подходящую тренировку
  (`walking_time_s` ≥ 30 мин) за всю историю и её крейсерскую скорость из
  `raw_samples`. Демон применяет её на переходе в `Walking` без pre-pause
  скорости, только если лента на заводском crawl (`≤0.8`); read-time,
  переиспользует bounded BLE-write задачи 012.
- `src/recompute.rs` — команда `recompute-segments`: проигрывает `raw_samples`
  через тот же `ActivityAccumulator` (scratch in-memory `Store` переиспользует
  `advance_baseline`+`credit_activity` verbatim) и транзакционно/идемпотентно
  перестраивает `activity_segments` из ground-truth. `daily_stats`/`raw_samples`/
  `workouts` не трогает. Read-only по BLE (задача 015).
- `src/store.rs` — SQLite (`~/Library/Application Support/treadmill-bluetooth-macos/treadmill.db`),
  дневная статистика (шаги/дистанция/время ходьбы), restart-safe дельта-накопление.
  Тренировки хранятся как порог-независимые **сегменты** (`activity_segments`,
  задача 014) — непрерывное зачтённое шагание; отображаемые тренировки
  выводятся на **чтении** чистой `merge_segments(&[Segment], gap_minutes)`, так
  что `workout_gap_minutes` меняется ретроактивно без пересчёта. `daily_stats`
  — строго календарный, не тронут. Старая таблица `workouts` оставлена архивом
  (сид сегментов из неё одноразовый, ничто в неё больше не пишет).
- `src/daemon/` — фоновый цикл (LaunchAgent; задача 055 split): авто-скан/
  коннект/реконнект + presence + toast. Подмодули: `run_loop` (outer loop +
  ScanRecovery/panic hook), `session` (`stream_with_presence` thin wiring,
  задача 053: arm → метод session-структуры → side-effect), `state`
  (`DaemonState` + tolerate/persist), `watchdog`, `commands` (control queue),
  `speed` (restore/default writes), `hr` (spawned HR connect), `config`
  (hot-reload effects), `zone_write` (`ZoneWrite` → BLE). Открывает/продлевает
  **сегмент** активности на зачтённом шаге и закрывает его (in-memory
  `current_segment=None`) в presence-переходе при уходе из `Walking` (задача 014);
  на resume после паузы авто-восстанавливает pre-pause скорость ленты через
  `control.rs` (bounded BLE-write, см. `docs/tasks/012`).
  Единственный владелец BLE-линка: команды управления (`tm speed`/`start`/`stop`)
  от CLI идут через SQLite-очередь `control_commands` и исполняются здесь на живом
  подключении (задача 013). CLI напрямую открывает BLE только если демон не держит линк.
  Авто-пауза простаивающей ленты (задача 020): если `AwayWhileRunning` длится
  дольше `auto_pause_minutes` (дефолт 5, `0` — выкл.), демон шлёт `Stop` (тот же
  bounded Control-Point round-trip), лента гаснет своим встроенным shutoff'ом;
  решение в `AutoPause` (`due` / spell latch), shell пишет Control Point.
- `src/auto_pause.rs` — `AutoPause` (задача 053/020): away-spell, one-shot fire,
  retry cooldown; время инъекцией.
- `src/treadmill_link.rs` — `TreadmillLink` (задача 053): silence clock
  (`silence_deadline` / absolute `sleep_until`), speed history + cruising,
  pause/resume memory, once-per-session default-speed flag.
- `src/hr_session.rs` — `HrSession` (задача 053/025/033): HR link + contact +
  battery + connect latch; `link_up` paired with shell `hr_notifications`;
  invariant `hr_connected=false ⇒ last_bpm=None`.
- `src/zone_session.rs` — `ZoneSession` (задача 053/027): phase machine,
  pure `tick` → `ZoneWrite` (decision/effect split), override window, snapshot;
  config-reload effects as method receivers of `ConfigDelta` (задача 052).
  Пульс (задача 025): второй, независимый BLE-линк (HR-датчик, напр. Polar
  H10). Коннект/реконнект — best-effort на **отдельной spawned-таске**
  (`spawn_hr_connect_attempt`), чтобы скан (до 15с, нормальный исход когда
  датчик не надет) не блокировал телеметрию дорожки; результат приходит через
  `mpsc`-канал. Живой стрим `0x2A37` — отдельная ветка в том же `select!`,
  свой bounded timeout (10с) — пропажа датчика не роняет цикл дорожки.
  Сэмплы пишутся в `hr_samples`, снапшот (`hr_connected`+`last_bpm`+`last_bpm_ts`)
  — в `daemon_status` вместе с остальным heartbeat'ом.
  Порог свежести bpm в `tm widget`/`tm status` — свой, `HR_STALE_THRESHOLD_S`
  (15с, `main.rs`), а не `WATCHDOG_STALE_THRESHOLD_S` (95с = scan+connect+запас):
  тот размерен под «демон повис», и застывший на полторы минуты пульс — ложь.
  Потеря контакта (задача 033) — **не** потеря линка: кадр гоняется через
  `hr::ContactTracker`, при `Lost` сэмпл **не пишется** (иначе замороженный bpm
  травит `hr_summary_for` — наблюдалось `♥ 111/111` на целой тренировке),
  `hr_connected`/`last_bpm`/`last_bpm_ts` сбрасываются, `WARN` — один раз на
  переход, а BLE-линк и заряд **сохраняются**: датчик вернули на грудь → RR
  вернулись → мгновенный `Live` без 15-секундного рескана. Трекер сбрасывается
  вместе с остальным HR-состоянием при реальной потере линка.
  Заряд батареи датчика (задача 026): читается один раз сразу при коннекте
  (в том же spawned-таске) + адаптивно перечитывается — раз в 60 мин, раз в
  30 мин при заряде ≤20% (`hr_battery_poll_interval`, чистая функция). Опрос
  не про экономию батареи H10 (single-byte read ничтожен на фоне её ~400ч
  ресурса) — просто чтобы не делать бесполезную работу. Сбрасывается при
  потере HR-линка вместе с остальным HR-состоянием.
  Живость (задачи 018, 031): два независимых сторожа. (1) Арм `select!` с
  **абсолютным** дедлайном `sleep_until(last_telemetry_at + NOTIFICATION_TIMEOUT)` —
  20с молчания `0x2ACD` → graceful reconnect. Именно `sleep_until`, а не
  `timeout(...)`: `select!` пересоздаёт future каждой ветки на каждой итерации,
  и relative-timeout сбрасывался бы соседними ветками (`command_tick` — 1с,
  HR-кадры — ~1/с), т.е. не срабатывал бы никогда. (2) `Watchdog` на отдельной
  таске — рестарт процесса через launchd. Его `touch()` едет на `State::persist()`
  (любая ветка цикла) и сторожит **живость event-loop** (`WATCHDOG_STALE_THRESHOLD`,
  120с); отдельный `touch_telemetry()` вызывается **только** на разобранном
  `0x2ACD` и сторожит поток дорожки (`STREAMING_STALE_THRESHOLD`, 30с, пока
  `streaming`). Смешивать их нельзя: надетый Polar шлёт ~1 кадр/с, и общий
  `touch()` держал бы тайт-порог вечно свежим (симптом задачи 031 — залипший
  `connected` после выключения дорожки).
- `src/power.rs` — детекция AC-питания (`pmset -g batt`); на батарее и без
  подключённой дорожки демон не сканирует, чтобы не сажать аккумулятор.
- `src/notify.rs` — нативные macOS-уведомления (`mac-notification-sys`,
  чистый Rust, без Swift в рантайме) с иконкой и именем "Treadmill";
  toast'ы presence/goal, компактный форматтер длительности `humanize_short`.
- `src/goals.rs` — дневные step-goal вехи: загрузка `config.toml` (TOML, задача 023),
  присвоение tier'ов (1–3), чистая функция «какие пороги праздновать сейчас».
  Плюс `load_workout_gap_minutes()` — read-time порог склейки сегментов в
  тренировки из того же `goals.json` (задача 014, дефолт 15). Плюс
  `load_auto_pause()` — порог авто-паузы простаивающей ленты из того же файла
  (задача 020, дефолт 5 мин, `0` — выключено), `None` = выключено.
- `src/logger.rs` — сырой JSONL-лог телеметрии (source-of-truth параллельно с SQLite).
- `src/store.rs` (доп., задача 025) — `hr_samples` (индекс по `ts_ms`, не по
  `session_id` — агрегаты джойнят по временному окну тренировки/дня) +
  `hr_summary_for(from, to)`: `♥ avg/max` = trimmed-mean (переиспользует
  `default_speed::trimmed_mean_speed`) / p95 (устойчив к единичному спайку).
  `None` при < 10 сэмплов в окне. Плюс `hr_battery_pct` в `daemon_status`
  (задача 026, `Option<i64>`, ALTER-колонка). Плюс `hr_samples_ordered` +
  `delete_hr_samples` (задача 034) — ground-truth для `recompute-hr`.
- `src/zone_hold/` — **Zone Hold** (задача 027; split 055): HR-адаптивная
  подстройка скорости под целевую пульсовую зону. Чистый модуль, без
  BLE/времени внутри. Подмодули: `mod` (типы/resolve/re-exports),
  `controller` (`next_speed`/`warmup`/`safety_force_reduce`), `config`
  (load/parse `[zone_hold]`), `cli_config` (`upsert_zone_hold_keys`/
  `replace_zones`). `hrmax_tanaka`, `resolve_zone_bpm` (`hrmax`/`karvonen`,
  не смешиваются), `ZoneHoldConfig` (absent-тихо/invalid-WARN как `goals.rs`),
  контроллер `band`/`center` (deadband, шаг, кламп — время и bpm инъекцией),
  `safety_cap_bpm`, `classify_position` (below/in/above для виджета).
  `ZoneDef.id` — стабильный идентификатор зоны (явный `id = "..."` в
  `[[zone_hold.zones]]` либо slug из `name`), `target_zone` принимает и число
  (1-based индекс), и id/имя-строку (`ZoneSelector::{Number,Id}`), `find_zone`
  резолвит: точный id → точное имя → подстрока имени, регистронезависимо; CLI
  пишет в конфиг канонический `id`, а не то, что ввёл оператор —
  переименование/реордер зон не ломает таргет.
  Демон (`daemon/`) держит per-session `ZoneHoldPhase`
  (`Ramp`→`Hold`, `Frozen`/`Grace` на сходе/возврате с ленты) и гоняет
  коррекцию тем же bounded speed-write (задача 012) на presence-тиках;
  safety-cap форсит уменьшение/`Stop` независимо от обычного цикла. Снапшот
  (`zone_hold_active`+`_phase`+`_target_lo/hi`+`_last_speed`+`_position`) —
  в `daemon_status`. CLI `tm zone on/off/setup/limits/target/list/add/edit/
  remove/mode` (без аргумента — статус); `on`/`setup` — интерактивный
  онбординг возраста; `list` — таблица всех зон (id, bpm-диапазон,
  max_speed) с меткой текущего таргета; `add`/`edit`/`remove` — интерактивные
  промпты (та же пара `prompt_*`-хелперов, что онбординг) поверх
  `zone_hold::replace_zones` — перезаписывают весь блок `[[zone_hold.zones]]`
  целиком (у array-of-tables нет стабильного якоря для точечного патча, как
  у скалярных ключей в `upsert_zone_hold_keys`); `add` материализует
  дефолтные 5 зон явно, если кастомных ещё не было, `edit` не даёт менять
  `id` (на него может ссылаться `target_zone`), `remove` не даёт удалить
  последнюю зону.
  Выключение (задача 032): `tm zone off` прилетает в демон как **hot-reload
  конфига**, а не как смена фазы, поэтому одного `zh_phase != Off` мало —
  живой `Ramp` возвращал скорость оператора к своей цели ещё долго после
  `enabled=false` (до следующего presence-перехода или рестарта). Три рубежа:
  явный `disengage_zone_hold` в ветке reload, гейт `should_run_zone_hold(enabled,
  phase)` на call site и ранний `return` в самой `zone_hold_tick` (функция,
  пишущая в Control Point, проверяет свой enable-флаг сама).
  `tm widget` — поле `HR_ZONE` (below/in/above/пусто, красится только в
  `walking` при активном контроллере); `tm status` — строка Zone Hold.
  `next_speed` / clamp-сравнения — на `CentiKmh` (задача 054): wire-скорости
  сравниваются integer'ом; representation gap задачи 030 устранён by
  construction. `MIN_SPEED_CHANGE = CentiKmh(5)` (0.05 км/ч) остаётся
  **controller deadband** (политика «не дёргай ленту из-за мелочи»), не
  float-glue. `ZoneSession::tick` принимает `Option<CentiKmh>` и молча
  пропускает тик, если в FTMS-фрейме скорости нет (легитимный
  `MORE_DATA`-сплит) — подстановка нуля читалась бы как «лента встала».
- `src/goals.rs` (доп., задача 029) — `load_show_speed()` (top-level
  `show_speed`, тот же absent-тихо/invalid-WARN стиль, дефолт `false`) +
  `upsert_top_level_key(path, key, value)` — line-based апдейт **top-level**
  ключа (не секции — ключ должен стоять до первого `[section]`, иначе
  невалидный TOML), тем же приёмом, что `zone_hold::upsert_zone_hold_keys`,
  но без секционного якоря. `src/store/`/`src/daemon/` — снапшот живой
  скорости ленты `last_speed_kmh`+`last_speed_ts` в `daemon_status`
  (`Option<f64>`/`Option<i64>` millis, ALTER-колонки), зеркалит `last_bpm`/
  `last_bpm_ts` (задача 025) — обновляется на **каждом** телеметрическом
  сэмпле независимо от Zone Hold (в отличие от `zone_hold_last_speed`,
  который `None`, пока контроллер не активен), сбрасывается при
  дисконнекте дорожки. `tm widget` — 12-е TSV-поле `speed_kmh`, уже
  отформатированное (`widget_speed_field`+`format_speed_kmh` в `main.rs`):
  округление до 0.1 half-up, `.0` отбрасывается (`3kmh`, не `3.0kmh`);
  пусто, если `show_speed` выключен, сэмпл протух, или скорость `0`
  (лента стоит). CLI `tm speed-widget on/off` (без аргумента — статус) —
  не `tm speed` (та уже занята заданием целевой скорости ленты через
  Control Point). tmux-виджет — текстовая нотация без иконки (набор
  Nerd Font спидометров не читался однозначно, решение оператора): число
  обычным цветом состояния, `kmh` — приглушённым, как день-тотал у
  остальных метрик.

## Liveness matrix (инвариант, задачи 031/035/038/053)

Не смешивать эти «живости» — каждый сигнал кормит свой контур и имеет
**своего** владельца-структуру (задача 053), чтобы новая фича не переиспользовала
чужой сброс таймера:

| Сигнал | Что значит | Кормит | Владелец |
|---|---|---|---|
| Event-loop progress | `persist()` / любой arm | `Watchdog::touch` (hang → exit) | Shell + `Watchdog` |
| Treadmill telemetry | decoded `0x2ACD` | `connected`, `touch_telemetry`, widget speed | `TreadmillLink` |
| HR BLE link | stream open | reconnect / battery | `HrSession` |
| HR body contact | meaningful bpm | `hr_samples`, widget ♥, zone bpm input | `HrSession` (отдельно от link) |
| Config intent | `enabled` flags | phase machines (zone, auto-pause) | `ZoneSession` / `AutoPause` (+ `config_apply`) |

Правила: absolute `sleep_until` для silence в `select!` (не relative `timeout`);
`hr_connected=false` ⇒ `last_bpm=None`; Zone Hold bpm только если sample fresh
(≤15s). Диагностика: `tm doctor`.

### Ops: BLE scan wedged (`start filtered BLE scan`)

Live 2026-07-11: after connect, **btleplug** can panic on a background thread
(`Got descriptors for a characteristic we don't know about`) without killing the
process; subsequent scans fail instantly with `err=start filtered BLE scan`.
**Fixed by задача [051](docs/tasks/051-ble-scan-auto-recover.md)** (закрыла
backlog [009](docs/backlog/009-btleplug-panic-wedges-ble-scan.md)): panic
fail-fast hook (exit 101), typed `ScanStartFailed` + `ScanRecovery` — recycle
адаптера после 3 подряд, exit 87 после 2 recycle; launchd KeepAlive
перезапускает. Exit-коды форензики: 86 watchdog, 87 scan-wedge, 88 persistent
DB failure, 101 panic. Ручной рычаг, если что-то новое всё же заклинит:

```bash
launchctl kickstart -k "gui/$(id -u)/com.korniychuk.treadmill-bluetooth-macos.daemon"
```

Wake the treadmill console if it stopped advertising. Reliability tasks
035–047: done; smoke notes [048](docs/tasks/048-live-smoke-035-047.md).

## Протокол

Большинство дорожек отдают стандартный GATT-профиль **FTMS** (Fitness Machine Service, `0x1826`).
Предполагаем его как основной путь. Возможен **vendor-specific** сервис Yesoul (как в их
мобильном приложении) — это ещё не реверс-инжинирилось; см. `docs/research/`.

Ключевые UUID:
- `0x1826` — Fitness Machine Service
- `0x2ACD` — Treadmill Data (notify)
- `0x2AD9` — Fitness Machine Control Point (write/indicate) — задел под управление
- `0x2ADA` — Fitness Machine Status (notify)
- `0x180D` — Heart Rate Service (задача 025, напр. Polar H10)
- `0x2A37` — Heart Rate Measurement (notify)
- `0x180F` — Battery Service (задача 026)
- `0x2A19` — Battery Level (read)

## Команды

```bash
cargo run             # = scan: перечислить BLE-устройства рядом (диагностика)
cargo run -- connect  # подключиться к первой FTMS-дорожке и стримить данные
cargo run -- daemon    # фоновый режим: авто-коннект + presence + toast (для интерактивной проверки)
cargo run -- stats     # статистика за сегодня; `stats --all` — за все дни
cargo run -- status    # состояние демона/дорожки/HR/zone (read-only, без BLE)
cargo run -- doctor    # матрица живости для диагностики (задача 038; без BLE)
cargo run -- widget    # компактный TSV текущей тренировки для status-bar виджета; пусто если дорожка off (см. docs/tasks/009)
cargo run -- recompute-segments  # пересобрать activity_segments из raw_samples (без BLE, идемпотентно; docs/tasks/015). Откажет, если демон жив (задача 044).
cargo run -- recompute-hr        # вычистить hr_samples, записанные со снятого датчика (без BLE, --dry-run; docs/tasks/034). Откажет, если демон жив.
cargo run -- default-speed  # показать расчётную дефолтную скорость на старте тренировки (без BLE; docs/tasks/016)
cargo run -- hr        # диагностика: подключиться к HR-датчику, печатать заряд + live bpm (docs/tasks/025,026)
cargo run -- zone      # Zone Hold: статус (без аргумента) или on/off/setup/limits/target/list/add/edit/remove/mode (docs/tasks/027)
cargo run -- speed-widget  # показ живой скорости в виджете: статус (без аргумента) или on/off (docs/tasks/029)
cargo run -- discover / sniff / fitshow-probe / fitshow-set  # reverse-engineering helpers (FitShow framing in fitshow.rs)
cargo run -- --help    # полный список команд
cargo test             # юнит-тесты
cargo clippy           # линт
RUST_LOG=debug cargo run  # подробные логи (env-filter)

scripts/install-daemon.sh    # собрать, подписать, поставить LaunchAgent (авто-старт при логине)
scripts/uninstall-daemon.sh  # снять LaunchAgent (данные в Application Support не трогает)
scripts/build-icon.sh        # перегенерировать macos/AppIcon.icns из SF Symbol (см. generate-icon.swift)
scripts/release.sh 0.2.0     # выпустить релиз: бамп версии + дата CHANGELOG + коммит + тег + пуш → Release-workflow (задача 024)
```

Короткий алиас `tm` — симлинк на release-бинарь в `~/.bin` (в `PATH`), чтобы
звать `tm stats` / `tm status` откуда угодно. Его **создаёт/обновляет
`install-daemon.sh` и снимает `uninstall-daemon.sh`** (переопределяется через
`LINK_DIR`/`LINK_NAME`, `LINK_NAME=""` — пропустить). Симлинк указывает на
артефакт сборки, поэтому подхватывает свежий бинарь после каждого rebuild.
Вручную (без демона): `ln -sfn "$PWD/target/release/treadmill-bluetooth-macos" ~/.bin/tm`.

## Конфиг (per-user)

Конфиг (цели, gap, авто-пауза, Zone Hold) — **per-user**, живёт **не в этом
репо**, а в домашней директории: **`~/.config/treadmill-bluetooth-macos/config.toml`**
(`$HOME`-anchored, работает под launchd). **TOML** (задача 023, был JSON
`config.json`/`goals.json`) — ради комментариев: дефолты в примере видны
закомментированными строками. Формат — см. `config/config.example.toml`:
`goals = [8000, 10000, 12000]` + опциональные `workout_gap_minutes` /
`auto_pause_minutes` / `show_speed` / `[zone_hold]` (задача 027, см.
`src/zone_hold/` выше — секцию обычно пишет `tm zone on`/`setup`, не
руки). Опциональный
`workout_gap_minutes` (задача 014, дефолт 15) —
read-time порог: соседние сегменты активности с разрывом ≤ него показываются
одной тренировкой; меняется ретроактивно (без пересчёта). Опциональный
`auto_pause_minutes` (задача 020, дефолт 5, `0` — выключено) — сколько лента может
крутиться `AwayWhileRunning` (человек сошёл) до того, как демон поставит её на
паузу; дальше лента гаснет своим встроенным механизмом. Опциональный
`show_speed` (задача 029, дефолт `false`) — показ живой скорости ленты
(км/ч) в tmux-виджете; обычно управляется через `tm speed-widget on`/`off`,
не руки. Отсутствует/битый ключ →
дефолт (absent — тихо, т.к. `widget` читает раз в 2 с; невалидное значение →
WARN). Резолвинг (задача 023, один путь): env `TREADMILL_CONFIG` →
`$HOME/.config/.../config.toml` → вшитые дефолты `[8000,10000,12000]` (JSON- и
legacy-env-фолбэки задачи 021 убраны). Нет файла — норма (INFO + дефолты); битый
файл — WARN. Каждый пользователь приносит свой файл (например, симлинком из
личного dotfiles-репо). Правки **подхватываются на лету без рестарта** (задача
017): демон следит за mtime `config.toml` и перечитывает цели **и** порог
авто-паузы при изменении
(≤5 с, только когда файл реально менялся). `workout_gap_minutes` и так read-time
(ретроактивен). Что **сейчас загружено в демоне** и когда он последний раз читал
файл — видно в `tm status` (задача 022): демон пишет снапшот загруженного
конфига (цели + авто-пауза + время чтения) в `daemon_status`, `status` его
печатает; `workout_gap_minutes` показывается отдельно как read-time. Мгновенно
применить всё равно можно рестартом (`launchctl kickstart -k` или переустановка). Tier (яркость toast'а) — из ранга по возрастанию: низший порог →
tier 1. Каждая цель празднуется ровно раз в день (local date, restart-safe через
таблицу `goal_celebrations`). См. `docs/tasks/011-...md`.

## Заметки по macOS

- Первый запуск запросит разрешение на Bluetooth (CoreBluetooth). Без него скан пуст.
- Адресов устройств на macOS нет — идентификатор это непрозрачный system UUID.

## Конвенции

- Комментарии в коде — только на английском.
- Логируем аномалии/edge cases, а не happy path.
- Держим файлы маленькими и однонаправленными; парсинг протокола отдельно от транспорта.
- Docs-first: перед задачей — заметка в `docs/tasks/`, после — обновить затронутые доки.
- **Cyan = конфигурируемое** (задача 057): в human-readable выводе read-команд
  и confirmation-строках write-команд значение, которое оператор может поменять
  в `config.toml`/через `tm ...`-сеттер, красится голубым (ANSI 36) через
  `commands::common::highlight_config`. Строго значение, не подпись; живое/
  вычисленное состояние (bpm, phase, power mode, computed speed) — без цвета.
  Гейт `color_enabled()` (TTY + `NO_COLOR`) — пайпы/скрипты/`tm widget` TSV
  получают чистый текст. Красим padded-колонки только pad-then-colour (ANSI-
  байты не должны попадать в `{:<N}`).
