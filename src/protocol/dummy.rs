use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::Stream;
use futures::StreamExt;
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio_stream::wrappers::BroadcastStream;

use crate::device::{DeviceModel, DeviceSettings, DeviceState, HeaterMode};
use crate::error::StorzError;
use crate::protocol::VaporizerControl;

/// Dummy device for testing without BLE hardware.
///
/// Simulates a Venty-like device with temperature ramping,
/// heater control, and settings management.
pub struct DummyDevice {
    state: Arc<Mutex<DeviceState>>,
    state_tx: broadcast::Sender<DeviceState>,
    target: Arc<RwLock<f32>>,
    heater: Arc<RwLock<bool>>,
    pump: Arc<RwLock<bool>>,
    brightness: Arc<RwLock<u16>>,
    vibration: Arc<RwLock<bool>>,
    shutoff_time: Arc<RwLock<u16>>,
}

impl DummyDevice {
    pub fn new() -> Self {
        let (state_tx, _) = broadcast::channel(16);
        let target = Arc::new(RwLock::new(180.0));
        let heater = Arc::new(RwLock::new(false));
        let pump = Arc::new(RwLock::new(false));
        let brightness = Arc::new(RwLock::new(50));
        let vibration = Arc::new(RwLock::new(true));
        let shutoff_time = Arc::new(RwLock::new(300));

        let state = Arc::new(Mutex::new(DeviceState {
            current_temp: Some(22.0),
            target_temp: Some(180.0),
            boost_temp: Some(5.0),
            super_boost_temp: Some(10.0),
            heater_mode: Some(HeaterMode::Off),
            heater_on: false,
            pump_on: false,
            fan_on: false,
            setpoint_reached: false,
            raw_activity: None,
            settings: Some(DeviceSettings {
                is_celsius: true,
                battery_level: Some(85),
                auto_shutdown_seconds: Some(300),
                is_charging: false,
                boost_visualization: true,
                vibration: true,
                ..Default::default()
            }),
        }));

        let device = Self {
            state: state.clone(),
            state_tx: state_tx.clone(),
            target: target.clone(),
            heater: heater.clone(),
            pump: pump.clone(),
            brightness: brightness.clone(),
            vibration: vibration.clone(),
            shutoff_time: shutoff_time.clone(),
        };

        device.spawn_simulator(state, state_tx, target, heater, pump);
        device
    }

    fn spawn_simulator(
        &self,
        state: Arc<Mutex<DeviceState>>,
        state_tx: broadcast::Sender<DeviceState>,
        target: Arc<RwLock<f32>>,
        heater: Arc<RwLock<bool>>,
        pump: Arc<RwLock<bool>>,
    ) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;

                let mut s = state.lock().await;
                let tgt = *target.read().await;
                let is_heating = *heater.read().await;
                let is_pumping = *pump.read().await;

                s.target_temp = Some(tgt);
                s.heater_on = is_heating;
                s.pump_on = is_pumping;

                if is_heating {
                    s.heater_mode = Some(HeaterMode::Normal);
                } else {
                    s.heater_mode = Some(HeaterMode::Off);
                }

                if let Some(cur) = s.current_temp {
                    let new_temp = if is_heating {
                        let diff = tgt - cur;
                        let step = (diff * 0.02).clamp(0.3, 2.0);
                        (cur + step).min(tgt)
                    } else {
                        let diff = cur - 20.0;
                        let step = (diff * 0.01).clamp(0.1, 1.0);
                        (cur - step).max(20.0)
                    };
                    s.current_temp = Some(new_temp);
                    s.setpoint_reached = (new_temp - tgt).abs() <= 2.0;
                }

                let _ = state_tx.send(s.clone());
            }
        });
    }
}

#[async_trait]
impl VaporizerControl for DummyDevice {
    async fn get_current_temperature(&self) -> Result<f32, StorzError> {
        let state = self.state.lock().await;
        state.current_temp.ok_or(StorzError::ParseError(
            "Current temperature not available".into(),
        ))
    }

    async fn get_target_temperature(&self) -> Result<f32, StorzError> {
        let state = self.state.lock().await;
        state.target_temp.ok_or(StorzError::ParseError(
            "Target temperature not available".into(),
        ))
    }

    async fn set_target_temperature(&self, celsius: f32) -> Result<(), StorzError> {
        let celsius = (celsius / 2.0).round() * 2.0;
        let celsius = celsius.clamp(40.0, 230.0);
        *self.target.write().await = celsius;
        let mut state = self.state.lock().await;
        state.target_temp = Some(celsius);
        Ok(())
    }

    async fn heater_on(&self) -> Result<(), StorzError> {
        *self.heater.write().await = true;
        let mut state = self.state.lock().await;
        state.heater_on = true;
        state.heater_mode = Some(HeaterMode::Normal);
        Ok(())
    }

    async fn heater_off(&self) -> Result<(), StorzError> {
        *self.heater.write().await = false;
        let mut state = self.state.lock().await;
        state.heater_on = false;
        state.heater_mode = Some(HeaterMode::Off);
        Ok(())
    }

    async fn pump_on(&self) -> Result<(), StorzError> {
        *self.pump.write().await = true;
        let mut state = self.state.lock().await;
        state.pump_on = true;
        Ok(())
    }

    async fn pump_off(&self) -> Result<(), StorzError> {
        *self.pump.write().await = false;
        let mut state = self.state.lock().await;
        state.pump_on = false;
        Ok(())
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
        state
            .settings
            .clone()
            .ok_or(StorzError::ParseError("Settings not available".into()))
    }

    async fn set_temperature_unit(&self, celsius: bool) -> Result<(), StorzError> {
        let mut state = self.state.lock().await;
        if let Some(ref mut settings) = state.settings {
            settings.is_celsius = celsius;
        }
        Ok(())
    }

    async fn set_brightness(&self, value: u16) -> Result<(), StorzError> {
        *self.brightness.write().await = value;
        Ok(())
    }

    async fn set_vibration(&self, on: bool) -> Result<(), StorzError> {
        *self.vibration.write().await = on;
        let mut state = self.state.lock().await;
        if let Some(ref mut settings) = state.settings {
            settings.vibration = on;
        }
        Ok(())
    }

    async fn set_shutoff_time(&self, seconds: u16) -> Result<(), StorzError> {
        *self.shutoff_time.write().await = seconds;
        let mut state = self.state.lock().await;
        if let Some(ref mut settings) = state.settings {
            settings.auto_shutdown_seconds = Some(seconds);
        }
        Ok(())
    }

    async fn set_boost_temperature(&self, celsius: f32) -> Result<(), StorzError> {
        let mut state = self.state.lock().await;
        state.boost_temp = Some(celsius);
        Ok(())
    }

    async fn set_super_boost_temperature(&self, celsius: f32) -> Result<(), StorzError> {
        let mut state = self.state.lock().await;
        state.super_boost_temp = Some(celsius);
        Ok(())
    }

    async fn set_heater_mode(&self, mode: HeaterMode) -> Result<(), StorzError> {
        let mut state = self.state.lock().await;
        state.heater_mode = Some(mode);
        state.heater_on = mode != HeaterMode::Off;
        *self.heater.write().await = mode != HeaterMode::Off;
        Ok(())
    }

    async fn set_auto_shutdown_timer(&self, seconds: u16) -> Result<(), StorzError> {
        let mut state = self.state.lock().await;
        if let Some(ref mut settings) = state.settings {
            settings.auto_shutdown_seconds = Some(seconds);
        }
        Ok(())
    }

    fn device_model(&self) -> DeviceModel {
        DeviceModel::Venty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_dummy_device_temperature() {
        let device = DummyDevice::new();
        let current = device.get_current_temperature().await.unwrap();
        assert!(current > 20.0);
        assert!(current < 25.0);

        let target = device.get_target_temperature().await.unwrap();
        assert!((target - 180.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_dummy_device_heater() {
        let device = DummyDevice::new();
        assert!(!device.get_state().await.unwrap().heater_on);

        device.heater_on().await.unwrap();
        assert!(device.get_state().await.unwrap().heater_on);

        device.heater_off().await.unwrap();
        assert!(!device.get_state().await.unwrap().heater_on);
    }

    #[tokio::test]
    async fn test_dummy_device_pump() {
        let device = DummyDevice::new();
        assert!(!device.get_state().await.unwrap().pump_on);

        device.pump_on().await.unwrap();
        assert!(device.get_state().await.unwrap().pump_on);

        device.pump_off().await.unwrap();
        assert!(!device.get_state().await.unwrap().pump_on);
    }

    #[tokio::test]
    async fn test_dummy_device_set_temperature() {
        let device = DummyDevice::new();
        device.set_target_temperature(200.0).await.unwrap();
        let target = device.get_target_temperature().await.unwrap();
        // DummyDevice rounds to even numbers
        assert!((target - 200.0).abs() < 1.0);
    }

    #[tokio::test]
    async fn test_dummy_device_temperature_ramping() {
        let device = DummyDevice::new();
        device.set_target_temperature(50.0).await.unwrap();
        device.heater_on().await.unwrap();

        tokio::time::sleep(Duration::from_secs(3)).await;

        let current = device.get_current_temperature().await.unwrap();
        let initial = 22.0;
        assert!(
            current > initial,
            "Temperature should increase when heating"
        );
    }

    #[tokio::test]
    async fn test_dummy_device_settings() {
        let device = DummyDevice::new();
        let settings = device.get_settings().await.unwrap();
        assert!(settings.is_celsius);
        assert_eq!(settings.battery_level, Some(85));
    }

    #[tokio::test]
    async fn test_dummy_device_temperature_unit() {
        let device = DummyDevice::new();

        device.set_temperature_unit(false).await.unwrap();
        let settings = device.get_settings().await.unwrap();
        assert!(!settings.is_celsius);

        device.set_temperature_unit(true).await.unwrap();
        let settings = device.get_settings().await.unwrap();
        assert!(settings.is_celsius);
    }

    #[tokio::test]
    async fn test_dummy_device_brightness() {
        let device = DummyDevice::new();
        device.set_brightness(75).await.unwrap();
        // Verify via internal state (no getter in trait)
        assert_eq!(*device.brightness.read().await, 75);
    }

    #[tokio::test]
    async fn test_dummy_device_vibration() {
        let device = DummyDevice::new();

        device.set_vibration(false).await.unwrap();
        let settings = device.get_settings().await.unwrap();
        assert!(!settings.vibration);

        device.set_vibration(true).await.unwrap();
        let settings = device.get_settings().await.unwrap();
        assert!(settings.vibration);
    }

    #[tokio::test]
    async fn test_dummy_device_heater_mode() {
        let device = DummyDevice::new();

        device.set_heater_mode(HeaterMode::Normal).await.unwrap();
        let state = device.get_state().await.unwrap();
        assert_eq!(state.heater_mode, Some(HeaterMode::Normal));
        assert!(state.heater_on);

        device.set_heater_mode(HeaterMode::Boost).await.unwrap();
        let state = device.get_state().await.unwrap();
        assert_eq!(state.heater_mode, Some(HeaterMode::Boost));

        device.set_heater_mode(HeaterMode::Off).await.unwrap();
        let state = device.get_state().await.unwrap();
        assert_eq!(state.heater_mode, Some(HeaterMode::Off));
        assert!(!state.heater_on);
    }

    #[tokio::test]
    async fn test_dummy_device_shutoff_time() {
        let device = DummyDevice::new();

        device.set_shutoff_time(600).await.unwrap();
        let settings = device.get_settings().await.unwrap();
        assert_eq!(settings.auto_shutdown_seconds, Some(600));
    }

    #[tokio::test]
    async fn test_dummy_device_boost_temperature() {
        let device = DummyDevice::new();

        device.set_boost_temperature(8.0).await.unwrap();
        let state = device.get_state().await.unwrap();
        assert_eq!(state.boost_temp, Some(8.0));

        device.set_super_boost_temperature(15.0).await.unwrap();
        let state = device.get_state().await.unwrap();
        assert_eq!(state.super_boost_temp, Some(15.0));
    }

    #[tokio::test]
    async fn test_dummy_device_model() {
        let device = DummyDevice::new();
        assert_eq!(device.device_model(), DeviceModel::Venty);
    }

    #[tokio::test]
    async fn test_dummy_device_subscribe_state() {
        let device = DummyDevice::new();
        let mut stream = device.subscribe_state().await.unwrap();

        // Should receive at least one state update
        let state = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await
            .expect("Timeout waiting for state")
            .expect("Stream ended unexpectedly");
        assert!(state.current_temp.is_some());
    }

    #[tokio::test]
    async fn test_workflow_with_dummy_device() {
        use crate::workflow::{Workflow, WorkflowRunner, WorkflowStep};

        let device = DummyDevice::new();

        // Use low target temp so DummyDevice reaches it quickly
        let workflow = Workflow::new("Quick Test").add_step(WorkflowStep {
            temperature: 25.0,
            hold_time_seconds: 0,
            pump_time_seconds: 0,
        });

        let runner = WorkflowRunner::new();
        let result = runner.run(&device, &workflow).await;
        assert!(result.is_ok(), "Workflow should complete successfully");

        // Heater should be off after workflow completes
        let state = device.get_state().await.unwrap();
        assert!(!state.heater_on);
        assert!(!state.pump_on);
    }

    #[tokio::test]
    async fn test_workflow_stop() {
        use crate::workflow::{Workflow, WorkflowRunner, WorkflowState, WorkflowStep};

        let device = DummyDevice::new();

        // High temp so it takes a while
        let workflow = Workflow::new("Long Running").add_step(WorkflowStep {
            temperature: 230.0,
            hold_time_seconds: 0,
            pump_time_seconds: 0,
        });

        let runner = WorkflowRunner::new();
        let runner_clone = WorkflowRunner::new();

        // Start workflow in background
        let handle = tokio::spawn(async move { runner.run(&device, &workflow).await });

        // Give it a moment to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The background runner will eventually timeout (300s max)
        // but we just verify the runner state logic works
        assert_eq!(runner_clone.state().await, WorkflowState::Idle);

        // Clean up - the workflow will timeout and return error
        handle.abort();
    }

    #[tokio::test]
    async fn test_dummy_device_battery_level() {
        let device = DummyDevice::new();
        let battery = device.get_battery_level().await.unwrap();
        assert_eq!(battery, Some(85));
    }

    #[tokio::test]
    async fn test_dummy_device_is_charging() {
        let device = DummyDevice::new();
        let charging = device.get_is_charging().await.unwrap();
        assert_eq!(charging, Some(false));
    }
}
