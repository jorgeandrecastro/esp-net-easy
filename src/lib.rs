//! # esp-net-easy
//! Copyright (C) 2026 Jorge Andre Castro
//!
//! Abstraction asynchrone `no_std` pour simplifier la configuration WiFi et la
//! mise en route de la pile réseau [Embassy](https://embassy.dev/) sur les puces ESP32.
//!
//! Cette version cible l'écosystème **esp-hal 1.x / esp-radio 0.18 / embassy-net 0.9 /
//! embassy-executor 0.10** (où `esp-rtos` remplace `esp-hal-embassy`, et où
//! `esp-wifi` est remplacé par `esp-radio`).
//!
//! Cette crate encapsule la « tuyauterie » habituellement nécessaire pour démarrer
//! un client WiFi (mode Station) avec une adresse IP statique : configuration de
//! la pile `embassy-net`, allocation statique des ressources requises par Embassy,
//! et lancement des tâches de fond (gestion de la connexion WiFi et exécution de
//! la pile réseau).
//!
//! ## Prérequis applicatifs
//!
//! L'initialisation du runtime Embassy (`esp_rtos::start`) reste à la charge de
//! l'application, car elle nécessite la possession d'un timer matériel qui ne
//! peut pas être abstrait sans imposer un câblage figé.
//!
//! Depuis `esp-radio` 0.18, il n'existe plus de `esp_radio::init()` ni de
//! `esp_radio::Controller` séparés : l'initialisation du contrôleur radio est
//! intégrée directement dans `esp_radio::wifi::new`, qui ne prend en entrée que
//! le périphérique `WIFI` de la HAL. Cette crate prend donc directement ce
//! périphérique en entrée.
//!
//! ## Exemple d'utilisation
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
//!     // `stack` est prête : on peut désormais créer des sockets TCP/UDP.
//!     loop {
//!         embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
//!     }
//! }
//! ```
//!
//! ## Fonctionnement interne
//!
//! 1. Le pilote radio `esp-radio` initialise le contrôleur et l'interface Station
//!    à partir du périphérique `WIFI` fourni par l'application
//!    (`esp_radio::wifi::new`).
//! 2. L'adresse IP statique, le masque `/24` et la passerelle sont assemblés en
//!    une configuration réseau `embassy-net` (avec un serveur DNS optionnel).
//! 3. Une graine aléatoire 64 bits est générée via le RNG matériel de l'ESP32 pour
//!    initialiser la pile réseau.
//! 4. Les ressources statiques de la pile (`StackResources`) sont allouées via
//!    [`static_cell`], évitant toute gestion manuelle de mémoire statique côté
//!    application.
//! 5. Deux tâches Embassy sont lancées en arrière-plan :
//!    - `[`net_task`]` fait tourner le `Runner` de la pile réseau.
//!    - `[`wifi_connection_task`]`configure le mode Station (SSID/mot de passe) —
//!      ce qui démarre implicitement le contrôleur WiFi — puis se connecte au
//!      point d'accès et tente une reconnexion automatique en cas de
//!      déconnexion.
//! 6. La fonction attend que la configuration réseau soit active
//!    (`stack.wait_config_up()`) avant de retourner la [`Stack`] prête à l'emploi.
//!
//! ## Limitations connues
//!
//! - Seul le mode Station (client) avec adressage **IPv4 statique** est supporté ;
//!   ni le DHCP, ni le mode Access Point, ni l'IPv6 ne sont gérés pour l'instant.
//! - Le masque de sous-réseau est figé à `/24`.
//! - Les paramètres `ssid`, `password`, `static_ip` et `gateway_ip` doivent être
//!   des chaînes statiques (`&'static str`).
//! - En cas de SSID/IP invalides, la fonction `panic!` (`.unwrap()` / `.expect()`),
//!   ce qui est jugé acceptable pour une configuration figée au démarrage sur
//!   systèmes embarqués.

#![no_std]

use core::str::FromStr;

use embassy_executor::Spawner;
use embassy_net::{Config as NetConfig, Ipv4Address, Ipv4Cidr, Stack, StaticConfigV4};
use embassy_time::{Duration, Timer};
use esp_hal::rng::Rng;
use esp_radio::wifi::{sta::StationConfig, Config as WifiConfig, Interface, WifiController};

/// Macro publique pour faciliter l'allocation statique requise par Embassy.
///
/// Permet de promouvoir une valeur à une référence `'static` sans `unsafe`,
/// en s'appuyant sur [`static_cell::StaticCell`]. Utilisée en interne par
/// [`init_esp_net_easy`] pour allouer les [`embassy_net::StackResources`], mais
/// reste exportée pour un usage applicatif (par exemple pour d'autres
/// ressources nécessitant une durée de vie `'static`).
#[macro_export]
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.uninit().write($val)
    }};
}

/// Tâche Embassy interne qui maintient la pile réseau active en arrière-plan.
///
/// Cette tâche exécute en boucle le [`embassy_net::Runner`] associé à la pile
/// réseau. Elle ne retourne jamais et doit rester active pendant toute la durée
/// de vie de l'application pour que les sockets TCP/UDP fonctionnent.
#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface<'static>>) {
    runner.run().await;
}

/// Tâche Embassy interne qui configure le mode Station, démarre implicitement
/// le contrôleur WiFi, et gère la connexion ainsi que les reconnexions.
///
/// Cette tâche :
/// - configure le contrôleur en mode Station avec le SSID et le mot de passe
///   fournis via [`WifiController::set_config`] — ce qui démarre (ou redémarre)
///   automatiquement le contrôleur WiFi sous-jacent (`esp_wifi_start`), sans
///   appel `start_async` séparé,
/// - tente de se connecter au point d'accès (`connect_async`),
/// - attend la déconnexion (`wait_for_disconnect_async`) et tente
///   automatiquement une reconnexion.
#[embassy_executor::task]
async fn wifi_connection_task(
    mut controller: WifiController<'static>,
    ssid: &'static str,
    password: &'static str,
) {
    esp_println::println!("esp-net-easy: Configuration du contrôleur WiFi en cours...");

    // `set_config` configure le mode Station ET démarre (ou redémarre) le
    // contrôleur de façon synchrone : aucune étape "start" séparée n'est
    // nécessaire avec esp-radio 0.18.
    let client_config = WifiConfig::Station(
        StationConfig::default()
            .with_ssid(ssid)
            .with_password(password.into()),
    );
    controller
        .set_config(&client_config)
        .expect("esp-net-easy: configuration WiFi invalide");

    loop {
        esp_println::println!("esp-net-easy: Connexion au point d'accès...");
        match controller.connect_async().await {
            Ok(_) => {
                esp_println::println!("WiFi: Connecté au point d'accès.");
            }
            Err(e) => {
                esp_println::println!("esp-net-easy: Échec de connexion WiFi: {:?}", e);
                Timer::after(Duration::from_millis(1000)).await;
                continue;
            }
        }

        // Attend la déconnexion avant de retenter une connexion.
        let info = controller.wait_for_disconnect_async().await.ok();
        esp_println::println!("WiFi: Déconnecté ({:?}). Tentative de reconnexion...", info);
        Timer::after(Duration::from_millis(1000)).await;
    }
}

/// Structure de configuration pour l'initialisation du réseau.
///
/// Tous les champs de type chaîne attendent des `&'static str` (généralement des
/// littéraux ou des constantes), car ils sont utilisés pour configurer le matériel
/// au démarrage et n'ont pas besoin d'être réalloués.
///
/// # Champs
///
/// - `ssid` : nom du point d'accès WiFi auquel se connecter.
/// - `password` : mot de passe associé au point d'accès.
/// - `static_ip` : adresse IPv4 statique à attribuer à l'ESP32 (ex. `"192.168.1.50"`).
/// - `gateway_ip` : adresse IPv4 de la passerelle/routeur (ex. `"192.168.1.1"`).
/// - `dns_server` : adresse IPv4 d'un serveur DNS optionnel (ex. `Some("8.8.8.8")`).
///   Si `None`, aucun serveur DNS n'est configuré. Si l'adresse fournie est
///   invalide, elle est silencieusement ignorée.
///
/// # Exemple
///
/// ```rust,ignore
/// let config = WifiEasyConfig {
///     ssid: "MonReseauWifi",
///     password: "MotDePasseSecret",
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

/// Initialise le WiFi de l'ESP32 et démarre la pile réseau Embassy de manière transparente.
///
/// Cette fonction crée le contrôleur et l'interface Station à partir du
/// périphérique `WIFI` fourni, configure une adresse IP statique, alloue les
/// ressources statiques requises, lance les tâches de fond d'Embassy et attend
/// que l'interface réseau soit active.
///
/// # Arguments
/// * `spawner` - Le `Spawner` d'Embassy permettant de lancer les tâches réseau en tâche de fond.
/// * `wifi_peripheral` - Le périphérique matériel `WIFI` de la HAL.
/// * `config` - Les paramètres réseau (SSID, mot de passe, IP statique).
///
/// # Retour
/// Retourne une référence statique sur la `Stack` Embassy, prête pour créer des sockets TCP/UDP.
///
/// # Panics
///
/// Cette fonction panique dans les cas suivants :
/// - `config.static_ip` ou `config.gateway_ip` n'est pas une adresse IPv4 valide
///   (échec de [`Ipv4Address::from_str`]).
/// - L'initialisation de l'interface WiFi `esp-radio` échoue (`esp_radio::wifi::new`).
/// - Le lancement des tâches `[`net_task`]` ou `[`wifi_connection_task`]` via le
///   [`Spawner`] échoue (par exemple si l'arène de tâches Embassy est pleine).
///
/// # Exemple
///
/// ```rust,ignore
/// let stack = init_esp_net_easy(&spawner, peripherals.WIFI, config).await;
/// // `stack` peut maintenant être utilisée pour créer des sockets TCP/UDP.
/// ```
pub async fn init_esp_net_easy(
    spawner: &Spawner,
    wifi_peripheral: esp_hal::peripherals::WIFI<'static>,
    config: WifiEasyConfig,
) -> Stack<'static> {
    // 1. Initialisation du contrôleur WiFi et de l'interface Station.
    //    Depuis esp-radio 0.18, `esp_radio::init()` / `esp_radio::Controller`
    //    n'existent plus : `wifi::new` ne prend que le périphérique WIFI et
    //    la configuration du contrôleur.
    let (wifi_controller, interfaces) =
        esp_radio::wifi::new(wifi_peripheral, Default::default())
            .expect("esp-net-easy: échec de l'initialisation de l'interface WiFi");

    let device = interfaces.station;

    // 2. Configuration de l'adresse IP statique et de la passerelle
    let static_ip = Ipv4Address::from_str(config.static_ip).expect("Format STATIC_IP invalide");
    let gateway_ip = Ipv4Address::from_str(config.gateway_ip).expect("Format GATEWAY_IP invalide");

    let mut dns_servers: heapless::Vec<Ipv4Address, 3> = heapless::Vec::new();
    if let Some(dns_str) = config.dns_server {
        if let Ok(dns_ip) = Ipv4Address::from_str(dns_str) {
            let _ = dns_servers.push(dns_ip);
        }
    }

    let net_config = NetConfig::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(static_ip, 24),
        gateway: Some(gateway_ip),
        dns_servers,
    });

    // 3. Génération d'une seed unique (générateur aléatoire) pour la pile réseau
    let rng = Rng::new();
    let seed = ((rng.random() as u64) << 32) | rng.random() as u64;

    // 4. Allocation statique des ressources de la pile réseau (évite la manipulation de static_cell dans le main)
    let stack_resources = mk_static!(
        embassy_net::StackResources<3>,
        embassy_net::StackResources::new()
    );

    let (stack, runner) = embassy_net::new(device, net_config, stack_resources, seed);

    // 5. Lancement des tâches asynchrones requises en tâche de fond.
    //    Depuis embassy-executor 0.10, les fonctions générées par
    //    `#[embassy_executor::task]` renvoient `Result<SpawnToken<_>, SpawnError>` ;
    //    `Spawner::spawn` prend le `SpawnToken` déballé et renvoie `()`.
    spawner.spawn(
        wifi_connection_task(wifi_controller, config.ssid, config.password)
            .expect("esp-net-easy: échec du spawn de wifi_connection_task"),
    );
    spawner.spawn(net_task(runner).expect("esp-net-easy: échec du spawn de net_task"));

    // 6. Blocage asynchrone jusqu'à ce que l'interface réseau soit configurée et active
    esp_println::println!("esp-net-easy: Attente de l'activation du réseau...");
    stack.wait_config_up().await;
    esp_println::println!("esp-net-easy: Configuration réseau validée avec succès !");

    stack
}