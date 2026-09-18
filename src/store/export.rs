//! Read-only paging over `raw_samples` for the local JSON export (задача 068).
//! Typed rows only — the JSON wire contract lives in `commands::json`.
use anyhow::{Context, Result, ensure};
use rusqlite::params;

use super::Store;

/// Upper bound for one `tm samples` page: keeps a single JSON document bounded.
pub const SAMPLES_PAGE_MAX: usize = 10_000;

/// One `raw_samples` row as stored: wire-scale columns, `None` where the frame
/// did not carry the field. Unlike [`super::RawSample`] nothing is decoded or
/// dropped — an external consumer gets exactly what was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleExportRow {
    pub id: i64,
    pub session_id: i64,
    pub ts_ms: i64,
    pub speed_centikmh: Option<i64>,
    pub avg_speed_centikmh: Option<i64>,
    pub distance_m: Option<i64>,
    pub energy_kcal: Option<i64>,
    pub elapsed_s: Option<i64>,
    pub steps: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SamplePage {
    pub rows: Vec<SampleExportRow>,
    /// Exclusive cursor for the next page; equals the input cursor on an empty page.
    pub next_after_id: i64,
    pub has_more: bool,
}

impl Store {
    /// Rows with `id > after_id`, ordered by row id rather than `ts_ms`, so a
    /// wall-clock step back can never make a cursor skip rows. `has_more` comes
    /// from fetching one extra row in the same query.
    pub fn raw_samples_page(&self, after_id: i64, limit: usize) -> Result<SamplePage> {
        ensure!(after_id >= 0, "sample cursor must be >= 0, got {after_id}");
        ensure!(
            (1..=SAMPLES_PAGE_MAX).contains(&limit),
            "sample page limit must be 1..={SAMPLES_PAGE_MAX}, got {limit}"
        );
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, session_id, ts_ms, speed_centikmh, avg_speed_centikmh,
                        distance_m, energy_kcal, elapsed_s, steps
                 FROM raw_samples WHERE id > ?1 ORDER BY id LIMIT ?2",
            )
            .context("prepare raw_samples_page query")?;
        let mut rows = stmt
            .query_map(params![after_id, limit as i64 + 1], |r| {
                Ok(SampleExportRow {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    ts_ms: r.get(2)?,
                    speed_centikmh: r.get(3)?,
                    avg_speed_centikmh: r.get(4)?,
                    distance_m: r.get(5)?,
                    energy_kcal: r.get(6)?,
                    elapsed_s: r.get(7)?,
                    steps: r.get(8)?,
                })
            })
            .context("run raw_samples_page query")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("collect raw_samples_page rows")?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_after_id = rows.last().map_or(after_id, |row| row.id);
        Ok(SamplePage {
            rows,
            next_after_id,
            has_more,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_sample(store: &Store, session: i64, ts_ms: i64) {
        store
            .conn
            .execute(
                "INSERT INTO raw_samples (session_id, ts_ms, raw_frame) VALUES (?1, ?2, x'00')",
                params![session, ts_ms],
            )
            .unwrap();
    }

    #[test]
    fn pages_by_row_id_even_when_the_clock_moves_back() {
        let store = super::super::memory_store();
        let session = store.start_session().unwrap();
        for ts_ms in [2000, 1000, 3000] {
            insert_sample(&store, session, ts_ms);
        }

        let first = store.raw_samples_page(0, 2).unwrap();
        let times: Vec<_> = first.rows.iter().map(|r| r.ts_ms).collect();
        assert_eq!(times, [2000, 1000]);
        assert_eq!(first.rows[0].speed_centikmh, None);
        assert!(first.has_more);

        let second = store.raw_samples_page(first.next_after_id, 2).unwrap();
        assert_eq!(second.rows.len(), 1);
        assert_eq!(second.rows[0].ts_ms, 3000);
        assert!(!second.has_more);

        let empty = store.raw_samples_page(second.next_after_id, 2).unwrap();
        assert!(empty.rows.is_empty());
        assert_eq!(empty.next_after_id, second.next_after_id);
    }

    #[test]
    fn rejects_out_of_range_cursor_and_limit() {
        let store = super::super::memory_store();
        assert!(store.raw_samples_page(-1, 10).is_err());
        assert!(store.raw_samples_page(0, 0).is_err());
        assert!(store.raw_samples_page(0, SAMPLES_PAGE_MAX + 1).is_err());
    }

    #[test]
    fn readonly_open_never_creates_or_migrates() {
        let directory = std::env::temp_dir().join(format!("tm-export-{}", uuid::Uuid::new_v4()));
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
        let tables: i64 = reader
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);

        drop(reader);
        drop(writer);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
