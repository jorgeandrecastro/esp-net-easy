# esp-net-easy

Abstraction asynchrone `no_std` pour simplifier la configuration WiFi et la
mise en route de la pile réseau [Embassy](https://embassy.dev/) sur les puces ESP32.

Cette crate encapsule la « tuyauterie » habituellement nécessaire pour démarrer
un client WiFi (mode Station) avec une adresse IP statique : configuration de
la pile `embassy-net`, allocation statique des ressources requises par Embassy,
et lancement des tâches de fond (gestion de la connexion WiFi et exécution de
la pile réseau).

## Versions ciblées

Cette version cible l'écosystème suivant :

| Crate               | Version |
|---------------------|---------|
| `esp-hal`           | 1.1     |
| `esp-radio`         | 0.18    |
| `esp-rtos`          | 0.3     |
| `embassy-executor`  | 0.10    |
| `embassy-net`       | 0.9     |
| `embassy-time`      | 0.5     |
| `heapless`          | 0.9     |

`esp-rtos` remplace `esp-hal-embassy`, et `esp-radio` remplace `esp-wifi`.

> ⚠️ Depuis `esp-radio` 0.18, `esp_radio::init()` et `esp_radio::Controller`
> n'existent plus. L'initialisation du contrôleur radio est intégrée
> directement dans `esp_radio::wifi::new`, qui ne prend en entrée que le
> périphérique `WIFI` de la HAL. Cette crate a été mise à jour en conséquence :
> [`init_esp_net_easy`] ne prend donc **plus** de paramètre
> `radio_controller`.

## Prérequis applicatifs

Contrairement au pilote radio, **l'initialisation du runtime Embassy
(`esp_rtos::start`) reste à la charge de l'application**, car elle nécessite
la possession d'un timer matériel qui ne peut pas être abstrait sans imposer
un câblage figé.

## Installation

```toml
[dependencies]
esp-net-easy = "0.1"
```

Activez la feature correspondant à votre puce (`esp32`, `esp32c3`, `esp32c6`
ou `esp32s3`) :

```toml
esp-net-easy = { version = "0.1", features = ["esp32c3"] }
```

## Exemple d'utilisation

```rust,ignore
#![no_std]
#![no_main]

use embassy_executor::Spawner;
use esp_hal::clock::CpuClock;
use esp_hal::rng::Rng;
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
        ssid: "MonReseauWifi",
        password: "MotDePasseSecret",
        static_ip: "192.168.1.50",
        gateway_ip: "192.168.1.1",
        dns_server: Some("8.8.8.8"),
    };

    let stack = init_esp_net_easy(&spawner, peripherals.WIFI, net_config).await;

    // `stack` est prête : on peut désormais créer des sockets TCP/UDP.
    loop {
        embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
    }
}
```

## Fonctionnement interne

1. Le pilote radio `esp-radio` initialise le contrôleur et l'interface Station
   à partir du périphérique `WIFI` fourni par l'application
   (`esp_radio::wifi::new`).
2. L'adresse IP statique, le masque `/24` et la passerelle sont assemblés en
   une configuration réseau `embassy-net` (avec un serveur DNS optionnel).
3. Une graine aléatoire 64 bits est générée via le RNG matériel de l'ESP32 pour
   initialiser la pile réseau.
4. Les ressources statiques de la pile (`StackResources`) sont allouées via
   `static_cell`, évitant toute gestion manuelle de mémoire statique côté
   application.
5. Deux tâches Embassy sont lancées en arrière-plan :
   - `net_task` fait tourner le `Runner` de la pile réseau.
   - `wifi_connection_task` configure le mode Station (SSID/mot de passe) —
     ce qui démarre implicitement le contrôleur WiFi via `set_config`
     (`esp_wifi_start` est appelé en interne, aucun `start_async` séparé n'est
     nécessaire) — puis se connecte au point d'accès et tente une
     reconnexion automatique en cas de déconnexion (`wait_for_disconnect_async`).
6. La fonction attend que la configuration réseau soit active
   (`stack.wait_config_up()`) avant de retourner la `Stack` prête à l'emploi.

## Limitations connues

- Seul le mode Station (client) avec adressage **IPv4 statique** est supporté ;
  ni le DHCP, ni le mode Access Point, ni l'IPv6 ne sont gérés pour l'instant.
- Le masque de sous-réseau est figé à `/24`.
- Les paramètres `ssid`, `password`, `static_ip` et `gateway_ip` doivent être
  des chaînes statiques (`&'static str`).
- En cas de SSID/IP invalides, la fonction `panic!` (`.unwrap()` / `.expect()`),
  ce qui est jugé acceptable pour une configuration figée au démarrage sur
  systèmes embarqués.

## Licence

GPL-2.0-or-later

Copyright (C) 2026 Jorge Andre Castro