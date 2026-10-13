//! Réveil de l'autre PC à distance (Wake-on-LAN).
//!
//! Une fois connectés par le réseau, les deux PC s'échangent les adresses
//! matérielles (MAC) de leurs cartes réseau et les mémorisent. Au démarrage du
//! partage, tant que l'autre PC ne répond pas, one-mouse lui envoie le « paquet
//! magique » qui le sort de veille, sur chacune de ses cartes.
//!
//! Conditions côté PC à réveiller (« Préparer ce PC au réveil » les règle) :
//! - une carte filaire (Ethernet) : le Wi-Fi ne réveille presque jamais un PC ;
//! - le réveil par paquet magique activé sur cette carte ;
//! - sous Windows, pour un PC éteint (pas seulement en veille), le démarrage
//!   rapide désactivé, et parfois le réveil par le réseau activé dans le BIOS.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

use serde::{Deserialize, Serialize};

/// Autre PC à réveiller : adresses MAC apprises lors d'une connexion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownPeer {
    /// Cartes de l'autre PC, la plus utile en premier.
    #[serde(default)]
    pub macs: Vec<[u8; 6]>,
    /// L'une de ces cartes est filaire (réveil possible).
    #[serde(default)]
    pub wired: bool,
    pub name: String,
    /// Ancien format (une seule adresse).
    #[serde(default, skip_serializing)]
    mac: Option<[u8; 6]>,
}

fn known_peer_path() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("com", "BloopStudio", "one-mouse")
        .map(|dirs| dirs.config_dir().join("wake-peer.json"))
}

/// Autre PC mémorisé, s'il y en a un.
pub fn known_peer() -> Option<KnownPeer> {
    let mut peer: KnownPeer =
        serde_json::from_slice(&std::fs::read(known_peer_path()?).ok()?).ok()?;
    if let Some(mac) = peer.mac.take()
        && peer.macs.is_empty()
    {
        peer.macs.push(mac);
        peer.wired = true;
    }
    (!peer.macs.is_empty()).then_some(peer)
}

/// Mémorise les adresses MAC de l'autre PC.
pub fn remember_peer(macs: Vec<[u8; 6]>, wired: bool, name: &str) {
    let macs: Vec<_> = macs.into_iter().filter(|mac| *mac != [0; 6]).collect();
    if macs.is_empty() {
        return;
    }
    let peer = KnownPeer {
        macs,
        wired,
        name: name.to_owned(),
        mac: None,
    };
    if known_peer().as_ref() == Some(&peer) {
        return;
    }
    let Some(path) = known_peer_path() else {
        return;
    };
    let saved = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, serde_json::to_vec(&peer).unwrap_or_default()));
    let list = peer
        .macs
        .iter()
        .map(|mac| format_mac(*mac))
        .collect::<Vec<_>>();
    match saved {
        Ok(()) if wired => log::info!(
            "adresse de « {name} » mémorisée ({}) : il pourra être réveillé à distance",
            list.join(", ")
        ),
        Ok(()) => log::warn!(
            "adresse de « {name} » mémorisée ({}), mais il n'a pas de carte filaire : \
             un PC relié en Wi-Fi ne peut presque jamais être réveillé à distance",
            list.join(", ")
        ),
        Err(e) => log::debug!("adresses MAC non mémorisées : {e}"),
    }
}

pub fn format_mac(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Carte sans fil, d'après son nom.
fn is_wireless(name: &str) -> bool {
    let lower = name.to_lowercase();
    ["wi-fi", "wifi", "wireless", "wlan", "sans fil"]
        .iter()
        .any(|word| lower.contains(word))
        || lower.starts_with("wl")
}

/// Carte virtuelle ou tunnel (machines virtuelles, VPN, conteneurs, Bluetooth) :
/// sans intérêt pour réveiller le PC.
fn is_virtual(name: &str) -> bool {
    let lower = name.to_lowercase();
    [
        "vethernet",
        "virtual",
        "vmware",
        "vbox",
        "hyper-v",
        "docker",
        "veth",
        "virbr",
        "br-",
        "tailscale",
        "zerotier",
        "wireguard",
        "tun",
        "tap",
        "vpn",
        "loopback",
        "bluetooth",
        "bnep",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

/// Adresses MAC des cartes de ce PC qui peuvent le réveiller : la carte de la
/// connexion, puis les autres cartes physiques, les filaires en premier.
/// Renvoie aussi si l'une d'elles est filaire.
pub fn local_macs(local_ip: IpAddr) -> (Vec<[u8; 6]>, bool) {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return (Vec::new(), false);
    };
    let mut cards: Vec<(bool, bool, [u8; 6])> = Vec::new();
    for iface in ifaces {
        if iface.is_loopback() || is_virtual(&iface.name) {
            continue;
        }
        let Some(mac) = mac_address::mac_address_by_name(&iface.name)
            .ok()
            .flatten()
            .map(|m| m.bytes())
            .filter(|mac| *mac != [0; 6])
        else {
            continue;
        };
        if cards.iter().any(|&(_, _, m)| m == mac) {
            continue;
        }
        let wired = !is_wireless(&iface.name);
        let current = iface.ip() == local_ip;
        cards.push((wired, current, mac));
    }
    // Filaire d'abord, puis la carte de la connexion.
    cards.sort_by_key(|&(wired, current, _)| (!wired, !current));
    let wired = cards.iter().any(|&(wired, _, _)| wired);
    (cards.into_iter().map(|(_, _, mac)| mac).collect(), wired)
}

/// Paquet magique : 6 octets 0xFF puis l'adresse MAC répétée 16 fois.
fn magic_packet(mac: [u8; 6]) -> Vec<u8> {
    let mut packet = vec![0xFF; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&mac);
    }
    packet
}

/// Envoie le paquet magique de chaque adresse sur toutes les cartes réseau
/// (ports 9 et 7, trois fois : une carte en veille en manque parfois un).
pub fn send(macs: &[[u8; 6]]) -> std::io::Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_broadcast(true)?;
    let mut targets = vec![Ipv4Addr::BROADCAST];
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if !iface.is_loopback()
                && let if_addrs::IfAddr::V4(v4) = iface.addr
                && let Some(broadcast) = v4.broadcast
                && !targets.contains(&broadcast)
            {
                targets.push(broadcast);
            }
        }
    }
    // Avec une seule carte, l'adresse 255.255.255.255 suffit ; avec plusieurs,
    // Windows ne l'envoie que sur l'une : on vise aussi chaque réseau local, en
    // partant de chaque carte (sinon le paquet peut sortir par la mauvaise).
    let mut sockets = vec![socket];
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if !iface.is_loopback()
                && let IpAddr::V4(ip) = iface.ip()
                && let Ok(socket) = UdpSocket::bind((ip, 0))
                && socket.set_broadcast(true).is_ok()
            {
                sockets.push(socket);
            }
        }
    }
    for _ in 0..3 {
        for mac in macs {
            let packet = magic_packet(*mac);
            for socket in &sockets {
                for ip in &targets {
                    for port in [9, 7] {
                        let _ = socket.send_to(&packet, SocketAddr::new(IpAddr::V4(*ip), port));
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(())
}

/// Réveille l'autre PC mémorisé (s'il y en a un). `announce` : l'écrire au journal.
pub fn wake_known_peer(announce: bool) -> bool {
    let Some(peer) = known_peer() else {
        if announce {
            log::info!(
                "réveil à distance : aucun autre PC mémorisé (il faut s'être connecté une \
                 fois par le réseau)"
            );
        }
        return false;
    };
    match send(&peer.macs) {
        Ok(()) => {
            if announce {
                let list = peer
                    .macs
                    .iter()
                    .map(|mac| format_mac(*mac))
                    .collect::<Vec<_>>();
                log::info!(
                    "réveil de « {} » demandé ({}), au cas où il serait en veille",
                    peer.name,
                    list.join(", ")
                );
            }
            true
        }
        Err(e) => {
            log::warn!("réveil impossible : {e}");
            false
        }
    }
}

/// Règle ce PC pour qu'il puisse être réveillé à distance (demande les droits
/// d'administrateur). `None` : ce système n'est pas pris en charge.
pub fn prepare_this_pc() -> Option<anyhow::Result<()>> {
    imp::prepare()
}

#[cfg(windows)]
mod imp {
    use std::process::Stdio;

    use anyhow::{Context, bail};

    /// Sur chaque carte physique : réveil par paquet magique autorisé (et pas par
    /// n'importe quel trafic, pour ne pas réveiller le PC sans raison), réveil
    /// autorisé dans le gestionnaire de périphériques ; démarrage rapide
    /// désactivé, sans quoi un PC « éteint » ne se réveille pas.
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'SilentlyContinue'
Get-NetAdapter -Physical | ForEach-Object {
    Set-NetAdapterPowerManagement -Name $_.Name -WakeOnMagicPacket Enabled -WakeOnPattern Disabled
    Set-NetAdapterAdvancedProperty -Name $_.Name -RegistryKeyword '*WakeOnMagicPacket' -RegistryValue 1
    Set-NetAdapterAdvancedProperty -Name $_.Name -RegistryKeyword 'S5WakeOnLan' -RegistryValue 1
    Set-NetAdapterAdvancedProperty -Name $_.Name -RegistryKeyword 'WakeOnLink' -RegistryValue 0
    powercfg /deviceenablewake "$($_.InterfaceDescription)" | Out-Null
}
Set-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Power' -Name HiberbootEnabled -Value 0 -Type DWord
exit 0
"#;

    pub fn prepare() -> Option<anyhow::Result<()>> {
        Some(run())
    }

    fn run() -> anyhow::Result<()> {
        // Le script tourne dans un PowerShell élevé : une seule confirmation.
        let script = format!(
            "$p = Start-Process -FilePath powershell -ArgumentList '-NoProfile','-EncodedCommand','{}' -Verb RunAs -WindowStyle Hidden -Wait -PassThru -ErrorAction Stop; exit $p.ExitCode",
            crate::powershell::encode(SCRIPT)
        );
        let status = crate::powershell::command(&script)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("lancement de PowerShell impossible")?;
        if !status.success() {
            bail!(
                "réglage non appliqué (confirmation refusée ?, code {})",
                status.code().unwrap_or(-1)
            );
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::process::{Command, Stdio};

    use anyhow::{Context, bail};

    /// Connexions filaires de NetworkManager : réveil par paquet magique, appliqué
    /// tout de suite et gardé après redémarrage.
    pub fn prepare() -> Option<anyhow::Result<()>> {
        let output = Command::new("nmcli")
            .args([
                "-t",
                "-f",
                "NAME,TYPE,DEVICE",
                "connection",
                "show",
                "--active",
            ])
            .stdin(Stdio::null())
            .output()
            .ok()?;
        Some(run(&String::from_utf8_lossy(&output.stdout)))
    }

    fn run(active: &str) -> anyhow::Result<()> {
        let mut done = 0;
        for line in active.lines() {
            // `nmcli -t` sépare par « : » et protège ceux des noms par « \ ».
            let fields: Vec<String> = split_terse(line);
            let [name, kind, device] = fields.as_slice() else {
                continue;
            };
            if kind != "802-3-ethernet" {
                continue;
            }
            let status = Command::new("nmcli")
                .args([
                    "connection",
                    "modify",
                    name,
                    "802-3-ethernet.wake-on-lan",
                    "magic",
                ])
                .stdin(Stdio::null())
                .status()
                .context("nmcli introuvable")?;
            if !status.success() {
                bail!("réglage refusé pour la connexion « {name} »");
            }
            let _ = Command::new("nmcli")
                .args(["device", "reapply", device])
                .stdin(Stdio::null())
                .status();
            done += 1;
        }
        if done == 0 {
            bail!("aucune connexion filaire active : le Wi-Fi ne permet pas le réveil à distance");
        }
        Ok(())
    }

    fn split_terse(line: &str) -> Vec<String> {
        let mut fields = vec![String::new()];
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(next) = chars.next() {
                        fields.last_mut().expect("champ").push(next);
                    }
                }
                ':' => fields.push(String::new()),
                _ => fields.last_mut().expect("champ").push(c),
            }
        }
        fields
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn terse_fields_are_split() {
            assert_eq!(
                super::split_terse(r"Connexion\: maison:802-3-ethernet:enp3s0"),
                ["Connexion: maison", "802-3-ethernet", "enp3s0"]
            );
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub fn prepare() -> Option<anyhow::Result<()>> {
        None
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

    #[test]
    fn card_kinds() {
        assert!(is_wireless("Wi-Fi"));
        assert!(is_wireless("wlp2s0"));
        assert!(!is_wireless("Ethernet"));
        assert!(!is_wireless("enp3s0"));
        assert!(is_virtual("vEthernet (WSL)"));
        assert!(is_virtual("docker0"));
        assert!(!is_virtual("Ethernet 2"));
        assert!(!is_virtual("eth0"));
    }

    #[test]
    fn old_file_format_is_read() {
        let peer: KnownPeer =
            serde_json::from_str(r#"{"mac":[0,26,43,60,77,94],"name":"PC"}"#).unwrap();
        assert_eq!(peer.mac, Some([0, 26, 43, 60, 77, 94]));
        assert!(peer.macs.is_empty());
    }
}
