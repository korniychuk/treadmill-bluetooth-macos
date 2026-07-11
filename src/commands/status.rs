//! `status` and `doctor` CLI commands.

use anyhow::Result;
use chrono::{DateTime, Local, Utc};

use crate::commands::common::{
    HR_STALE_THRESHOLD_S, WATCHDOG_STALE_THRESHOLD_S, daemon_process_alive, describe_timestamp,
    highlight_config,
};
use crate::commands::stats::print_workout_line;
use crate::goals;
use crate::store;
use crate::widget::widget_hr_field;
use crate::zone_hold;

/// Liveness matrix for one-shot diagnosis (задача 038). Read-only: SQLite +
/// config + launchctl — never opens BLE.
pub(crate) fn run_doctor() -> Result<()> {
    let store = store::Store::open()?;
    let status = store.daemon_status()?;
    let daemon_alive = daemon_process_alive();
    let zone_enabled = zone_hold::load_zone_hold_config().enabled;
    let now_ms = Utc::now().timestamp_millis();
    let report = format_doctor_report(
        daemon_alive,
        status.as_ref(),
        zone_enabled,
        now_ms,
        WATCHDOG_STALE_THRESHOLD_S,
        HR_STALE_THRESHOLD_S,
    );
    print!("{report}");
    Ok(())
}

/// Pure doctor text (задача 038). Ages use wall-clock `now_ms` (Unix millis)
/// so unit tests inject a fixed clock. WARN lines are prefixed `WARN:` for grepping.
pub(crate) fn format_doctor_report(
    daemon_alive: bool,
    status: Option<&store::DaemonStatus>,
    zone_config_enabled: bool,
    now_ms: i64,
    watchdog_stale_s: i64,
    hr_stale_s: i64,
) -> String {
    let mut out = String::new();
    out.push_str("daemon\n");
    out.push_str(&format!(
        "  process:          {}\n",
        if daemon_alive { "alive" } else { "dead" }
    ));
    match status {
        None => {
            out.push_str("  heartbeat age:    n/a (never recorded)\n");
            out.push_str("  power:            n/a\n");
            out.push_str("\ntreadmill liveness\n");
            out.push_str("  connected flag:   n/a\n");
            out.push_str("  last 0x2ACD age:  n/a\n");
            out.push_str("  presence:         n/a\n");
            out.push_str("\nhr liveness\n");
            out.push_str("  hr_connected:     n/a\n");
            out.push_str("  last HR frame age:n/a\n");
            out.push_str("  last bpm:         n/a\n");
            out.push_str("  battery:          n/a\n");
            out.push_str("  contact (inferred): n/a\n");
            out.push_str("\nzone hold\n");
            out.push_str(&format!(
                "  config enabled:   {}\n",
                highlight_config(zone_config_enabled)
            ));
            out.push_str("  phase snapshot:   n/a\n");
            out.push_str("  active flag:      n/a\n");
            out.push_str("\nlegend: loop=process+heartbeat · treadmill=connected+last_speed_ts · hr=hr_connected+last_bpm_ts · config=enabled vs phase\n");
            return out;
        }
        Some(s) => {
            let hb_age = age_secs_rfc3339(&s.updated_at, now_ms);
            match hb_age {
                Some(age) => {
                    out.push_str(&format!("  heartbeat age:    {age}s (updated_at)\n"));
                    if daemon_alive && age > watchdog_stale_s {
                        out.push_str(&format!(
                            "  WARN: heartbeat older than {watchdog_stale_s}s while process alive — possible hang\n"
                        ));
                    }
                }
                None => out.push_str("  heartbeat age:    n/a (unparseable updated_at)\n"),
            }
            out.push_str(&format!("  power:            {}\n", s.power_mode));

            out.push_str("\ntreadmill liveness\n");
            out.push_str(&format!("  connected flag:   {}\n", s.connected));
            match (s.last_speed_ts, s.last_speed_kmh) {
                (Some(ts), _) => {
                    let age = (now_ms - ts) / 1000;
                    out.push_str(&format!(
                        "  last 0x2ACD age:  {age}s (from last_speed_ts)\n"
                    ));
                    if s.connected && age > hr_stale_s * 2 {
                        // 2× sample freshness: belt telem ~1/s; long silence while
                        // `connected` is the 031-class symptom.
                        out.push_str(
                            "  WARN: connected=true but last belt sample is stale — possible stuck connected\n",
                        );
                    }
                }
                (None, _) => out.push_str("  last 0x2ACD age:  n/a\n"),
            }
            out.push_str(&format!(
                "  presence:         {}\n",
                s.presence_state.as_deref().unwrap_or("n/a")
            ));

            out.push_str("\nhr liveness\n");
            out.push_str(&format!("  hr_connected:     {}\n", s.hr_connected));
            let hr_age = s.last_bpm_ts.map(|ts| (now_ms - ts) / 1000);
            match hr_age {
                Some(age) => out.push_str(&format!("  last HR frame age:{age}s (last_bpm_ts)\n")),
                None => out.push_str("  last HR frame age:n/a\n"),
            }
            match s.last_bpm {
                Some(b) => out.push_str(&format!("  last bpm:         {b}\n")),
                None => out.push_str("  last bpm:         n/a\n"),
            }
            match s.hr_battery_pct {
                Some(p) => out.push_str(&format!("  battery:          {p}%\n")),
                None => out.push_str("  battery:          n/a\n"),
            }
            let contact = match (s.hr_connected, hr_age) {
                (true, Some(age)) if age <= hr_stale_s => "live",
                (true, Some(_)) => "stale",
                (true, None) => "stale",
                (false, _) => "no-link",
            };
            out.push_str(&format!("  contact (inferred): {contact}\n"));
            if s.hr_connected
                && let Some(age) = hr_age
                && age > hr_stale_s
            {
                out.push_str(
                    "  WARN: hr_connected=true but last bpm is stale — link may be silent\n",
                );
            }

            out.push_str("\nzone hold\n");
            out.push_str(&format!(
                "  config enabled:   {}\n",
                highlight_config(zone_config_enabled)
            ));
            let phase = s.zone_hold_phase.as_deref().unwrap_or("n/a");
            out.push_str(&format!("  phase snapshot:   {phase}\n"));
            out.push_str(&format!("  active flag:      {}\n", s.zone_hold_active));
            let phase_off = phase == "off" || phase == "n/a";
            if !zone_config_enabled && (s.zone_hold_active || !phase_off) {
                out.push_str(
                    "  WARN: config enabled=false but phase/active still engaged (032-class)\n",
                );
            }
            if zone_config_enabled
                && phase_off
                && s.presence_state.as_deref() == Some("Walking")
                && !s.zone_hold_active
            {
                out.push_str(
                    "  note: enabled=true, walking, phase off — controller not engaged yet\n",
                );
            }
        }
    }

    out.push_str(
        "\nlegend: loop=process+heartbeat · treadmill=connected+last_speed_ts · \
         hr=hr_connected+last_bpm_ts · config=enabled vs phase\n",
    );
    out
}

pub(crate) fn age_secs_rfc3339(rfc3339: &str, now_ms: i64) -> Option<i64> {
    let dt = DateTime::parse_from_rfc3339(rfc3339).ok()?;
    let then_ms = dt.with_timezone(&Utc).timestamp_millis();
    Some((now_ms - then_ms) / 1000)
}
/// Print daemon/treadmill/power state and today's workouts, reading only
/// SQLite (`daemon_status` + `activity_segments`) and `launchctl` — never
/// touches the BLE adapter, so it cannot contend with a running daemon for it.
pub(crate) fn run_status() -> Result<()> {
    let store = store::Store::open()?;
    let status = store.daemon_status()?;
    let daemon_alive = daemon_process_alive();

    println!(
        "daemon process: {}",
        if daemon_alive { "alive" } else { "NOT running" }
    );

    match &status {
        None => {
            println!("daemon status: never recorded (fresh install, or the daemon has never run)")
        }
        Some(status) => {
            if status.connected {
                let presence = status.presence_state.as_deref().unwrap_or("Unknown");
                let since = status
                    .last_connected_at
                    .as_deref()
                    .map(describe_timestamp)
                    .unwrap_or_else(|| "unknown".to_string());
                println!("treadmill: connected, presence = {presence} (since {since})");
            } else {
                let ago = status
                    .last_disconnected_at
                    .as_deref()
                    .map(describe_timestamp)
                    .unwrap_or_else(|| "never connected".to_string());
                println!("treadmill: not connected (last seen {ago})");
            }

            // Heart-rate line (задача 025) — mirrors the freshness gate the
            // widget uses ([`widget_hr_field`]) so `status` never shows a
            // frozen bpm from a sensor that's actually been removed. Battery
            // (задача 026) is appended when known — it's read independently
            // of bpm freshness, so it can be present even right after connect.
            if status.hr_connected && !widget_hr_field(status).is_empty() {
                let battery = status
                    .hr_battery_pct
                    .map(|pct| format!(", battery {pct}%"))
                    .unwrap_or_default();
                println!(
                    "heart rate: sensor connected, {} bpm{battery}",
                    status.last_bpm.unwrap_or(0)
                );
            } else {
                println!("heart rate: no sensor");
            }

            // Zone Hold line (задача 027) — only printed once the mode is
            // configured (age set), same "only show what's actually loaded"
            // stance as the config line below.
            let zh_config = zone_hold::load_zone_hold_config();
            if zh_config.enabled {
                if status.zone_hold_active {
                    // Live snapshot — phase/target bpm are runtime state, not
                    // config, so nothing here is cyan (задача 057).
                    let phase = status.zone_hold_phase.as_deref().unwrap_or("?");
                    let range = match (status.zone_hold_target_lo, status.zone_hold_target_hi) {
                        (Some(lo), Some(hi)) => format!("{lo}-{hi} bpm"),
                        _ => "? bpm".to_string(),
                    };
                    println!("zone hold: active, phase {phase}, target {range}");
                } else {
                    // `on`/`off` mirror the config `enabled` flag → cyan.
                    println!("zone hold: {} (not currently engaged)", highlight_config("on"));
                }
            } else {
                println!("zone hold: {}", highlight_config("off"));
            }

            let mode_desc = match status.power_mode.as_str() {
                "ac_scanning" => "on AC power, actively scanning",
                "battery_idle" => "on battery, idling (scanning paused to save power)",
                other => other,
            };
            println!(
                "power mode: {mode_desc}, since {}",
                describe_timestamp(&status.power_mode_since)
            );
            if status.power_mode == "battery_idle" {
                println!(
                    "  exits battery-idle immediately on: AC power restored, or system wake \
                     (event-driven power hooks, no polling delay — see docs/tasks/006, задача A)"
                );
            }

            // Config the daemon currently holds in memory (задача 022): answers
            // "what's loaded right now" and "when did it last read the file".
            // Only printed once a 022-aware daemon has written the snapshot
            // (older rows leave these columns NULL).
            if let Some(loaded_at) = &status.config_loaded_at {
                // goals / auto-pause / workout-gap are all config values → cyan
                // (задача 057); the read timestamp is live, so it stays plain.
                let goals_desc = highlight_config(
                    status
                        .config_goals
                        .as_deref()
                        .map(format_goal_list)
                        .unwrap_or_else(|| "—".to_string()),
                );
                let auto_pause = highlight_config(match status.config_auto_pause_secs {
                    Some(secs) => format_secs_short(secs),
                    None => "off".to_string(),
                });
                println!(
                    "config (in daemon): goals {goals_desc} · auto-pause {auto_pause} · read {}",
                    describe_timestamp(loaded_at)
                );
                // The workout-gap is read-time (задача 014) — the CLI resolves it
                // itself, the daemon does not hold it; shown here for completeness.
                println!(
                    "  workout gap: {} (read-time, applied when stats are read)",
                    highlight_config(format!("{}m", goals::load_workout_gap_minutes()))
                );
            }

            match DateTime::parse_from_rfc3339(&status.updated_at) {
                Ok(updated_at) => {
                    let stale_s = (Utc::now() - updated_at.with_timezone(&Utc)).num_seconds();
                    if daemon_alive && stale_s > WATCHDOG_STALE_THRESHOLD_S {
                        println!(
                            "  WARNING: daemon_status last updated {stale_s}s ago (> {WATCHDOG_STALE_THRESHOLD_S}s \
                             threshold) while the process is alive — possible silent hang, see docs/tasks/006, задача D"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(%err, updated_at = %status.updated_at, "status: unparseable daemon_status.updated_at")
                }
            }
        }
    }

    println!();
    println!("today's workouts:");
    let today = Local::now().format("%Y-%m-%d").to_string();
    let workouts = store.workouts_for(&today, goals::load_workout_gap_minutes())?;
    if workouts.is_empty() {
        println!("  (none yet today)");
    } else {
        let last_id = workouts.last().map(|w| w.id);
        let in_progress = status
            .as_ref()
            .is_some_and(|s| s.connected && s.presence_state.as_deref() == Some("Walking"));
        for (i, workout) in workouts.iter().enumerate() {
            let marker = if in_progress && Some(workout.id) == last_id {
                " [in progress]"
            } else {
                ""
            };
            print_workout_line(&store, i + 1, workout, marker);
        }
    }

    Ok(())
}
/// `now (Xm ago)`-style rendering of an RFC3339 timestamp in local time.
/// Render a comma-joined goal list ("8500,10750,13000") for `status`
/// ("8500 / 10750 / 13000"). Kept as the stored CSV in `daemon_status` so the
/// daemon does not couple to a display format (задача 022).
pub(crate) fn format_goal_list(csv: &str) -> String {
    csv.split(',').collect::<Vec<_>>().join(" / ")
}

/// Compact duration for the config line (задача 022): whole minutes as `5m`,
/// anything else as raw seconds (`90s`). Auto-pause is always whole minutes, so
/// the seconds branch is just a defensive fallback.
pub(crate) fn format_secs_short(secs: i64) -> String {
    if secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_report_flags_stale_heartbeat_and_zone_mismatch() {
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        let status = store::DaemonStatus {
            connected: true,
            presence_state: Some("Walking".into()),
            updated_at: (now - chrono::Duration::seconds(200)).to_rfc3339(),
            power_mode: "ac_scanning".into(),
            hr_connected: true,
            last_bpm: Some(111),
            last_bpm_ts: Some(now_ms - 60_000),
            last_speed_kmh: Some(3.0),
            last_speed_ts: Some(now_ms - 1_000),
            zone_hold_active: true,
            zone_hold_phase: Some("ramp".into()),
            ..Default::default()
        };
        let report = format_doctor_report(true, Some(&status), false, now_ms, 120, 15);
        assert!(report.contains("WARN: heartbeat older than 120s"));
        assert!(report.contains("WARN: hr_connected=true but last bpm is stale"));
        assert!(report.contains("WARN: config enabled=false but phase/active still engaged"));
        assert!(report.contains("contact (inferred): stale"));
    }

    #[test]
    fn doctor_report_handles_missing_status() {
        let report = format_doctor_report(false, None, false, 0, 120, 15);
        assert!(report.contains("process:          dead"));
        assert!(report.contains("never recorded"));
    }
}
