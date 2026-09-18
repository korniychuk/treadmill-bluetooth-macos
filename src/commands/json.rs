//! Versioned local JSON exports (задача 068): `tm stats --json` and
//! `tm samples`. Read-only SQLite, no Bluetooth, no outbound client (ADR 0002).
//! Fields are mapped explicitly so internal struct refactors cannot leak into
//! the wire contract; stdout carries exactly one JSON document.
use anyhow::Result;
use serde_json::{Value, json};

use crate::config;
use crate::store::{DailyStats, SampleExportRow, SamplePage, Store, Workout};

/// Bump on any breaking change to the documents below.
const SCHEMA_VERSION: u32 = 1;

/// `tm samples --limit` default: a comfortable page well under the store cap.
pub(crate) const SAMPLES_PAGE_DEFAULT: u64 = 1000;

pub(crate) fn run_stats_json(all: bool) -> Result<()> {
    let Some(store) = Store::open_readonly()? else {
        return print_document(&stats_document(&[]));
    };
    let days = if all {
        store.all_stats()?
    } else {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        store.stats_for(&today)?.into_iter().collect()
    };
    let gap_minutes = config::load_workout_gap_minutes();
    let mut report = Vec::with_capacity(days.len());
    for day in days {
        let workouts = store.workouts_for(&day.date, gap_minutes)?;
        report.push((day, workouts));
    }
    print_document(&stats_document(&report))
}

pub(crate) fn run_samples_json(after_id: i64, limit: usize) -> Result<()> {
    let page = match Store::open_readonly()? {
        Some(store) => store.raw_samples_page(after_id, limit)?,
        None => SamplePage {
            rows: Vec::new(),
            next_after_id: after_id,
            has_more: false,
        },
    };
    print_document(&samples_document(&page))
}

fn stats_document(days: &[(DailyStats, Vec<Workout>)]) -> Value {
    let days: Vec<Value> = days
        .iter()
        .map(|(day, workouts)| {
            json!({
                "date": day.date,
                "steps": day.steps,
                "distance_m": day.distance_m,
                "walking_time_s": day.walking_time_s,
                "workouts": workouts.iter().map(workout_json).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({"schema_version": SCHEMA_VERSION, "days": days})
}

fn workout_json(w: &Workout) -> Value {
    json!({
        "id": w.id,
        "started_at": w.started_at,
        "ended_at": w.ended_at,
        "steps": w.steps,
        "distance_m": w.distance_m,
        "walking_time_s": w.walking_time_s,
    })
}

fn samples_document(page: &SamplePage) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "samples": page.rows.iter().map(sample_json).collect::<Vec<_>>(),
        "next_after_id": page.next_after_id,
        "has_more": page.has_more,
    })
}

fn sample_json(s: &SampleExportRow) -> Value {
    json!({
        "id": s.id,
        "session_id": s.session_id,
        "ts_ms": s.ts_ms,
        "speed_centikmh": s.speed_centikmh,
        "avg_speed_centikmh": s.avg_speed_centikmh,
        "distance_m": s.distance_m,
        "energy_kcal": s.energy_kcal,
        "elapsed_s": s.elapsed_s,
        "steps": s.steps,
    })
}

fn print_document(document: &Value) -> Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, document)?;
    writeln!(out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_document_is_versioned_with_exact_units() {
        assert_eq!(
            stats_document(&[]),
            json!({"schema_version": 1, "days": []})
        );
        let day = DailyStats {
            date: "2026-01-01".into(),
            distance_m: 1234,
            steps: 2345,
            walking_time_s: 678,
        };
        let workout = Workout {
            id: 7,
            date: "2026-01-01".into(),
            started_at: "2026-01-01T10:00:00+00:00".into(),
            ended_at: "2026-01-01T10:30:00+00:00".into(),
            distance_m: 1000,
            steps: 2000,
            walking_time_s: 600,
        };
        assert_eq!(
            stats_document(&[(day, vec![workout])])["days"][0],
            json!({"date": "2026-01-01", "steps": 2345, "distance_m": 1234,
                   "walking_time_s": 678, "workouts": [{"id": 7,
                   "started_at": "2026-01-01T10:00:00+00:00",
                   "ended_at": "2026-01-01T10:30:00+00:00",
                   "steps": 2000, "distance_m": 1000, "walking_time_s": 600}]})
        );
    }

    #[test]
    fn samples_document_keeps_nulls_and_cursor() {
        let page = SamplePage {
            rows: vec![SampleExportRow {
                id: 5,
                session_id: 1,
                ts_ms: 1000,
                speed_centikmh: Some(320),
                avg_speed_centikmh: None,
                distance_m: None,
                energy_kcal: None,
                elapsed_s: Some(60),
                steps: None,
            }],
            next_after_id: 5,
            has_more: true,
        };
        let doc = samples_document(&page);
        assert_eq!(doc["schema_version"], 1);
        assert_eq!(doc["samples"][0]["speed_centikmh"], 320);
        assert!(doc["samples"][0]["steps"].is_null());
        assert_eq!(doc["next_after_id"], 5);
        assert_eq!(doc["has_more"], true);
    }
}
