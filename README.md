
# esp-net-easy

`no_std` asynchronous abstraction that simplifies Wi-Fi setup and network
stack startup on ESP32 chips using the [Hazard3](https://github.com/jorgeandrecastro/Hazard3)
RISC-V ecosystem.

This crate packages the setup needed to start a Wi-Fi station with a static IPv4 address: network stack configuration, static stack storage and packet pool resources, and background task execution.

## Target versions

| Package | Version |
|---|---|
| `esp-hal` | 1.1 |
| `esp-radio` | 0.18 |
| `esp-rtos` | 0.3 |
| `hazard3-executor` | 0.1.2 |
| `hazard3-net-async` | 0.3.1 |
| `hazard3-time` | 0.1.3 |
| `heapless` | 0.9 |

The dependencies use crate aliases retained by Hazard3:

```toml
embassy-executor = { package = "hazard3-executor", version = "0.1.2" }
embassy-net      = { package = "hazard3-net-async", version = "0.3.1" }
embassy-time     = { package = "hazard3-time", version = "0.1.3" }
```

Rust imports such as `embassy_executor`, `embassy_net`, and `embassy_time` remain valid because Hazard3 preserves the upstream crate names, though `hazard3-net-async` exposes its own stack/pool/runner types rather than a drop-in identical `embassy-net` API.

## ESP radio initialization

Since `esp-radio` 0.18, `esp_radio::init()` and `esp_radio::Controller` no longer exist. The radio controller is initialized by `esp_radio::wifi::new`, which takes the HAL `WIFI` peripheral directly. Therefore, `init_esp_net_easy` does not need a separate radio-controller argument.

## Application requirements

The application remains responsible for starting the ESP timer/runtime with `esp_rtos::start`, because it needs exclusive ownership of a hardware timer.

## Installation

Add the crate to your `Cargo.toml`:

```toml
[dependencies]
esp-net-easy = "1.0.0"
```

Enable the feature matching the target chip (`esp32`, `esp32c3`, `esp32c6`, or `esp32s3`):

```toml
esp-net-easy = { version = "1.0.0", features = ["esp32c3"] }
```

## Usage example

```rust
#![no_std]
#![no_main]

use embassy_executor::Spawner;
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_net_easy::{init_esp_net_easy, WifiEasyConfig};

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    let net_config = WifiEasyConfig {
        ssid: "MyWiFiNetwork",
        password: "SecretPassword",
        static_ip: "192.168.1.50",
        gateway_ip: "192.168.1.1",
        dns_server: Some("8.8.8.8"),
    };

    let stack = init_esp_net_easy(&spawner, peripherals.WIFI, net_config).await;

    // The stack is ready for TCP and UDP sockets.
    loop {
        embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
    }
}
```

## Internal operation

1. `esp-radio` initializes the controller and station interface from the application-provided `WIFI` peripheral.
2. The static IPv4 address, `/24` network mask, gateway, and optional DNS server are parsed and prepared for the interface configuration.
3. The ESP32 hardware RNG generates a 64-bit seed for the network stack.
4. Static `StackStorage` and packet pool resources are allocated through `static_cell`.
5. Hazard3-compatible background tasks are spawned:
   - `net_task` runs the network stack runner.
   - `wifi_connection_task` configures station mode, connects, waits for disconnection, and reconnects automatically.
6. The function returns the initialized stack.

## Known limitations

- Only Wi-Fi Station mode with static **IPv4** is supported.
- DHCP, Access Point mode, and IPv6 are not currently implemented.
- The subnet mask is fixed at `/24`.
- `ssid`, `password`, `static_ip`, and `gateway_ip` must be `&'static str`.
- Invalid SSID or IP values may panic; this is intended for fixed startup configuration.

## License

GPL-2.0-or-later

Copyright (C) 2026 Jorge Andre Castro
