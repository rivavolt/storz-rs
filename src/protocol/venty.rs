use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use btleplug::api::{Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures::Stream;
use futures::StreamExt;
use tokio::sync::{Mutex, broadcast};
use tokio_stream::wrappers::BroadcastStream;
use tracing::{debug, warn};

use crate::device::{DeviceInfo, DeviceModel, DeviceSettings, DeviceState, HeaterMode};
use crate::error::StorzError;
use crate::protocol::VaporizerControl;
use crate::utils;
use crate::uuids::*;

/// Venty or Veazy device, both share the same protocol.
pub struct Venty {
    peripheral: Peripheral,
    model: DeviceModel,
    state: Arc<Mutex<DeviceState>>,
    device_info: Arc<Mutex<DeviceInfo>>,
    state_tx: broadcast::Sender<DeviceState>,
}

impl Venty {
    pub async fn new(peripheral: Peripheral, model: DeviceModel) -> Result<Self, StorzError> {
        let (state_tx, _) = broadcast::channel(16);

        let device = Self {
            peripheral,
            model,
            state: Arc::new(Mutex::new(DeviceState::default())),
            device_info: Arc::new(Mutex::new(DeviceInfo::default())),
            state_tx,
        };

        device.init().await?;
        device.spawn_notification_loop();
        Ok(device)
    }

    async fn control_characteristic(&self) -> Result<btleplug::api::Characteristic, StorzError> {
        self.peripheral
            .characteristics()
            .into_iter()
            .find(|c| c.uuid == VENTY_CONTROL)
            .ok_or_else(|| StorzError::ParseError("Venty control characteristic not found".into()))
    }

    async fn write_command(&self, buf: &[u8]) -> Result<(), StorzError> {
        let ch = self.control_characteristic().await?;
        self.peripheral
            .write(&ch, buf, WriteType::WithoutResponse)
            .await?;
        Ok(())
    }

    async fn init(&self) -> Result<(), StorzError> {
        let ch = self.control_characteristic().await?;

        // Subscribe to notifications first
        self.peripheral.subscribe(&ch).await?;
        debug!("Subscribed to Venty/Veazy control notifications");

        // Send initialization sequence: 0x02, 0x1D, 0x01, 0x04
        for &cmd in &[0x02u8, 0x1Du8, 0x01u8, 0x04u8] {
            let buf = utils::build_venty_command(cmd, 0, &[]);
            self.write_command(&buf).await?;
            debug!("Venty/Veazy init command 0x{cmd:02X} sent");
        }

        Ok(())
    }

    fn spawn_notification_loop(&self) {
        let peripheral = self.peripheral.clone();
        let state = self.state.clone();
        let device_info = self.device_info.clone();
        let state_tx = self.state_tx.clone();
        let model = self.model;

        tokio::spawn(async move {
            let mut stream = match peripheral.notifications().await {
                Ok(s) => s,
                Err(e) => {
                    warn!("Venty/Veazy failed to start notification stream: {e}");
                    return;
                }
            };

            while let Some(data) = stream.next().await {
                Self::debug_dump_notification(&data.value);
                Self::handle_notification_inner(
                    &state,
                    &device_info,
                    &state_tx,
                    model,
                    &data.value,
                );
            }

            warn!("{model} notification stream ended (disconnect?)");
        });
    }

    fn debug_dump_notification(data: &[u8]) {
        if data.is_empty() {
            debug!("Venty notification: empty");
            return;
        }
        let cmd = data[0];
        let hex: String = data
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ");
        debug!(
            "Venty notification: cmd=0x{cmd:02X} len={} [{hex}]",
            data.len()
        );

        if data.len() >= 2 {
            debug!("  [0] cmd_id    = 0x{:02X}", data[0]);
            debug!("  [1] mask      = 0x{:02X} ({:08b})", data[1], data[1]);
        }
        if data.len() >= 4 {
            let raw23 = u16::from_le_bytes([data[2], data[3]]);
            debug!(
                "  [2:3] bytes   = 0x{:04X} (u16={raw23}, as_temp={:.1}°C — UNUSED)",
                raw23,
                raw23 as f32 / 10.0
            );
        }
        if data.len() >= 6 {
            let raw45 = u16::from_le_bytes([data[4], data[5]]);
            debug!(
                "  [4:5] target  = 0x{:04X} (u16={raw45}, {:.1}°C)",
                raw45,
                raw45 as f32 / 10.0
            );
        }
        if data.len() >= 7 {
            debug!("  [6]   boost   = {}°C", data[6]);
        }
        if data.len() >= 8 {
            debug!("  [7]   sboost  = {}°C", data[7]);
        }
        if data.len() >= 9 {
            debug!("  [8]   battery = {}%", data[8]);
        }
        if data.len() >= 11 {
            let timer = data[9] as u16 + (data[10] as u16) * 256;
            debug!("  [9:10] timer  = {timer}s");
        }
        if data.len() >= 12 {
            let mode = match data[11] {
                0 => "off",
                1 => "normal",
                2 => "boost",
                3 => "superboost",
                _ => "unknown",
            };
            debug!("  [11]  heater  = {mode}");
        }
        if data.len() >= 14 {
            debug!("  [13]  charger = {}", data[13]);
        }
        if data.len() >= 15 {
            let s = data[14];
            debug!(
                "  [14]  settings= 0x{s:02X} (cel={},setpoint={},vibrate={})",
                if s & 1 == 0 { "yes" } else { "no" },
                if s & 2 != 0 { "yes" } else { "no" },
                if s & 0x40 != 0 { "yes" } else { "no" }
            );
        }
    }

    fn handle_notification_inner(
        state: &Arc<Mutex<DeviceState>>,
        device_info: &Arc<Mutex<DeviceInfo>>,
        state_tx: &broadcast::Sender<DeviceState>,
        _model: DeviceModel,
        data: &[u8],
    ) {
        if data.is_empty() {
            return;
        }

        let cmd_id = data[0];

        match cmd_id {
            0x01 | 0x05 if data.len() >= 15 && cmd_id == 0x01 => {
                let mut state = match state.try_lock() {
                    Ok(s) => s,
                    Err(_) => return,
                };

                // Bytes 4+5: target temperature (u16 LE, /10)
                if data.len() >= 6 {
                    let raw = u16::from_le_bytes([data[4], data[5]]);
                    state.target_temp = Some((raw as f32) / 10.0);
                }

                // Byte 6: boost temperature offset
                if data.len() >= 7 {
                    state.boost_temp = Some(data[6] as f32);
                }

                // Byte 7: super-boost temperature offset
                if data.len() >= 8 {
                    state.super_boost_temp = Some(data[7] as f32);
                }

                // Byte 11: heater mode (0=off, 1=normal, 2=boost, 3=superboost)
                if data.len() >= 12 {
                    let mode = HeaterMode::from_u8(data[11]);
                    state.heater_on = mode != HeaterMode::Off;
                    state.heater_mode = Some(mode);
                }

                // Parse settings from notification
                let mut settings = state.settings.take().unwrap_or_default();

                // Byte 8: battery level
                if data.len() >= 9 {
                    settings.battery_level = Some(data[8]);
                }

                // Bytes 9+10: auto shutdown timer
                if data.len() >= 11 {
                    let timer = data[9] as u16 + (data[10] as u16) * 256;
                    settings.auto_shutdown_seconds = Some(timer);
                }

                // Byte 13: charger connected
                if data.len() >= 14 {
                    settings.is_charging = data[13] > 0;
                }

                // Byte 14: settings flags
                if data.len() >= 15 {
                    let s = data[14];
                    settings.is_celsius = (s & 0x01) == 0;
                    settings.setpoint_reached = (s & 0x02) != 0;
                    settings.charge_current_optimization = (s & 0x08) != 0;
                    settings.charge_voltage_limit = (s & 0x20) != 0;
                    settings.boost_visualization = (s & 0x40) != 0;
                    settings.vibration = (s & 0x40) != 0;
                    state.setpoint_reached = settings.setpoint_reached;
                }

                // Byte 16: permanent bluetooth
                if data.len() >= 17 {
                    settings.permanent_bluetooth = (data[16] & 0x01) != 0;
                }

                state.settings = Some(settings);

                // Venty/Veazy don't have pumps
                state.pump_on = false;

                let _ = state_tx.send(state.clone());
            }
            0x02 if data.len() >= 9 => {
                // Firmware response: bytes 1-4 firmware version, bytes 5-8 bootloader version
                let mut info = match device_info.try_lock() {
                    Ok(i) => i,
                    Err(_) => return,
                };
                let firmware = format!("{}.{}.{}.{}", data[1], data[2], data[3], data[4]);
                let bootloader = format!("{}.{}.{}.{}", data[5], data[6], data[7], data[8]);
                info.firmware_version = Some(firmware);
                info.firmware_ble_version = Some(bootloader);
                debug!(
                    "Venty/Veazy firmware: {}, bootloader: {}",
                    info.firmware_version.as_deref().unwrap_or("?"),
                    info.firmware_ble_version.as_deref().unwrap_or("?")
                );
            }
            0x04 if data.len() >= 7 => {
                // Extended device data: bytes 1-3 heater runtime (uint24), bytes 4-6 charging time (uint24)
                let mut info = match device_info.try_lock() {
                    Ok(i) => i,
                    Err(_) => return,
                };
                let heater_runtime =
                    data[1] as u32 + (data[2] as u32) * 256 + (data[3] as u32) * 65536;
                let charging_time =
                    data[4] as u32 + (data[5] as u32) * 256 + (data[6] as u32) * 65536;
                info.heater_runtime_minutes = Some(heater_runtime);
                info.battery_charging_time_minutes = Some(charging_time);
                debug!(
                    "Venty/Veazy heater runtime: {heater_runtime}min, charging time: {charging_time}min"
                );
            }
            0x05 if data.len() >= 19 => {
                // Device data: serial number (bytes 9-14 + 15-16), color index (byte 18)
                let mut info = match device_info.try_lock() {
                    Ok(i) => i,
                    Err(_) => return,
                };
                // Serial: prefix (bytes 15-16) + name (bytes 9-14)
                let prefix = String::from_utf8_lossy(&data[15..17]);
                let name = String::from_utf8_lossy(&data[9..15]);
                info.serial_number = Some(format!("{prefix}{name}"));
                info.color_index = Some(data[18]);
                debug!(
                    "Venty/Veazy serial: {}, color: {}",
                    info.serial_number.as_deref().unwrap_or("?"),
                    data[18]
                );
            }
            0x06 if data.len() >= 7 => {
                // Brightness/vibration/boost_timeout response
                debug!(
                    "Venty/Veazy brightness={}, vibration={}, boost_timeout={}",
                    data[2], data[5], data[6]
                );
                // These are read-only responses; state is maintained via CMD 0x01 notifications
            }
            0x0d => {
                // Find-my-device response (CMD 0x29 when entering advertising mode)
                debug!("Venty/Veazy find-my-device response received");
            }
            _ => {
                debug!(
                    "Venty/Veazy unhandled notification cmd=0x{cmd_id:02X} len={}",
                    data.len()
                );
            }
        }
    }
}

#[async_trait]
impl VaporizerControl for Venty {
    async fn get_current_temperature(&self) -> Result<f32, StorzError> {
        // Venty doesn't expose current temp directly via a read;
        // it comes through notifications. Return cached value.
        let state = self.state.lock().await;
        state.current_temp.ok_or(StorzError::ParseError(
            "Current temperature not yet available from device notifications".into(),
        ))
    }

    async fn get_target_temperature(&self) -> Result<f32, StorzError> {
        let state = self.state.lock().await;
        state.target_temp.ok_or(StorzError::ParseError(
            "Target temperature not yet available from device notifications".into(),
        ))
    }

    async fn set_target_temperature(&self, celsius: f32) -> Result<(), StorzError> {
        let raw = (celsius * 10.0).round() as u16;
        let low = (raw & 0xFF) as u8;
        let high = ((raw >> 8) & 0xFF) as u8;

        let buf = utils::build_venty_command(
            0x01, // Write command
            0x02, // SET_TEMPERATURE mask
            &[(4, low), (5, high)],
        );
        self.write_command(&buf).await?;
        debug!("Venty/Veazy target temp set to {celsius}°C");
        Ok(())
    }

    async fn heater_on(&self) -> Result<(), StorzError> {
        let buf = utils::build_venty_command(
            0x01,
            0x20,       // HEATER mask
            &[(11, 1)], // heater mode = 1 (normal)
        );
        self.write_command(&buf).await?;
        debug!("Venty/Veazy heater ON");
        Ok(())
    }

    async fn heater_off(&self) -> Result<(), StorzError> {
        let buf = utils::build_venty_command(
            0x01,
            0x20,       // HEATER mask
            &[(11, 0)], // heater mode = 0 (off)
        );
        self.write_command(&buf).await?;
        debug!("Venty/Veazy heater OFF");
        Ok(())
    }

    async fn pump_on(&self) -> Result<(), StorzError> {
        Err(StorzError::UnsupportedOperation {
            device: self.model.to_string(),
            operation: "pump_on".into(),
        })
    }

    async fn pump_off(&self) -> Result<(), StorzError> {
        Err(StorzError::UnsupportedOperation {
            device: self.model.to_string(),
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

    async fn get_settings(&self) -> Result<DeviceSettings, StorzError> {
        let state = self.state.lock().await;
        state.settings.clone().ok_or(StorzError::ParseError(
            "Settings not yet available from device notifications".into(),
        ))
    }

    async fn set_temperature_unit(&self, celsius: bool) -> Result<(), StorzError> {
        let buf = utils::build_venty_command(
            0x01,
            0x80, // SETTINGS mask
            &[
                (14, if celsius { 0 } else { 1 }),
                (15, 0x01), // BIT_SETTINGS_UNIT
            ],
        );
        self.write_command(&buf).await?;
        debug!(
            "Venty/Veazy temperature unit set to {}",
            if celsius { "Celsius" } else { "Fahrenheit" }
        );
        Ok(())
    }

    async fn set_boost_temperature(&self, celsius: f32) -> Result<(), StorzError> {
        let raw = celsius.round() as u8;
        let buf = utils::build_venty_command(
            0x01,
            0x04, // SET_BOOST mask
            &[(6, raw)],
        );
        self.write_command(&buf).await?;
        debug!("Venty/Veazy boost temp set to {celsius}°C");
        Ok(())
    }

    async fn set_super_boost_temperature(&self, celsius: f32) -> Result<(), StorzError> {
        let raw = celsius.round() as u8;
        let buf = utils::build_venty_command(
            0x01,
            0x08, // SET_SUPERBOOST mask
            &[(7, raw)],
        );
        self.write_command(&buf).await?;
        debug!("Venty/Veazy super-boost temp set to {celsius}°C");
        Ok(())
    }

    async fn set_auto_shutdown_timer(&self, seconds: u16) -> Result<(), StorzError> {
        let low = (seconds & 0xFF) as u8;
        let high = ((seconds >> 8) & 0xFF) as u8;
        let buf = utils::build_venty_command(
            0x01,
            0x10, // Auto-shutdown mask (bit 4)
            &[(9, low), (10, high)],
        );
        self.write_command(&buf).await?;
        debug!("Venty/Veazy auto-shutdown timer set to {seconds}s");
        Ok(())
    }

    async fn set_heater_mode(&self, mode: HeaterMode) -> Result<(), StorzError> {
        let buf = utils::build_venty_command(
            0x01,
            0x20,                // HEATER mask
            &[(11, mode as u8)], // heater mode
        );
        self.write_command(&buf).await?;
        debug!("Venty/Veazy heater mode set to {mode}");
        Ok(())
    }

    async fn factory_reset(&self) -> Result<(), StorzError> {
        let buf = utils::build_venty_command(
            0x01,
            0x80, // SETTINGS mask
            &[
                (14, 0x04), // BIT_SETTINGS_FACTORY_RESET
                (15, 0x04), // Mask for bit 2
            ],
        );
        self.write_command(&buf).await?;
        debug!("Venty/Veazy factory reset triggered");
        Ok(())
    }

    async fn set_boost_visualization(&self, enabled: bool) -> Result<(), StorzError> {
        let should_set = if self.model == DeviceModel::Veazy {
            !enabled // Veazy inverts the logic
        } else {
            enabled
        };
        let buf = utils::build_venty_command(
            0x01,
            0x80, // SETTINGS mask
            &[
                (14, if should_set { 0x40 } else { 0x00 }),
                (15, 0x40), // Mask for bit 6
            ],
        );
        self.write_command(&buf).await?;
        debug!("Venty/Veazy boost visualization set to {enabled}");
        Ok(())
    }

    async fn set_charge_current_optimization(&self, enabled: bool) -> Result<(), StorzError> {
        let buf = utils::build_venty_command(
            0x01,
            0x80, // SETTINGS mask
            &[
                (14, if enabled { 0x08 } else { 0x00 }),
                (15, 0x08), // Mask for bit 3
            ],
        );
        self.write_command(&buf).await?;
        debug!("Venty/Veazy charge current optimization set to {enabled}");
        Ok(())
    }

    async fn set_charge_voltage_limit(&self, enabled: bool) -> Result<(), StorzError> {
        let buf = utils::build_venty_command(
            0x01,
            0x80, // SETTINGS mask
            &[
                (14, if enabled { 0x20 } else { 0x00 }),
                (15, 0x20), // Mask for bit 5
            ],
        );
        self.write_command(&buf).await?;
        debug!("Venty/Veazy charge voltage limit set to {enabled}");
        Ok(())
    }

    async fn set_permanent_bluetooth(&self, enabled: bool) -> Result<(), StorzError> {
        let buf = utils::build_venty_command(
            0x01,
            0x80, // SETTINGS mask
            &[
                (16, if enabled { 0x01 } else { 0x00 }),
                (17, 0x01), // BIT_SETTINGS2_BLE_PERMANENT
            ],
        );
        self.write_command(&buf).await?;
        debug!("Venty/Veazy permanent bluetooth set to {enabled}");
        Ok(())
    }

    async fn set_vibration(&self, on: bool) -> Result<(), StorzError> {
        // CMD 0x06 with mask bit 3 (1 << 3 = 8)
        let mut buf = [0u8; 7];
        buf[0] = 0x06;
        buf[1] = 1 << 3; // Vibration mask
        buf[5] = on as u8;
        self.write_command(&buf).await?;
        debug!("Venty/Veazy vibration set to {on}");
        Ok(())
    }

    async fn set_brightness(&self, value: u16) -> Result<(), StorzError> {
        // CMD 0x06 with mask bit 0 (1 << 0 = 1)
        let mut buf = [0u8; 7];
        buf[0] = 0x06;
        buf[1] = 1 << 0; // Brightness mask
        buf[2] = (value & 0xFF) as u8;
        self.write_command(&buf).await?;
        debug!("Venty/Veazy brightness set to {value}");
        Ok(())
    }

    async fn set_boost_timeout(&self, seconds: u8) -> Result<(), StorzError> {
        // CMD 0x06 with mask bit 4 (1 << 4 = 16)
        let mut buf = [0u8; 7];
        buf[0] = 0x06;
        buf[1] = 1 << 4; // BoostTimeout mask
        buf[6] = seconds;
        self.write_command(&buf).await?;
        debug!("Venty/Veazy boost timeout set to {seconds}s");
        Ok(())
    }

    async fn get_device_info(&self) -> Result<DeviceInfo, StorzError> {
        // Request firmware data via CMD 0x02
        let buf = utils::build_venty_command(0x02, 0, &[]);
        self.write_command(&buf).await?;
        // Request extended data via CMD 0x04
        let buf = utils::build_venty_command(0x04, 0, &[]);
        self.write_command(&buf).await?;
        // Request device data via CMD 0x05
        let buf = utils::build_venty_command(0x05, 0, &[]);
        self.write_command(&buf).await?;
        debug!("Venty/Veazy device info requests sent");
        // Return cached data from notification parsing
        let info = self.device_info.lock().await;
        Ok(info.clone())
    }

    async fn find_my_device(&self) -> Result<(), StorzError> {
        // CMD 0x0D (13) with mask 0x01
        let buf = utils::build_venty_command(0x0D, 0x01, &[]);
        self.write_command(&buf).await?;
        debug!("Venty/Veazy find-my-device triggered");
        Ok(())
    }

    fn device_model(&self) -> DeviceModel {
        self.model
    }
}
