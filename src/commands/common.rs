//! Shared helpers used by two or more CLI commands.

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};

use crate::daemon;
use crate::store;
use crate::zone_hold;

/// Seconds form of [`daemon::WATCHDOG_STALE_THRESHOLD`] (задача 043 — single
/// source of truth; do not re-derive as an independent literal).
pub(crate) const WATCHDOG_STALE_THRESHOLD_S: i64 =
    daemon::WATCHDOG_STALE_THRESHOLD.as_secs() as i64;

/// How old a bpm (or belt-speed) reading may be before the widget/status stop
/// showing it (задачи 033/029/043). Deliberately *not*
/// [`WATCHDOG_STALE_THRESHOLD_S`], which is sized for "the daemon hung" (~120s)
/// — a pulse or speed frozen for two minutes is a lie. A worn strap / moving
/// belt notifies ~1/s and the daemon's HR silence window is 10s, so 15s covers
/// one missed cycle and nothing more.
pub(crate) const HR_STALE_THRESHOLD_S: i64 = 15;

/// Refuse repair commands while the daemon holds a live heartbeat (задача 044).
/// `recompute-segments` renumbers segment ids; a live daemon caching an open id
/// would then credit into a different historical row.
pub(crate) fn refuse_if_daemon_live(cmd: &str) -> Result<()> {
    if !daemon_process_alive() {
        return Ok(());
    }
    let store = store::Store::open()?;
    if let Some(status) = store.daemon_status()?
        && daemon_status_fresh(&status)
    {
        anyhow::bail!(
            "{cmd}: daemon is running with a fresh heartbeat — stop it first \
             (`launchctl kickstart -k gui/$(id -u)/com.korniychuk.treadmill-bluetooth-macos` \
             or `scripts/uninstall-daemon.sh`) so open segment ids cannot collide"
        );
    }
    Ok(())
}
pub(crate) fn describe_timestamp(rfc3339: &str) -> String {
    match DateTime::parse_from_rfc3339(rfc3339) {
        Ok(dt) => {
            let utc = dt.with_timezone(&Utc);
            format!(
                "{} ({})",
                utc.with_timezone(&Local).format("%Y-%m-%d %H:%M"),
                humanize_ago(Utc::now() - utc)
            )
        }
        Err(err) => {
            tracing::warn!(%err, rfc3339, "status: unparseable timestamp");
            "unknown".to_string()
        }
    }
}

pub(crate) fn format_local_time(rfc3339: &str) -> String {
    match DateTime::parse_from_rfc3339(rfc3339) {
        Ok(dt) => dt.with_timezone(&Local).format("%H:%M").to_string(),
        Err(_) => rfc3339.to_string(),
    }
}

pub(crate) fn humanize_ago(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Is the daemon LaunchAgent actually running right now (real PID), not just
/// present-but-stale in `daemon_status`? Shells out to `launchctl print`
/// rather than trusting the DB row alone — a dead process can leave a
/// perfectly plausible-looking last-known state behind (see docs/tasks/006,
/// задача B's explicit warning against trusting stale DB rows).
pub(crate) fn daemon_process_alive() -> bool {
    let uid = match std::process::Command::new("id").arg("-u").output() {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        Ok(output) => {
            tracing::warn!(code = ?output.status.code(), "status: `id -u` failed, assuming daemon not running");
            return false;
        }
        Err(err) => {
            tracing::warn!(%err, "status: failed to spawn `id -u`, assuming daemon not running");
            return false;
        }
    };

    let target = format!("gui/{uid}/com.korniychuk.treadmill-bluetooth-macos.daemon");
    match std::process::Command::new("launchctl")
        .args(["print", &target])
        .output()
    {
        Ok(output) if output.status.success() => {
            // `launchctl print` succeeds for a *loaded* service even if it
            // crashed and isn't currently running — only a real `pid = N`
            // line means it's actually alive right now.
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.trim_start().starts_with("pid = "))
        }
        Ok(_) => false, // not loaded at all
        Err(err) => {
            tracing::warn!(%err, "status: failed to spawn `launchctl print`, assuming daemon not running");
            false
        }
    }
}
/// Whether the daemon heartbeat (`daemon_status.updated_at`) is recent enough
/// to trust — an unparseable timestamp counts as not fresh (route to fallback).
pub(crate) fn daemon_status_fresh(status: &store::DaemonStatus) -> bool {
    match DateTime::parse_from_rfc3339(&status.updated_at) {
        Ok(updated_at) => {
            (Utc::now() - updated_at.with_timezone(&Utc)).num_seconds()
                <= WATCHDOG_STALE_THRESHOLD_S
        }
        Err(err) => {
            tracing::warn!(%err, updated_at = %status.updated_at, "control: unparseable daemon_status.updated_at — treating daemon as not holding the link");
            false
        }
    }
}
pub(crate) fn zone_hold_config_path() -> Result<std::path::PathBuf> {
    let path = zone_hold::config_path().context(
        "could not resolve the config path ($HOME unset) — set TREADMILL_CONFIG explicitly",
    )?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    Ok(path)
}
pub(crate) fn fmt_duration(seconds: i64) -> String {
    format!("{}m{:02}s", seconds / 60, seconds % 60)
}
