use std::sync::Arc;
use std::time::Duration;

use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;
use storz_rs::{HttpDevice, Workflow, WorkflowRunner, WorkflowStep, VaporizerControl};
use tokio::sync::Mutex;

/// Device access: either the lib's auto-reconnecting BLE manager, or a remote volcano-daemon holding the connection (VOLCANO_DAEMON).
enum Backend {
    Ble(storz_rs::DeviceManager),
    Daemon { url: String, cached: Mutex<Option<Arc<HttpDevice>>> },
}

struct DeviceManager {
    backend: Backend,
}

impl DeviceManager {
    fn new(filter: Option<String>, daemon: Option<String>) -> Self {
        let backend = match daemon {
            Some(url) => Backend::Daemon { url, cached: Mutex::new(None) },
            None => Backend::Ble(storz_rs::DeviceManager::new(filter)),
        };
        Self { backend }
    }

    async fn get(&self) -> Result<Arc<dyn VaporizerControl>, McpError> {
        match &self.backend {
            Backend::Ble(manager) => Ok(manager.get().await.map_err(err)?),
            Backend::Daemon { url, cached } => {
                let mut guard = cached.lock().await;
                if let Some(device) = guard.as_ref() {
                    return Ok(device.clone());
                }
                let device = Arc::new(HttpDevice::connect(url).await.map_err(err)?);
                *guard = Some(device.clone());
                Ok(device)
            }
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct TemperatureArgs {
    /// Target temperature in degrees Celsius (Volcano range: 40-230)
    celsius: f32,
}

#[derive(Deserialize, JsonSchema)]
struct SwitchArgs {
    /// true = on, false = off
    on: bool,
}

#[derive(Deserialize, JsonSchema)]
struct FillArgs {
    /// Total pump time in seconds
    seconds: u64,
    /// Pulse mode: pump in bursts of this many seconds so the chamber reheats between them (denser vapor)
    pulse_seconds: Option<u64>,
    /// Pause between pulses in seconds (default 10, only with pulse_seconds)
    rest_seconds: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct BrightnessArgs {
    /// Display brightness, 0-100
    value: u16,
}

#[derive(Deserialize, JsonSchema)]
struct WaitTempArgs {
    /// Temperature to wait for in Celsius; omit to use the device's current target
    celsius: Option<f32>,
    /// Give up after this many seconds (default 300)
    timeout_seconds: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct WorkflowStepArg {
    /// Target temperature in Celsius
    temperature: f32,
    /// Seconds to hold at temperature before pumping
    hold_seconds: u32,
    /// Seconds to run the pump
    pump_seconds: u32,
}

#[derive(Deserialize, JsonSchema)]
struct WorkflowArgs {
    /// Ordered steps to execute
    steps: Vec<WorkflowStepArg>,
}

#[derive(Clone)]
struct VolcanoServer {
    manager: Arc<DeviceManager>,
}

fn ok(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text.into())])
}

fn err(e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

#[tool_router]
impl VolcanoServer {
    fn new(filter: Option<String>, daemon: Option<String>) -> Self {
        Self { manager: Arc::new(DeviceManager::new(filter, daemon)) }
    }

    #[tool(
        description = "Get the vaporizer's current state: model, current/target temperature, heater and pump status. Connects to the device if not yet connected."
    )]
    async fn status(&self) -> Result<CallToolResult, McpError> {
        let device = self.manager.get().await?;
        let mut state = device.get_state().await.map_err(err)?;
        // Notifications keep the state fresh on a held connection; explicit reads are only needed right after connect, before the first notification lands.
        if state.current_temp.is_none() || state.target_temp.is_none() {
            state.current_temp = device.get_current_temperature().await.ok().or(state.current_temp);
            state.target_temp = device.get_target_temperature().await.ok().or(state.target_temp);
            tokio::time::sleep(Duration::from_millis(300)).await;
            let refreshed = device.get_state().await.map_err(err)?;
            state.heater_on = refreshed.heater_on;
            state.pump_on = refreshed.pump_on;
        }
        let json = serde_json::json!({
            "model": device.device_model().to_string(),
            "current_temp_c": state.current_temp,
            "target_temp_c": state.target_temp,
            "heater_on": state.heater_on,
            "pump_on": state.pump_on,
        });
        Ok(ok(json.to_string()))
    }

    #[tool(description = "Set the target temperature in degrees Celsius.")]
    async fn set_temperature(&self, Parameters(args): Parameters<TemperatureArgs>) -> Result<CallToolResult, McpError> {
        let device = self.manager.get().await?;
        device.set_target_temperature(args.celsius).await.map_err(err)?;
        Ok(ok(format!("target set to {:.1}°C", args.celsius)))
    }

    #[tool(description = "Turn the heater on or off.")]
    async fn heater(&self, Parameters(args): Parameters<SwitchArgs>) -> Result<CallToolResult, McpError> {
        let device = self.manager.get().await?;
        if args.on {
            device.heater_on().await.map_err(err)?;
        } else {
            device.heater_off().await.map_err(err)?;
        }
        Ok(ok(format!("heater {}", if args.on { "on" } else { "off" })))
    }

    #[tool(description = "Turn the air pump on or off (Volcano Hybrid only).")]
    async fn pump(&self, Parameters(args): Parameters<SwitchArgs>) -> Result<CallToolResult, McpError> {
        let device = self.manager.get().await?;
        if args.on {
            device.pump_on().await.map_err(err)?;
        } else {
            device.pump_off().await.map_err(err)?;
        }
        Ok(ok(format!("pump {}", if args.on { "on" } else { "off" })))
    }

    #[tool(
        description = "Run the pump to fill a balloon bag: either one continuous run of `seconds`, or with pulse_seconds set, bursts with reheat pauses until `seconds` of total pump time."
    )]
    async fn fill_bag(&self, Parameters(args): Parameters<FillArgs>) -> Result<CallToolResult, McpError> {
        let device = self.manager.get().await?;
        let pulse = args.pulse_seconds.unwrap_or(args.seconds).max(1);
        let rest = args.rest_seconds.unwrap_or(10);
        let mut pumped = 0;
        while pumped < args.seconds {
            if pumped > 0 {
                tokio::time::sleep(Duration::from_secs(rest)).await;
            }
            let burst = pulse.min(args.seconds - pumped);
            device.pump_on().await.map_err(err)?;
            tokio::time::sleep(Duration::from_secs(burst)).await;
            device.pump_off().await.map_err(err)?;
            pumped += burst;
        }
        Ok(ok(format!("pumped for {}s total", args.seconds)))
    }

    #[tool(
        description = "Block until the device reaches a temperature (defaults to its current target, within 1°C). Returns the reached temperature or times out."
    )]
    async fn wait_for_temperature(
        &self,
        Parameters(args): Parameters<WaitTempArgs>,
    ) -> Result<CallToolResult, McpError> {
        let device = self.manager.get().await?;
        let target = match args.celsius {
            Some(t) => t,
            None => device.get_target_temperature().await.map_err(err)?,
        };
        let timeout = Duration::from_secs(args.timeout_seconds.unwrap_or(300));
        let reached = tokio::time::timeout(timeout, async {
            loop {
                if let Ok(current) = device.get_current_temperature().await {
                    if current >= target - 1.0 {
                        return current;
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        })
        .await
        .map_err(|_| McpError::internal_error(format!("timed out waiting for {target:.1}°C"), None))?;
        Ok(ok(format!("reached {reached:.1}°C")))
    }

    #[tool(
        description = "Run a multi-step session workflow: each step heats to a temperature, holds, then pumps. Blocks until all steps complete."
    )]
    async fn run_workflow(&self, Parameters(args): Parameters<WorkflowArgs>) -> Result<CallToolResult, McpError> {
        let device = self.manager.get().await?;
        let mut workflow = Workflow::new("mcp");
        for step in args.steps {
            workflow = workflow.add_step(WorkflowStep {
                temperature: step.temperature,
                hold_time_seconds: step.hold_seconds,
                pump_time_seconds: step.pump_seconds,
            });
        }
        let count = workflow.steps.len();
        let runner = WorkflowRunner::new();
        runner.run(device.as_ref(), &workflow).await.map_err(err)?;
        Ok(ok(format!("workflow complete ({count} steps)")))
    }

    #[tool(description = "Get device info: serial number, firmware versions, total heating time.")]
    async fn device_info(&self) -> Result<CallToolResult, McpError> {
        let device = self.manager.get().await?;
        let info = device.get_device_info().await.map_err(err)?;
        Ok(ok(serde_json::to_string(&info).map_err(err)?))
    }

    #[tool(description = "Set display brightness (0-100).")]
    async fn set_brightness(&self, Parameters(args): Parameters<BrightnessArgs>) -> Result<CallToolResult, McpError> {
        let device = self.manager.get().await?;
        device.set_brightness(args.value).await.map_err(err)?;
        Ok(ok(format!("brightness {}", args.value)))
    }
}

#[tool_handler]
impl ServerHandler for VolcanoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Controls a Storz & Bickel vaporizer (Volcano Hybrid, Venty, Crafty) over Bluetooth LE. \
             The device must be powered on and in BLE range. The first tool call scans and connects; \
             the connection is reused and re-established automatically.",
        )
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    // Optional device name/address filter, e.g. `volcano-mcp VOLCANO` or VOLCANO_DEVICE env.
    let filter = std::env::args().nth(1).or_else(|| std::env::var("VOLCANO_DEVICE").ok());
    let daemon = std::env::var("VOLCANO_DAEMON").ok();

    let service = VolcanoServer::new(filter, daemon).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
