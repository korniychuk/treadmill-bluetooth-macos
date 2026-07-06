//! Workout logger — appends decoded telemetry samples as JSON Lines.
//!
//! One file per `connect` session under `workouts/`, one JSON object per
//! sample. JSONL keeps the writer trivial (append-only, crash-safe per line)
//! and the data trivially greppable / importable.

use std::fs::{File, create_dir_all};
use std::io::{BufWriter, Write as _};
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use tracing::info;

use crate::ftms::TreadmillData;

/// Directory (repo-relative) where workout logs accumulate.
const WORKOUTS_DIR: &str = "workouts";

pub struct WorkoutLogger {
    writer: BufWriter<File>,
    path: PathBuf,
    samples: u64,
}

impl WorkoutLogger {
    /// Create `workouts/workout-<UTC timestamp>.jsonl` for this session.
    pub fn create() -> Result<Self> {
        create_dir_all(WORKOUTS_DIR).context("create workouts dir")?;
        let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
        let path = PathBuf::from(format!("{WORKOUTS_DIR}/workout-{stamp}.jsonl"));
        let file = File::create(&path).with_context(|| format!("create {}", path.display()))?;
        info!(path = %path.display(), "workout log started");
        Ok(Self {
            writer: BufWriter::new(file),
            path,
            samples: 0,
        })
    }

    /// Append one telemetry sample with a wall-clock timestamp.
    pub fn log(&mut self, data: &TreadmillData) -> Result<()> {
        let ts = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        // Hand-rolled JSON keeps the dependency surface small; all fields are
        // numeric or fixed-format strings, so no escaping is needed.
        let mut line = format!("{{\"ts\":\"{ts}\"");
        push_num(&mut line, "speed_kmh", data.speed_kmh);
        push_num(&mut line, "avg_speed_kmh", data.avg_speed_kmh);
        push_num(&mut line, "incline_percent", data.incline_percent);
        push_num(&mut line, "distance_m", data.total_distance_m);
        push_num(&mut line, "energy_kcal", data.total_energy_kcal);
        push_num(&mut line, "elapsed_s", data.elapsed_s);
        push_num(&mut line, "steps", data.steps);
        line.push('}');

        writeln!(self.writer, "{line}").context("append workout sample")?;
        self.samples += 1;
        // Flush every sample: ~2 Hz write rate is negligible, and a crash or
        // Ctrl-C must not lose the tail of a workout.
        self.writer.flush().context("flush workout log")?;
        Ok(())
    }

    /// Log a session summary on shutdown.
    pub fn finish(&mut self) {
        info!(path = %self.path.display(), samples = self.samples, "workout log closed");
    }
}

fn push_num<T: std::fmt::Display>(line: &mut String, key: &str, value: Option<T>) {
    if let Some(v) = value {
        line.push_str(&format!(",\"{key}\":{v}"));
    }
}
