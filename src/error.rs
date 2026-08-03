use thiserror::Error;

#[derive(Error, Debug)]
pub enum StorzError {
    #[error("Bluetooth error: {0}")]
    Bluetooth(#[from] btleplug::Error),

    #[error("Device not found")]
    DeviceNotFound,

    #[error("Unsupported operation '{operation}' for device '{device}'")]
    UnsupportedOperation { device: String, operation: String },

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Operation timed out")]
    Timeout,

    #[error("Not connected to device")]
    NotConnected,

    #[error("{0}")]
    Other(String),
}
