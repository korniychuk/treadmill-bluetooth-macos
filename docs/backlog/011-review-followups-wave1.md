# 011 — Follow-ups из адверсариального ревью волны 1 (Low)

**Status:** backlog — взять после мержа 053 (session state extract), оба
пункта в зонах, которые 053 переписывает.
**Source:** ревью `git diff 2e2bb1a..d8aa483` (049/050/051/052/010),
2026-07-11. Находки Medium/#2 уже закрыты (`38de7da`).

## 1. Расширить persist-tolerance на соседние `state.persist(...)?`

Backlog 010 обернул только per-sample путь (`insert_raw_sample` +
`advance_baseline`) и status-event insert. Остались фатальными для стрима:
`state.persist(store, watchdog)?` в status-event ветке, 30с `persist_tick`,
config-reload ветке и хвосте телеметрической ветки. После WAL практически
недостижимо (single writer), но гарантия «keep stream alive on DB busy» уже,
чем заявлена. Прогнать через тот же tolerate-and-skip механизм (счётчик
`db_persist_failures` уже есть) — либо явно зафиксировать сужение в 010.

## 2. Field-only reload `[zone_hold]` применяется молча

`config_apply::push_zone_effects` эмитит эффект только для
Disengage/Engage/ReResolve/WarmupRetarget. Строки матрицы 052 «field update +
лог» (например смена `target_zone`/`min_speed`/`deadband`/`safety_cap_bpm`
при `phase=Off`) пишут конфиг, но экзекьютор не логирует ничего — старый код
(до 052) логировал безусловно. Оператор: `tm zone target 3` стоя не на ленте
— применилось, в логе демона ни следа. Фикс: log-only эффект
`ZoneConfigChanged` (матрица остаётся exhaustive) + поправить тесты с
`expect: &[]`.
