use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use storz_rs::{DeviceManager, VaporizerControl};
use tracing::info;

struct App {
    manager: DeviceManager,
}

type S = State<Arc<App>>;

struct Error(storz_rs::StorzError);

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (StatusCode::BAD_GATEWAY, self.0.to_string()).into_response()
    }
}

impl From<storz_rs::StorzError> for Error {
    fn from(e: storz_rs::StorzError) -> Self {
        Error(e)
    }
}

async fn device(app: &App) -> Result<Arc<dyn VaporizerControl>, Error> {
    Ok(app.manager.get().await?)
}

async fn state(State(app): S) -> Result<Json<serde_json::Value>, Error> {
    let device = device(&app).await?;
    let mut state = device.get_state().await?;
    // Notifications keep the state fresh on a held connection; explicit reads are only needed right after connect, before the first notification lands.
    if state.current_temp.is_none() || state.target_temp.is_none() {
        state.current_temp = device.get_current_temperature().await.ok().or(state.current_temp);
        state.target_temp = device.get_target_temperature().await.ok().or(state.target_temp);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let refreshed = device.get_state().await?;
        state.heater_on = refreshed.heater_on;
        state.pump_on = refreshed.pump_on;
    }
    let mut json = serde_json::to_value(&state).map_err(|e| Error(storz_rs::StorzError::Other(e.to_string())))?;
    json["model"] = serde_json::to_value(device.device_model())
        .map_err(|e| Error(storz_rs::StorzError::Other(e.to_string())))?;
    Ok(Json(json))
}

async fn info(State(app): S) -> Result<Json<storz_rs::DeviceInfo>, Error> {
    Ok(Json(device(&app).await?.get_device_info().await?))
}

#[derive(Deserialize)]
struct TempBody {
    celsius: f32,
}

async fn target_temp(State(app): S, Json(body): Json<TempBody>) -> Result<StatusCode, Error> {
    device(&app).await?.set_target_temperature(body.celsius).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct OnBody {
    on: bool,
}

async fn heater(State(app): S, Json(body): Json<OnBody>) -> Result<StatusCode, Error> {
    let device = device(&app).await?;
    if body.on { device.heater_on().await? } else { device.heater_off().await? }
    Ok(StatusCode::NO_CONTENT)
}

async fn pump(State(app): S, Json(body): Json<OnBody>) -> Result<StatusCode, Error> {
    let device = device(&app).await?;
    if body.on { device.pump_on().await? } else { device.pump_off().await? }
    Ok(StatusCode::NO_CONTENT)
}

async fn vibration(State(app): S, Json(body): Json<OnBody>) -> Result<StatusCode, Error> {
    device(&app).await?.set_vibration(body.on).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ValueBody {
    value: u16,
}

async fn brightness(State(app): S, Json(body): Json<ValueBody>) -> Result<StatusCode, Error> {
    device(&app).await?.set_brightness(body.value).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SecondsBody {
    seconds: u16,
}

async fn shutoff_time(State(app): S, Json(body): Json<SecondsBody>) -> Result<StatusCode, Error> {
    device(&app).await?.set_shutoff_time(body.seconds).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct UnitBody {
    celsius: bool,
}

async fn unit(State(app): S, Json(body): Json<UnitBody>) -> Result<StatusCode, Error> {
    device(&app).await?.set_temperature_unit(body.celsius).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();

    let filter = std::env::var("VOLCANO_DEVICE").ok();
    let addr = std::env::var("VOLCANO_ADDR").unwrap_or_else(|_| "0.0.0.0:8814".into());

    let app = Arc::new(App { manager: DeviceManager::new(filter) });

    // Warm the connection at startup so the first request doesn't pay the scan+connect cost; failure is fine, the manager reconnects on demand.
    if let Err(e) = app.manager.get().await {
        info!("initial connect failed (will retry on demand): {e}");
    }

    let router = Router::new()
        .route("/state", get(state))
        .route("/info", get(info))
        .route("/target-temp", post(target_temp))
        .route("/heater", post(heater))
        .route("/pump", post(pump))
        .route("/brightness", post(brightness))
        .route("/vibration", post(vibration))
        .route("/shutoff-time", post(shutoff_time))
        .route("/unit", post(unit))
        .with_state(app);

    info!("volcano-daemon listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}
