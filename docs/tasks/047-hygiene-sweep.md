# 047 — Hygiene sweep: мелкие несистемные находки review 004

> **Статус: open**
> **Источник:** [research/004](../research/004-independent-reliability-review.md) §3 N7
> **Класс:** hygiene / docs drift
> **Приоритет:** low — чеклист «между делом», не блокирует ничего; можно дробить

Каждый пункт самодостаточен; порядок произвольный.

## Чеклист

- [ ] **`tm notify-test` не покрывает `auto_paused`** (`main.rs` ~396–437, `notify.rs` ~158): единственный прод-тост вне smoke-таблицы, чей смысл — «fire every toast once». Добавить в массив.
- [ ] **`presence_state` — stringly-typed `Debug`-контракт**: писатель `format!("{next_state:?}")` (`daemon.rs` ~749), читатели матчат строки с молчаливым `_ => "unknown"` без WARN (`main.rs` ~1644, ~1304). Минимум: `warn!` на нераспознанное значение (конвенция «log every edge case»); лучше: явный `wire()` как у `ZonePosition` (`zone_hold.rs` ~327), чтобы rename enum'а не ломал контракт молча.
- [ ] **`fitshow.rs`/`discover.rs`/`sniff.rs` — живые CLI-команды вне доков и тестов**: реальные match-arms в `main.rs` (~286–301), но ни одна не упомянута в CLAUDE.md (ни в архитектуре, ни в «Команды»), ноль тестов — при том что `fitshow::frame`/`parse_frame` (XOR-фрейминг, ~53–75) — чистые функции ровно того сорта, что проект обычно покрывает. Добавить абзац в CLAUDE.md + юнит-тесты на framing.
- [ ] **`compute_default_speed` на каждом presence-transition при выключенном Zone Hold** (`daemon.rs` ~841–845): DB-запрос со сканом истории считается до вызова `zone_hold_on_transition`, который тут же early-return'ится на `!enabled`. Гейтнуть за `config.zone_hold.enabled`.
- [ ] **`goals.rs`: до 4× чтение+парс одного файла за widget-tick**: `GapSetting`/`AutoPauseSetting`/`ShowSpeedSetting` — три клона enum'а с парными `read_*`/`load_*`, каждый сам делает `read_to_string` + `toml::from_str`. Один generic-читатель / один распарсенный `toml::Value` на вызов. (DRY + меньше независимых silent-fallback поверхностей.)
- [ ] **`default_speed.rs` ~101: `partial_cmp(...).expect("never NaN")`** на float'ах из внешних данных → `total_cmp` — panic-proof за ту же цену.

## Acceptance

- [ ] Все чекбоксы закрыты (или явно вынесены в отдельные задачи)
- [ ] CLAUDE.md обновлён (fitshow/discover/sniff)
- [ ] Никаких поведенческих изменений сверх описанного

## Связанное

- research 004 §3 N7
- 038 — doctor (сосед по observability-гигиене)
