//! One-shot diagnostic and reverse-engineering CLI commands.

use anyhow::{Context, Result, bail};
use btleplug::api::Peripheral as _;
use btleplug::platform::Adapter;
use futures::StreamExt;
use tokio::signal;
use tracing::info;

use crate::daemon;
use crate::discover;
use crate::fitshow;
use crate::hr;
use crate::notify;
use crate::scan;
use crate::sniff;

pub(crate) async fn run_connect(adapter: &Adapter) -> Result<()> {
    let peripheral = scan::connect_treadmill(adapter).await?;

    // Stop streaming on Ctrl-C so the peripheral is dropped (and disconnected)
    // cleanly instead of leaking the CoreBluetooth connection.
    tokio::select! {
        result = scan::stream_treadmill_data(&peripheral) => result?,
        _ = signal::ctrl_c() => info!("interrupted — disconnecting"),
    }

    Ok(())
}

/// Diagnostic: connect to an HR sensor and print live bpm until Ctrl-C
/// (задача 025). Opens BLE directly — fine even while the daemon is running,
/// since the H10 offers two simultaneous connection slots.
pub(crate) async fn run_hr(adapter: &Adapter) -> Result<()> {
    let peripheral = scan::connect_hr(adapter).await?;
    if !scan::subscribe_hr(&peripheral).await {
        bail!("Heart Rate Measurement characteristic (0x2A37) not found on this device");
    }
    match scan::read_hr_battery(&peripheral).await {
        Some(pct) => println!("battery: {pct}%"),
        None => println!("battery: unknown (read failed — see logs)"),
    }

    let mut notifications = peripheral
        .notifications()
        .await
        .context("open HR notification stream")?;

    tokio::select! {
        _ = async {
            while let Some(notification) = notifications.next().await {
                if notification.uuid != hr::HEART_RATE_MEASUREMENT {
                    continue;
                }
                if let Some(m) = hr::parse_hr_measurement(&notification.value) {
                    println!("{} bpm", m.bpm);
                }
            }
        } => {}
        _ = signal::ctrl_c() => info!("interrupted — disconnecting"),
    }

    scan::disconnect_best_effort(&peripheral).await;
    Ok(())
}

pub(crate) async fn run_discover(adapter: &Adapter) -> Result<()> {
    let peripheral = scan::connect_treadmill(adapter).await?;
    discover::dump_gatt(&peripheral).await
}

pub(crate) async fn run_sniff(adapter: &Adapter) -> Result<()> {
    let peripheral = scan::connect_treadmill(adapter).await?;
    tokio::select! {
        result = sniff::sniff_all(&peripheral) => result?,
        _ = signal::ctrl_c() => info!("interrupted — disconnecting"),
    }
    Ok(())
}

/// Run the presence-aware background daemon: scan → connect → stream →
/// reconnect forever, until interrupted (Ctrl-C or LaunchAgent stop).
pub(crate) async fn run_daemon(adapter: &Adapter) -> Result<()> {
    tokio::select! {
        result = daemon::run(adapter) => result?,
        _ = signal::ctrl_c() => info!("interrupted — shutting down daemon"),
    }
    Ok(())
}

/// Fire every toast once, spaced out so they render as separate banners
/// instead of collapsing into one Notification Center group.
pub(crate) fn run_notify_test() -> Result<()> {
    // Closures wrap the toasts whose signatures now take arguments (away/pause
    // duration, goal tier) so they still fit the uniform `fn()` smoke-test
    // table. Sample durations/goals are illustrative — this path never touches
    // BLE or the real presence state.
    let sample_away = std::time::Duration::from_secs(157);
    let toasts: [(&str, &dyn Fn()); 11] = [
        ("found", &notify::treadmill_found),
        ("lost", &notify::treadmill_lost),
        ("away", &notify::walker_away),
        (
            "resumed (from away, with duration)",
            &(|| notify::walker_resumed(Some(sample_away))),
        ),
        ("paused", &notify::treadmill_paused),
        (
            "auto-paused (idle belt)",
            &(|| notify::auto_paused(sample_away)),
        ),
        (
            "resumed (from pause, duration + speed restore)",
            &(|| {
                notify::treadmill_resumed(
                    Some(sample_away),
                    Some(notify::SpeedRestore {
                        from_kmh: 0.5,
                        to_kmh: 2.5,
                    }),
                )
            }),
        ),
        (
            "default speed applied (workout start)",
            &(|| notify::default_speed_applied(0.5, 2.5)),
        ),
        ("goal tier 1", &(|| notify::goal_reached(8000, 1))),
        ("goal tier 2", &(|| notify::goal_reached(10000, 2))),
        ("goal tier 3", &(|| notify::goal_reached(12000, 3))),
    ];
    for (label, send) in toasts {
        println!("sending: {label}");
        send();
        std::thread::sleep(std::time::Duration::from_millis(800));
    }
    Ok(())
}

pub(crate) async fn run_fitshow_probe(adapter: &Adapter) -> Result<()> {
    let peripheral = scan::connect_treadmill(adapter).await?;
    let fs = fitshow::FitShow::attach(&peripheral).await?;
    fs.probe_info().await?;
    Ok(())
}

pub(crate) async fn run_fitshow_set(adapter: &Adapter, kmh: f32, incline_level: u8) -> Result<()> {
    let peripheral = scan::connect_treadmill(adapter).await?;
    let fs = fitshow::FitShow::attach(&peripheral).await?;
    fs.set_speed_incline(kmh, incline_level).await?;
    Ok(())
}
