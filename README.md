# storz-rs

**Control your Storz & Bickel vaporizers from Rust via Bluetooth Low Energy.**

[![crates.io](https://img.shields.io/crates/v/storz-rs)](https://crates.io/crates/storz-rs)
[![docs.rs](https://img.shields.io/docsrs/storz-rs)](https://docs.rs/storz-rs)
[![CI](https://github.com/flakesonnix/storz-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/flakesonnix/storz-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A Rust library for controlling Storz & Bickel vaporizers over BLE. Built on [btleplug](https://github.com/deviceplug/btleplug) for cross-platform support. Protocol reverse-engineered from [reactive-volcano-app](https://github.com/firsttris/reactive-volcano-app).

> This fork ([rivavolt/storz-rs](https://github.com/rivavolt/storz-rs)) restructures the repo as a workspace and adds two binaries on top of the upstream library:
>
> - **`cli/`** — the `volcano` CLI: `volcano status`, `volcano temp 185`, `volcano heat on`, `volcano fill 8`, `volcano watch --json`, `volcano workflow "185:60:8,195:45:8"`, …
> - **`mcp/`** — the `volcano-mcp` MCP server (stdio, [rmcp](https://crates.io/crates/rmcp)): exposes `status`, `set_temperature`, `heater`, `pump`, `fill_bag`, `wait_for_temperature`, `run_workflow`, `device_info`, `set_brightness` as tools, holding one lazily-established, auto-reconnecting BLE session.
>
> Library additions: `discover_first()` (event-driven scan that returns on first match instead of sleeping out the scan window), `set_temperature_unit` on the Volcano Hybrid, and optional `serde` derives behind the `serde` feature.

![Storz & Bickel devices](docs/devices.png)

## Supported Devices

| Device | Tested | Notes |
|--------|--------|-------|
| Venty | ✅ | Full support: temp, heater, boost, brightness, vibration, settings |
| Volcano Hybrid | ✅ | Full support: temp, heater, pump, fan, workflow automation |
| Veazy | 🔬 | Same protocol as Venty |
| Crafty+ | 🔬 | Temp, heater, boost, LED brightness, factory reset |

> Venty and Volcano Hybrid have been verified with real hardware. Veazy and Crafty+ should work based on the shared protocol but haven't been tested yet. If you have one and want to help test, open an issue.

## Quick Start

```rust
use std::time::Duration;
use futures::StreamExt;
use storz_rs::{connect, discover_vaporizers, get_adapter};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let adapter = get_adapter().await?;
    let peripherals = discover_vaporizers(&adapter, Duration::from_secs(10)).await?;

    let device = connect(peripherals.into_iter().next().unwrap()).await?;

    device.set_target_temperature(185.0).await?;
    device.heater_on().await?;

    // Stream live state updates from BLE notifications
    let mut state = device.subscribe_state().await?;
    while let Some(s) = state.next().await {
        println!("{s}");
    }
    Ok(())
}
```

## Installation

```toml
[dependencies]
storz-rs = "0.2"
```

## API

Full docs at [docs.rs/storz-rs](https://docs.rs/storz-rs).

- `discover_vaporizers()` — BLE scan for S&B devices
- `connect()` — auto-detect model, connect, run init sequence
- `VaporizerControl` trait — uniform async API across all devices

| Method | Volcano | Venty | Veazy | Crafty |
|--------|---------|-------|-------|--------|
| `get_current_temperature` | ✅ | ✅ | ✅ | ✅ |
| `get/set_target_temperature` | ✅ | ✅ | ✅ | ✅ |
| `heater_on/off` | ✅ | ✅ | ✅ | ✅ |
| `pump_on/off` | ✅ | ❌ | ❌ | ❌ |
| `set_brightness` | ✅ | ✅ | ✅ | ✅ |
| `set_vibration` | ✅ | ✅ | ✅ | ❌ |
| `set_boost_temperature` | ❌ | ✅ | ✅ | ✅ |
| `set_auto_shutdown_timer` | ✅ | ✅ | ✅ | ❌ |
| `factory_reset` | ❌ | ✅ | ✅ | ✅ |
| `find_my_device` | ❌ | ✅ | ✅ | ❌ |
| `get_device_info` | ✅ | ✅ | ✅ | ✅ |
| `subscribe_state` | ✅ | ✅ | ✅ | ✅ |

`pump_on/off` returns `UnsupportedOperation` on devices without a pump.

## Platform Support

| Platform | BLE Backend | Status |
|----------|-------------|--------|
| Linux | BlueZ | ✅ |
| macOS | CoreBluetooth | ✅ |
| Windows | WinRT | ✅ |

Requires a BLE adapter.

## Troubleshooting

<details>
<summary>Linux: "No discovery started" / D-Bus error</summary>

BlueZ needs permissions to start BLE scans.

**Arch Linux** — no `bluetooth` group, use polkit:

```bash
sudo tee /etc/polkit-1/rules.d/50-bluetooth.rules << 'EOF'
polkit.addRule(function(action, subject) {
    if (action.id === "org.bluez.Adapter.StartDiscovery" ||
        action.id === "org.bluez.Adapter.SetDiscoveryFilter") {
        return polkit.Result.YES;
    }
});
EOF
```

**Debian/Ubuntu/Fedora** — add yourself to the `bluetooth` group:

```bash
sudo usermod -aG bluetooth $USER
```

Log out and back in after.

</details>

<details>
<summary>Linux: Bluetooth adapter not found</summary>

```bash
rfkill unblock bluetooth
bluetoothctl power on
systemctl status bluetooth
```

</details>

## Companion Client

Looking for a ready-to-use terminal app? Check out [fumar](https://github.com/flakesonnix/fumar) — a TUI + CLI client built on storz-rs.

```bash
cargo install fumar
fumar           # TUI mode
fumar --cli status  # CLI mode
```

## Contributing

Fork, branch, PR. Run before submitting:

```bash
cargo fmt && cargo clippy -- -D warnings
```

Especially interested in testing on real hardware for Volcano Hybrid, Veazy, and Crafty+.

## Disclaimer

Not affiliated with Storz & Bickel GmbH. Use at your own risk. This is unofficial reverse-engineered software.

## License

MIT — see [LICENSE](LICENSE).
