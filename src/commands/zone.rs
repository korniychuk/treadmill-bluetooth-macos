//! `zone` CLI command (Zone Hold config).

use anyhow::{Result, bail};

use crate::ZoneAction;
use crate::commands::common::{highlight_config, zone_hold_config_path};
use crate::commands::zone_prompts::{
    prompt_age, prompt_line, prompt_optional_max_speed, prompt_optional_resting_hr,
    prompt_zone_bounds, prompt_zone_id,
};
use crate::store;
use crate::zone_hold;

/// Dispatch a `tm zone` sub-action (задача 027). Read/write config + SQLite
/// only — no BLE, same constraint as `status`/`widget`.
pub(crate) fn run_zone(action: Option<ZoneAction>) -> Result<()> {
    match action {
        None => print_zone_status(),
        Some(ZoneAction::On) => zone_on(),
        Some(ZoneAction::Off) => set_zone_hold_key("enabled", "false".to_string()).map(|()| {
            println!("Zone Hold {}.", highlight_config("disabled"));
        }),
        Some(ZoneAction::Setup) => zone_onboarding_prompt(),
        Some(ZoneAction::Limits { max, min, min_flag }) => zone_limits(max, min.or(min_flag)),
        Some(ZoneAction::Target { zone }) => zone_target(&zone),
        Some(ZoneAction::List) => zone_list(),
        Some(ZoneAction::Add) => zone_add(),
        Some(ZoneAction::Edit { zone }) => zone_edit(&zone),
        Some(ZoneAction::Remove { zone }) => zone_remove(&zone),
        Some(ZoneAction::Mode { tracking }) => zone_mode(&tracking),
    }
}

/// `tm zone on`: enable the master switch, running the interactive onboarding
/// prompt first if `age` isn't configured yet (задача 027, §Onboarding).
pub(crate) fn zone_on() -> Result<()> {
    if zone_hold::load_zone_hold_config().age.is_none() {
        return zone_onboarding_prompt();
    }
    set_zone_hold_key("enabled", "true".to_string())?;
    println!("Zone Hold {}.", highlight_config("enabled"));
    Ok(())
}

/// Interactive age/resting-HR prompt (задача 027, §Onboarding) — writes
/// `enabled = true` alongside whatever was entered, since the whole point of
/// running this is to turn Zone Hold on. Used by both `tm zone on` (first run)
/// and `tm zone setup` (reconfigure).
pub(crate) fn zone_onboarding_prompt() -> Result<()> {
    println!("Zone Hold setup");
    let age = prompt_age()?;
    let resting_hr = prompt_optional_resting_hr()?;

    let mut updates = vec![("enabled", "true".to_string()), ("age", age.to_string())];
    if let Some(resting_hr) = resting_hr {
        updates.push(("resting_hr", resting_hr.to_string()));
    }
    let path = zone_hold_config_path()?;
    zone_hold::upsert_zone_hold_keys(&path, &updates)?;

    let hrmax = zone_hold::hrmax_tanaka(age);
    println!("HRmax (Tanaka) \u{2248} {hrmax:.0} bpm.");
    println!(
        "Zone Hold enabled — target zone #{} (default: Aerobic base, 60-70% HRmax).",
        zone_hold::DEFAULT_TARGET_ZONE
    );
    println!(
        "See all zones with `tm zone list`; change the target with \
         `tm zone target <n|id|name>`, tune limits with `tm zone limits`, or edit \
         `[[zone_hold.zones]]` in config.toml directly for custom bounds."
    );
    Ok(())
}

/// `tm zone limits <max> [<min>]` / `tm zone limits --min <min>` (задача 027,
/// §Min/max) — writes the *global* `max_speed`/`min_speed` keys; per-zone
/// overrides stay a manual config edit.
pub(crate) fn zone_limits(max: Option<f32>, min: Option<f32>) -> Result<()> {
    if max.is_none() && min.is_none() {
        bail!(
            "specify at least a max speed, e.g. `tm zone limits 5` or `tm zone limits --min 2.5`"
        );
    }
    let mut updates = Vec::new();
    if let Some(max) = max {
        updates.push(("max_speed", max.to_string()));
    }
    if let Some(min) = min {
        updates.push(("min_speed", min.to_string()));
    }
    let path = zone_hold_config_path()?;
    zone_hold::upsert_zone_hold_keys(&path, &updates)?;
    println!(
        "Zone Hold limits updated:{}{}",
        max.map(|m| format!(" max {} km/h", highlight_config(m)))
            .unwrap_or_default(),
        min.map(|m| format!(" min {} km/h", highlight_config(m)))
            .unwrap_or_default(),
    );
    Ok(())
}

/// `tm zone target <n|id|name-substring>` — resolves against the currently
/// configured zones and persists the *canonical id* (not the raw input), so
/// a fuzzy name match today still points at the right zone if `config.toml`
/// is reordered later. Bare numbers keep the legacy numeric form, since that
/// selector has no id to normalise to.
pub(crate) fn zone_target(raw: &str) -> Result<()> {
    let config = zone_hold::load_zone_hold_config();
    let selector = parse_zone_selector(raw);
    let Some((number, zone)) = zone_hold::find_zone(&config.zones, &selector) else {
        let known: Vec<String> = config
            .zones
            .iter()
            .enumerate()
            .map(|(i, z)| format!("#{} {} ({})", i + 1, z.id, z.name))
            .collect();
        bail!(
            "no zone matches `{}` — configured zones: {}",
            raw.trim(),
            known.join(", ")
        );
    };
    let value = match selector {
        zone_hold::ZoneSelector::Number(n) => n.to_string(),
        zone_hold::ZoneSelector::Id(_) => format!("\"{}\"", zone.id),
    };
    set_zone_hold_key("target_zone", value)?;
    println!(
        "Zone Hold target zone set to {}.",
        highlight_config(format!("#{number} {} ({})", zone.id, zone.name))
    );
    Ok(())
}

/// `tm zone list` — every configured zone with its id and bpm range, so
/// `target` isn't a guessing game (задача 027 follow-up). Falls back to the
/// raw percent/bpm bounds when `age` isn't configured yet (no HRmax to
/// resolve percent zones against).
pub(crate) fn zone_list() -> Result<()> {
    let config = zone_hold::load_zone_hold_config();
    let hrmax = config.hrmax();
    let method_label = match config.method {
        zone_hold::Method::HrMax => "hrmax",
        zone_hold::Method::Karvonen => "karvonen",
    };
    if hrmax.is_none() {
        println!(
            "Configured zones (age not set — showing raw bounds, not bpm; run `tm zone setup`):"
        );
    } else {
        println!("Configured zones ({}):", highlight_config(method_label));
    }
    let target_number = zone_hold::find_zone(&config.zones, &config.target_zone).map(|(n, _)| n);
    for (index, zone) in config.zones.iter().enumerate() {
        let number = index + 1;
        let marker = if target_number == Some(number) {
            "*"
        } else {
            " "
        };
        let range = match hrmax {
            Some(hrmax) => {
                let (low, high) = zone_hold::resolve_zone_bpm(
                    hrmax,
                    config.resting_hr,
                    config.method,
                    zone.bounds,
                );
                format!("{low}-{high} bpm")
            }
            None => match zone.bounds {
                zone_hold::ZoneBounds::Percent { min, max } => format!("{min:.0}-{max:.0}% HRmax"),
                zone_hold::ZoneBounds::Absolute { min_bpm, max_bpm } => {
                    format!("{min_bpm}-{max_bpm} bpm")
                }
            },
        };
        let max_speed = zone.max_speed_kmh.unwrap_or(config.max_speed_kmh);
        // name / id / max-speed are config → cyan; the bpm range is resolved at
        // runtime → plain. Pad *before* colouring so the ANSI bytes don't count
        // toward the `{:<N}` column widths and break alignment (задача 057).
        let name_col = highlight_config(format!("{:<14}", zone.name));
        let id_col = highlight_config(format!("{:<16}", zone.id));
        let max_col = highlight_config(format!("max {max_speed} km/h"));
        println!("{marker} #{number} {name_col} id={id_col} {range:<16} {max_col}");
    }
    println!("(* = current target; select with `tm zone target <id|name|number>`)");
    Ok(())
}

/// `tm zone add` — interactively append a custom zone (задача 027 follow-up).
/// `load_zone_hold_config` already materialises the built-in 5 zones when
/// `config.toml` has none configured, so pushing onto `config.zones` and
/// writing it back via `replace_zones` keeps every existing zone (custom or
/// default) — it never silently drops down to just the new one.
pub(crate) fn zone_add() -> Result<()> {
    println!("Add a custom Zone Hold zone.");
    let mut config = zone_hold::load_zone_hold_config();
    let name = prompt_line("Name", None)?;
    let default_id = zone_hold::slugify(&name);
    let id = prompt_zone_id(&default_id, &config.zones, None)?;
    let bounds = prompt_zone_bounds(None)?;
    let max_speed_kmh = prompt_optional_max_speed(None)?;
    config.zones.push(zone_hold::ZoneDef {
        id: id.clone(),
        name: name.clone(),
        bounds,
        max_speed_kmh,
    });
    let path = zone_hold_config_path()?;
    zone_hold::replace_zones(&path, &config.zones)?;
    // The just-written zone identity is cyan; the trailing `tm zone target {id}`
    // is a command hint, left plain (задача 057).
    println!(
        "Added zone `{}` ({}). See it with `tm zone list`; select it with `tm zone target {id}`.",
        highlight_config(&id),
        highlight_config(&name),
    );
    Ok(())
}

/// `tm zone edit <zone>` — interactively change an existing zone's
/// name/bounds/max-speed override, keeping its `id` stable (it's what
/// `target_zone` may already point at, so renaming it here would silently
/// break that reference — remove + re-add is the explicit way to rename).
pub(crate) fn zone_edit(raw: &str) -> Result<()> {
    let mut config = zone_hold::load_zone_hold_config();
    let selector = parse_zone_selector(raw);
    let Some((index, zone)) = zone_hold::find_zone(&config.zones, &selector) else {
        bail!("no zone matches `{}` — see `tm zone list`", raw.trim());
    };
    let index = index - 1;
    println!(
        "Editing zone `{}` ({}) — press Enter to keep the current value.",
        zone.id, zone.name
    );
    let name = prompt_line("Name", Some(&zone.name))?;
    let bounds = prompt_zone_bounds(Some(zone.bounds))?;
    let max_speed_kmh = prompt_optional_max_speed(zone.max_speed_kmh)?;
    let id = zone.id.clone();
    config.zones[index] = zone_hold::ZoneDef {
        id: id.clone(),
        name,
        bounds,
        max_speed_kmh,
    };
    let path = zone_hold_config_path()?;
    zone_hold::replace_zones(&path, &config.zones)?;
    println!("Updated zone `{}`.", highlight_config(&id));
    Ok(())
}

/// `tm zone remove <zone>` — refuses to drop the last remaining zone (Zone
/// Hold needs at least one to resolve `target_zone` against).
pub(crate) fn zone_remove(raw: &str) -> Result<()> {
    let mut config = zone_hold::load_zone_hold_config();
    if config.zones.len() <= 1 {
        bail!("can't remove the last zone — Zone Hold needs at least one configured");
    }
    let selector = parse_zone_selector(raw);
    let Some((_, zone)) = zone_hold::find_zone(&config.zones, &selector) else {
        bail!("no zone matches `{}` — see `tm zone list`", raw.trim());
    };
    let removed_id = zone.id.clone();
    let removed_name = zone.name.clone();
    config.zones.retain(|z| z.id != removed_id);
    let path = zone_hold_config_path()?;
    zone_hold::replace_zones(&path, &config.zones)?;
    println!(
        "Removed zone `{}` ({}).",
        highlight_config(&removed_id),
        highlight_config(&removed_name)
    );
    if config.resolve_target_zone().is_none() {
        println!(
            "Note: the current target_zone no longer resolves — pick a new one with `tm zone target`."
        );
    }
    Ok(())
}

/// A bare number parses as the legacy 1-based position; anything else is
/// looked up as an id/name (задача 027 follow-up — shared by
/// target/edit/remove so all three accept the same selector syntax).
pub(crate) fn parse_zone_selector(raw: &str) -> zone_hold::ZoneSelector {
    let trimmed = raw.trim();
    match trimmed.parse::<u8>() {
        Ok(n) => zone_hold::ZoneSelector::Number(n),
        Err(_) => zone_hold::ZoneSelector::Id(trimmed.to_string()),
    }
}

/// `tm zone mode <band|center>` (задача 027, §Режимы).
pub(crate) fn zone_mode(tracking: &str) -> Result<()> {
    match tracking {
        "band" | "center" => {
            set_zone_hold_key("tracking", format!("\"{tracking}\""))?;
            println!(
                "Zone Hold tracking mode set to `{}`.",
                highlight_config(tracking)
            );
            Ok(())
        }
        other => bail!("unknown tracking mode `{other}` — use `band` or `center`"),
    }
}
/// Update a single `[zone_hold]` key in place, creating the config directory
/// if this is the first write to it.
pub(crate) fn set_zone_hold_key(key: &str, value: String) -> Result<()> {
    let path = zone_hold_config_path()?;
    zone_hold::upsert_zone_hold_keys(&path, &[(key, value)])
}
/// `tm zone` (no sub-action): current config plus, when the daemon has a
/// fresh snapshot, whether the controller is actively engaged right now
/// (задача 027).
pub(crate) fn print_zone_status() -> Result<()> {
    let config = zone_hold::load_zone_hold_config();
    // `on`/`off` mirrors the config `enabled` flag → cyan (задача 057).
    println!(
        "Zone Hold: {}",
        highlight_config(if config.enabled { "on" } else { "off" })
    );
    if !config.enabled {
        println!("  enable with `tm zone on`");
        return Ok(());
    }

    match config.age {
        None => println!("  not configured yet — run `tm zone setup`"),
        Some(age) => {
            let method = match config.method {
                zone_hold::Method::HrMax => "hrmax",
                zone_hold::Method::Karvonen => "karvonen",
            };
            let tracking = match config.tracking {
                zone_hold::Tracking::Band => "band",
                zone_hold::Tracking::Center => "center",
            };
            // age / method / tracking are all config values → cyan.
            println!(
                "  age {}, method {}, tracking {}",
                highlight_config(age),
                highlight_config(method),
                highlight_config(tracking),
            );
            match config.resolve_target_zone() {
                Some(resolved) => {
                    // The targeted zone's identity and the speed limits are
                    // config; the bpm range is resolved from HRmax at runtime,
                    // so it stays plain (задача 057).
                    let target = highlight_config(format!(
                        "#{} {} ({})",
                        resolved.number, resolved.id, resolved.name
                    ));
                    let min_speed = highlight_config(config.min_speed_kmh.to_string());
                    let max_speed = highlight_config(resolved.effective_max_speed_kmh.to_string());
                    println!(
                        "  target zone {target}: {}-{} bpm \u{2022} speed {min_speed}-{max_speed} km/h",
                        resolved.low_bpm, resolved.high_bpm,
                    );
                }
                None => println!(
                    "  target zone not found among {} configured zones — see `tm zone list`",
                    config.zones.len()
                ),
            }
        }
    }

    let store = store::Store::open()?;
    match store.daemon_status()? {
        Some(status) if status.zone_hold_active => {
            let phase = status.zone_hold_phase.as_deref().unwrap_or("?");
            let range = match (status.zone_hold_target_lo, status.zone_hold_target_hi) {
                (Some(lo), Some(hi)) => format!("{lo}-{hi} bpm"),
                _ => "? bpm".to_string(),
            };
            let speed = status
                .zone_hold_last_speed
                .map(|s| format!("{s:.1} km/h"))
                .unwrap_or_else(|| "?".to_string());
            let position = status.zone_hold_position.as_deref().unwrap_or("\u{2014}");
            println!(
                "  active now: phase {phase}, {range}, last speed {speed}, position {position}"
            );
        }
        _ => println!("  not currently engaged (not walking, sensor not worn, or daemon idle)"),
    }
    Ok(())
}
