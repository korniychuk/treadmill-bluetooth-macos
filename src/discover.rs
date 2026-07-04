//! Full GATT discovery dump for the connected treadmill.
//!
//! Phase-2 tooling for the reverse-engineering effort (docs/tasks/003): connect
//! to the FTMS treadmill, enumerate every service / characteristic / descriptor,
//! log them, and write a structured JSON snapshot into `docs/research/`.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use btleplug::api::{CharPropFlags, Peripheral as _};
use btleplug::platform::Peripheral;
use tracing::{info, warn};

/// Where the machine-readable GATT snapshot lands (repo-relative).
const SNAPSHOT_PATH: &str = "docs/research/gatt-snapshot.json";

/// Dump the full GATT database of an already-connected peripheral.
///
/// Also attempts a `read` on every readable characteristic — static values
/// (device name, feature bitmaps, supported speed/incline ranges) are gold for
/// protocol mapping.
pub async fn dump_gatt(peripheral: &Peripheral) -> Result<()> {
    let props = peripheral.properties().await.ok().flatten();
    let name = props
        .as_ref()
        .and_then(|p| p.local_name.clone())
        .unwrap_or_else(|| "<unknown>".to_string());
    info!(id = %peripheral.id(), %name, "dumping GATT database");

    let mut json = String::from("{\n");
    let _ = writeln!(json, "  \"device_name\": {:?},", name);
    let _ = writeln!(json, "  \"peripheral_id\": {:?},", peripheral.id().to_string());
    json.push_str("  \"services\": [\n");

    let services = peripheral.services();
    for (si, service) in services.iter().enumerate() {
        info!(service = %service.uuid, primary = service.primary, "service");
        let _ = writeln!(json, "    {{\n      \"uuid\": {:?},", service.uuid.to_string());
        let _ = writeln!(json, "      \"primary\": {},", service.primary);
        json.push_str("      \"characteristics\": [\n");

        for (ci, ch) in service.characteristics.iter().enumerate() {
            let props_str = format_props(ch.properties);
            // Read static values where allowed — feature bitmaps & ranges live here.
            let value = if ch.properties.contains(CharPropFlags::READ) {
                match peripheral.read(ch).await {
                    Ok(bytes) => Some(bytes),
                    Err(err) => {
                        warn!(char = %ch.uuid, %err, "read failed on readable characteristic");
                        None
                    }
                }
            } else {
                None
            };
            let hex = value.as_deref().map(to_hex);
            info!(char = %ch.uuid, props = %props_str, value = hex.as_deref().unwrap_or("-"), "characteristic");

            let _ = writeln!(json, "        {{\n          \"uuid\": {:?},", ch.uuid.to_string());
            let _ = writeln!(json, "          \"properties\": {:?},", props_str);
            match &hex {
                Some(h) => {
                    let _ = writeln!(json, "          \"value_hex\": {:?},", h);
                }
                None => json.push_str("          \"value_hex\": null,\n"),
            }
            json.push_str("          \"descriptors\": [");
            let descs: Vec<String> = ch
                .descriptors
                .iter()
                .map(|d| format!("{:?}", d.uuid.to_string()))
                .collect();
            json.push_str(&descs.join(", "));
            json.push_str("]\n        }");
            json.push_str(if ci + 1 < service.characteristics.len() { ",\n" } else { "\n" });
        }

        json.push_str("      ]\n    }");
        json.push_str(if si + 1 < services.len() { ",\n" } else { "\n" });
    }
    json.push_str("  ]\n}\n");

    std::fs::write(SNAPSHOT_PATH, &json)
        .with_context(|| format!("write GATT snapshot to {SNAPSHOT_PATH}"))?;
    info!(path = SNAPSHOT_PATH, "GATT snapshot written");

    Ok(())
}

fn format_props(flags: CharPropFlags) -> String {
    const ALL: [(CharPropFlags, &str); 8] = [
        (CharPropFlags::BROADCAST, "broadcast"),
        (CharPropFlags::READ, "read"),
        (CharPropFlags::WRITE_WITHOUT_RESPONSE, "write-no-rsp"),
        (CharPropFlags::WRITE, "write"),
        (CharPropFlags::NOTIFY, "notify"),
        (CharPropFlags::INDICATE, "indicate"),
        (CharPropFlags::AUTHENTICATED_SIGNED_WRITES, "signed-write"),
        (CharPropFlags::EXTENDED_PROPERTIES, "extended"),
    ];
    let parts: Vec<&str> = ALL
        .iter()
        .filter(|(f, _)| flags.contains(*f))
        .map(|(_, s)| *s)
        .collect();
    parts.join("|")
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}
