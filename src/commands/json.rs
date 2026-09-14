//! Versioned local JSON output. No Bluetooth or outbound client.
use anyhow::Result;
use serde_json::{Value, json};

use crate::{config, store::Store};

fn print_document(document: &Value) -> Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, document)?;
    writeln!(out)?;
    Ok(())
}

pub(crate) fn run_stats_json(all: bool) -> Result<()> {
    let document = match Store::open_readonly()? {
        Some(store) => store.stats_json(all, config::load_workout_gap_minutes())?,
        None => json!({"schema_version": 1, "days": []}),
    };
    print_document(&document)
}

pub(crate) fn run_samples_json(after_id: i64, limit: i64) -> Result<()> {
    let document = match Store::open_readonly()? {
        Some(store) => store.samples_json(after_id, limit)?,
        None => {
            json!({"schema_version": 1, "samples": [], "next_after_id": after_id, "has_more": false})
        }
    };
    print_document(&document)
}
