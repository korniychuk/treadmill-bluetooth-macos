//! BLE discovery and connection over CoreBluetooth (via `btleplug`).
//!
//! On macOS `btleplug` talks to CoreBluetooth, which requires the host process
//! to have Bluetooth permission (granted on first run). There are no numeric
//! addresses on macOS — peripherals are identified by an opaque system UUID.

use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use tokio::time::{sleep, timeout};
use tracing::{info, warn};

use crate::ftms;
use crate::hr;
use crate::logger::WorkoutLogger;

/// How long to scan before giving up on finding a treadmill.
const SCAN_TIMEOUT: Duration = Duration::from_secs(15);

/// Marker context for a failed `start_scan` so callers can classify the
/// failure without string-matching (backlog 009: a wedged `CBCentralManager`
/// fails scan starts instantly and forever).
#[derive(Debug)]
pub struct ScanStartFailed;

impl fmt::Display for ScanStartFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("start filtered BLE scan")
    }
}

impl std::error::Error for ScanStartFailed {}

/// How long to wait on a single CoreBluetooth call (`connect()`,
/// `discover_services()`, `subscribe()`, `notifications()`, `disconnect()`)
/// before giving up. These calls have no built-in bound in `btleplug` — two
/// live incidents (2026-07-04/05, see `docs/tasks/007-...md`) saw the daemon
/// hang for hours inside one of them (`disconnect()` on a hard-powered-off
/// treadmill), with no scan and no error, until an external restart. Wrapping
/// every such call in a timeout turns a silent hang into a normal `Err` the
/// caller's retry loop can handle.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

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
    info!(
        timeout_s = SCAN_TIMEOUT.as_secs(),
        "scanning for BLE devices"
    );
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
                timeout(CONNECT_TIMEOUT, peripheral.connect())
                    .await
                    .context("connect timed out (possible CoreBluetooth hang)")?
                    .context("connect")?;
                timeout(CONNECT_TIMEOUT, peripheral.discover_services())
                    .await
                    .context("discover services timed out (possible CoreBluetooth hang)")?
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
        .context(ScanStartFailed)?;

    let deadline = tokio::time::Instant::now() + SCAN_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        for peripheral in adapter.peripherals().await.context("list peripherals")? {
            if is_treadmill(&peripheral).await {
                info!(id = %peripheral.id(), "connecting to treadmill");
                timeout(CONNECT_TIMEOUT, peripheral.connect())
                    .await
                    .context("connect timed out (possible CoreBluetooth hang)")?
                    .context("connect")?;
                timeout(CONNECT_TIMEOUT, peripheral.discover_services())
                    .await
                    .context("discover services timed out (possible CoreBluetooth hang)")?
                    .context("discover services")?;
                return Ok(peripheral);
            }
        }
        sleep(Duration::from_millis(500)).await;
    }

    bail!("no FTMS treadmill found within {:?}", SCAN_TIMEOUT)
}

/// Best-effort disconnect bounded by [`CONNECT_TIMEOUT`]: on a hard
/// power-off CoreBluetooth may never complete the disconnect (observed live
/// hanging for ~10 hours), so a timeout here only gets logged — the caller
/// proceeds to rescan either way, and CoreBluetooth finishes the teardown on
/// its own whenever the peripheral reappears.
pub async fn disconnect_best_effort(peripheral: &Peripheral) {
    match timeout(CONNECT_TIMEOUT, peripheral.disconnect()).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => warn!(%err, "disconnect failed — continuing to rescan"),
        Err(_) => warn!(
            timeout_s = CONNECT_TIMEOUT.as_secs(),
            "disconnect timed out (possible CoreBluetooth hang) — continuing to rescan"
        ),
    }
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

/// Find and connect to the first peripheral advertising the Heart Rate
/// Service (`0x180D`) — e.g. a Polar H10 chest strap (задача 025). A separate
/// scan pass from [`connect_treadmill`]: CoreBluetooth filters advertisements
/// by service UUID, so the two device classes can't share one filtered scan.
pub async fn connect_hr(adapter: &Adapter) -> Result<Peripheral> {
    adapter
        .start_scan(ScanFilter {
            services: vec![hr::HEART_RATE_SERVICE],
        })
        .await
        // Same typed marker as the treadmill path so a wedged adapter is
        // classifiable; the daemon's recycle streak only counts treadmill
        // connect failures (HR is best-effort on a spawned task).
        .context(ScanStartFailed)?;

    let deadline = tokio::time::Instant::now() + SCAN_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        for peripheral in adapter.peripherals().await.context("list peripherals")? {
            if is_hr_sensor(&peripheral).await {
                info!(id = %peripheral.id(), "connecting to HR sensor");
                timeout(CONNECT_TIMEOUT, peripheral.connect())
                    .await
                    .context("connect timed out (possible CoreBluetooth hang)")?
                    .context("connect")?;
                timeout(CONNECT_TIMEOUT, peripheral.discover_services())
                    .await
                    .context("discover services timed out (possible CoreBluetooth hang)")?
                    .context("discover services")?;
                return Ok(peripheral);
            }
        }
        sleep(Duration::from_millis(500)).await;
    }

    bail!("no HR sensor found within {:?}", SCAN_TIMEOUT)
}

async fn is_hr_sensor(peripheral: &Peripheral) -> bool {
    peripheral
        .properties()
        .await
        .ok()
        .flatten()
        .map(|p| p.services.contains(&hr::HEART_RATE_SERVICE))
        .unwrap_or(false)
}

/// Subscribe to Heart Rate Measurement (`0x2A37`) notifications on an already
/// connected HR peripheral. Best-effort: not every device is guaranteed to
/// expose the characteristic (though a Polar H10 always does), and a wearer
/// not having the strap on is a normal, expected outcome — so this returns
/// `false` (logged WARN) rather than an error.
pub async fn subscribe_hr(peripheral: &Peripheral) -> bool {
    let Some(characteristic) = peripheral
        .characteristics()
        .into_iter()
        .find(|c| c.uuid == hr::HEART_RATE_MEASUREMENT)
    else {
        warn!("Heart Rate Measurement characteristic (0x2A37) not found — no pulse this session");
        return false;
    };

    match timeout(CONNECT_TIMEOUT, peripheral.subscribe(&characteristic)).await {
        Ok(Ok(())) => {
            info!("subscribed to Heart Rate Measurement notifications");
            true
        }
        Ok(Err(err)) => {
            warn!(%err, "failed to subscribe to Heart Rate Measurement");
            false
        }
        Err(_) => {
            warn!("subscribe to Heart Rate Measurement timed out (possible CoreBluetooth hang)");
            false
        }
    }
}

/// Read the HR sensor's battery level (0-100%) via the standard Battery
/// Service (`0x180F`/`0x2A19`, задача 026). Best-effort: `None` (logged WARN)
/// when the characteristic is missing, the read fails, or it times out — a
/// missing battery reading must never affect the HR link itself. Polar
/// devices only support Read (no continuous notify) here, so callers must
/// re-invoke this periodically rather than subscribe once.
pub async fn read_hr_battery(peripheral: &Peripheral) -> Option<u8> {
    let characteristic = peripheral
        .characteristics()
        .into_iter()
        .find(|c| c.uuid == hr::BATTERY_LEVEL)?;

    match timeout(CONNECT_TIMEOUT, peripheral.read(&characteristic)).await {
        Ok(Ok(bytes)) => match bytes.first().copied() {
            Some(pct) => Some(pct),
            None => {
                warn!("HR battery level read returned an empty payload");
                None
            }
        },
        Ok(Err(err)) => {
            warn!(%err, "failed to read HR battery level");
            None
        }
        Err(_) => {
            warn!("read HR battery level timed out (possible CoreBluetooth hang)");
            None
        }
    }
}

/// Subscribe to Treadmill Data (`0x2ACD`) notifications on an already
/// connected peripheral. Shared by the interactive `connect` stream and the
/// presence-aware daemon loop.
pub async fn subscribe_treadmill_data(peripheral: &Peripheral) -> Result<()> {
    let characteristic = peripheral
        .characteristics()
        .into_iter()
        .find(|c| c.uuid == ftms::TREADMILL_DATA)
        .context("Treadmill Data characteristic (0x2ACD) not found")?;

    timeout(CONNECT_TIMEOUT, peripheral.subscribe(&characteristic))
        .await
        .context("subscribe to Treadmill Data timed out (possible CoreBluetooth hang)")?
        .context("subscribe to Treadmill Data")?;
    info!("subscribed to Treadmill Data notifications");
    Ok(())
}

/// Subscribe to Fitness Machine Status (`0x2ADA`) notifications, if the
/// characteristic exists — not every FTMS device implements it, so this is
/// best-effort and returns `false` (not an error) when it's missing.
pub async fn subscribe_treadmill_status(peripheral: &Peripheral) -> Result<bool> {
    let Some(characteristic) = peripheral
        .characteristics()
        .into_iter()
        .find(|c| c.uuid == ftms::FITNESS_MACHINE_STATUS)
    else {
        warn!(
            "Fitness Machine Status characteristic (0x2ADA) not present on this device — status events won't be logged"
        );
        return Ok(false);
    };

    timeout(CONNECT_TIMEOUT, peripheral.subscribe(&characteristic))
        .await
        .context("subscribe to Fitness Machine Status timed out (possible CoreBluetooth hang)")?
        .context("subscribe to Fitness Machine Status")?;
    info!("subscribed to Fitness Machine Status notifications");
    Ok(true)
}

/// Subscribe to Treadmill Data notifications and log decoded frames until the
/// stream ends (device disconnects) or the task is cancelled.
pub async fn stream_treadmill_data(peripheral: &Peripheral) -> Result<()> {
    subscribe_treadmill_data(peripheral).await?;

    let mut notifications = peripheral
        .notifications()
        .await
        .context("open notification stream")?;
    let mut logger = WorkoutLogger::create()?;

    while let Some(notification) = notifications.next().await {
        if notification.uuid != ftms::TREADMILL_DATA {
            continue;
        }
        match ftms::parse_treadmill_data(&notification.value) {
            Some(data) => {
                info!(?data, "treadmill data");
                logger.log(&data)?;
            }
            None => warn!(bytes = ?notification.value, "undecodable treadmill frame"),
        }
    }

    logger.finish();
    warn!("notification stream ended (device disconnected?)");
    Ok(())
}
