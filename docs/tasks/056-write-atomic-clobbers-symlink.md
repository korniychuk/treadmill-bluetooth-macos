# 056 — `write_atomic` затирает симлинк на dotfiles

**Status:** done (2026-07-11)
**Severity:** medium — оператор теряет связь конфига с dotfiles-репо молча

## Symptom

`~/.config/treadmill-bluetooth-macos/config.toml` у оператора — симлинк в
личный dotfiles-репо (`ankor-dotfiles/treadmill/config.toml`). После любой
CLI-записи конфига (`tm zone on/off/target/...`, `tm speed-widget on/off`)
симлинк превращается в обычный файл. Правки в dotfiles перестают действовать
(live 2026-07-11: правка `auto_pause_minutes` в репо — без эффекта, демон
читал отвязанную копию).

## Root cause

Задача 037 ввела `goals::write_atomic` (same-dir temp + `rename(2)`) ради
crash-safety — `std::fs::write` truncate-ит первым и мог обнулить конфиг.
Но `rename(2)` заменяет **сам симлинк**, а не его цель: temp-файл — обычный
файл, и после rename путь указывает на него. `fs::write` симлинки следовал,
поэтому до 037 проблемы не было.

## Fix

`write_atomic` резолвит цепочку симлинков (`fs::read_link` до `MAX_SYMLINK_HOPS`,
относительные цели — от родителя линка) и делает temp+rename **рядом с целью**:
атомарность сохраняется (тот же каталог = та же FS), симлинк переживает запись,
контент едет в dotfiles-репо, как и ждёт оператор. Висячий линк тоже резолвится
— файл создаётся в месте цели, линк оживает.

Сопутствующее (в ankor-dotfiles): `install.sh` линковал устаревший
`goals.json` (до миграции 023 на TOML) — обновлён на `config.toml`, чтобы
переустановка dotfiles восстанавливала актуальную ссылку.

## Acceptance

- [x] unit-тест: запись через симлинк сохраняет симлинк, контент — в цели
- [x] unit-тест: обычный файл — поведение прежнее (temp+rename на месте)
- [x] живой симлинк оператора восстановлен на dotfiles
