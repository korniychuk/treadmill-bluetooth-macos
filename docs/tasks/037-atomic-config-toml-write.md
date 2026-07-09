# 037 — Atomic write для `config.toml` (truncate wipe)

> **Статус: done**  
> **Источник:** [research/003](../research/003-reliability-architecture-review.md) §3.8, Phase 0.3  
> **Класс:** durability / config I/O  
> **Приоритет:** medium-high (дешёвый, data-loss path)

## Симптом

`std::fs::write` сначала **truncate** файл, потом пишет тело. Если процесс убит / panic / power loss между truncate и complete write — `config.toml` оператора пустой или partial. Daemon hot-reload (5с) подхватит битый/пустой конфиг → goals/zone/auto-pause defaults, Zone Hold может disengage или включиться с defaults.

Главный риск — **не** half-read демоном (окно микросекундное), а **permanent wipe** конфига.

### Доп. следствие half-read (review [004](../research/004-independent-reliability-review.md), уточнение к «окно микросекундное»)

Если config-tick всё же поймает truncate/partial-окно параллельного `tm zone …`: `load_zone_hold_config` на parse-fail возвращает `disabled_default()` → reload-ветка делает `disengage_zone_hold` → **mid-session сброс Zone Hold**. Через ≤5с конфиг перечитается целым, но восстановление идёт через catch-up «zone on» = **свежий 5-минутный Ramp** — оператор молча теряет `Hold` от любой CLI-правки конфига, попавшей в окно. Atomic rename закрывает и это, не только crash-wipe.

## Где пишут

| Файл | ≈ строки | Функции |
|---|---:|---|
| `src/zone_hold.rs` | 682, 704, 769 | `upsert_zone_hold_keys`, `replace_zones` (и родственные line-patch) |
| `src/goals.rs` | 335 | `upsert_top_level_key` (goals, auto_pause, show_speed, …) |

Тестовые `fs::write` во временные файлы **не** трогать (fixtures).

## Решение

Один shared helper (предпочтительно в `goals.rs` или tiny `src/config_io.rs`):

```rust
fn write_atomic(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?; // or path.with_extension("toml.tmp")
    tmp.write_all(contents.as_ref())?;
    tmp.persist(path)?; // atomic rename on same FS
    Ok(())
}
```

Ограничения:

- temp файл **в том же directory**, что и target (rename across volumes не atomic);
- на macOS same-dir `rename(2)` atomic для replace;
- permissions/ownership: preserve mode target if we care (optional; config is user-owned);
- без новой тяжёлой зависимости, если можно: `path.with_extension("toml.tmp")` + `fs::write(tmp)` + `fs::rename(tmp, path)` — **три строки**, YAGNI `tempfile` crate unless already present.

Все production writers (`upsert_zone_hold_keys`, `replace_zones`, `upsert_top_level_key`) → helper.

## Тесты

- Unit: write to temp dir, kill-mid-write simulation hard; instead:
  - happy path: content round-trips;
  - rename replaces existing file;
  - if using manual tmp: failed write to tmp must **not** delete target (write tmp first, rename only on success).
- Existing zone/goals upsert tests keep green (they use real writes to temp dirs).

## Acceptance

- [x] No production `std::fs::write(config_path, …)` that truncates the live config in place
- [x] Crash after tmp write / before rename leaves old config intact
- [x] `tm zone …` / goals upsert still work; hot-reload still sees mtime change after successful rename
- [x] Helper used from both `zone_hold` and `goals`

## Затронутые файлы

- `src/goals.rs`, `src/zone_hold.rs` (± tiny `config_io` module)
- `Cargo.toml` only if adding `tempfile` (prefer std-only)

## Связанное

- 017 — hot-reload
- 023 — config.toml migration
- 027/029/032 — frequent config writers
- research 003 §3.8, Phase 0.3
