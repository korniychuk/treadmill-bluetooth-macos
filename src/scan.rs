//! BLE discovery and connection over CoreBluetooth (via `btleplug`).
//!
//! On macOS `btleplug` talks to CoreBluetooth, which requires the host process
//! to have Bluetooth permission (granted on first run). There are no numeric
//! addresses on macOS — peripherals are identified by an opaque system UUID.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::ftms;

/// How long to scan before giving up on finding a treadmill.
const SCAN_TIMEOUT: Duration = Duration::from_secs(15);

/// Acquire the first available Bluetooth adapter.
pub async fn first_adapter() -> Result<Adapter> {
    let manager = Manager::new().await.context("init CoreBluetooth manager")?;
    let adapters = manager.adapters().await.context("enumerate adapters")?;
    adapters
        .into_iter()
        .next()
        .context("no Bluetooth adapter found — is Bluetooth enabled?")
}

/// Scan and log every discovered peripheral, annotating likely treadmills.
///
/// Intended as a diagnostic pass while the Yesoul protocol is still being
/// reverse engineered — run it, walk the treadmill through its states, and
/// watch what appears.
pub async fn scan_and_list(adapter: &Adapter) -> Result<()> {
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("start BLE scan")?;
    info!(timeout_s = SCAN_TIMEOUT.as_secs(), "scanning for BLE devices");
    sleep(SCAN_TIMEOUT).await;

    let peripherals = adapter.peripherals().await.context("list peripherals")?;
    if peripherals.is_empty() {
        warn!("no BLE peripherals discovered");
        return Ok(());
    }

    for peripheral in peripherals {
        let props = peripheral.properties().await.ok().flatten();
        let name = props
            .as_ref()
            .and_then(|p| p.local_name.clone())
            .unwrap_or_else(|| "<unknown>".to_string());
        let has_ftms = props
            .as_ref()
            .map(|p| p.services.contains(&ftms::FITNESS_MACHINE_SERVICE))
            .unwrap_or(false);
        // Advertisement details help identify nameless devices (e.g. a vendor
        // remote): manufacturer data carries the company id, services hint at
        // the device class.
        let manufacturer = props
            .as_ref()
            .map(|p| {
                p.manufacturer_data
                    .iter()
                    .map(|(id, data)| {
                        let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
                        format!("{id:#06x}:{hex}")
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let services = props
            .as_ref()
            .map(|p| {
                p.services
                    .iter()
                    .map(|u| u.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let rssi = props.as_ref().and_then(|p| p.rssi);
        info!(id = %peripheral.id(), %name, ftms = has_ftms, ?rssi, %manufacturer, %services, "discovered");
    }

    Ok(())
}

/// Connect to a specific peripheral by its (opaque macOS) UUID string.
///
/// Used for probing devices that do not advertise FTMS (e.g. identifying an
/// unknown advertiser suspected to be the treadmill in a vendor-specific mode).
pub async fn connect_by_id(adapter: &Adapter, id: &str) -> Result<Peripheral> {
    adapter
        .start_scan(ScanFilter::default())
        .await
        .context("start BLE scan")?;

    let deadline = tokio::time::Instant::now() + SCAN_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        for peripheral in adapter.peripherals().await.context("list peripherals")? {
            if peripheral.id().to_string() == id {
                info!(id = %peripheral.id(), "connecting by id");
                peripheral.connect().await.context("connect")?;
                peripheral
                    .discover_services()
                    .await
                    .context("discover services")?;
                return Ok(peripheral);
            }
        }
        sleep(Duration::from_millis(500)).await;
    }

    bail!("peripheral {id} not seen within {:?}", SCAN_TIMEOUT)
}

/// Find and connect to the first peripheral advertising the FTMS service.
pub async fn connect_treadmill(adapter: &Adapter) -> Result<Peripheral> {
    adapter
        .start_scan(ScanFilter {
            services: vec![ftms::FITNESS_MACHINE_SERVICE],
        })
        .await
        .context("start filtered BLE scan")?;

    let deadline = tokio::time::Instant::now() + SCAN_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        for peripheral in adapter.peripherals().await.context("list peripherals")? {
            if is_treadmill(&peripheral).await {
                info!(id = %peripheral.id(), "connecting to treadmill");
                peripheral.connect().await.context("connect")?;
                peripheral
                    .discover_services()
                    .await
                    .context("discover services")?;
                return Ok(peripheral);
            }
        }
        sleep(Duration::from_millis(500)).await;
    }

    bail!("no FTMS treadmill found within {:?}", SCAN_TIMEOUT)
}

async fn is_treadmill(peripheral: &Peripheral) -> bool {
    peripheral
        .properties()
        .await
        .ok()
        .flatten()
        .map(|p| p.services.contains(&ftms::FITNESS_MACHINE_SERVICE))
        .unwrap_or(false)
}

/// Subscribe to Treadmill Data notifications and log decoded frames until the
/// stream ends (device disconnects) or the task is cancelled.
pub async fn stream_treadmill_data(peripheral: &Peripheral) -> Result<()> {
    let characteristic = peripheral
        .characteristics()
        .into_iter()
        .find(|c| c.uuid == ftms::TREADMILL_DATA)
        .context("Treadmill Data characteristic (0x2ACD) not found")?;

    peripheral
        .subscribe(&characteristic)
        .await
        .context("subscribe to Treadmill Data")?;
    info!("subscribed to Treadmill Data notifications");

    let mut notifications = peripheral
        .notifications()
        .await
        .context("open notification stream")?;

    while let Some(notification) = notifications.next().await {
        if notification.uuid != ftms::TREADMILL_DATA {
            continue;
        }
        match ftms::parse_treadmill_data(&notification.value) {
            Some(data) => info!(?data, "treadmill data"),
            None => warn!(bytes = ?notification.value, "undecodable treadmill frame"),
        }
    }

    warn!("notification stream ended (device disconnected?)");
    Ok(())
}
