//! Automated workflow execution for Volcano Hybrid.
//!
//! A workflow is a sequence of steps, each specifying a target temperature,
//! a hold time, and a pump duration. The runner executes steps sequentially
//! by controlling the heater and pump through the [`VaporizerControl`] trait.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::error::StorzError;
use crate::protocol::VaporizerControl;

/// A single step in a workflow.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowStep {
    /// Target temperature in Celsius.
    pub temperature: f32,
    /// Seconds to hold at target temperature before pumping.
    pub hold_time_seconds: u32,
    /// Seconds to run the pump.
    pub pump_time_seconds: u32,
}

/// A named workflow consisting of multiple steps.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq)]
pub struct Workflow {
    /// Human-readable name.
    pub name: String,
    /// Ordered list of steps.
    pub steps: Vec<WorkflowStep>,
}

impl Workflow {
    /// Create a new workflow with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            steps: Vec::new(),
        }
    }

    /// Add a step to the workflow.
    pub fn add_step(mut self, step: WorkflowStep) -> Self {
        self.steps.push(step);
        self
    }
}

/// State of a running workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowState {
    /// Workflow is not running.
    Idle,
    /// Workflow is executing a step.
    Running,
    /// Workflow is paused.
    Paused,
    /// Workflow completed successfully.
    Completed,
    /// Workflow was stopped.
    Stopped,
    /// Workflow encountered an error.
    Error,
}

/// Executes a workflow on a vaporizer device.
pub struct WorkflowRunner {
    state: Arc<Mutex<WorkflowState>>,
    current_step: Arc<Mutex<usize>>,
}

impl WorkflowRunner {
    /// Create a new workflow runner.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(WorkflowState::Idle)),
            current_step: Arc::new(Mutex::new(0)),
        }
    }

    /// Get the current workflow state.
    pub async fn state(&self) -> WorkflowState {
        *self.state.lock().await
    }

    /// Get the current step index (0-based).
    pub async fn current_step(&self) -> usize {
        *self.current_step.lock().await
    }

    /// Execute a workflow on the given device.
    ///
    /// This runs all steps sequentially. Each step:
    /// 1. Sets the target temperature
    /// 2. Turns on the heater if needed
    /// 3. Waits until temperature is reached (within ±1°C)
    /// 4. Holds at temperature for the hold duration
    /// 5. Activates the pump for the pump duration
    ///
    /// The workflow can be stopped at any time by calling [`stop`](Self::stop).
    pub async fn run(
        &self,
        device: &dyn VaporizerControl,
        workflow: &Workflow,
    ) -> Result<(), StorzError> {
        {
            let mut state = self.state.lock().await;
            if *state == WorkflowState::Running {
                return Err(StorzError::ParseError("Workflow already running".into()));
            }
            *state = WorkflowState::Running;
        }

        *self.current_step.lock().await = 0;
        info!(
            "Starting workflow '{}' with {} steps",
            workflow.name,
            workflow.steps.len()
        );

        for (i, step) in workflow.steps.iter().enumerate() {
            // Check if we should stop
            if *self.state.lock().await != WorkflowState::Running {
                info!("Workflow '{}' stopped at step {}", workflow.name, i);
                return Ok(());
            }

            *self.current_step.lock().await = i;
            info!(
                "Workflow '{}' step {}/{}: target={}°C hold={}s pump={}s",
                workflow.name,
                i + 1,
                workflow.steps.len(),
                step.temperature,
                step.hold_time_seconds,
                step.pump_time_seconds
            );

            if let Err(e) = self.execute_step(device, step).await {
                warn!("Workflow '{}' failed at step {}: {e}", workflow.name, i);
                *self.state.lock().await = WorkflowState::Error;
                // Try to turn off pump on error
                let _ = device.pump_off().await;
                let _ = device.heater_off().await;
                return Err(e);
            }
        }

        // Turn off heater and pump after workflow completes
        let _ = device.pump_off().await;
        let _ = device.heater_off().await;

        *self.state.lock().await = WorkflowState::Completed;
        *self.current_step.lock().await = workflow.steps.len();
        info!("Workflow '{}' completed", workflow.name);
        Ok(())
    }

    async fn execute_step(
        &self,
        device: &dyn VaporizerControl,
        step: &WorkflowStep,
    ) -> Result<(), StorzError> {
        // 1. Set target temperature
        device.set_target_temperature(step.temperature).await?;
        debug!("Target temperature set to {}°C", step.temperature);

        // 2. Ensure heater is on
        device.heater_on().await?;
        tokio::time::sleep(Duration::from_millis(750)).await;

        // 3. Wait for temperature to be reached (±1°C tolerance)
        self.wait_for_temperature(device, step.temperature, 1.0)
            .await?;

        // 4. Hold at temperature
        if step.hold_time_seconds > 0 {
            debug!("Holding for {}s", step.hold_time_seconds);
            tokio::time::sleep(Duration::from_secs(step.hold_time_seconds as u64)).await;
        }

        // 5. Run pump
        if step.pump_time_seconds > 0 {
            debug!("Pumping for {}s", step.pump_time_seconds);
            device.pump_on().await?;
            tokio::time::sleep(Duration::from_secs(step.pump_time_seconds as u64)).await;
            device.pump_off().await?;
        }

        Ok(())
    }

    async fn wait_for_temperature(
        &self,
        device: &dyn VaporizerControl,
        target: f32,
        tolerance: f32,
    ) -> Result<(), StorzError> {
        const MAX_WAIT_SECS: u64 = 300;
        const POLL_INTERVAL_MS: u64 = 1500;
        let mut elapsed = 0u64;

        loop {
            if *self.state.lock().await != WorkflowState::Running {
                return Ok(());
            }

            match device.get_current_temperature().await {
                Ok(current) => {
                    if (current - target).abs() <= tolerance {
                        debug!("Temperature reached: {current}°C (target: {target}°C)");
                        return Ok(());
                    }
                }
                Err(e) => {
                    warn!("Failed to read temperature: {e}");
                }
            }

            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
            elapsed += POLL_INTERVAL_MS / 1000;

            if elapsed >= MAX_WAIT_SECS {
                return Err(StorzError::Timeout);
            }
        }
    }

    /// Pause the currently running workflow.
    pub async fn pause(&self) {
        let mut state = self.state.lock().await;
        if *state == WorkflowState::Running {
            *state = WorkflowState::Paused;
            info!("Workflow paused");
        }
    }

    /// Resume a paused workflow.
    ///
    /// Note: This resets the state to Running. The caller should call [`run`](Self::run)
    /// again with the remaining steps.
    pub async fn resume(&self) {
        let mut state = self.state.lock().await;
        if *state == WorkflowState::Paused {
            *state = WorkflowState::Running;
            info!("Workflow resumed");
        }
    }

    /// Stop the currently running workflow.
    pub async fn stop(&self, device: &dyn VaporizerControl) {
        *self.state.lock().await = WorkflowState::Stopped;
        let _ = device.pump_off().await;
        let _ = device.heater_off().await;
        info!("Workflow stopped");
    }
}

impl Default for WorkflowRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workflow_builder() {
        let workflow = Workflow::new("Test")
            .add_step(WorkflowStep {
                temperature: 180.0,
                hold_time_seconds: 10,
                pump_time_seconds: 5,
            })
            .add_step(WorkflowStep {
                temperature: 200.0,
                hold_time_seconds: 5,
                pump_time_seconds: 10,
            });

        assert_eq!(workflow.name, "Test");
        assert_eq!(workflow.steps.len(), 2);
        assert_eq!(workflow.steps[0].temperature, 180.0);
        assert_eq!(workflow.steps[1].pump_time_seconds, 10);
    }

    #[test]
    fn test_workflow_step_equality() {
        let a = WorkflowStep {
            temperature: 185.0,
            hold_time_seconds: 0,
            pump_time_seconds: 5,
        };
        let b = WorkflowStep {
            temperature: 185.0,
            hold_time_seconds: 0,
            pump_time_seconds: 5,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn test_default_workflows() {
        let balloon = Workflow::new("Balloon")
            .add_step(WorkflowStep {
                temperature: 170.0,
                hold_time_seconds: 0,
                pump_time_seconds: 5,
            })
            .add_step(WorkflowStep {
                temperature: 175.0,
                hold_time_seconds: 0,
                pump_time_seconds: 5,
            })
            .add_step(WorkflowStep {
                temperature: 180.0,
                hold_time_seconds: 0,
                pump_time_seconds: 5,
            })
            .add_step(WorkflowStep {
                temperature: 185.0,
                hold_time_seconds: 0,
                pump_time_seconds: 5,
            })
            .add_step(WorkflowStep {
                temperature: 190.0,
                hold_time_seconds: 0,
                pump_time_seconds: 5,
            })
            .add_step(WorkflowStep {
                temperature: 195.0,
                hold_time_seconds: 0,
                pump_time_seconds: 5,
            })
            .add_step(WorkflowStep {
                temperature: 200.0,
                hold_time_seconds: 0,
                pump_time_seconds: 5,
            })
            .add_step(WorkflowStep {
                temperature: 205.0,
                hold_time_seconds: 0,
                pump_time_seconds: 5,
            })
            .add_step(WorkflowStep {
                temperature: 210.0,
                hold_time_seconds: 0,
                pump_time_seconds: 5,
            })
            .add_step(WorkflowStep {
                temperature: 215.0,
                hold_time_seconds: 0,
                pump_time_seconds: 5,
            })
            .add_step(WorkflowStep {
                temperature: 220.0,
                hold_time_seconds: 0,
                pump_time_seconds: 5,
            });

        assert_eq!(balloon.steps.len(), 11);
        assert_eq!(balloon.steps[0].temperature, 170.0);
        assert_eq!(balloon.steps[10].temperature, 220.0);
    }

    #[test]
    fn test_workflow_state_transitions() {
        let runner = WorkflowRunner::new();
        assert_eq!(
            futures::executor::block_on(runner.state()),
            WorkflowState::Idle
        );
        assert_eq!(futures::executor::block_on(runner.current_step()), 0);
    }
}
