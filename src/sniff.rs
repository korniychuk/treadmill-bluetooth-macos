//! Promiscuous notification sniffer for protocol reverse engineering.
//!
//! Subscribes to every characteristic that supports notify or indicate and
//! dumps raw frames as hex with the source UUID. Used to observe what the
//! treadmill reports while the physical BT remote drives it (e.g. incline
//! buttons) — the vendor command format often mirrors these status frames.

use anyhow::{Context, Result};
use btleplug::api::{CharPropFlags, Peripheral as _};
use btleplug::platform::Peripheral;
use futures::StreamExt;
use tracing::{info, warn};

use crate::ftms;

/// Subscribe to every notify/indicate characteristic and log raw frames.
pub async fn sniff_all(peripheral: &Peripheral) -> Result<()> {
    let subscribable: Vec<_> = peripheral
        .characteristics()
        .into_iter()
        .filter(|c| {
            c.properties.contains(CharPropFlags::NOTIFY)
                || c.properties.contains(CharPropFlags::INDICATE)
        })
        .collect();

    for ch in &subscribable {
        match peripheral.subscribe(ch).await {
            Ok(()) => info!(char = %ch.uuid, "subscribed"),
            Err(err) => warn!(char = %ch.uuid, %err, "subscribe failed"),
        }
    }

    let mut notifications = peripheral
        .notifications()
        .await
        .context("open notification stream")?;
    info!("sniffing all notifications — drive the treadmill with the remote now");

    while let Some(n) = notifications.next().await {
        let hex: String = n.value.iter().map(|b| format!("{b:02x} ")).collect();
        // Treadmill Data is noisy (~2 Hz); annotate it so the interesting
        // vendor frames stand out, but keep logging everything.
        let label = if n.uuid == ftms::TREADMILL_DATA { "2acd(data)" } else { "frame" };
        info!(char = %n.uuid, bytes = hex.trim(), "{label}");
    }

    warn!("notification stream ended (device disconnected?)");
    Ok(())
}
