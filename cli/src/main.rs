use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use storz_rs::{VaporizerControl, Workflow, WorkflowRunner, WorkflowStep, connect, discover_first, get_adapter};

#[derive(Parser)]
#[command(name = "volcano", about = "Control a Storz & Bickel vaporizer over BLE", infer_subcommands = true)]
struct Cli {
    /// Device name substring or BLE address to connect to (default: first S&B device found)
    #[arg(short, long, global = true)]
    device: Option<String>,

    /// Scan timeout in seconds
    #[arg(short, long, global = true, default_value_t = 15)]
    timeout: u64,

    /// Route through a volcano-daemon instead of connecting over BLE directly, e.g. http://watts:8814
    #[arg(long, global = true, env = "VOLCANO_DAEMON")]
    daemon: Option<String>,

    /// JSON output where applicable
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show a state snapshot (temperatures, heater, pump)
    Status,
    /// Get or set the target temperature in °C
    Temp { celsius: Option<f32> },
    /// Heater control
    Heat {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Pump control (Volcano only)
    Pump {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Run the pump for a fixed duration, e.g. to fill a bag
    Fill {
        /// Total pump duration in seconds
        seconds: u64,
        /// Pulse mode: pump in bursts of this many seconds, pausing between them so the chamber reheats
        #[arg(long)]
        pulse: Option<u64>,
        /// Pause between pulses in seconds
        #[arg(long, default_value_t = 10, requires = "pulse")]
        rest: u64,
    },
    /// Stream live state updates until interrupted
    Watch,
    /// Show device info (serial, firmware, heating hours)
    Info,
    /// Set display brightness (0-100)
    Brightness { value: u16 },
    /// Vibration on touch
    Vibration {
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Set the auto-shutoff time in seconds
    Shutoff { seconds: u16 },
    /// Set the displayed temperature unit
    Unit {
        #[arg(value_parser = ["c", "f", "celsius", "fahrenheit"])]
        unit: String,
    },
    /// Show the device's settings, or set one: `config`, `config setpoint-alert off`
    Config {
        /// brightness | setpoint-alert | display-on-cooling | shutoff | unit
        key: Option<String>,
        value: Option<String>,
    },
    /// Run a workflow: comma-separated steps of TEMP:HOLD_SECS:PUMP_SECS, e.g. "185:60:8,195:45:8"
    Workflow { steps: String },
    /// Wait until the current temperature reaches the target (or a given temperature)
    WaitTemp {
        /// Temperature to wait for; defaults to the device's target
        celsius: Option<f32>,
    },
    /// Emit shell completions on stdout
    #[command(hide = true)]
    Completions { shell: clap_complete::Shell },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    {
        use clap::CommandFactory;
        clap_complete::CompleteEnv::with_factory(Cli::command).complete();
    }

    let cli = Cli::parse();

    // Completions run before any device connect, so a powered-off vaporizer never breaks shell startup.
    if let Command::Completions { shell } = cli.command {
        use clap::CommandFactory;
        clap_complete::generate(shell, &mut Cli::command(), "volcano", &mut std::io::stdout());
        return Ok(());
    }

    let device: Box<dyn VaporizerControl> = match &cli.daemon {
        Some(url) => Box::new(storz_rs::HttpDevice::connect(url).await.context("daemon connect failed")?),
        None => {
            let adapter = get_adapter().await.context("no BLE adapter")?;
            let peripheral = discover_first(&adapter, Duration::from_secs(cli.timeout), cli.device.as_deref())
                .await
                .context("no Storz & Bickel device found (is it powered on?)")?;
            connect(peripheral).await.context("connect failed")?
        }
    };

    run(&cli, device.as_ref()).await
}

async fn run(cli: &Cli, device: &dyn VaporizerControl) -> Result<()> {
    match &cli.command {
        Command::Status => {
            // Reads seed the snapshot; notification state fills in heater/pump flags shortly after connect.
            let current = device.get_current_temperature().await.ok();
            let target = device.get_target_temperature().await.ok();
            tokio::time::sleep(Duration::from_millis(300)).await;
            let mut state = device.get_state().await?;
            state.current_temp = current.or(state.current_temp);
            state.target_temp = target.or(state.target_temp);
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&state)?);
            } else {
                let fmt = |t: Option<f32>| t.map_or("?".into(), |t| format!("{t:.1}°C"));
                println!("model:   {}", device.device_model());
                println!("current: {}", fmt(state.current_temp));
                println!("target:  {}", fmt(state.target_temp));
                println!("heater:  {}", if state.heater_on { "on" } else { "off" });
                println!("pump:    {}", if state.pump_on { "on" } else { "off" });
            }
        }
        Command::Temp { celsius: None } => {
            let current = device.get_current_temperature().await?;
            let target = device.get_target_temperature().await?;
            if cli.json {
                println!("{}", serde_json::json!({"current": current, "target": target}));
            } else {
                println!("current {current:.1}°C → target {target:.1}°C");
            }
        }
        Command::Temp { celsius: Some(t) } => {
            device.set_target_temperature(*t).await?;
            println!("target set to {t:.1}°C");
        }
        Command::Heat { state } => {
            if state == "on" {
                device.heater_on().await?;
            } else {
                device.heater_off().await?;
            }
            println!("heater {state}");
        }
        Command::Pump { state } => {
            if state == "on" {
                device.pump_on().await?;
            } else {
                device.pump_off().await?;
            }
            println!("pump {state}");
        }
        Command::Fill { seconds, pulse: None, .. } => {
            device.pump_on().await?;
            tokio::time::sleep(Duration::from_secs(*seconds)).await;
            device.pump_off().await?;
            println!("pumped for {seconds}s");
        }
        Command::Fill { seconds, pulse: Some(pulse), rest } => {
            let pulse = (*pulse).max(1);
            let mut pumped = 0;
            while pumped < *seconds {
                if pumped > 0 {
                    tokio::time::sleep(Duration::from_secs(*rest)).await;
                }
                let burst = pulse.min(*seconds - pumped);
                device.pump_on().await?;
                tokio::time::sleep(Duration::from_secs(burst)).await;
                device.pump_off().await?;
                pumped += burst;
                println!("{pumped}/{seconds}s");
            }
        }
        Command::Watch => {
            let mut stream = device.subscribe_state().await?;
            while let Some(state) = stream.next().await {
                if cli.json {
                    println!("{}", serde_json::to_string(&state)?);
                } else {
                    println!("{state}");
                }
            }
        }
        Command::Info => {
            let info = device.get_device_info().await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                println!("model:    {}", device.device_model());
                if let Some(v) = &info.serial_number {
                    println!("serial:   {v}");
                }
                if let Some(v) = &info.firmware_version {
                    println!("firmware: {v}");
                }
                if let Some(v) = &info.firmware_ble_version {
                    println!("ble fw:   {v}");
                }
                if let (Some(h), Some(m)) = (info.hours_of_heating, info.minutes_of_heating) {
                    println!("heating:  {h}h {m}m");
                }
            }
        }
        Command::Brightness { value } => {
            device.set_brightness(*value).await?;
            println!("brightness {value}");
        }
        Command::Vibration { state } => {
            device.set_vibration(state == "on").await?;
            println!("vibration {state}");
        }
        Command::Shutoff { seconds } => {
            device.set_shutoff_time(*seconds).await?;
            println!("shutoff {seconds}s");
        }
        Command::Unit { unit } => {
            let celsius = unit.starts_with('c');
            device.set_temperature_unit(celsius).await?;
            println!("unit {}", if celsius { "celsius" } else { "fahrenheit" });
        }
        Command::Config { key, value } => {
            match (key.as_deref(), value.as_deref()) {
                (None, _) => {
                    let s = device.get_settings().await?;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&s)?);
                    } else {
                        let on = |b: bool| if b { "on" } else { "off" };
                        println!("brightness:         {}", s.brightness.map_or("?".into(), |v| v.to_string()));
                        println!("setpoint-alert:     {}", on(s.vibration));
                        println!("display-on-cooling: {}", on(s.display_on_cooling));
                        println!("shutoff:            {}", s.shutoff_seconds.map_or("?".into(), |v| format!("{v}s")));
                        println!("unit:               {}", if s.is_celsius { "C" } else { "F" });
                    }
                }
                (Some(k), Some(v)) => {
                    let flag = || match v {
                        "on" | "true" | "yes" => Ok(true),
                        "off" | "false" | "no" => Ok(false),
                        _ => Err(anyhow::anyhow!("{k} takes on|off, got {v}")),
                    };
                    match k {
                        "brightness" => device.set_brightness(v.parse().context("brightness takes 0-100")?).await?,
                        // The Volcano has no vibration motor: the protocol's vibration flag is the setpoint-reached alert, which the device sounds by pulsing the pump.
                        "setpoint-alert" | "vibration" => device.set_vibration(flag()?).await?,
                        "display-on-cooling" => device.set_display_on_cooling(flag()?).await?,
                        "shutoff" => device.set_shutoff_time(v.parse().context("shutoff takes seconds")?).await?,
                        "unit" => device.set_temperature_unit(matches!(v, "c" | "C" | "celsius")).await?,
                        other => anyhow::bail!("unknown setting {other}"),
                    }
                    println!("{k} set to {v}");
                }
                (Some(k), None) => anyhow::bail!("{k} needs a value"),
            }
        }
        Command::Workflow { steps } => {
            let workflow = parse_workflow(steps)?;
            println!("running workflow: {} step(s)", workflow.steps.len());
            let runner = WorkflowRunner::new();
            runner.run(device, &workflow).await?;
            println!("workflow complete");
        }
        Command::WaitTemp { celsius } => {
            let target = match celsius {
                Some(t) => *t,
                None => device.get_target_temperature().await?,
            };
            let mut stream = device.subscribe_state().await?;
            loop {
                let current = device.get_current_temperature().await?;
                if current >= target - 1.0 {
                    println!("reached {current:.1}°C");
                    break;
                }
                eprintln!("{current:.1}°C / {target:.1}°C");
                tokio::select! {
                    _ = stream.next() => {}
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
            }
        }
        Command::Completions { .. } => unreachable!("handled before device connect"),
    }
    Ok(())
}

fn parse_workflow(spec: &str) -> Result<Workflow> {
    let mut workflow = Workflow::new("cli");
    for part in spec.split(',') {
        let fields: Vec<&str> = part.trim().split(':').collect();
        if fields.len() != 3 {
            bail!("bad step {part:?}: expected TEMP:HOLD_SECS:PUMP_SECS");
        }
        workflow = workflow.add_step(WorkflowStep {
            temperature: fields[0].parse().context("bad temperature")?,
            hold_time_seconds: fields[1].parse().context("bad hold time")?,
            pump_time_seconds: fields[2].parse().context("bad pump time")?,
        });
    }
    if workflow.steps.is_empty() {
        bail!("empty workflow");
    }
    Ok(workflow)
}
