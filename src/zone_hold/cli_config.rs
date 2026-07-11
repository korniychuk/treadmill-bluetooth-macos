//! Zone Hold config file writers for `tm zone` CLI.

use std::path::Path;

use super::{ZoneBounds, ZoneDef};

/// Write (or update) the `[zone_hold]` table in the per-user config, preserving
/// every other section verbatim. Used by the `tm zone` CLI (on/setup/limits/
/// target/mode) so onboarding never clobbers goals/auto-pause the operator
/// already configured. `updates` are applied as string key/value TOML
/// fragments (already-formatted, e.g. `"age = 34"`) — simple line-based
/// upsert rather than a full TOML AST rewrite, which would risk reformatting
/// (and losing the comments in) the rest of a hand-edited file.
pub fn upsert_zone_hold_keys(path: &Path, updates: &[(&str, String)]) -> anyhow::Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();

    let section_start = lines.iter().position(|l| l.trim() == "[zone_hold]");
    let Some(section_start) = section_start else {
        // No existing section — append a fresh one with just these keys.
        if !existing.is_empty() && !existing.ends_with('\n') {
            lines.push(String::new());
        }
        lines.push("[zone_hold]".to_string());
        for (key, value) in updates {
            lines.push(format!("{key} = {value}"));
        }
        lines.push(String::new());
        crate::goals::write_atomic(path, lines.join("\n"))?;
        return Ok(());
    };

    let section_end = lines[section_start + 1..]
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .map(|offset| section_start + 1 + offset)
        .unwrap_or(lines.len());

    for (key, value) in updates {
        let prefix = format!("{key} =");
        let existing_line = lines[section_start + 1..section_end]
            .iter()
            .position(|l| l.trim_start().starts_with(&prefix));
        let new_line = format!("{key} = {value}");
        match existing_line {
            Some(offset) => lines[section_start + 1 + offset] = new_line,
            None => lines.insert(section_end, new_line),
        }
    }

    crate::goals::write_atomic(path, lines.join("\n") + "\n")?;
    Ok(())
}

/// Escape `"` and `\` for a TOML basic string — the zone `id`/`name` values
/// [`replace_zones`] writes are operator-typed free text, not guaranteed
/// TOML-safe as-is.
fn escape_toml_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Rewrite every `[[zone_hold.zones]]` block wholesale from `zones` — used by
/// `tm zone add/edit/remove` (задача 027 follow-up). Unlike
/// [`upsert_zone_hold_keys`]'s targeted key patch, this always regenerates
/// the whole zones section: an array-of-tables has no stable per-field
/// anchor to patch in place, and zones are edited as a unit (one CLI action
/// touches one zone, but the *file* always reflects the full current list).
/// The rest of the file (scalar `[zone_hold]` keys, comments, `goals` etc.)
/// is left untouched.
pub fn replace_zones(path: &Path, zones: &[ZoneDef]) -> anyhow::Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();

    // Strip every existing zone block: the `[[zone_hold.zones]]` header
    // through the line before the next top-level `[...]` header (or EOF).
    // Repeated because consecutive zone blocks each start a fresh header.
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "[[zone_hold.zones]]" {
            let mut end = i + 1;
            while end < lines.len() && !lines[end].trim_start().starts_with('[') {
                end += 1;
            }
            lines.drain(i..end);
        } else {
            i += 1;
        }
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    if !lines.is_empty() {
        lines.push(String::new());
    }

    for zone in zones {
        lines.push("[[zone_hold.zones]]".to_string());
        lines.push(format!("id = \"{}\"", escape_toml_string(&zone.id)));
        lines.push(format!("name = \"{}\"", escape_toml_string(&zone.name)));
        match zone.bounds {
            ZoneBounds::Percent { min, max } => {
                lines.push(format!("min_percent = {min}"));
                lines.push(format!("max_percent = {max}"));
            }
            ZoneBounds::Absolute { min_bpm, max_bpm } => {
                lines.push(format!("min_bpm = {min_bpm}"));
                lines.push(format!("max_bpm = {max_bpm}"));
            }
        }
        if let Some(max_speed) = zone.max_speed_kmh {
            lines.push(format!("max_speed = {max_speed}"));
        }
        lines.push(String::new());
    }

    crate::goals::write_atomic(path, lines.join("\n"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::config::parse_zone_hold_config;
    use super::super::{ZoneBounds, ZoneDef};
    use super::*;
    use crate::speed::CentiKmh;

    #[test]
    fn upsert_zone_hold_keys_appends_new_section() {
        let dir = std::env::temp_dir().join(format!("tm-zh-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "goals = [8000]\n").unwrap();

        upsert_zone_hold_keys(
            &path,
            &[("enabled", "true".to_string()), ("age", "34".to_string())],
        )
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("[zone_hold]"));
        assert!(written.contains("enabled = true"));
        assert!(written.contains("age = 34"));
        assert!(written.contains("goals = [8000]"));

        // A second upsert must update in place, not duplicate the key.
        upsert_zone_hold_keys(&path, &[("age", "40".to_string())]).unwrap();
        let updated = std::fs::read_to_string(&path).unwrap();
        assert_eq!(updated.matches("age =").count(), 1);
        assert!(updated.contains("age = 40"));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn replace_zones_round_trips_through_parse() {
        let dir = std::env::temp_dir().join(format!("tm-zh-test-rz-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "goals = [8000]\n\n[zone_hold]\nenabled = true\nage = 34\n",
        )
        .unwrap();

        let zones = vec![
            ZoneDef {
                id: "easy".to_string(),
                name: "Easy".to_string(),
                bounds: ZoneBounds::Percent {
                    min: 50.0,
                    max: 60.0,
                },
                max_speed_kmh: None,
            },
            ZoneDef {
                id: "hard".to_string(),
                name: "Hard \"quoted\"".to_string(),
                bounds: ZoneBounds::Absolute {
                    min_bpm: 140,
                    max_bpm: 160,
                },
                max_speed_kmh: Some(CentiKmh::from_wire(500)),
            },
        ];
        replace_zones(&path, &zones).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("goals = [8000]"));
        assert!(written.contains("enabled = true"));
        assert_eq!(written.matches("[[zone_hold.zones]]").count(), 2);

        let config = parse_zone_hold_config(&written);
        assert_eq!(config.zones.len(), 2);
        assert_eq!(config.zones[0].id, "easy");
        assert_eq!(config.zones[1].name, "Hard \"quoted\"");
        assert_eq!(
            config.zones[1].max_speed_kmh,
            Some(CentiKmh::from_wire(500))
        );

        // Replacing again must not leave the old blocks behind (regression:
        // the previous zones section, not just appended-to).
        replace_zones(&path, &zones[..1]).unwrap();
        let updated = parse_zone_hold_config(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(updated.zones.len(), 1);

        std::fs::remove_file(&path).ok();
    }
}
