use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use btleplug::api::{Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures::{Stream, StreamExt};
use tokio::sync::{Mutex, broadcast};
use tokio_stream::wrappers::BroadcastStream;
use tracing::{debug, warn};

use crate::device::{DeviceInfo, DeviceModel, DeviceState};
use crate::error::StorzError;
use crate::protocol::VaporizerControl;
use crate::utils;
use crate::uuids::*;

pub struct Crafty {
    peripheral: Peripheral,
    state: Arc<Mutex<DeviceState>>,
    state_tx: broadcast::Sender<DeviceState>,
}

impl Crafty {
    pub async fn new(peripheral: Peripheral) -> Result<Self, StorzError> {
        let (state_tx, _) = broadcast::channel(16);

        let device = Self {
            peripheral,
            state: Arc::new(Mutex::new(DeviceState::default())),
            state_tx,
        };

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
        let ch = self.characteristic(CRAFTY_CURRENT_TEMP_CHANGED).await?;
        self.peripheral.subscribe(&ch).await?;
        debug!("Subscribed to Crafty current temp notifications");
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
                    warn!("Crafty failed to start notification stream: {e}");
                    return;
                }
            };

            while let Some(data) = stream.next().await {
                debug!(
                    "Crafty raw notification uuid={} bytes={:02X?}",
                    data.uuid, data.value
                );
                Self::handle_notification_inner(&state, &state_tx, data.uuid, &data.value);
            }

            warn!("Crafty notification stream ended (disconnect?)");
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

        if uuid == CRAFTY_CURRENT_TEMP_CHANGED {
            if let Ok(temp) = utils::raw_to_celsius_u16(data) {
                state.current_temp = Some(temp);
                let _ = state_tx.send(state.clone());
            }
        } else {
            debug!(
                "Crafty unhandled notification uuid={} len={}",
                uuid,
                data.len()
            );
        }
    }
}

#[async_trait]
impl VaporizerControl for Crafty {
    async fn get_current_temperature(&self) -> Result<f32, StorzError> {
        let ch = self.characteristic(CRAFTY_CURRENT_TEMP_CHANGED).await?;
        let data = self.peripheral.read(&ch).await?;
        utils::raw_to_celsius_u16(&data)
    }

    async fn get_target_temperature(&self) -> Result<f32, StorzError> {
        let ch = self.characteristic(CRAFTY_WRITE_TEMP).await?;
        let data = self.peripheral.read(&ch).await?;
        utils::raw_to_celsius_u16(&data)
    }

    async fn set_target_temperature(&self, celsius: f32) -> Result<(), StorzError> {
        let ch = self.characteristic(CRAFTY_WRITE_TEMP).await?;
        let raw = utils::celsius_to_raw_u16(celsius)?;
        self.peripheral
            .write(&ch, &raw, WriteType::WithoutResponse)
            .await?;
        debug!("Crafty target temp set to {celsius}°C");
        Ok(())
    }

    async fn heater_on(&self) -> Result<(), StorzError> {
        let ch = self.characteristic(CRAFTY_HEATER_ON).await?;
        self.peripheral
            .write(&ch, &[0x00], WriteType::WithoutResponse)
            .await?;
        debug!("Crafty heater ON");
        Ok(())
    }

    async fn heater_off(&self) -> Result<(), StorzError> {
        let ch = self.characteristic(CRAFTY_HEATER_OFF).await?;
        self.peripheral
            .write(&ch, &[0x00], WriteType::WithoutResponse)
            .await?;
        debug!("Crafty heater OFF");
        Ok(())
    }

    async fn pump_on(&self) -> Result<(), StorzError> {
        Err(StorzError::UnsupportedOperation {
            device: "Crafty".into(),
            operation: "pump_on".into(),
        })
    }

    async fn pump_off(&self) -> Result<(), StorzError> {
        Err(StorzError::UnsupportedOperation {
            device: "Crafty".into(),
            operation: "pump_off".into(),
        })
    }

    async fn get_state(&self) -> Result<DeviceState, StorzError> {
        let state = self.state.lock().await;
        Ok(state.clone())
    }

    async fn subscribe_state(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = DeviceState> + Send>>, StorzError> {
        let rx = self.state_tx.subscribe();
        Ok(Box::pin(
            BroadcastStream::new(rx).filter_map(|r| async move { r.ok() }),
        ))
    }

    async fn set_boost_temperature(&self, celsius: f32) -> Result<(), StorzError> {
        let clamped = celsius.clamp(0.0, 30.0);
        let ch = self.characteristic(CRAFTY_WRITE_BOOST_TEMP).await?;
        let raw = ((clamped * 10.0).round() as u16).to_le_bytes();
        self.peripheral
            .write(&ch, &raw, WriteType::WithoutResponse)
            .await?;
        debug!("Crafty boost temp set to {clamped}°C");
        Ok(())
    }

    async fn set_brightness(&self, value: u16) -> Result<(), StorzError> {
        let ch = self.characteristic(CRAFTY_LED_BRIGHTNESS).await?;
        let raw = value.to_le_bytes();
        self.peripheral
            .write(&ch, &raw, WriteType::WithoutResponse)
            .await?;
        debug!("Crafty LED brightness set to {value}");
        Ok(())
    }

    async fn get_device_info(&self) -> Result<DeviceInfo, StorzError> {
        let firmware_version = self.read_string(CRAFTY_FIRMWARE_VERSION).await.ok();
        let firmware_ble_version = self.read_string(CRAFTY_FIRMWARE_BLE_VERSION).await.ok();
        let hours_of_heating = self.read_u16(CRAFTY_USE_HOURS).await.ok();
        let minutes_of_heating = self.read_u16(CRAFTY_USE_MINUTES).await.ok();

        // Read system status and battery status (may not be available on old Crafty)
        if let Ok(ch) = self.characteristic(CRAFTY_SYSTEM_STATUS).await {
            if let Ok(data) = self.peripheral.read(&ch).await {
                debug!("Crafty system status: {:02X?}", data);
            }
        }
        if let Ok(ch) = self.characteristic(CRAFTY_AKKU_STATUS).await {
            if let Ok(data) = self.peripheral.read(&ch).await {
                debug!("Crafty akku status: {:02X?}", data);
            }
        }

        Ok(DeviceInfo {
            firmware_version,
            firmware_ble_version,
            hours_of_heating,
            minutes_of_heating,
            ..Default::default()
        })
    }

    async fn factory_reset(&self) -> Result<(), StorzError> {
        let ch = self.characteristic(CRAFTY_FACTORY_RESET).await?;
        self.peripheral
            .write(&ch, &[0x00], WriteType::WithoutResponse)
            .await?;
        debug!("Crafty factory reset triggered");
        Ok(())
    }

    async fn set_auto_off_countdown(&self, seconds: u16) -> Result<(), StorzError> {
        let ch = self.characteristic(CRAFTY_AUTO_OFF_COUNTDOWN).await?;
        let raw = seconds.to_le_bytes();
        self.peripheral
            .write(&ch, &raw, WriteType::WithoutResponse)
            .await?;
        debug!("Crafty auto-off countdown set to {seconds}s");
        Ok(())
    }

    async fn get_project_register(&self) -> Result<u16, StorzError> {
        let ch = self.characteristic(CRAFTY_PROJECT_REGISTER).await?;
        let data = self.peripheral.read(&ch).await?;
        utils::raw_to_u16(&data)
    }

    async fn set_security_code(&self, code: u16) -> Result<(), StorzError> {
        let ch = self.characteristic(CRAFTY_SICHERHEITSCODE).await?;
        let raw = code.to_le_bytes();
        self.peripheral
            .write(&ch, &raw, WriteType::WithoutResponse)
            .await?;
        debug!("Crafty security code set");
        Ok(())
    }

    fn device_model(&self) -> DeviceModel {
        DeviceModel::Crafty
    }
}
