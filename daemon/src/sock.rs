//! Fleet-convention unix socket: newline-delimited JSON, one request line in, one reply line out; `{"op":"watch"}` turns the connection into a stream of `{"device":"volcano","ts":…,"payload":{…}}` lines, the same shape reflex's zigbee source feeds its engine.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use serde_json::{Value, json};
use storz_rs::{DeviceManager, DeviceState};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{info, warn};

pub async fn serve(manager: Arc<DeviceManager>, path: &Path) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    info!("volcano socket listening on {}", path.display());
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(handle(manager.clone(), stream));
    }
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn event_line(state: &DeviceState, model: &str) -> Value {
    let mut payload = serde_json::to_value(state).unwrap_or_else(|_| json!({}));
    payload["model"] = json!(model);
    json!({"device": "volcano", "ts": now_ts(), "payload": payload})
}

async fn handle(manager: Arc<DeviceManager>, stream: UnixStream) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = reply(
                    &mut write,
                    json!({"ok": false, "error": format!("bad json: {e}")}),
                )
                .await;
                continue;
            }
        };
        let op = req["op"].as_str().unwrap_or("");

        if op == "watch" {
            match watch(&manager, &mut write).await {
                Ok(()) => {}
                Err(e) => warn!("watch stream ended: {e}"),
            }
            return;
        }

        let resp = dispatch(&manager, op, &req)
            .await
            .unwrap_or_else(|e| json!({"ok": false, "error": e}));
        if reply(&mut write, resp).await.is_err() {
            return;
        }
    }
}

async fn reply(write: &mut (impl AsyncWriteExt + Unpin), v: Value) -> std::io::Result<()> {
    write.write_all(format!("{v}\n").as_bytes()).await
}

async fn watch(
    manager: &DeviceManager,
    write: &mut (impl AsyncWriteExt + Unpin),
) -> anyhow::Result<()> {
    let device = manager.get().await?;
    let model = device.device_model().to_string();
    // Seed the subscriber with the current state so a rule engine starting mid-session has a value immediately.
    if let Ok(state) = device.get_state().await {
        reply(write, event_line(&state, &model)).await?;
    }
    let mut stream = device.subscribe_state().await?;
    while let Some(state) = stream.next().await {
        reply(write, event_line(&state, &model)).await?;
    }
    Ok(())
}

async fn dispatch(manager: &DeviceManager, op: &str, req: &Value) -> Result<Value, String> {
    let device = manager.get().await.map_err(|e| e.to_string())?;
    let err = |e: storz_rs::StorzError| e.to_string();

    match op {
        "state" => {
            let state = device.get_state().await.map_err(err)?;
            let mut v = event_line(&state, &device.device_model().to_string());
            v["ok"] = json!(true);
            Ok(v)
        }
        "info" => {
            let info = device.get_device_info().await.map_err(err)?;
            Ok(json!({"ok": true, "info": info}))
        }
        "set-temp" => {
            let celsius = req["celsius"].as_f64().ok_or("missing celsius")? as f32;
            device.set_target_temperature(celsius).await.map_err(err)?;
            Ok(json!({"ok": true}))
        }
        "heater" => {
            let on = req["on"].as_bool().ok_or("missing on")?;
            if on {
                device.heater_on().await
            } else {
                device.heater_off().await
            }
            .map_err(err)?;
            Ok(json!({"ok": true}))
        }
        "pump" => {
            let on = req["on"].as_bool().ok_or("missing on")?;
            if on {
                device.pump_on().await
            } else {
                device.pump_off().await
            }
            .map_err(err)?;
            Ok(json!({"ok": true}))
        }
        "brightness" => {
            let value = req["value"].as_u64().ok_or("missing value")? as u16;
            device.set_brightness(value).await.map_err(err)?;
            Ok(json!({"ok": true}))
        }
        "vibration" => {
            let on = req["on"].as_bool().ok_or("missing on")?;
            device.set_vibration(on).await.map_err(err)?;
            Ok(json!({"ok": true}))
        }
        "shutoff" => {
            let seconds = req["seconds"].as_u64().ok_or("missing seconds")? as u16;
            device.set_shutoff_time(seconds).await.map_err(err)?;
            Ok(json!({"ok": true}))
        }
        "unit" => {
            let celsius = req["celsius"].as_bool().ok_or("missing celsius")?;
            device.set_temperature_unit(celsius).await.map_err(err)?;
            Ok(json!({"ok": true}))
        }
        other => Err(format!("unknown op {other:?}")),
    }
}
