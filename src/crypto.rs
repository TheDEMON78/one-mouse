//! Canal chiffré et authentifié par mot de passe partagé.
//!
//! - Le mot de passe est transformé en clé maîtresse avec Argon2id.
//! - À chaque connexion, les deux côtés échangent 32 octets aléatoires ; une clé
//!   par sens de communication en est dérivée (HKDF-SHA256).
//! - Chaque message est chiffré avec ChaCha20-Poly1305 et un compteur comme nonce,
//!   ce qui empêche la lecture, la modification et le rejeu des messages.

use anyhow::{Context, Result, bail, ensure};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocol::{Message, PeerInfo};

pub const MIN_PASSWORD_LEN: usize = 8;
const MAGIC: &[u8; 8] = b"OMFTPC01";
const ARGON2_SALT: &[u8] = b"one-mouse-for-two-pc/psk/v1";
/// Taille maximale d'une trame chiffrée.
const MAX_FRAME_LEN: usize = 1024 * 1024;

/// Clé maîtresse dérivée du mot de passe partagé.
#[derive(Clone)]
pub struct Psk([u8; 32]);

impl Psk {
    pub fn from_password(password: &str) -> Result<Self> {
        ensure!(
            password.chars().count() >= MIN_PASSWORD_LEN,
            "le mot de passe doit faire au moins {MIN_PASSWORD_LEN} caractères"
        );
        let mut key = [0u8; 32];
        argon2::Argon2::default()
            .hash_password_into(password.as_bytes(), ARGON2_SALT, &mut key)
            .map_err(|e| anyhow::anyhow!("dérivation du mot de passe impossible : {e}"))?;
        Ok(Self(key))
    }
}

/// Qui a ouvert la connexion : sert à attribuer une clé différente à chaque sens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Listener,
    Connector,
}

pub struct SecureReader<R> {
    inner: R,
    cipher: ChaCha20Poly1305,
    counter: u64,
}

pub struct SecureWriter<W> {
    inner: W,
    cipher: ChaCha20Poly1305,
    counter: u64,
}

fn nonce(counter: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::from(bytes)
}

impl<R: AsyncRead + Unpin> SecureReader<R> {
    pub async fn recv(&mut self) -> Result<Message> {
        let len = self.inner.read_u32().await.context("connexion fermée")? as usize;
        ensure!(len <= MAX_FRAME_LEN, "trame trop grande ({len} octets)");
        let mut frame = vec![0u8; len];
        self.inner
            .read_exact(&mut frame)
            .await
            .context("connexion fermée")?;
        let plain = self
            .cipher
            .decrypt(&nonce(self.counter), frame.as_slice())
            .map_err(|_| {
                anyhow::anyhow!("message illisible : les mots de passe sont-ils identiques ?")
            })?;
        self.counter += 1;
        postcard::from_bytes(&plain)
            .context("message incompris : mettez one-mouse à jour sur les deux PC")
    }
}

impl<W: AsyncWrite + Unpin> SecureWriter<W> {
    pub async fn send(&mut self, msg: &Message) -> Result<()> {
        let plain = postcard::to_stdvec(msg)?;
        let frame = self
            .cipher
            .encrypt(&nonce(self.counter), plain.as_slice())
            .map_err(|_| anyhow::anyhow!("chiffrement impossible"))?;
        self.counter += 1;
        ensure!(frame.len() <= MAX_FRAME_LEN, "message trop grand");
        let mut buf = Vec::with_capacity(4 + frame.len());
        buf.extend_from_slice(&(frame.len() as u32).to_be_bytes());
        buf.extend_from_slice(&frame);
        self.inner.write_all(&buf).await?;
        Ok(())
    }
}

/// Chiffrement des datagrammes UDP (son) : `compteur (8 octets) ‖ chiffré`.
///
/// Le compteur sert de nonce et doit croître : un paquet rejoué ou arrivé en
/// retard est rejeté (pour le son, un paquet en retard est inutile de toute façon).
pub struct UdpCrypto {
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
    send_counter: u64,
    last_recv: Option<u64>,
}

impl UdpCrypto {
    fn new(send_key: &[u8; 32], recv_key: &[u8; 32]) -> Self {
        Self {
            send: ChaCha20Poly1305::new(Key::from_slice(send_key)),
            recv: ChaCha20Poly1305::new(Key::from_slice(recv_key)),
            send_counter: 0,
            last_recv: None,
        }
    }

    pub fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        let counter = self.send_counter;
        self.send_counter += 1;
        let cipher = self
            .send
            .encrypt(&nonce(counter), plain)
            .expect("chiffrement d'un datagramme");
        let mut packet = Vec::with_capacity(8 + cipher.len());
        packet.extend_from_slice(&counter.to_be_bytes());
        packet.extend_from_slice(&cipher);
        packet
    }

    pub fn open(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        let (head, cipher) = packet.split_first_chunk::<8>()?;
        let counter = u64::from_be_bytes(*head);
        if self.last_recv.is_some_and(|last| counter <= last) {
            return None;
        }
        let plain = self.recv.decrypt(&nonce(counter), cipher).ok()?;
        self.last_recv = Some(counter);
        Some(plain)
    }
}

/// Échange les nonces de session, dérive les clés et vérifie le mot de passe
/// grâce au message `Hello`. Renvoie le canal chiffré et ce que l'autre PC annonce.
pub async fn handshake<R, W>(
    mut reader: R,
    mut writer: W,
    role: Role,
    psk: &Psk,
    local: &PeerInfo,
) -> Result<(SecureReader<R>, SecureWriter<W>, PeerInfo, UdpCrypto)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut local_nonce = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut local_nonce);
    let mut hello = Vec::with_capacity(40);
    hello.extend_from_slice(MAGIC);
    hello.extend_from_slice(&local_nonce);
    writer.write_all(&hello).await?;

    let mut remote = [0u8; 40];
    reader
        .read_exact(&mut remote)
        .await
        .context("pas de réponse de l'autre PC")?;
    if &remote[..8] != MAGIC {
        bail!("l'autre programme n'est pas one-mouse (ou version incompatible)");
    }
    let remote_nonce = &remote[8..];

    let (listener_nonce, connector_nonce) = match role {
        Role::Listener => (&local_nonce[..], remote_nonce),
        Role::Connector => (remote_nonce, &local_nonce[..]),
    };
    let mut salt = Vec::with_capacity(64);
    salt.extend_from_slice(listener_nonce);
    salt.extend_from_slice(connector_nonce);
    let hk = Hkdf::<Sha256>::new(Some(&salt), &psk.0);
    let mut l2c = [0u8; 32];
    let mut c2l = [0u8; 32];
    hk.expand(b"listener->connector", &mut l2c)
        .expect("longueur HKDF valide");
    hk.expand(b"connector->listener", &mut c2l)
        .expect("longueur HKDF valide");
    // Clés distinctes pour les paquets UDP (son), une par sens.
    let mut udp_l2c = [0u8; 32];
    let mut udp_c2l = [0u8; 32];
    hk.expand(b"udp listener->connector", &mut udp_l2c)
        .expect("longueur HKDF valide");
    hk.expand(b"udp connector->listener", &mut udp_c2l)
        .expect("longueur HKDF valide");
    let udp = match role {
        Role::Listener => UdpCrypto::new(&udp_l2c, &udp_c2l),
        Role::Connector => UdpCrypto::new(&udp_c2l, &udp_l2c),
    };
    let (send_key, recv_key) = match role {
        Role::Listener => (l2c, c2l),
        Role::Connector => (c2l, l2c),
    };

    let mut secure_writer = SecureWriter {
        inner: writer,
        cipher: ChaCha20Poly1305::new(Key::from_slice(&send_key)),
        counter: 0,
    };
    let mut secure_reader = SecureReader {
        inner: reader,
        cipher: ChaCha20Poly1305::new(Key::from_slice(&recv_key)),
        counter: 0,
    };

    secure_writer
        .send(&Message::Hello {
            version: crate::protocol::PROTOCOL_VERSION,
            name: local.name.clone(),
            main: local.main,
        })
        .await?;
    match secure_reader.recv().await.context("connexion refusée")? {
        Message::Hello {
            version,
            name,
            main,
        } => {
            ensure!(
                version == crate::protocol::PROTOCOL_VERSION,
                "version de protocole différente (locale {}, distante {version})",
                crate::protocol::PROTOCOL_VERSION
            );
            Ok((secure_reader, secure_writer, PeerInfo { name, main }, udp))
        }
        other => bail!("message inattendu pendant la connexion : {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::WireEvent;

    fn peer(name: &str, main: bool) -> PeerInfo {
        PeerInfo {
            name: name.into(),
            main,
        }
    }

    async fn pair(
        pw_a: &str,
        pw_b: &str,
    ) -> (
        Result<(
            SecureReader<impl AsyncRead + Unpin>,
            SecureWriter<impl AsyncWrite + Unpin>,
            PeerInfo,
            UdpCrypto,
        )>,
        Result<(
            SecureReader<impl AsyncRead + Unpin>,
            SecureWriter<impl AsyncWrite + Unpin>,
            PeerInfo,
            UdpCrypto,
        )>,
    ) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let psk_a = Psk::from_password(pw_a).unwrap();
        let psk_b = Psk::from_password(pw_b).unwrap();
        let (info_a, info_b) = (peer("pc-a", true), peer("pc-b", false));
        tokio::join!(
            handshake(ar, aw, Role::Listener, &psk_a, &info_a),
            handshake(br, bw, Role::Connector, &psk_b, &info_b),
        )
    }

    #[tokio::test]
    async fn same_password_connects_and_exchanges() {
        let (a, b) = pair("mot de passe", "mot de passe").await;
        let (mut ar, mut aw, a_peer, mut a_udp) = a.unwrap();
        let (mut br, mut bw, b_peer, mut b_udp) = b.unwrap();
        assert_eq!(a_peer, peer("pc-b", false));
        assert_eq!(b_peer, peer("pc-a", true));

        let msg = Message::Input(WireEvent::Motion { dx: 4.0, dy: -2.0 });
        aw.send(&msg).await.unwrap();
        assert_eq!(br.recv().await.unwrap(), msg);
        let msg = Message::Clipboard("bonjour".into());
        bw.send(&msg).await.unwrap();
        assert_eq!(ar.recv().await.unwrap(), msg);

        // Datagrammes UDP : déchiffrés dans l'autre sens, rejeu et altération rejetés.
        let first = a_udp.seal(b"son 1");
        let second = a_udp.seal(b"son 2");
        assert_eq!(b_udp.open(&second).as_deref(), Some(&b"son 2"[..]));
        assert_eq!(b_udp.open(&first), None, "paquet en retard");
        assert_eq!(b_udp.open(&second), None, "paquet rejoué");
        let mut altered = a_udp.seal(b"son 3");
        *altered.last_mut().unwrap() ^= 1;
        assert_eq!(b_udp.open(&altered), None, "paquet modifié");
        assert_eq!(
            a_udp.open(&b_udp.seal(b"retour")).as_deref(),
            Some(&b"retour"[..])
        );
    }

    #[tokio::test]
    async fn wrong_password_is_rejected() {
        let (a, b) = pair("mot de passe", "autre mot de passe").await;
        assert!(a.is_err());
        assert!(b.is_err());
    }

    #[test]
    fn short_password_is_rejected() {
        assert!(Psk::from_password("court").is_err());
    }
}
