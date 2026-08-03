//! # storz-rs
//!
//! Rust library for controlling Storz & Bickel vaporizers via Bluetooth Low Energy.
//!
//! Supports **Volcano Hybrid**, **Venty**, **Veazy**, and **Crafty+**.
//!
//! ## Quick Start
//!
//! ```no_run
//! use std::time::Duration;
//! use storz_rs::{discover_vaporizers, get_adapter, connect, VaporizerControl};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // 1. Get a Bluetooth adapter
//! let adapter = get_adapter().await?;
//!
//! // 2. Scan for vaporizers
//! let peripherals = discover_vaporizers(&adapter, Duration::from_secs(10)).await?;
//! let peripheral = peripherals.into_iter().next().expect("No devices found");
//!
//! // 3. Connect and get a controller
//! let device = connect(peripheral).await?;
//!
//! // 4. Read temperature
//! let temp = device.get_current_temperature().await?;
//! println!("Current temperature: {temp}°C");
//!
//! // 5. Set target temperature
//! device.set_target_temperature(185.0).await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Supported Features
//!
//! | Feature | Volcano | Venty/Veazy | Crafty |
//! |---|---|---|---|
//! | Temperature control | ✓ | ✓ | ✓ |
//! | Heater on/off | ✓ | ✓ | ✓ |
//! | Pump on/off | ✓ | — | — |
//! | Brightness | ✓ | ✓ | ✓ |
//! | Vibration | ✓ | ✓ | — |
//! | Boost temperature | — | ✓ | ✓ |
//! | Auto-shutdown timer | ✓ | ✓ | ✓ |
//! | Factory reset | — | ✓ | ✓ |
//! | Device info (serial, firmware) | ✓ | ✓ | ✓ |
//! | Workflow automation | ✓ | — | — |
//!

pub mod device;
pub mod discovery;
pub mod error;
#[cfg(feature = "client")]
pub mod http_client;
pub mod manager;
pub mod protocol;
pub mod utils;
pub mod uuids;
pub mod workflow;

pub use device::{DeviceInfo, DeviceModel, DeviceState, HeaterMode};
pub use discovery::{discover_first, discover_vaporizers, get_adapter, select_peripheral};
pub use error::StorzError;
#[cfg(feature = "client")]
pub use http_client::HttpDevice;
pub use manager::DeviceManager;
pub use protocol::{Crafty, VaporizerControl, Venty, VolcanoHybrid};
pub use workflow::{Workflow, WorkflowRunner, WorkflowState, WorkflowStep};

use btleplug::api::Peripheral as _;
use btleplug::platform::Peripheral;
use std::time::Duration;
use tracing::{debug, info};

/// Auto-detect the device model from its advertised BLE services.
///
/// Call this after `discover_services()` has completed.
pub async fn detect_model(peripheral: &Peripheral) -> Option<DeviceModel> {
    // Try name-based detection first
    if let Ok(Some(props)) = peripheral.properties().await {
        if let Some(name) = props.local_name.as_deref() {
            if name.contains("VOLCANO") {
                return Some(DeviceModel::VolcanoHybrid);
            }
            if name.contains("VY") || name.to_lowercase().contains("venty") {
                return Some(DeviceModel::Venty);
            }
            if name.contains("VZ") || name.to_lowercase().contains("veazy") {
                return Some(DeviceModel::Veazy);
            }
            if name.to_lowercase().contains("crafty") {
                return Some(DeviceModel::Crafty);
            }
        }
    }

    // Fall back to service UUID inspection
    let services = peripheral.services();
    let service_uuids: Vec<_> = services.iter().map(|s| s.uuid).collect();

    if service_uuids.contains(&uuids::VOLCANO_SERVICE_STATE) || service_uuids.contains(&uuids::VOLCANO_SERVICE_CONTROL)
    {
        return Some(DeviceModel::VolcanoHybrid);
    }
    if service_uuids.contains(&uuids::VENTY_SERVICE_PRIMARY) {
        return Some(DeviceModel::Venty);
    }
    if service_uuids.contains(&uuids::CRAFTY_SERVICE_1)
        || service_uuids.contains(&uuids::CRAFTY_SERVICE_2)
        || service_uuids.contains(&uuids::CRAFTY_SERVICE_3)
    {
        return Some(DeviceModel::Crafty);
    }

    None
}

/// Connect to a discovered peripheral and return a trait-object controller.
///
/// This function:
/// 1. Establishes a BLE connection
/// 2. Discovers GATT services
/// 3. Auto-detects the device model
/// 4. Returns the appropriate protocol implementation
/// 5. For Venty/Veazy: runs the init sequence automatically
pub async fn connect(peripheral: Peripheral) -> Result<Box<dyn VaporizerControl>, StorzError> {
    info!("Connecting to peripheral…");
    peripheral.connect().await?;
    peripheral.discover_services().await?;
    debug!("Services discovered");

    let model = detect_model(&peripheral).await.unwrap_or_else(|| {
        debug!("Could not auto-detect model, defaulting to Venty");
        DeviceModel::Venty
    });
    info!("Detected device model: {model}");

    let controller: Box<dyn VaporizerControl> = match model {
        DeviceModel::VolcanoHybrid => Box::new(VolcanoHybrid::new(peripheral).await?),
        DeviceModel::Venty | DeviceModel::Veazy => Box::new(Venty::new(peripheral, model).await?),
        DeviceModel::Crafty => Box::new(Crafty::new(peripheral).await?),
    };

    Ok(controller)
}

/// Connect to a peripheral with a timeout.
///
/// Same as [`connect`] but fails with [`StorzError::Timeout`] if the connection
/// or initialization takes longer than the specified duration.
///
/// # Example
///
/// ```no_run
/// use std::time::Duration;
/// use storz_rs::{discover_vaporizers, get_adapter, connect_with_timeout};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let adapter = get_adapter().await?;
/// let peripherals = discover_vaporizers(&adapter, Duration::from_secs(10)).await?;
/// let peripheral = peripherals.into_iter().next().unwrap();
///
/// let device = connect_with_timeout(peripheral, Duration::from_secs(15)).await?;
/// # Ok(())
/// # }
/// ```
pub async fn connect_with_timeout(
    peripheral: Peripheral,
    timeout: Duration,
) -> Result<Box<dyn VaporizerControl>, StorzError> {
    tokio::time::timeout(timeout, connect(peripheral)).await.map_err(|_| StorzError::Timeout)?
}
