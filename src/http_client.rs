//! HTTP client implementing [`VaporizerControl`] against a `volcano-daemon` instance, so CLIs and MCP servers can go through a daemon that holds the device's single BLE connection instead of connecting directly.

use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use futures::Stream;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::device::{DeviceInfo, DeviceModel, DeviceState};
use crate::error::StorzError;
use crate::protocol::VaporizerControl;

#[derive(serde::Deserialize)]
struct StateResponse {
    model: DeviceModel,
    #[serde(flatten)]
    state: DeviceState,
}

/// Remote vaporizer behind a `volcano-daemon` HTTP API.
pub struct HttpDevice {
    base: String,
    client: reqwest::Client,
    model: DeviceModel,
}

impl HttpDevice {
    /// Connect to a daemon at `base` (e.g. `http://watts:8814`). Fails fast if the daemon (or its device) is unreachable.
    pub async fn connect(base: &str) -> Result<Self, StorzError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| StorzError::Other(e.to_string()))?;
        let base = base.trim_end_matches('/').to_string();
        let resp: StateResponse = get_json(&client, &format!("{base}/state")).await?;
        Ok(Self { base, client, model: resp.model })
    }

    async fn state(&self) -> Result<StateResponse, StorzError> {
        get_json(&self.client, &format!("{}/state", self.base)).await
    }

    async fn post(&self, path: &str, body: &impl Serialize) -> Result<(), StorzError> {
        let resp = self
            .client
            .post(format!("{}{path}", self.base))
            .json(body)
            .send()
            .await
            .map_err(|e| StorzError::Other(format!("daemon unreachable: {e}")))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            Err(StorzError::Other(format!("daemon error {status}: {text}")))
        }
    }
}

async fn get_json<T: DeserializeOwned>(client: &reqwest::Client, url: &str) -> Result<T, StorzError> {
    let resp = client.get(url).send().await.map_err(|e| StorzError::Other(format!("daemon unreachable: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(StorzError::Other(format!("daemon error {status}: {text}")));
    }
    resp.json().await.map_err(|e| StorzError::Other(format!("bad daemon response: {e}")))
}

#[async_trait]
impl VaporizerControl for HttpDevice {
    async fn get_current_temperature(&self) -> Result<f32, StorzError> {
        self.state().await?.state.current_temp.ok_or(StorzError::NotConnected)
    }

    async fn get_target_temperature(&self) -> Result<f32, StorzError> {
        self.state().await?.state.target_temp.ok_or(StorzError::NotConnected)
    }

    async fn set_target_temperature(&self, celsius: f32) -> Result<(), StorzError> {
        self.post("/target-temp", &serde_json::json!({ "celsius": celsius })).await
    }

    async fn heater_on(&self) -> Result<(), StorzError> {
        self.post("/heater", &serde_json::json!({ "on": true })).await
    }

    async fn heater_off(&self) -> Result<(), StorzError> {
        self.post("/heater", &serde_json::json!({ "on": false })).await
    }

    async fn pump_on(&self) -> Result<(), StorzError> {
        self.post("/pump", &serde_json::json!({ "on": true })).await
    }

    async fn pump_off(&self) -> Result<(), StorzError> {
        self.post("/pump", &serde_json::json!({ "on": false })).await
    }

    async fn get_state(&self) -> Result<DeviceState, StorzError> {
        Ok(self.state().await?.state)
    }

    async fn get_settings(&self) -> Result<crate::device::DeviceSettings, StorzError> {
        get_json(&self.client, &format!("{}/config", self.base)).await
    }

    async fn set_display_on_cooling(&self, on: bool) -> Result<(), StorzError> {
        self.post("/display-on-cooling", &serde_json::json!({ "on": on })).await
    }

    async fn subscribe_state(&self) -> Result<Pin<Box<dyn Stream<Item = DeviceState> + Send>>, StorzError> {
        // Streaming responses must not be subject to the client-wide request timeout, so /events gets its own client.
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/events", self.base))
            .send()
            .await
            .map_err(|e| StorzError::Other(format!("daemon unreachable: {e}")))?;
        if !resp.status().is_success() {
            return Err(StorzError::Other(format!("daemon error {}", resp.status())));
        }
        let stream = futures::stream::unfold((resp.bytes_stream(), String::new()), |(mut bytes, mut buf)| async move {
            loop {
                if let Some(end) = buf.find("\n\n") {
                    let frame: String = buf.drain(..end + 2).collect();
                    for line in frame.lines() {
                        if let Some(data) = line.strip_prefix("data: ") {
                            if let Ok(state) = serde_json::from_str::<DeviceState>(data) {
                                return Some((state, (bytes, buf)));
                            }
                        }
                    }
                    continue;
                }
                use futures::StreamExt;
                match bytes.next().await {
                    Some(Ok(chunk)) => buf.push_str(&String::from_utf8_lossy(&chunk)),
                    _ => return None,
                }
            }
        });
        Ok(Box::pin(stream))
    }

    async fn set_brightness(&self, value: u16) -> Result<(), StorzError> {
        self.post("/brightness", &serde_json::json!({ "value": value })).await
    }

    async fn set_vibration(&self, on: bool) -> Result<(), StorzError> {
        self.post("/vibration", &serde_json::json!({ "on": on })).await
    }

    async fn set_shutoff_time(&self, seconds: u16) -> Result<(), StorzError> {
        self.post("/shutoff-time", &serde_json::json!({ "seconds": seconds })).await
    }

    async fn set_temperature_unit(&self, celsius: bool) -> Result<(), StorzError> {
        self.post("/unit", &serde_json::json!({ "celsius": celsius })).await
    }

    async fn get_device_info(&self) -> Result<DeviceInfo, StorzError> {
        get_json(&self.client, &format!("{}/info", self.base)).await
    }

    fn device_model(&self) -> DeviceModel {
        self.model
    }
}
