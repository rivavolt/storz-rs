use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use storz_rs::{DeviceManager, VaporizerControl};
use tracing::info;

mod sock;

struct App {
    manager: Arc<DeviceManager>,
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
    json["model"] =
        serde_json::to_value(device.device_model()).map_err(|e| Error(storz_rs::StorzError::Other(e.to_string())))?;
    Ok(Json(json))
}

async fn events(State(app): S) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, Error> {
    let device = device(&app).await?;
    let stream =
        device.subscribe_state().await?.map(|state| Ok(Event::default().json_data(&state).unwrap_or_default()));
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn config(State(app): S) -> Result<Json<storz_rs::DeviceSettings>, Error> {
    Ok(Json(device(&app).await?.get_settings().await?))
}

async fn display_on_cooling(State(app): S, Json(body): Json<OnBody>) -> Result<StatusCode, Error> {
    device(&app).await?.set_display_on_cooling(body.on).await?;
    Ok(StatusCode::NO_CONTENT)
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
    if body.on {
        device.heater_on().await?
    } else {
        device.heater_off().await?
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn pump(State(app): S, Json(body): Json<OnBody>) -> Result<StatusCode, Error> {
    let device = device(&app).await?;
    if body.on {
        device.pump_on().await?
    } else {
        device.pump_off().await?
    }
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

/// Tie the display to heater activity: light it at `on_brightness` while the heater runs, dark otherwise. Follows the connection through reconnects without spinning when Bluetooth is unavailable.
async fn display_automation(app: Arc<App>, on_brightness: u16) {
    use futures::StreamExt;
    const RETRY_DELAY: Duration = Duration::from_secs(2);

    let mut last: Option<bool> = None;
    loop {
        let device = match app.manager.get().await {
            Ok(d) => d,
            Err(_) => {
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };
        let mut stream = match device.subscribe_state().await {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };
        if let Ok(state) = device.get_state().await {
            apply_display(&*device, &mut last, state.heater_on, on_brightness).await;
        }
        while let Some(state) = stream.next().await {
            apply_display(&*device, &mut last, state.heater_on, on_brightness).await;
        }
        info!("display automation: state stream ended, re-subscribing");
    }
}

async fn apply_display(device: &dyn VaporizerControl, last: &mut Option<bool>, heater_on: bool, on_brightness: u16) {
    if *last == Some(heater_on) {
        return;
    }
    let value = if heater_on { on_brightness } else { 0 };
    match device.set_brightness(value).await {
        Ok(()) => {
            info!("display automation: heater {heater_on} -> brightness {value}");
            *last = Some(heater_on);
        }
        Err(e) => info!("display automation: brightness write failed: {e}"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();

    let filter = std::env::var("VOLCANO_DEVICE").ok();
    let addr = std::env::var("VOLCANO_ADDR").unwrap_or_else(|_| "0.0.0.0:8814".into());

    let app = Arc::new(App { manager: Arc::new(DeviceManager::new(filter)) });

    // Warm the connection at startup so the first request doesn't pay the scan+connect cost; failure is fine, the manager reconnects on demand.
    if let Err(e) = app.manager.get().await {
        info!("initial connect failed (will retry on demand): {e}");
    }

    // VOLCANO_DISPLAY_AUTO=<brightness> keeps the display lit only while the heater runs.
    if let Some(on_brightness) = std::env::var("VOLCANO_DISPLAY_AUTO").ok().and_then(|v| v.parse::<u16>().ok()) {
        info!("display automation on: heater -> brightness {on_brightness}, idle -> 0");
        tokio::spawn(display_automation(app.clone(), on_brightness));
    }

    let router = Router::new()
        .route("/state", get(state))
        .route("/events", get(events))
        .route("/info", get(info))
        .route("/target-temp", post(target_temp))
        .route("/heater", post(heater))
        .route("/pump", post(pump))
        .route("/brightness", post(brightness))
        .route("/vibration", post(vibration))
        .route("/shutoff-time", post(shutoff_time))
        .route("/unit", post(unit))
        .route("/config", get(config))
        .route("/display-on-cooling", post(display_on_cooling))
        .with_state(app.clone());

    let sock_path = std::env::var("VOLCANO_SOCKET").unwrap_or_else(|_| {
        format!("{}/volcano.sock", std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()))
    });
    {
        let manager = app.manager.clone();
        let path = std::path::PathBuf::from(&sock_path);
        tokio::spawn(async move {
            if let Err(e) = sock::serve(manager, &path).await {
                info!("socket server failed: {e}");
            }
        });
    }

    info!("volcano-daemon listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}
