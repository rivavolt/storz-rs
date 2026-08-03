use std::fmt;

/// Supported Storz & Bickel device models.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceModel {
    VolcanoHybrid,
    Venty,
    Veazy,
    Crafty,
}

impl fmt::Display for DeviceModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceModel::VolcanoHybrid => write!(f, "Volcano Hybrid"),
            DeviceModel::Venty => write!(f, "Venty"),
            DeviceModel::Veazy => write!(f, "Veazy"),
            DeviceModel::Crafty => write!(f, "Crafty"),
        }
    }
}

/// Heater mode for Venty/Veazy devices.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeaterMode {
    /// Heater off
    Off = 0,
    /// Normal heating
    Normal = 1,
    /// Boost mode
    Boost = 2,
    /// Superboost mode
    SuperBoost = 3,
}

impl fmt::Display for HeaterMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeaterMode::Off => write!(f, "off"),
            HeaterMode::Normal => write!(f, "normal"),
            HeaterMode::Boost => write!(f, "boost"),
            HeaterMode::SuperBoost => write!(f, "superboost"),
        }
    }
}

impl HeaterMode {
    /// Parse from raw byte value.
    pub fn from_u8(val: u8) -> Self {
        match val {
            1 => HeaterMode::Normal,
            2 => HeaterMode::Boost,
            3 => HeaterMode::SuperBoost,
            _ => HeaterMode::Off,
        }
    }
}

/// Current state snapshot of a vaporizer device.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DeviceState {
    /// Current measured temperature in Celsius.
    pub current_temp: Option<f32>,
    /// Target temperature in Celsius.
    pub target_temp: Option<f32>,
    /// Boost temperature offset in Celsius (Venty/Veazy).
    pub boost_temp: Option<f32>,
    /// Superboost temperature offset in Celsius (Venty/Veazy).
    pub super_boost_temp: Option<f32>,
    /// Heater mode (Venty/Veazy).
    pub heater_mode: Option<HeaterMode>,
    /// Heater is active.
    pub heater_on: bool,
    /// Pump is active (Volcano only).
    pub pump_on: bool,
    /// Fan is active (Volcano only).
    pub fan_on: bool,
    /// Target temperature has been reached.
    pub setpoint_reached: bool,
    /// Raw activity flags (Volcano).
    pub raw_activity: Option<u32>,
    /// Device settings (Venty/Veazy).
    pub settings: Option<DeviceSettings>,
}

impl fmt::Display for DeviceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "State(heater={}, pump={}, fan={}, current={:?}°C, target={:?}°C)",
            self.heater_on, self.pump_on, self.fan_on, self.current_temp, self.target_temp
        )
    }
}

/// Device settings (Venty/Veazy).
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DeviceSettings {
    /// Temperature unit: true = Celsius, false = Fahrenheit
    pub is_celsius: bool,
    /// Setpoint reached flag
    pub setpoint_reached: bool,
    /// Boost visualization enabled
    pub boost_visualization: bool,
    /// Charge current optimization
    pub charge_current_optimization: bool,
    /// Charge voltage limit enabled
    pub charge_voltage_limit: bool,
    /// Permanent Bluetooth enabled
    pub permanent_bluetooth: bool,
    /// Vibration enabled
    pub vibration: bool,
    /// Auto shutdown timer in seconds
    pub auto_shutdown_seconds: Option<u16>,
    /// Battery level 0-100
    pub battery_level: Option<u8>,
    /// Is charging
    pub is_charging: bool,
}

/// Device information (serial number, firmware, etc.).
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DeviceInfo {
    /// Serial number string.
    pub serial_number: Option<String>,
    /// Firmware version string.
    pub firmware_version: Option<String>,
    /// BLE firmware version string.
    pub firmware_ble_version: Option<String>,
    /// Color index (Venty/Veazy).
    pub color_index: Option<u8>,
    /// Total heater runtime in minutes.
    pub heater_runtime_minutes: Option<u32>,
    /// Total battery charging time in minutes.
    pub battery_charging_time_minutes: Option<u32>,
    /// Hours of heating (Volcano).
    pub hours_of_heating: Option<u16>,
    /// Minutes of heating (Volcano).
    pub minutes_of_heating: Option<u16>,
}

/// Bitmask flags from the Volcano activity characteristic.
pub mod volcano_flags {
    pub const HEATER_ENABLED: u16 = 0x0020;
    pub const FAN_ENABLED: u16 = 0x0400;
    pub const AUTO_SHUTDOWN: u16 = 0x0200;
    pub const PUMP_ENABLED: u16 = 0x2000;
    pub const DISPLAY_ON_COOLING: u16 = 0x1000;
    pub const FAHRENHEIT_ENA: u16 = 0x0200;
}

/// Bitmask flags for the Volcano vibration characteristic.
pub mod volcano_vibration_flags {
    pub const VIBRATION: u32 = 0x0400;
}
