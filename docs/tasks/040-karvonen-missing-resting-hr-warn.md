# 040 — Karvonen без `resting_hr`: WARN + явный fallback

> **Статус: open**  
> **Источник:** [research/003](../research/003-reliability-architecture-review.md) §3.5, Phase 0.6  
> **Класс:** config semantics / observability  
> **Приоритет:** low-medium (не measurement bug; дешёвый)

## Симптом

`method = "karvonen"` при `resting_hr = None` / missing:

```rust
// zone_hold.rs ~297
let resting = resting_hr.unwrap_or(0) as f32;
let hrr = (hrmax - resting).max(0.0);
// → zones = hrmax * pct/100  (exactly Method::HrMax algebra)
```

**Не** мусор и не класс 030: оператор просит Karvonen, получает **численные** HRmax-проценты. Молча.

Уточнение severity (review [004](../research/004-independent-reliability-review.md)): fallback-зоны **систематически ниже** задуманных (Karvonen с реальным resting всегда даёт границы выше HRmax-процентов) — оператор тренируется ниже целевой аэробной зоны, т.е. деградация направленная, не «эквивалентный метод». Фикс тот же (WARN + явный fallback), но молчать об этом нельзя.

## Решение

В `resolve_zone_bpm` (или на resolve path `ResolvedZone`):

1. `Method::Karvonen` + `resting_hr.is_none()` (or 0 if we treat 0 as invalid):
   - `warn!(…, "zone_hold: method=karvonen but resting_hr missing — falling back to hrmax percents")` **один раз на resolve/config apply**, не на каждый tick (rate-limit / only when building ResolvedZone from config).
   - compute via same algebra as HrMax **or** branch to `Method::HrMax` path explicitly.
2. Optional: at config load (`load_zone_hold_config`), if method karvonen && resting missing → WARN already (best place — once per reload).
3. Docs: `CLAUDE.md` / zone help one line — Karvonen requires `resting_hr`.

**Не** hard-fail config load (daemon must stay up); **не** invent resting HR.

## Тесты

- `resolve_zone_bpm(…, None, Karvonen, bounds)` equals `resolve_zone_bpm(…, None, HrMax, bounds)` (or equals resting=0 path documented).
- Existing `resolve_zone_bpm_karvonen_sits_higher_than_hrmax` stays (with `Some(resting)`).

## Acceptance

- [ ] Missing resting + karvonen → WARN (load or resolve)
- [ ] Numeric zones = HrMax percents (documented)
- [ ] No panic / no engage block solely due to missing resting
- [ ] Unit test for equality / fallback

## Затронутые файлы

- `src/zone_hold.rs`
- optionally zone status text in `main.rs` if we surface «degraded method»

## Связанное

- 027 — zone hold
- research 003 §3.5 (errata: not a 030-class bug)
