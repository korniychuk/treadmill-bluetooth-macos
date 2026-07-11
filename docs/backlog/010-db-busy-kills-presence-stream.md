# 010 — SQLITE_BUSY на записи роняет presence-стрим (нет WAL)

**Status:** backlog → взять сразу после мержа 049 (store split)
**Severity:** high — живые разрывы телеметрии во время тренировки
**Seen live:** 2026-07-11 ~03:19 и ~03:33 UTC, во время тренировки оператора

## Symptom

Лог демона (2 раза за одну тренировку):

```
WARN presence stream ended with an error err=update device_baseline
```

→ стрим завершается → полный reconnect-цикл (~10 с без телеметрии, у
оператора виджет гаснет посреди ходьбы). Второй случай каскадировал в
`no telemetry received (20s)` + `disconnect timed out (possible CoreBluetooth hang)`.

## Root cause (две независимые проблемы)

1. **Нет WAL.** `store.rs::open_at` ставит `busy_timeout(3s)`, но
   `journal_mode` остаётся дефолтным (rollback journal): любой reader
   (`tm widget` каждые 2 с из tmux, `tm stats`, `tm status`) держит
   SHARED-lock на время чтения и **блокирует** writer. Под высокой
   системной нагрузкой (в инциденте — load average ~20 от параллельных
   cargo-сборок) чтение растягивается дольше 3 с → у демона SQLITE_BUSY.
   В WAL readers не блокируют writer в принципе.
2. **DB-ошибка фатальна для BLE-стрима.** `update device_baseline` (и
   соседние записи) пробрасываются `?` из телеметрического цикла — одна
   неудавшаяся запись рвёт живой BLE-линк, хотя линк здоров. Потеря
   одного сэмпла — recoverable anomaly (WARN + skip), не повод для
   reconnect: дельта-накопление restart-safe by design и переживает
   пропущенный persist.

## Fix plan

1. В `Store::open_at` (после 049 — `src/store/mod.rs`): `PRAGMA
   journal_mode=WAL;` + оставить busy_timeout. WAL персистентен для
   файла БД — включится один раз, переживает рестарты; читатели старых
   версий бинаря продолжают работать.
2. В телеметрической ветке демона: обернуть per-sample persist так,
   чтобы `Err` логировался (`WARN`, стабильный тег, offending
   операция) и **не** завершал стрим. Watchdog `touch()` при этом
   всё равно едет (event-loop жив). Порог деградации: если persist
   фейлится подряд > N (например 30 сэмплов ≈ 30 с), тогда ERROR —
   что-то реально сломано (диск, схема), и рестарт через watchdog
   оправдан.

## Acceptance

- [ ] `PRAGMA journal_mode` на живой базе = `wal` после первого запуска
- [ ] Симулированный SQLITE_BUSY (держать write-lock из второго процесса
      >3 с) не рвёт стрим: WARN в логе, телеметрия продолжается
- [ ] Серия неудачных persist > N подряд даёт ERROR (не молчит вечно)

## Non-goals

- Ретраи отдельных SQL-запросов (busy_timeout уже есть; WAL убирает класс)
- Смена схемы/переезд БД
