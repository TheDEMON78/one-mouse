//! Interfaces réseau : liaison Bluetooth entre les deux PC.
//!
//! Deux PC associés en Bluetooth peuvent former un « réseau personnel » (PAN) :
//! une carte réseau de plus, avec sa propre adresse IP, par laquelle one-mouse
//! passe comme par un câble. Son débit (1 à 2 Mbit/s) ne suffit pas pour le son
//! sans perte : il est alors compressé.

use std::net::IpAddr;

/// Carte réseau Bluetooth, d'après son nom : « Connexion réseau Bluetooth »
/// sous Windows, `bnep0` / `pan0` / `bn0` sous Linux.
fn is_bluetooth_interface(name: &str) -> bool {
    let lower = name.to_lowercase();
    if lower.contains("bluetooth") {
        return true;
    }
    ["bnep", "pan", "bn"].iter().any(|prefix| {
        lower
            .strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
    })
}

/// Adresse IPv4 de ce PC sur sa liaison Bluetooth, si elle est active.
pub fn bluetooth_ip() -> Option<IpAddr> {
    if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .filter(|iface| is_bluetooth_interface(&iface.name) && !iface.is_loopback())
        .map(|iface| iface.ip())
        .find(IpAddr::is_ipv4)
}

/// `true` si `local` (adresse locale d'une connexion) appartient à la carte Bluetooth.
pub fn is_bluetooth_ip(local: IpAddr) -> bool {
    if_addrs::get_if_addrs().is_ok_and(|ifaces| {
        ifaces
            .iter()
            .any(|iface| iface.ip() == local && is_bluetooth_interface(&iface.name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bluetooth_interfaces_detected() {
        for name in [
            "Connexion réseau Bluetooth",
            "Bluetooth Network Connection 2",
            "bnep0",
            "pan0",
            "bn1",
        ] {
            assert!(is_bluetooth_interface(name), "{name}");
        }
        for name in ["Ethernet", "Wi-Fi", "eth0", "wlan0", "panel", "bnx", "lo"] {
            assert!(!is_bluetooth_interface(name), "{name}");
        }
    }
}
