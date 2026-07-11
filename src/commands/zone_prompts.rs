//! Interactive stdin prompts for `tm zone` (задача 050).

use anyhow::{Context, Result};

use crate::zone_hold;

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
