//! Réveil de l'autre PC à distance (Wake-on-LAN).
//!
//! Une fois connectés par le réseau, les deux PC s'échangent l'adresse
//! matérielle (MAC) de leur carte réseau et la mémorisent. Au démarrage du
//! partage, tant que l'autre PC ne répond pas, one-mouse lui envoie le « paquet
//! magique » qui le sort de veille. La carte réseau de l'autre PC doit avoir le
//! réveil par le réseau activé (paramètres de la carte, parfois du BIOS).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

use serde::{Deserialize, Serialize};

/// Autre PC à réveiller : adresse MAC apprise lors d'une connexion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownPeer {
    pub mac: [u8; 6],
    pub name: String,
}

fn known_peer_path() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("com", "BloopStudio", "one-mouse")
        .map(|dirs| dirs.config_dir().join("wake-peer.json"))
}

/// Autre PC mémorisé, s'il y en a un.
pub fn known_peer() -> Option<KnownPeer> {
    serde_json::from_slice(&std::fs::read(known_peer_path()?).ok()?).ok()
}

/// Mémorise l'adresse MAC de l'autre PC.
pub fn remember_peer(mac: [u8; 6], name: &str) {
    let peer = KnownPeer {
        mac,
        name: name.to_owned(),
    };
    if mac == [0; 6] || known_peer().as_ref() == Some(&peer) {
        return;
    }
    let Some(path) = known_peer_path() else {
        return;
    };
    let saved = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, serde_json::to_vec(&peer).unwrap_or_default()));
    match saved {
        Ok(()) => log::info!(
            "adresse de « {name} » mémorisée ({}) : il pourra être réveillé à distance",
            format_mac(mac)
        ),
        Err(e) => log::debug!("adresse MAC non mémorisée : {e}"),
    }
}

pub fn format_mac(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Adresse MAC de la carte réseau qui porte cette adresse IP locale.
pub fn local_mac(local_ip: IpAddr) -> Option<[u8; 6]> {
    let iface = if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .find(|iface| iface.ip() == local_ip)?;
    let mac = mac_address::mac_address_by_name(&iface.name).ok()??.bytes();
    (mac != [0; 6]).then_some(mac)
}

/// Paquet magique : 6 octets 0xFF puis l'adresse MAC répétée 16 fois.
fn magic_packet(mac: [u8; 6]) -> Vec<u8> {
    let mut packet = vec![0xFF; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&mac);
    }
    packet
}

/// Envoie le paquet magique sur toutes les cartes réseau (ports 9 et 7).
pub fn send(mac: [u8; 6]) -> std::io::Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    let packet = magic_packet(mac);
    let mut targets = vec![Ipv4Addr::BROADCAST];
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if !iface.is_loopback()
                && let if_addrs::IfAddr::V4(v4) = iface.addr
                && let Some(broadcast) = v4.broadcast
            {
                targets.push(broadcast);
            }
        }
    }
    targets.dedup();
    for ip in targets {
        for port in [9, 7] {
            let _ = socket.send_to(&packet, SocketAddr::new(IpAddr::V4(ip), port));
        }
    }
    Ok(())
}

/// Réveille l'autre PC mémorisé (s'il y en a un). `announce` : l'écrire au journal.
pub fn wake_known_peer(announce: bool) -> bool {
    let Some(peer) = known_peer() else {
        return false;
    };
    match send(peer.mac) {
        Ok(()) => {
            if announce {
                log::info!(
                    "réveil de « {} » demandé ({}), au cas où il serait en veille",
                    peer.name,
                    format_mac(peer.mac)
                );
            }
            true
        }
        Err(e) => {
            log::debug!("réveil impossible : {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_packet_layout() {
        let mac = [0x00, 0x1A, 0x2B, 0x3C, 0x4D, 0x5E];
        let packet = magic_packet(mac);
        assert_eq!(packet.len(), 102);
        assert_eq!(&packet[..6], &[0xFF; 6]);
        assert!(packet[6..].chunks(6).all(|chunk| chunk == mac));
        assert_eq!(format_mac(mac), "00:1A:2B:3C:4D:5E");
    }
}
