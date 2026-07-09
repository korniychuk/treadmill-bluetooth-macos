# 042 — HR link-state hygiene: залипающий `hr_connect_in_flight` + сверка reset-путей

> **Статус: open**
> **Источник:** [research/004](../research/004-independent-reliability-review.md) §3 N4
> **Класс:** `liveness` / state reset divergence (класс 033)
> **Приоритет:** medium (после 035; low-likelihood, но без recovery-пути)

## Проблема 1: `hr_connect_in_flight` может залипнуть навсегда

`src/daemon.rs`: флаг ставится `true` перед `spawn_hr_connect_attempt` (~624/~1133), снимается **только** в `hr_rx.recv()`-ветке (~1018). Spawned-таска best-effort всегда шлёт в канал — но если она паникует внутри `connect_hr`/`subscribe_hr` или send проигрывает teardown'у, флаг остаётся `true`, reconnect tick гейтится `!hr_connect_in_flight` (~1132) → **ни одной новой попытки HR-коннекта до конца сессии**. Никакого deadline/heartbeat на in-flight нет.

### Решение

- Запоминать `hr_connect_started_at: Instant` при спавне.
- В reconnect tick: если `hr_connect_in_flight` и `started_at.elapsed() > HR_CONNECT_ATTEMPT_DEADLINE` (константа: scan 15с + connect + запас, ~45–60с) → WARN «HR connect attempt vanished — resetting latch», сбросить флаг, разрешить новую попытку.
- Дёшево и не меняет happy path; альтернатива (JoinHandle + abort) — сложнее, YAGNI.

## Проблема 2: reset-пути HR-состояния расходятся по полям

Три места сбрасывают HR-состояние, каждое — свой набор полей:

| Путь | `hr_connected` | `last_bpm`/`ts` | tracker | battery |
|---|---|---|---|---|
| contact Lost (~1090) | ✓ false | ✓ None | — (жив, link-scoped) | — (жив) |
| link loss `Ok(None)`/`Err` (~1100/~1114) | ✓ false | ✗ **не чистится** | ✓ default | ✓ None |
| session teardown (~499) | ✓ | ✓ | — | — |

Незачищенный `last_bpm` при `hr_connected=false` сегодня маскируется тем, что все читатели гейтятся на `hr_connected` первым — но это ровно класс «одна ветка забыла поле» (033). Чистка `last_bpm`/`last_bpm_ts` на link-loss ветках делается в **035** (те же строки переписываются под sleep_until); эта задача — добить остальное и закрепить:

### Решение

1. После 035 свести все сбросы к **одной** функции/методу (`reset_hr_link_state(...)` или зачаток `HrLink::on_lost()` из backlog [005](../backlog/005-session-state-extract.md)) с явным различием contact-scoped vs link-scoped полей.
2. Unit-тест-чеклист: после link-loss все link-scoped поля пусты; после contact-Lost link-scoped поля живы.

## Acceptance

- [ ] Залипший in-flight латч самовосстанавливается через deadline (+ WARN)
- [ ] Один reset-путь для link-loss; нет ветки, чистящей подмножество полей
- [ ] `hr_connected=false` ⇒ `last_bpm/last_bpm_ts = None` (инвариант, тест)
- [ ] Существующее contact-vs-link поведение (033) не изменилось

## Затронутые файлы

- `src/daemon.rs` — HR-ветки `select!`, reconnect tick

## Связанное

- 035 — переписывает те же link-loss ветки (координация: 035 первым)
- 033 — contact ≠ link (не сломать)
- backlog 005 — `HrLink` struct как окончательный дом этой логики
- research 004 §3 N4
