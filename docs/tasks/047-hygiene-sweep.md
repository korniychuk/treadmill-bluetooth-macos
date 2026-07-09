# 047 — Hygiene sweep: мелкие несистемные находки review 004

> **Статус: done**
> **Источник:** [research/004](../research/004-independent-reliability-review.md) §3 N7
> **Класс:** hygiene / docs drift
> **Приоритет:** low — чеклист «между делом», не блокирует ничего; можно дробить

Каждый пункт самодостаточен; порядок произвольный.

## Чеклист

- [x] **`tm notify-test` не покрывает `auto_paused`**
- [x] **`presence_state` — `PresenceState::wire()` + WARN on unrecognised**
- [x] **`fitshow` framing tests + CLAUDE.md reverse-eng commands**
- [x] **`compute_default_speed` gated on zone_hold.enabled**
- [x] **`goals.rs`: shared `read_config_value` for top-level key readers**
- [x] **`default_speed.rs`: `total_cmp` instead of `partial_cmp().expect`**

## Acceptance

- [x] Все чекбоксы закрыты (или явно вынесены в отдельные задачи)
- [x] CLAUDE.md обновлён (fitshow/discover/sniff)
- [x] Никаких поведенческих изменений сверх описанного

## Связанное

- research 004 §3 N7
- 038 — doctor (сосед по observability-гигиене)
