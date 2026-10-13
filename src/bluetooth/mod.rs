//! Liaison Bluetooth directe entre les deux PC, sans réseau ni adresse.
//!
//! Le PC qui attend publie un service Bluetooth one-mouse (canal série RFCOMM,
//! identifié par [`SERVICE_UUID`]) et se rend visible pour l'association. Le PC
//! qui se connecte essaie ce service sur chaque PC déjà associé : la connexion
//! se fait toute seule, il suffit d'avoir associé les deux PC une fois (le
//! système demande alors de confirmer un code sur chacun, ce qu'aucune
//! application ne peut faire à sa place).
//!
//! - Windows : sockets Bluetooth de Winsock, service publié par SDP.
//! - Linux : profil BlueZ enregistré par D-Bus.
//! - macOS : pas encore disponible (réseau personnel Bluetooth à la place).

use std::io;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;
#[cfg(not(any(windows, target_os = "linux")))]
mod unsupported;
#[cfg(not(any(windows, target_os = "linux")))]
use unsupported as imp;

pub use imp::{Server, connect_paired, local_address};

/// Autre PC dont l'adresse Bluetooth a été apprise lors d'une connexion par le
/// réseau : en Bluetooth, il est visé directement (et associé si besoin).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KnownPeer {
    pub address: u64,
    pub name: String,
}

fn known_peer_path() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("com", "BloopStudio", "one-mouse")
        .map(|dirs| dirs.config_dir().join("bluetooth-peer.json"))
}

/// Mémorise l'adresse Bluetooth de l'autre PC.
pub fn remember_peer(address: u64, name: &str) {
    let peer = KnownPeer {
        address,
        name: name.to_owned(),
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
    match saved {
        Ok(()) => log::info!(
            "adresse Bluetooth de « {name} » mémorisée ({}) : en Bluetooth, il sera visé directement",
            format_address(address)
        ),
        Err(e) => log::debug!("adresse Bluetooth non mémorisée : {e}"),
    }
}

/// Autre PC mémorisé, s'il y en a un.
pub fn known_peer() -> Option<KnownPeer> {
    let data = std::fs::read(known_peer_path()?).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Adresse Bluetooth au format « AA:BB:CC:DD:EE:FF ».
pub fn format_address(address: u64) -> String {
    let bytes = address.to_be_bytes();
    bytes[2..]
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Lit une adresse « AA:BB:CC:DD:EE:FF ».
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_address(text: &str) -> Option<u64> {
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    parts.iter().try_fold(0u64, |acc, part| {
        u8::from_str_radix(part, 16)
            .ok()
            .map(|b| (acc << 8) | u64::from(b))
    })
}

/// Identifiant du service one-mouse (« one-mouse-bloop1 » en ASCII). Sous
/// Windows, le même sous forme de GUID (`windows::SERVICE_GUID`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const SERVICE_UUID: &str = "6f6e652d-6d6f-7573-652d-626c6f6f7031";

/// Aucun PC associé : message commun aux systèmes.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
const NO_PAIRED: &str = "aucun PC associé en Bluetooth : associez les deux PC une fois \
     (bouton « Associer en Bluetooth » de one-mouse)";
/// PC associés, mais aucun n'attend avec one-mouse.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
const NOT_FOUND: &str = "aucun PC associé n'attend en Bluetooth : démarrez le partage sur \
     l'autre PC (« Attendre l'autre PC », liaison Bluetooth), à moins de 10 m";

/// Flux d'une connexion Bluetooth, utilisé comme une connexion TCP.
pub type Stream = DuplexStream;

/// Ouvre les réglages Bluetooth du système, pour associer les deux PC.
pub fn open_settings() {
    use std::process::Command;
    #[cfg(windows)]
    let _ = Command::new("explorer.exe")
        .arg("ms-settings:bluetooth")
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = Command::new("open")
        .arg("x-apple.systempreferences:com.apple.BluetoothSettings")
        .spawn();
    #[cfg(target_os = "linux")]
    {
        let candidates: [&[&str]; 3] = [
            &["gnome-control-center", "bluetooth"],
            &["systemsettings", "kcm_bluetooth"],
            &["blueman-manager"],
        ];
        for args in candidates {
            if Command::new(args[0]).args(&args[1..]).spawn().is_ok() {
                break;
            }
        }
    }
}

/// Socket bloquant (Bluetooth) lu et écrit par des threads dédiés.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
trait BlockingSocket: Send + Sync + 'static {
    fn recv(&self, buf: &mut [u8]) -> io::Result<usize>;
    fn send(&self, buf: &[u8]) -> io::Result<usize>;
    /// Coupe la connexion dans les deux sens (débloque un `recv` en cours).
    fn shutdown(&self);
}

#[cfg(unix)]
impl BlockingSocket for std::os::unix::net::UnixStream {
    fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut &*self, buf)
    }

    fn send(&self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut &*self, buf)
    }

    fn shutdown(&self) {
        let _ = std::os::unix::net::UnixStream::shutdown(self, std::net::Shutdown::Both);
    }
}

/// Relie un socket bloquant à un flux asynchrone : un thread lit le socket, un
/// autre y écrit. `keep_alive` reste en vie tant que la connexion dure (ex. la
/// connexion D-Bus qui a ouvert le socket).
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
fn bridge<S: BlockingSocket>(socket: S, keep_alive: Box<dyn Send>) -> Stream {
    const CHUNK: usize = 16 * 1024;
    let socket = Arc::new(socket);
    let (app, pipe) = tokio::io::duplex(256 * 1024);
    let (mut pipe_read, mut pipe_write) = tokio::io::split(pipe);

    // Socket → session.
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let reader = socket.clone();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match reader.recv(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if in_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    tokio::spawn(async move {
        while let Some(chunk) = in_rx.recv().await {
            if pipe_write.write_all(&chunk).await.is_err() {
                break;
            }
        }
        let _ = pipe_write.shutdown().await;
    });

    // Session → socket.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    tokio::spawn(async move {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match pipe_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    std::thread::spawn(move || {
        let _keep_alive = keep_alive;
        'chunks: while let Some(chunk) = out_rx.blocking_recv() {
            let mut sent = 0;
            while sent < chunk.len() {
                match socket.send(&chunk[sent..]) {
                    Ok(0) | Err(_) => break 'chunks,
                    Ok(n) => sent += n,
                }
            }
        }
        // Session terminée (ou socket fermé) : débloque aussi le thread de lecture.
        socket.shutdown();
    });
    app
}

#[cfg(test)]
mod address_tests {
    use super::{format_address, parse_address};

    #[test]
    fn address_roundtrip() {
        let address = 0x00_1A_7D_DA_71_13;
        assert_eq!(format_address(address), "00:1A:7D:DA:71:13");
        assert_eq!(parse_address("00:1A:7D:DA:71:13"), Some(address));
        assert_eq!(parse_address("pas une adresse"), None);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::bridge;

    #[tokio::test]
    async fn bridge_carries_data_both_ways_and_closes() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut stream = bridge(ours, Box::new(()));

        stream.write_all(b"bonjour").await.unwrap();
        let mut buf = [0u8; 7];
        let mut remote = theirs.try_clone().unwrap();
        tokio::task::spawn_blocking(move || {
            remote.read_exact(&mut buf).unwrap();
            buf
        })
        .await
        .map(|buf| assert_eq!(&buf, b"bonjour"))
        .unwrap();

        // Gros envoi dans l'autre sens : tout arrive, dans l'ordre.
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let expected = data.clone();
        let writer = std::thread::spawn(move || {
            theirs.write_all(&data).unwrap();
            // L'autre PC ferme : la session voit la fin du flux.
            drop(theirs);
        });
        let mut received = Vec::new();
        stream.read_to_end(&mut received).await.unwrap();
        writer.join().unwrap();
        assert_eq!(received, expected);
    }

    #[tokio::test]
    async fn closing_the_session_closes_the_socket() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let stream = bridge(ours, Box::new(()));
        drop(stream);
        let n = tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 8];
            theirs.read(&mut buf).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(n, 0, "l'autre extrémité doit voir la fermeture");
    }
}
