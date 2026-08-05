use std::time::Duration;

use btleplug::api::{Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use tokio::time;
use tracing::{debug, info, warn};

use crate::error::StorzError;
use crate::uuids::DEVICE_NAME_PREFIXES;

/// Scan for Storz & Bickel devices via BLE and return discovered peripherals.
///
/// The `timeout` controls how long to scan before returning results.
pub async fn discover_vaporizers(
    adapter: &Adapter,
    timeout: Duration,
) -> Result<Vec<Peripheral>, StorzError> {
    info!("Starting BLE scan for Storz & Bickel devices ({timeout:?})…");

    // Ensure adapter is powered on before scanning
    match adapter.adapter_info().await {
        Ok(info_str) => debug!("Adapter info: {info_str}"),
        Err(e) => warn!("Could not read adapter info: {e}"),
    }

    adapter
        .start_scan(ScanFilter::default())
        .await
        .map_err(|e| {
            StorzError::Bluetooth(btleplug::Error::Other(
                format!(
                    "Failed to start BLE scan: {e}\n\n\
                     Troubleshooting:\n\
                     1. Ensure Bluetooth is enabled: rfkill unblock bluetooth\n\
                     2. Ensure adapter is powered on: bluetoothctl power on\n\
                     3. Try running with elevated permissions (sudo or bluetooth group)\n\
                     4. Check: bluetoothctl show"
                )
                .into(),
            ))
        })?;

    time::sleep(timeout).await;
    adapter.stop_scan().await?;

    let peripherals = adapter.peripherals().await?;
    let mut found = Vec::new();

    for p in peripherals {
        if let Ok(Some(props)) = p.properties().await {
            if let Some(name) = props.local_name.as_ref() {
                if DEVICE_NAME_PREFIXES
                    .iter()
                    .any(|prefix| name.contains(prefix))
                {
                    info!("Found device: {name}");
                    found.push(p);
                }
            }
        }
    }

    if found.is_empty() {
        warn!("No Storz & Bickel devices found during scan");
    } else {
        debug!("Discovered {} device(s)", found.len());
    }

    Ok(found)
}

/// Scan until the first matching Storz & Bickel device appears, returning it immediately instead of waiting out the full scan window. `filter` further narrows by substring match on the advertised name or the peripheral address.
pub async fn discover_first(
    adapter: &Adapter,
    timeout: Duration,
    filter: Option<&str>,
) -> Result<Peripheral, StorzError> {
    let matches = |name: &str, address: &str| {
        DEVICE_NAME_PREFIXES.iter().any(|p| name.contains(p))
            && filter.is_none_or(|f| {
                name.to_lowercase().contains(&f.to_lowercase())
                    || address.to_lowercase() == f.to_lowercase()
            })
    };

    let mut events = adapter.events().await?;
    adapter.start_scan(ScanFilter::default()).await?;

    let check = |id: btleplug::platform::PeripheralId| {
        let adapter = adapter.clone();
        async move {
            let p = adapter.peripheral(&id).await.ok()?;
            let props = p.properties().await.ok().flatten()?;
            let name = props.local_name.as_deref().unwrap_or("");
            if matches(name, &p.address().to_string()) {
                info!("Found device: {name}");
                Some(p)
            } else {
                None
            }
        }
    };

    let result = time::timeout(timeout, async {
        // Devices already known to the adapter don't re-emit DeviceDiscovered, so sweep the cache first.
        for p in adapter.peripherals().await.unwrap_or_default() {
            if let Some(found) = check(p.id()).await {
                return Some(found);
            }
        }
        while let Some(event) = events.next().await {
            if let CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) = event {
                if let Some(found) = check(id).await {
                    return Some(found);
                }
            }
        }
        None
    })
    .await;

    adapter.stop_scan().await.ok();
    match result {
        Ok(Some(p)) => Ok(p),
        Ok(None) => Err(StorzError::DeviceNotFound),
        Err(_) => Err(StorzError::DeviceNotFound),
    }
}

/// Obtain the default BLE adapter.
pub async fn get_adapter() -> Result<Adapter, StorzError> {
    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    adapters
        .into_iter()
        .next()
        .ok_or(StorzError::DeviceNotFound)
}

/// Select a single peripheral from a list.
///
/// If only one device is found, returns it immediately.
/// If multiple are found, prints them and prompts for selection via stdin.
pub async fn select_peripheral(peripherals: Vec<Peripheral>) -> Result<Peripheral, StorzError> {
    if peripherals.is_empty() {
        return Err(StorzError::DeviceNotFound);
    }
    if peripherals.len() == 1 {
        return Ok(peripherals.into_iter().next().unwrap());
    }

    println!("\nMultiple devices found:");
    let mut names = Vec::new();
    for (i, p) in peripherals.iter().enumerate() {
        let name = p
            .properties()
            .await
            .ok()
            .flatten()
            .and_then(|props| props.local_name)
            .unwrap_or_else(|| "Unknown".into());
        names.push(name.clone());
        println!("  [{}] {}", i + 1, name);
    }

    loop {
        print!("\nSelect device [1-{}]: ", peripherals.len());
        use std::io::Write;
        std::io::stdout().flush().ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        if let Ok(idx) = input.trim().parse::<usize>() {
            if idx >= 1 && idx <= peripherals.len() {
                info!("Selected: {}", names[idx - 1]);
                return Ok(peripherals.into_iter().nth(idx - 1).unwrap());
            }
        }
        println!("Invalid selection. Try again.");
    }
}
