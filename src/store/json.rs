//! JSON contracts use explicit fields so internal struct refactors stay private.
use anyhow::{Result, ensure};
use serde_json::{Value, json};

use super::Store;

impl Store {
    pub(crate) fn stats_json(&self, all: bool, gap_minutes: i64) -> Result<Value> {
        let days = if all {
            self.all_stats()?
        } else {
            self.stats_for(&chrono::Local::now().format("%Y-%m-%d").to_string())?
                .into_iter()
                .collect()
        };
        let mut output = Vec::new();
        for day in days {
            let workouts: Vec<_> = self.workouts_for(&day.date, gap_minutes)?.into_iter().map(|w| json!({
                "id": w.id, "started_at": w.started_at, "ended_at": w.ended_at,
                "steps": w.steps, "distance_m": w.distance_m, "walking_time_s": w.walking_time_s,
            })).collect();
            output.push(json!({"date": day.date, "steps": day.steps,
                "distance_m": day.distance_m, "walking_time_s": day.walking_time_s,
                "workouts": workouts}));
        }
        Ok(json!({"schema_version": 1, "days": output}))
    }

    pub(crate) fn samples_json(&self, after_id: i64, limit: i64) -> Result<Value> {
        ensure!(
            after_id >= 0 && (1..=10000).contains(&limit),
            "invalid cursor or page limit"
        );
        let mut query = self.conn.prepare(
            "SELECT id,session_id,ts_ms,speed_centikmh,
            avg_speed_centikmh,distance_m,energy_kcal,elapsed_s,steps
            FROM raw_samples WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = query.query_map(rusqlite::params![after_id, limit + 1], |r| {
            Ok(json!({
                "id": r.get::<_,i64>(0)?, "session_id": r.get::<_,i64>(1)?, "ts_ms": r.get::<_,i64>(2)?,
                "speed_centikmh": r.get::<_,Option<i64>>(3)?, "avg_speed_centikmh": r.get::<_,Option<i64>>(4)?,
                "distance_m": r.get::<_,Option<i64>>(5)?, "energy_kcal": r.get::<_,Option<i64>>(6)?,
                "elapsed_s": r.get::<_,Option<i64>>(7)?, "steps": r.get::<_,Option<i64>>(8)?,
            }))
        })?;
        let mut samples = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = samples.len() > limit as usize;
        samples.truncate(limit as usize);
        let next = samples
            .last()
            .and_then(|v| v["id"].as_i64())
            .unwrap_or(after_id);
        Ok(
            json!({"schema_version": 1, "samples": samples, "next_after_id": next, "has_more": has_more}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_preserve_nulls_and_order_by_id_even_when_clock_moves_back() {
        let store = super::super::memory_store();
        let session = store.start_session().unwrap();
        for time in [2000, 1000, 3000] {
            store
                .conn
                .execute(
                    "INSERT INTO raw_samples (session_id,ts_ms,raw_frame) VALUES (?1,?2,x'00')",
                    rusqlite::params![session, time],
                )
                .unwrap();
        }
        let first = store.samples_json(0, 2).unwrap();
        assert_eq!(first["samples"][0]["ts_ms"], 2000);
        assert_eq!(first["samples"][1]["ts_ms"], 1000);
        assert!(first["samples"][0]["speed_centikmh"].is_null());
        assert_eq!(first["has_more"], true);
        let second = store
            .samples_json(first["next_after_id"].as_i64().unwrap(), 2)
            .unwrap();
        assert_eq!(second["samples"].as_array().unwrap().len(), 1);
        assert_eq!(second["samples"][0]["ts_ms"], 3000);
        assert_eq!(second["has_more"], false);
        let empty = store
            .samples_json(second["next_after_id"].as_i64().unwrap(), 2)
            .unwrap();
        assert_eq!(empty["samples"], json!([]));
        assert_eq!(empty["next_after_id"], second["next_after_id"]);
        assert!(store.samples_json(-1, 10).is_err());
        assert!(store.samples_json(0, 0).is_err());
        assert!(store.samples_json(0, 10001).is_err());
    }

    #[test]
    fn stats_are_versioned_and_keep_exact_units() {
        let store = super::super::memory_store();
        assert_eq!(
            store.stats_json(true, 15).unwrap(),
            json!({"schema_version":1,"days":[]})
        );
        store
            .conn
            .execute(
                "INSERT INTO daily_stats VALUES ('2026-01-01',1234,2345,678)",
                [],
            )
            .unwrap();
        let out = store.stats_json(true, 15).unwrap();
        assert_eq!(
            out["days"][0],
            json!({"date":"2026-01-01","distance_m":1234,
            "steps":2345,"walking_time_s":678,"workouts":[]})
        );
    }

    #[test]
    fn readonly_open_never_creates_or_migrates() {
        let directory = std::env::temp_dir().join(format!("tm-json-{}", uuid::Uuid::new_v4()));
        let path = directory.join("store.db");
        assert!(Store::open_readonly_at(&path).unwrap().is_none());
        assert!(!directory.exists());
        std::fs::create_dir(&directory).unwrap();
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer
            .execute("CREATE TABLE marker (id INTEGER)", [])
            .unwrap();
        let reader = Store::open_readonly_at(&path).unwrap().unwrap();
        assert!(
            reader
                .conn
                .execute("INSERT INTO marker VALUES (1)", [])
                .is_err()
        );
        assert_eq!(
            reader
                .conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        drop(reader);
        drop(writer);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
