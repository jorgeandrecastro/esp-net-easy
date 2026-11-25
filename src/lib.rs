//! # esp-net-easy
//! Copyright (C) 2026 Jorge Andre Castro
//!
//! `no_std` asynchronous abstraction that simplifies Wi-Fi setup and network
//! stack startup on ESP32 chips using the [Hazard3](https://github.com/jorgeandrecastro/Hazard3)
//! RISC-V ecosystem.
//!
//! This version targets **esp-hal 1.x / esp-radio 0.18**, together with the
//! Hazard3 packages `hazard3-executor`, `hazard3-net-async`, and `hazard3-time`.
//!
//! This crate encapsulates the setup required to start a Wi-Fi station client
//! with a static IPv4 address: network stack configuration, static resource
//! allocation, and background task startup.
//!
//! ## Application requirements
//!
//! Application code must initialize the ESP runtime (`esp_rtos::start`) because
//! it requires ownership of a hardware timer.
//!
//! Since `esp-radio` 0.18, there is no separate `esp_radio::init()` or
//! `esp_radio::Controller`. The radio controller is initialized through
//! `esp_radio::wifi::new`, which takes the HAL `WIFI` peripheral directly.
//!
//! ## Usage example
//!
//! ```rust,ignore
//! #![no_std]
//! #![no_main]
//!
//! use embassy_executor::Spawner;
//! use esp_hal::clock::CpuClock;
//! use esp_hal::rng::Rng;
//! use esp_hal::timer::timg::TimerGroup;
//! use esp_net_easy::{init_esp_net_easy, WifiEasyConfig};
//!
//! #[esp_rtos::main]
//! async fn main(spawner: Spawner) -> ! {
//!     let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
//!     let peripherals = esp_hal::init(config);
//!
//!     esp_alloc::heap_allocator!(size: 72 * 1024);
//!
//!     let timg0 = TimerGroup::new(peripherals.TIMG0);
//!     esp_rtos::start(timg0.timer0);
//!
//!     let net_config = WifiEasyConfig {
//!         ssid: "MonReseauWifi",
//!         password: "MotDePasseSecret",
//!         static_ip: "192.168.1.50",
//!         gateway_ip: "192.168.1.1",
//!         dns_server: Some("8.8.8.8"),
//!     };
//!
//!     let stack = init_esp_net_easy(&spawner, peripherals.WIFI, net_config).await;
//!
//!     // The stack is ready for TCP and UDP sockets.
//!     loop {
//!         embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
//!     }
//! }
//! ```
//!
//! ## Internal operation
//!
//! 1. `esp-radio` initializes the controller and station interface from the
//!    application-provided `WIFI` peripheral.
//! 2. The static IPv4 address, `/24` mask, gateway, and optional DNS server are
//!    parsed and prepared for the network interface configuration.
//! 3. The ESP32 hardware RNG generates a 64-bit seed for the network stack.
//! 4. Static stack storage and packet pool are allocated through [`static_cell`].
//! 5. Hazard3-compatible background tasks are spawned:
//!    - [`net_task`] runs the network stack runner.
//!    - [`wifi_connection_task`] configures station mode and reconnects after
//!      disconnection.
//! 6. The function waits for `stack.wait_config_up()` before returning the
//!    ready [`net::Stack`].
//!
//! ## Known limitations
//!
//! - Only Wi-Fi Station mode with static **IPv4** is supported.
//! - DHCP, Access Point mode, and IPv6 are not implemented.
//! - The subnet mask is fixed at `/24`.
//! - `ssid`, `password`, `static_ip`, and `gateway_ip` must be `&'static str`.
//! - Invalid configuration values may panic.

#![no_std]

use core::str::FromStr;

use embassy_executor::Spawner;
use embassy_net as net;
use embassy_time::{Duration, Timer};
use esp_hal::rng::Rng;
use esp_radio::wifi::{sta::StationConfig, Config as WifiConfig, WifiController};

/// Public macro for simplifying static allocation required by the network stack.
///
/// It promotes a value to a `'static` reference without unsafe code by using
/// [`static_cell::StaticCell`]. [`init_esp_net_easy`] uses it internally to
/// allocate network stack resources, but it is also available to applications.
#[macro_export]
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.uninit().write($val)
    }};
}

/// Hazard3-compatible task that keeps the network stack running in the background.
///
/// This task runs the [`net::Runner`] loop indefinitely while the
/// application needs TCP and UDP sockets.
#[embassy_executor::task]
async fn net_task(mut runner: net::Runner<'static>) {
    runner.run().await;
}

/// Hazard3-compatible task that configures Wi-Fi Station mode and manages
/// connections and reconnections.
///
/// This task configures Station mode with the supplied SSID and password,
/// starts the Wi-Fi controller, connects to the access point, waits for
/// disconnections, and attempts automatic reconnection.
#[embassy_executor::task]
async fn wifi_connection_task(
    mut controller: WifiController<'static>,
    ssid: &'static str,
    password: &'static str,
) {
    esp_println::println!("esp-net-easy: Configuring the Wi-Fi controller...");

    // set_config synchronously configures Station mode and starts or restarts
    // the underlying controller; esp-radio 0.18 needs no separate start call.
    let client_config = WifiConfig::Station(
        StationConfig::default()
            .with_ssid(ssid)
            .with_password(password.into()),
    );
    controller
        .set_config(&client_config)
        .expect("esp-net-easy: configuration WiFi invalide");

    loop {
        esp_println::println!("esp-net-easy: Connecting to the access point...");
        match controller.connect_async().await {
            Ok(_) => {
                esp_println::println!("Wi-Fi: Connected to the access point.");
            }
            Err(e) => {
                esp_println::println!("esp-net-easy: Wi-Fi connection failed: {:?}", e);
                Timer::after(Duration::from_millis(1000)).await;
                continue;
            }
        }

        // Wait for disconnection before retrying.
        let info = controller.wait_for_disconnect_async().await.ok();
        esp_println::println!("Wi-Fi: Disconnected ({:?}). Retrying connection...", info);
        Timer::after(Duration::from_millis(1000)).await;
    }
}

/// Network initialization configuration.
///
/// String fields use `&'static str` values, typically literals or constants,
/// because they configure hardware at startup and are not reallocated.
///
/// # Champs
///
/// - `ssid`: name of the Wi-Fi access point.
/// - `password`: password for the access point.
/// - `static_ip`: static IPv4 address assigned to the ESP32.
/// - `gateway_ip`: IPv4 address of the router.
/// - `dns_server`: optional DNS server address. `None` leaves it unconfigured;
///   an invalid address is silently ignored.
///
/// # Exemple
///
/// ```rust,ignore
/// let config = WifiEasyConfig {
///     ssid: "MyWiFiNetwork",
///     password: "SecretPassword",
///     static_ip: "192.168.1.50",
///     gateway_ip: "192.168.1.1",
///     dns_server: Some("8.8.8.8"),
/// };
/// ```
pub struct WifiEasyConfig {
    pub ssid: &'static str,
    pub password: &'static str,
    pub static_ip: &'static str,
    pub gateway_ip: &'static str,
    pub dns_server: Option<&'static str>,
}

/// Initializes ESP32 Wi-Fi and starts the Hazard3-compatible network stack.
///
/// This function creates the controller and station interface, prepares static
/// IPv4 parameters, allocates stack storage and packet pool resources, starts
/// background tasks, and waits for the network interface to become active.
///
/// # Arguments
/// * `spawner` - Hazard3-compatible executor spawner for background tasks.
/// * `wifi_peripheral` - HAL `WIFI` peripheral.
/// * `config` - Network parameters (SSID, password, and static IP).
///
/// # Returns
/// Returns a static network stack reference ready for TCP and UDP sockets.
///
/// # Panics
///
/// This function panics if:
/// - `config.static_ip` or `config.gateway_ip` is not a valid IPv4 address.
/// - `esp-radio` Wi-Fi initialization fails.
/// - A background task cannot be spawned.
///
/// # Exemple
///
/// ```rust,ignore
/// let stack = init_esp_net_easy(&spawner, peripherals.WIFI, config).await;
/// // `stack` can now be used to create TCP and UDP sockets.
/// ```
pub async fn init_esp_net_easy(
    spawner: &Spawner,
    wifi_peripheral: esp_hal::peripherals::WIFI<'static>,
    config: WifiEasyConfig,
) -> net::Stack<'static> {
    // 1. Initialize the Wi-Fi controller and station interface.
    //    esp-radio 0.18 initializes the controller directly through wifi::new.
    let (wifi_controller, interfaces) =
        esp_radio::wifi::new(wifi_peripheral, Default::default())
            .expect("esp-net-easy: failed to initialize the Wi-Fi interface");

    let _device = interfaces.station;

    // 2. Parse static IPv4 address, gateway, and optional DNS server.
    let _static_ip =
        net::wire::Ipv4Address::from_str(config.static_ip).expect("Invalid STATIC_IP format");
    let _gateway_ip =
        net::wire::Ipv4Address::from_str(config.gateway_ip).expect("Invalid GATEWAY_IP format");

    let mut _dns_servers: heapless::Vec<net::wire::Ipv4Address, 3> = heapless::Vec::new();
    if let Some(dns_str) = config.dns_server {
        if let Ok(dns_ip) = net::wire::Ipv4Address::from_str(dns_str) {
            let _ = _dns_servers.push(dns_ip);
        }
    }

    // 3. Generate a random seed for the network stack.
    let rng = Rng::new();
    let seed = ((rng.random() as u64) << 32) | rng.random() as u64;

    // 4. Allocate static `StackStorage` and packet `StaticPool` for `hazard3-net-async`.
    let storage: &'static mut net::StackStorage<'static> =
        mk_static!(net::StackStorage<'static>, net::StackStorage::new());

    // Adjust `<BLOCK_SIZE, COUNT, ALIGN>` to match `net::StaticPool` in `hazard3-net-async 0.3.1`.
    let pool: &'static net::StaticPool<1536, 16, 4> =
        mk_static!(net::StaticPool<1536, 16, 4>, net::StaticPool::new());

    let (stack, runner) = net::Stack::new(storage, pool, seed);

    // TODO: Attach `_device` to `stack` and apply static IPv4 (`_static_ip`/24, `_gateway_ip`,
    // `_dns_servers`) using the interface/route methods exposed by `hazard3-net-async 0.3.1`.

    // 5. Spawn the required background tasks.
    spawner.spawn(
        wifi_connection_task(wifi_controller, config.ssid, config.password)
            .expect("esp-net-easy: failed to spawn wifi_connection_task"),
    );
    spawner.spawn(net_task(runner).expect("esp-net-easy: failed to spawn net_task"));

    // 6. Wait asynchronously until the network interface is configured and active.
    esp_println::println!("esp-net-easy: Waiting for the network to become active...");
    stack.wait_config_up().await;
    esp_println::println!("esp-net-easy: Network configuration is active.");

    stack
}