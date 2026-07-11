//! `zone` CLI command (Zone Hold config).

use anyhow::{Context, Result, bail};

use crate::ZoneAction;
use crate::commands::common::zone_hold_config_path;
use crate::store;
use crate::zone_hold;

/// Dispatch a `tm zone` sub-action (задача 027). Read/write config + SQLite
/// only — no BLE, same constraint as `status`/`widget`.
pub(crate) fn run_zone(action: Option<ZoneAction>) -> Result<()> {
    match action {
        None => print_zone_status(),
        Some(ZoneAction::On) => zone_on(),
        Some(ZoneAction::Off) => set_zone_hold_key("enabled", "false".to_string()).map(|()| {
            println!("Zone Hold disabled.");
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
    println!("Zone Hold enabled.");
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

pub(crate) fn prompt_age() -> Result<u32> {
    use std::io::Write;
    loop {
        print!("Your age (for the HRmax estimate): ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("read age from stdin")?;
        match line.trim().parse::<u32>() {
            Ok(age) if (1..=120).contains(&age) => return Ok(age),
            _ => println!("please enter a whole number between 1 and 120"),
        }
    }
}

/// `None` on an empty line (skip) or an implausible value — resting HR is
/// optional, so a bad entry just falls back to skipping it rather than
/// looping forever on an optional field.
pub(crate) fn prompt_optional_resting_hr() -> Result<Option<u16>> {
    use std::io::Write;
    print!("Resting heart rate, bpm (optional — press Enter to skip): ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("read resting HR from stdin")?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match trimmed.parse::<u16>() {
        Ok(hr) if (30..=120).contains(&hr) => Ok(Some(hr)),
        _ => {
            println!("not a plausible resting heart rate — skipping (hrmax method will be used)");
            Ok(None)
        }
    }
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
        max.map(|m| format!(" max {m} km/h")).unwrap_or_default(),
        min.map(|m| format!(" min {m} km/h")).unwrap_or_default(),
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
        "Zone Hold target zone set to #{number} {} ({}).",
        zone.id, zone.name
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
        println!("Configured zones ({method_label}):");
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
        println!(
            "{marker} #{number} {:<14} id={:<16} {range:<16} max {max_speed:.1} km/h",
            zone.name, zone.id,
        );
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
    println!(
        "Added zone `{id}` ({name}). See it with `tm zone list`; select it with `tm zone target {id}`."
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
    println!("Updated zone `{id}`.");
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
    println!("Removed zone `{removed_id}` ({removed_name}).");
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

/// Prompt for a line of text. `default` is shown in `[brackets]` and kept on
/// an empty line; with no default, an empty line reprompts.
pub(crate) fn prompt_line(label: &str, default: Option<&str>) -> Result<String> {
    use std::io::Write;
    loop {
        match default {
            Some(d) => print!("{label} [{d}]: "),
            None => print!("{label}: "),
        }
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .with_context(|| format!("read {label} from stdin"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if let Some(d) = default {
                return Ok(d.to_string());
            }
            println!("{label} can't be empty");
            continue;
        }
        return Ok(trimmed.to_string());
    }
}

/// Prompt for a positive `f32`, keeping `default` (if any) on an empty line.
pub(crate) fn prompt_f32(label: &str, default: Option<f32>) -> Result<f32> {
    use std::io::Write;
    loop {
        match default {
            Some(d) => print!("{label} [{d}]: "),
            None => print!("{label}: "),
        }
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .with_context(|| format!("read {label} from stdin"))?;
        let trimmed = line.trim();
        if trimmed.is_empty()
            && let Some(d) = default
        {
            return Ok(d);
        }
        if let Ok(n) = trimmed.parse::<f32>()
            && n > 0.0
        {
            return Ok(n);
        }
        println!("please enter a positive number");
    }
}

/// Prompt for a positive `u16` (bpm values), same default handling as
/// [`prompt_f32`].
pub(crate) fn prompt_u16(label: &str, default: Option<u16>) -> Result<u16> {
    use std::io::Write;
    loop {
        match default {
            Some(d) => print!("{label} [{d}]: "),
            None => print!("{label}: "),
        }
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .with_context(|| format!("read {label} from stdin"))?;
        let trimmed = line.trim();
        if trimmed.is_empty()
            && let Some(d) = default
        {
            return Ok(d);
        }
        if let Ok(n) = trimmed.parse::<u16>()
            && n > 0
        {
            return Ok(n);
        }
        println!("please enter a positive whole number");
    }
}

/// Prompt for a zone's bpm bounds — either a percent-of-HRmax/HRR range or an
/// absolute bpm range. `default` pre-fills both the kind and the values when
/// editing an existing zone; `None` (adding a new zone) asks for everything.
pub(crate) fn prompt_zone_bounds(
    default: Option<zone_hold::ZoneBounds>,
) -> Result<zone_hold::ZoneBounds> {
    use std::io::Write;
    let default_kind = default.map(|b| match b {
        zone_hold::ZoneBounds::Percent { .. } => "percent",
        zone_hold::ZoneBounds::Absolute { .. } => "bpm",
    });
    loop {
        match default_kind {
            Some(k) => print!("Bounds — `percent` (of HRmax/HRR) or `bpm` (absolute) [{k}]: "),
            None => print!("Bounds — `percent` (of HRmax/HRR) or `bpm` (absolute): "),
        }
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("read bounds kind from stdin")?;
        let raw = line.trim().to_lowercase();
        let kind = if raw.is_empty() {
            match default_kind {
                Some(k) => k.to_string(),
                None => {
                    println!("please type `percent` or `bpm`");
                    continue;
                }
            }
        } else {
            raw
        };
        match kind.as_str() {
            "percent" | "%" => {
                let (dmin, dmax) = match default {
                    Some(zone_hold::ZoneBounds::Percent { min, max }) => (Some(min), Some(max)),
                    _ => (None, None),
                };
                let min = prompt_f32("Min percent", dmin)?;
                let max = prompt_f32("Max percent", dmax)?;
                if min >= max {
                    println!("min must be less than max");
                    continue;
                }
                return Ok(zone_hold::ZoneBounds::Percent { min, max });
            }
            "bpm" => {
                let (dmin, dmax) = match default {
                    Some(zone_hold::ZoneBounds::Absolute { min_bpm, max_bpm }) => {
                        (Some(min_bpm), Some(max_bpm))
                    }
                    _ => (None, None),
                };
                let min = prompt_u16("Min bpm", dmin)?;
                let max = prompt_u16("Max bpm", dmax)?;
                if min >= max {
                    println!("min must be less than max");
                    continue;
                }
                return Ok(zone_hold::ZoneBounds::Absolute {
                    min_bpm: min,
                    max_bpm: max,
                });
            }
            other => println!("unrecognised `{other}` — type `percent` or `bpm`"),
        }
    }
}

/// Prompt for an optional per-zone max-speed override. Empty line keeps
/// `default`; `none`/`-` explicitly clears it (only meaningful when editing);
/// an implausible number keeps `default` rather than erroring out on a typo.
pub(crate) fn prompt_optional_max_speed(default: Option<f32>) -> Result<Option<f32>> {
    use std::io::Write;
    match default {
        Some(d) => print!("Per-zone max speed override, km/h (`none` to clear) [{d}]: "),
        None => print!("Per-zone max speed override, km/h (optional — Enter to skip): "),
    }
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("read max speed from stdin")?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    if trimmed.eq_ignore_ascii_case("none") || trimmed == "-" {
        return Ok(None);
    }
    match trimmed.parse::<f32>() {
        Ok(n) if n > 0.0 => Ok(Some(n)),
        _ => {
            println!("not a plausible speed — keeping the previous value");
            Ok(default)
        }
    }
}

/// Prompt for a zone `id`, defaulting to `default_id` (typically a slug of
/// the name just entered) and rejecting a clash with any other configured
/// zone. `editing` is the id already owned by the zone being edited (exempt
/// from the clash check against itself) — `None` when adding a new zone.
pub(crate) fn prompt_zone_id(
    default_id: &str,
    existing: &[zone_hold::ZoneDef],
    editing: Option<&str>,
) -> Result<String> {
    use std::io::Write;
    loop {
        print!("Zone id [{default_id}]: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("read zone id from stdin")?;
        let trimmed = line.trim();
        let id = if trimmed.is_empty() {
            default_id.to_string()
        } else {
            trimmed.to_string()
        };
        if id.is_empty() {
            println!("id can't be empty");
            continue;
        }
        let clash = existing
            .iter()
            .any(|z| z.id.eq_ignore_ascii_case(&id) && Some(z.id.as_str()) != editing);
        if clash {
            println!("id `{id}` is already used by another zone — pick a different one");
            continue;
        }
        return Ok(id);
    }
}

/// `tm zone mode <band|center>` (задача 027, §Режимы).
pub(crate) fn zone_mode(tracking: &str) -> Result<()> {
    match tracking {
        "band" | "center" => {
            set_zone_hold_key("tracking", format!("\"{tracking}\""))?;
            println!("Zone Hold tracking mode set to `{tracking}`.");
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
    println!("Zone Hold: {}", if config.enabled { "on" } else { "off" });
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
            println!("  age {age}, method {method}, tracking {tracking}");
            match config.resolve_target_zone() {
                Some(resolved) => println!(
                    "  target zone #{} {} ({}): {}-{} bpm \u{2022} speed {:.1}-{:.1} km/h",
                    resolved.number,
                    resolved.id,
                    resolved.name,
                    resolved.low_bpm,
                    resolved.high_bpm,
                    config.min_speed_kmh,
                    resolved.effective_max_speed_kmh,
                ),
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
