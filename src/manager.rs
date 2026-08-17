//! Auto-reconnecting device handle.
//!
//! [`DeviceManager`] lazily establishes the BLE connection on first use and re-establishes it transparently when it drops, so long-lived processes (daemons, MCP servers) can treat the device as always-available.

use std::sync::Arc;
use std::time::Duration;

use btleplug::api::{Central, CentralEvent, Peripheral as _};
use futures::StreamExt;
use tokio::sync::Mutex;
use tracing::info;

use crate::error::StorzError;
use crate::protocol::VaporizerControl;
use crate::{connect_with_timeout, discover_first, get_adapter};

/// Lazily-connected, auto-reconnecting handle to a vaporizer.
pub struct DeviceManager {
    device: Mutex<Option<Arc<dyn VaporizerControl>>>,
    filter: Option<String>,
    scan_timeout: Duration,
}

impl DeviceManager {
    /// `filter` narrows discovery by device name substring or BLE address.
    pub fn new(filter: Option<String>) -> Self {
        Self {
            device: Mutex::new(None),
            filter,
            scan_timeout: Duration::from_secs(20),
        }
    }

    pub fn with_scan_timeout(mut self, timeout: Duration) -> Self {
        self.scan_timeout = timeout;
        self
    }

    /// Get the connected device, connecting or reconnecting as needed.
    pub async fn get(&self) -> Result<Arc<dyn VaporizerControl>, StorzError> {
        let mut guard = self.device.lock().await;

        if let Some(device) = guard.as_ref() {
            // A cheap read doubles as a liveness probe; on failure fall through to reconnect. Bounded, because a probe on a half-dead connection can hang while this holds the manager lock.
            let probe =
                tokio::time::timeout(Duration::from_secs(5), device.get_current_temperature())
                    .await;
            if matches!(probe, Ok(Ok(_))) {
                return Ok(device.clone());
            }
            info!("device unresponsive, reconnecting");
            *guard = None;
        }

        let adapter = get_adapter().await?;
        let peripheral =
            discover_first(&adapter, self.scan_timeout, self.filter.as_deref()).await?;
        let peripheral_id = peripheral.id();
        // The event stream is opened before connecting so a disconnect racing the handshake can't slip between.
        let mut events = adapter.events().await?;
        // Bounded for the same reason: a hung BLE connect must not wedge every caller behind the lock.
        let device: Arc<dyn VaporizerControl> =
            Arc::from(connect_with_timeout(peripheral, Duration::from_secs(20)).await?);

        // Relay the adapter's disconnect event into the device handle so its state streams terminate — BlueZ does not reliably end notification streams when the device powers itself off, but it does flip the Connected property, which surfaces here.
        {
            let device = device.clone();
            tokio::spawn(async move {
                while let Some(event) = events.next().await {
                    if let CentralEvent::DeviceDisconnected(id) = event {
                        if id == peripheral_id {
                            device.mark_disconnected();
                            return;
                        }
                    }
                }
                // The event stream itself ending means the adapter is gone, which is a disconnect too.
                device.mark_disconnected();
            });
        }

        *guard = Some(device.clone());
        Ok(device)
    }
}
