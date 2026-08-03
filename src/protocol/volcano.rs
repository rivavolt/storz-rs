use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use btleplug::api::{Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures::Stream;
use futures::StreamExt;
use tokio::sync::{Mutex, broadcast};
use tokio_stream::wrappers::BroadcastStream;
use tracing::{debug, warn};

use crate::device::{DeviceInfo, DeviceModel, DeviceState, volcano_flags, volcano_vibration_flags};
use crate::error::StorzError;
use crate::protocol::VaporizerControl;
use crate::utils;
use crate::uuids::*;

pub struct VolcanoHybrid {
    peripheral: Peripheral,
    state: Arc<Mutex<DeviceState>>,
    state_tx: broadcast::Sender<DeviceState>,
}

impl VolcanoHybrid {
    pub async fn new(peripheral: Peripheral) -> Result<Self, StorzError> {
        let (state_tx, _) = broadcast::channel(16);

        let device = Self { peripheral, state: Arc::new(Mutex::new(DeviceState::default())), state_tx };

        device.init_notifications().await?;
        device.spawn_notification_loop();
        Ok(device)
    }

    async fn characteristic(&self, uuid: uuid::Uuid) -> Result<Characteristic, StorzError> {
        self.peripheral
            .characteristics()
            .into_iter()
            .find(|c| c.uuid == uuid)
            .ok_or_else(|| StorzError::ParseError(format!("Characteristic {uuid} not found")))
    }

    async fn write_u8(&self, uuid: uuid::Uuid) -> Result<(), StorzError> {
        let ch = self.characteristic(uuid).await?;
        self.peripheral.write(&ch, &[0x00], WriteType::WithoutResponse).await?;
        Ok(())
    }

    async fn read_string(&self, uuid: uuid::Uuid) -> Result<String, StorzError> {
        let ch = self.characteristic(uuid).await?;
        let data = self.peripheral.read(&ch).await?;
        String::from_utf8(data).map_err(|e| StorzError::ParseError(format!("Invalid UTF-8: {e}")))
    }

    async fn read_u16(&self, uuid: uuid::Uuid) -> Result<u16, StorzError> {
        let ch = self.characteristic(uuid).await?;
        let data = self.peripheral.read(&ch).await?;
        utils::raw_to_u16(&data)
    }

    async fn init_notifications(&self) -> Result<(), StorzError> {
        let ch = self.characteristic(VOLCANO_ACTIVITY).await?;
        self.peripheral.subscribe(&ch).await?;
        debug!("Subscribed to Volcano activity notifications");

        let ch = self.characteristic(VOLCANO_CURRENT_TEMP).await?;
        self.peripheral.subscribe(&ch).await?;
        debug!("Subscribed to Volcano current temp notifications");

        let ch = self.characteristic(VOLCANO_TARGET_TEMP).await?;
        self.peripheral.subscribe(&ch).await?;
        debug!("Subscribed to Volcano target temp notifications");

        // Subscribe to display characteristic for isCelsius and displayOnCooling flags
        if let Ok(ch) = self.characteristic(VOLCANO_DISPLAY).await {
            if self.peripheral.subscribe(&ch).await.is_ok() {
                debug!("Subscribed to Volcano display notifications");
            }
        }

        // Subscribe to current auto-off countdown value
        if let Ok(ch) = self.characteristic(VOLCANO_CURRENT_AUTO_OFF).await {
            if self.peripheral.subscribe(&ch).await.is_ok() {
                debug!("Subscribed to Volcano current auto-off notifications");
            }
        }

        Ok(())
    }

    fn spawn_notification_loop(&self) {
        let peripheral = self.peripheral.clone();
        let state = self.state.clone();
        let state_tx = self.state_tx.clone();

        tokio::spawn(async move {
            let mut stream = match peripheral.notifications().await {
                Ok(s) => s,
                Err(e) => {
                    warn!("Volcano Hybrid failed to start notification stream: {e}");
                    return;
                }
            };

            while let Some(data) = stream.next().await {
                debug!("Volcano Hybrid raw notification uuid={} bytes={:02X?}", data.uuid, data.value);
                Self::handle_notification_inner(&state, &state_tx, data.uuid, &data.value);
            }

            warn!("Volcano Hybrid notification stream ended (disconnect?)");
        });
    }

    fn handle_notification_inner(
        state: &Arc<Mutex<DeviceState>>,
        state_tx: &broadcast::Sender<DeviceState>,
        uuid: uuid::Uuid,
        data: &[u8],
    ) {
        let mut state = match state.try_lock() {
            Ok(s) => s,
            Err(_) => return,
        };

        match uuid {
            VOLCANO_ACTIVITY if data.len() >= 2 => {
                let flags = u16::from_le_bytes([data[0], data[1]]);
                state.raw_activity = Some(flags as u32);
                state.heater_on = (flags & volcano_flags::HEATER_ENABLED) != 0;
                state.pump_on = (flags & volcano_flags::PUMP_ENABLED) != 0;
                state.fan_on = (flags & volcano_flags::FAN_ENABLED) != 0;
                let _ = state_tx.send(state.clone());
            }
            VOLCANO_CURRENT_TEMP => {
                if let Ok(temp) = utils::raw_to_celsius_u16(data) {
                    state.current_temp = Some(temp);
                    let _ = state_tx.send(state.clone());
                }
            }
            VOLCANO_TARGET_TEMP => {
                if let Ok(temp) = utils::raw_to_celsius_u32(data) {
                    state.target_temp = Some(temp);
                    let _ = state_tx.send(state.clone());
                }
            }
            VOLCANO_DISPLAY if data.len() >= 2 => {
                let flags = u16::from_le_bytes([data[0], data[1]]);
                let mut settings = state.settings.take().unwrap_or_default();
                // FAHRENHEIT_ENA (0x0200): if NOT set, celsius
                settings.is_celsius = (flags & volcano_flags::FAHRENHEIT_ENA) == 0;
                state.settings = Some(settings);
                debug!(
                    "Volcano display flags: 0x{flags:04X} (celsius={})",
                    (flags & volcano_flags::FAHRENHEIT_ENA) == 0
                );
                let _ = state_tx.send(state.clone());
            }
            VOLCANO_CURRENT_AUTO_OFF if data.len() >= 2 => {
                let value = u16::from_le_bytes([data[0], data[1]]);
                let mut settings = state.settings.take().unwrap_or_default();
                settings.auto_shutdown_seconds = Some(value);
                state.settings = Some(settings);
                debug!("Volcano current auto-off: {value}s");
                let _ = state_tx.send(state.clone());
            }
            _ => {
                debug!("Volcano Hybrid unhandled notification uuid={} len={}", uuid, data.len());
            }
        }
    }
}

#[async_trait]
impl VaporizerControl for VolcanoHybrid {
    async fn get_current_temperature(&self) -> Result<f32, StorzError> {
        let ch = self.characteristic(VOLCANO_CURRENT_TEMP).await?;
        let data = self.peripheral.read(&ch).await?;
        utils::raw_to_celsius_u16(&data)
    }

    async fn get_target_temperature(&self) -> Result<f32, StorzError> {
        let ch = self.characteristic(VOLCANO_TARGET_TEMP).await?;
        let data = self.peripheral.read(&ch).await?;
        utils::raw_to_celsius_u32(&data)
    }

    async fn set_target_temperature(&self, celsius: f32) -> Result<(), StorzError> {
        let ch = self.characteristic(VOLCANO_TARGET_TEMP).await?;
        let raw = utils::celsius_to_raw_u32(celsius)?;
        self.peripheral.write(&ch, &raw, WriteType::WithoutResponse).await?;
        debug!("Volcano target temp set to {celsius}°C");
        Ok(())
    }

    async fn heater_on(&self) -> Result<(), StorzError> {
        self.write_u8(VOLCANO_HEATER_ON).await?;
        debug!("Volcano heater ON");
        Ok(())
    }

    async fn heater_off(&self) -> Result<(), StorzError> {
        self.write_u8(VOLCANO_HEATER_OFF).await?;
        debug!("Volcano heater OFF");
        Ok(())
    }

    async fn pump_on(&self) -> Result<(), StorzError> {
        self.write_u8(VOLCANO_PUMP_ON).await?;
        debug!("Volcano pump ON");
        Ok(())
    }

    async fn pump_off(&self) -> Result<(), StorzError> {
        self.write_u8(VOLCANO_PUMP_OFF).await?;
        debug!("Volcano pump OFF");
        Ok(())
    }

    async fn get_state(&self) -> Result<DeviceState, StorzError> {
        let state = self.state.lock().await;
        Ok(state.clone())
    }

    async fn subscribe_state(&self) -> Result<Pin<Box<dyn Stream<Item = DeviceState> + Send>>, StorzError> {
        let rx = self.state_tx.subscribe();
        Ok(Box::pin(BroadcastStream::new(rx).filter_map(|r| async move { r.ok() })))
    }

    async fn set_brightness(&self, value: u16) -> Result<(), StorzError> {
        let ch = self.characteristic(VOLCANO_BRIGHTNESS).await?;
        let raw = value.to_le_bytes();
        self.peripheral.write(&ch, &raw, WriteType::WithoutResponse).await?;
        debug!("Volcano brightness set to {value}");
        Ok(())
    }

    async fn set_vibration(&self, on: bool) -> Result<(), StorzError> {
        let ch = self.characteristic(VOLCANO_VIBRATION).await?;
        let raw: u32 =
            if on { volcano_vibration_flags::VIBRATION } else { 0x10000 + volcano_vibration_flags::VIBRATION };
        self.peripheral.write(&ch, &raw.to_le_bytes(), WriteType::WithoutResponse).await?;
        debug!("Volcano vibration set to {on}");
        Ok(())
    }

    async fn get_device_info(&self) -> Result<DeviceInfo, StorzError> {
        let serial_number = self.read_string(VOLCANO_SERIAL_NUMBER).await.ok();
        let firmware_version = self.read_string(VOLCANO_FIRMWARE_VERSION).await.ok();
        let firmware_ble_version = self.read_string(VOLCANO_FIRMWARE_BLE_VERSION).await.ok();
        let hours_of_heating = self.read_u16(VOLCANO_HOURS_OF_HEATING).await.ok();
        let minutes_of_heating = self.read_u16(VOLCANO_MINUTES_OF_HEATING).await.ok();

        Ok(DeviceInfo {
            serial_number,
            firmware_version,
            firmware_ble_version,
            hours_of_heating,
            minutes_of_heating,
            ..Default::default()
        })
    }

    async fn set_shutoff_time(&self, seconds: u16) -> Result<(), StorzError> {
        let ch = self.characteristic(VOLCANO_SHUTOFF_TIME).await?;
        let raw = seconds.to_le_bytes();
        self.peripheral.write(&ch, &raw, WriteType::WithoutResponse).await?;
        debug!("Volcano shutoff time set to {seconds}s");
        Ok(())
    }

    async fn set_temperature_unit(&self, celsius: bool) -> Result<(), StorzError> {
        let ch = self.characteristic(VOLCANO_DISPLAY).await?;
        // Register-mask write: low 16 bits select the FAHRENHEIT_ENA flag, bit 16 clears instead of sets, so celsius=true clears the flag.
        let raw: u32 =
            if celsius { 0x10000 + volcano_flags::FAHRENHEIT_ENA as u32 } else { volcano_flags::FAHRENHEIT_ENA as u32 };
        self.peripheral.write(&ch, &raw.to_le_bytes(), WriteType::WithoutResponse).await?;
        debug!("Volcano temperature unit set to celsius={celsius}");
        Ok(())
    }

    async fn set_display_on_cooling(&self, on: bool) -> Result<(), StorzError> {
        let ch = self.characteristic(VOLCANO_DISPLAY).await?;
        let raw: u32 = if on {
            volcano_flags::DISPLAY_ON_COOLING as u32
        } else {
            0x10000 + volcano_flags::DISPLAY_ON_COOLING as u32
        };
        self.peripheral.write(&ch, &raw.to_le_bytes(), WriteType::WithoutResponse).await?;
        debug!("Volcano display-on-cooling set to {on}");
        Ok(())
    }

    fn device_model(&self) -> DeviceModel {
        DeviceModel::VolcanoHybrid
    }
}
