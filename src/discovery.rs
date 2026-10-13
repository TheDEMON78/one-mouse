//! Découverte automatique sur le réseau local : plus d'adresse à saisir.
//!
//! Le PC qui attend s'annonce chaque seconde par diffusion UDP (port
//! [`PORT`]). L'annonce est signée avec la clé du mot de passe : le PC qui se
//! connecte n'accepte que celles d'un PC qui a le même mot de passe, et s'y
//! connecte. Le mot de passe lui-même ne circule jamais.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;

use crate::crypto::Psk;

/// Port des annonces (le port de connexion + 1).
pub const PORT: u16 = 24851;
const MAGIC: [u8; 8] = *b"1MOUSE01";
const INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Serialize, Deserialize)]
struct Beacon {
    magic: [u8; 8],
    /// Nom du PC qui attend.
    name: String,
    /// Port TCP où s'y connecter.
    port: u16,
    /// Aléa : deux annonces ne sont jamais identiques.
    nonce: [u8; 16],
    /// Signature par la clé du mot de passe.
    tag: [u8; 16],
}

fn signed_data(name: &str, port: u16, nonce: &[u8; 16]) -> Vec<u8> {
    let mut data = Vec::with_capacity(name.len() + 18);
    data.extend_from_slice(name.as_bytes());
    data.extend_from_slice(&port.to_le_bytes());
    data.extend_from_slice(nonce);
    data
}

fn make_beacon(psk: &Psk, name: &str, port: u16) -> Vec<u8> {
    let mut nonce = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut nonce);
    let beacon = Beacon {
        magic: MAGIC,
        name: name.to_owned(),
        port,
        nonce,
        tag: psk.sign(&signed_data(name, port, &nonce)),
    };
    postcard::to_stdvec(&beacon).unwrap_or_default()
}

/// Annonce valide (bon mot de passe) : (port TCP, nom du PC).
fn check_beacon(psk: &Psk, data: &[u8]) -> Option<(u16, String)> {
    let beacon: Beacon = postcard::from_bytes(data).ok()?;
    if beacon.magic != MAGIC {
        return None;
    }
    let expected = psk.sign(&signed_data(&beacon.name, beacon.port, &beacon.nonce));
    // Comparaison sans court-circuit (temps constant).
    let same = expected
        .iter()
        .zip(beacon.tag.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0;
    same.then_some((beacon.port, beacon.name))
}

/// Adresses de diffusion de chaque carte réseau, plus la diffusion générale et
/// la boucle locale (deux instances sur le même PC).
fn targets() -> Vec<SocketAddr> {
    let mut addresses: Vec<IpAddr> = vec![
        IpAddr::V4(Ipv4Addr::BROADCAST),
        IpAddr::V4(Ipv4Addr::LOCALHOST),
    ];
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if !iface.is_loopback()
                && let if_addrs::IfAddr::V4(v4) = iface.addr
                && let Some(broadcast) = v4.broadcast
            {
                addresses.push(IpAddr::V4(broadcast));
            }
        }
    }
    addresses.dedup();
    addresses
        .into_iter()
        .map(|ip| SocketAddr::new(ip, PORT))
        .collect()
}

/// Annonce ce PC sur le réseau tant que la tâche tourne.
pub async fn announce(psk: Psk, name: String, port: u16) {
    let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await {
        Ok(socket) => socket,
        Err(e) => {
            log::warn!("annonce sur le réseau impossible : {e}");
            return;
        }
    };
    let _ = socket.set_broadcast(true);
    let mut ticker = tokio::time::interval(INTERVAL);
    loop {
        ticker.tick().await;
        let beacon = make_beacon(&psk, &name, port);
        for target in targets() {
            let _ = socket.send_to(&beacon, target).await;
        }
    }
}

/// Attend l'annonce d'un PC qui a le même mot de passe : (adresse, nom).
pub async fn find(psk: &Psk, timeout: Duration) -> Result<(SocketAddr, String)> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, PORT))
        .await
        .with_context(|| format!("écoute des annonces impossible (port UDP {PORT})"))?;
    let mut buf = [0u8; 512];
    let found = tokio::time::timeout(timeout, async {
        loop {
            let (len, from) = socket.recv_from(&mut buf).await?;
            if let Some((port, name)) = check_beacon(psk, &buf[..len]) {
                return anyhow::Ok((SocketAddr::new(from.ip(), port), name));
            }
        }
    })
    .await;
    match found {
        Ok(result) => result,
        Err(_) => bail!(
            "aucun PC one-mouse trouvé sur le réseau : démarrez le partage sur l'autre PC \
             (« Attendre l'autre PC », même mot de passe). Si cela dure, autorisez one-mouse \
             dans le pare-feu des deux PC, ou saisissez son adresse"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_needs_the_same_password() {
        let psk = Psk::from_password("motdepasse123").unwrap();
        let other = Psk::from_password("autre-mot-de-passe").unwrap();
        let beacon = make_beacon(&psk, "PC-SALON", 24850);
        assert_eq!(
            check_beacon(&psk, &beacon),
            Some((24850, "PC-SALON".to_owned()))
        );
        assert_eq!(check_beacon(&other, &beacon), None);
        // Annonce modifiée (autre port) : refusée.
        let mut forged: Beacon = postcard::from_bytes(&beacon).unwrap();
        forged.port = 4444;
        assert_eq!(
            check_beacon(&psk, &postcard::to_stdvec(&forged).unwrap()),
            None
        );
    }

    #[tokio::test]
    async fn listener_is_found_on_the_local_machine() {
        let psk = Psk::from_password("motdepasse123").unwrap();
        let announcer = tokio::spawn(announce(psk.clone(), "PC-TEST".into(), 24850));
        let (addr, name) = find(&psk, Duration::from_secs(3)).await.unwrap();
        announcer.abort();
        assert_eq!(name, "PC-TEST");
        assert_eq!(addr.port(), 24850);
    }
}
