//! Une connexion active avec l'autre PC : capture, émulation et presse-papier.
//!
//! Les deux PC jouent le même rôle. Celui dont le curseur franchit le bord de
//! l'écran du côté de l'autre PC devient « émetteur » : ses entrées locales sont
//! capturées et envoyées, l'autre PC les rejoue. Quand le curseur rejoué atteint
//! le bord du retour, le récepteur envoie `Leave` et l'émetteur rend la main.
//!
//! Son : par défaut, le PC contrôlé envoie son son au PC qui le contrôle, et le
//! micro du PC qui contrôle peut être envoyé au PC contrôlé (voir `audio::Mode`).

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use input_capture::{CaptureEvent, Position};
use input_emulation::InputEmulation;
use input_event::{Event, KeyboardEvent, PointerEvent, scancode};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;

use crate::audio::{self, AudioCapture, AudioConfig, AudioFrame, AudioPlayer, Source};
use crate::capture::Capture;
use crate::clipboard::ClipboardSync;
use crate::codec::{AudioDecoder, AudioEncoder, Quality};
use crate::crypto::{self, Psk, Role, UdpCrypto};
use crate::default_mic;
use crate::protocol::{AudioFormat, AudioKind, MAX_CLIPBOARD_LEN, Message, PeerInfo};
use crate::status::{self, LinkStatus, MicStatus, Status};

/// Identifiant de l'unique autre PC (la V1 gère une seule connexion).
const PEER: u64 = 0;
const PING_INTERVAL: Duration = Duration::from_secs(5);
/// Sans message de l'autre PC pendant cette durée, la connexion est considérée perdue.
const PEER_TIMEOUT: Duration = Duration::from_secs(15);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Intervalle des datagrammes de test tant que l'UDP n'est pas confirmé.
const PROBE_INTERVAL: Duration = Duration::from_millis(500);

/// Raccourci de secours pour reprendre la main : Ctrl + Maj + Alt + Méta (Windows/Cmd).
const RELEASE_KEYS: [scancode::Linux; 4] = [
    scancode::Linux::KeyLeftCtrl,
    scancode::Linux::KeyLeftShift,
    scancode::Linux::KeyLeftAlt,
    scancode::Linux::KeyLeftMeta,
];

/// Ressources locales, conservées d'une connexion à l'autre.
pub struct Local {
    /// `None` si ce PC ne peut que recevoir (ex. Linux sous X11).
    pub capture: Option<Capture>,
    pub emulation: InputEmulation,
    pub clipboard: Option<ClipboardSync>,
    pub psk: Psk,
    pub name: String,
    /// Position de l'autre PC par rapport à celui-ci.
    pub peer_side: Position,
    /// Décalage du bureau de l'autre PC par rapport à celui-ci, le long du bord
    /// partagé (en pixels ; positif : vers le bas ou la droite).
    pub peer_offset: i32,
    pub audio: AudioConfig,
    /// Multiplicateur des mouvements de souris envoyés à l'autre PC.
    pub pointer_speed: f64,
    /// Fichiers à envoyer à l'autre PC (glissés sur la fenêtre).
    pub files: Option<mpsc::Receiver<std::path::PathBuf>>,
    /// Verrouiller ce PC quand l'autre se verrouille (et inversement).
    pub lock_sync: bool,
    /// Pas de mise en veille tant que les deux PC sont connectés.
    pub keep_awake: bool,
    /// Réveiller l'autre PC (Wake-on-LAN) au démarrage, s'il ne répond pas.
    pub wake: bool,
    /// Garder l'historique des copies (affiché par la fenêtre).
    pub clipboard_history: bool,
}

#[derive(Default)]
struct State {
    /// Nom de l'autre PC.
    peer_name: String,
    /// Écrans de l'autre PC.
    peer_monitors: Vec<crate::screens::Monitor>,
    /// L'autre PC est le PC principal (souris, clavier, casque).
    peer_main: bool,
    /// Nos entrées sont capturées et envoyées à l'autre PC.
    sending: bool,
    /// L'autre PC nous envoie ses entrées.
    receiving: bool,
    /// Multiplicateur des mouvements envoyés.
    pointer_speed: f64,
    /// Fractions de pixel pas encore envoyées (reportées au mouvement suivant).
    motion_rest: (f64, f64),
    /// Dernier PC à avoir pris la main (`true` : ce PC). Sans PC principal coché,
    /// c'est lui qui en tient lieu : le son de l'autre lui parvient en permanence.
    we_controlled_last: Option<bool>,
    /// En cas de côtés incohérents, c'est le réglage de ce PC qui fait foi.
    side_authority: bool,
    /// Fichiers en cours de réception.
    incoming: crate::transfer::Incoming,
    /// Liaison lente (Bluetooth) : mouvements de souris regroupés.
    coalesce_motion: bool,
    /// Mouvement accumulé, pas encore envoyé (pixels entiers).
    pending_motion: (f64, f64),
    /// Dernière prise de main par l'autre PC / dernier retour du curseur ici :
    /// un passage en sens inverse juste après est ignoré (anti-rebond).
    entered_at: Option<Instant>,
    returned_at: Option<Instant>,
}

/// Caractéristiques de la liaison établie.
struct Link {
    /// File des fichiers envoyés.
    files: mpsc::Sender<Message>,
    quality: Quality,
    /// La liaison passe par le Bluetooth (son compressé).
    via_bluetooth: bool,
    /// En cas de côtés incohérents, le réglage de ce PC fait foi.
    side_authority: bool,
}

/// Paquets de son en attente au plus sur la connexion (~100 ms en Opus).
const STREAM_AUDIO_QUEUE: usize = 10;

/// En Bluetooth, intervalle d'envoi des mouvements de souris regroupés (125 par
/// seconde, comme une souris classique) : une souris à 1 000 Hz saturerait la
/// liaison, d'où des à-coups.
const MOTION_FLUSH: Duration = Duration::from_millis(8);

/// Une connexion a été établie depuis le démarrage (le réveil à distance s'arrête).
static CONNECTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn ever_connected() -> bool {
    CONNECTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Fréquence de vérification du verrouillage de ce PC.
const LOCK_CHECK: Duration = Duration::from_secs(1);
/// Verrouillage demandé par l'autre PC : pas renvoyé pendant ce délai.
const LOCK_ECHO: Duration = Duration::from_secs(10);

/// Durée pendant laquelle un passage en sens inverse est ignoré.
const EDGE_COOLDOWN: Duration = Duration::from_millis(300);

fn side_code(side: Position) -> u8 {
    match side {
        Position::Left => 0,
        Position::Right => 1,
        Position::Top => 2,
        Position::Bottom => 3,
    }
}

/// Côté où l'autre PC voit ce PC, à partir de son code : le côté opposé.
fn mirrored_side(code: u8) -> Option<Position> {
    Some(match code {
        0 => Position::Right,
        1 => Position::Left,
        2 => Position::Bottom,
        3 => Position::Top,
        _ => return None,
    })
}

pub async fn run(stream: TcpStream, role: Role, local: &mut Local) -> Result<()> {
    stream.set_nodelay(true)?;
    let peer_ip = stream.peer_addr()?.ip();
    // Réseau personnel Bluetooth : la connexion passe par la carte Bluetooth.
    // Sinon, même avec l'option Bluetooth cochée, c'est le réseau local : son
    // sans perte.
    let local_ip = stream.local_addr()?.ip();
    let via_bluetooth = crate::network::is_bluetooth_ip(local_ip);
    let mac = Some(crate::wake::local_macs(local_ip));
    run_on(stream, Some(peer_ip), via_bluetooth, mac, role, local).await
}

/// Session sur n'importe quel flux : connexion TCP, ou lien Bluetooth direct
/// (`peer_ip` absent : pas d'UDP, le son passe par le flux). `via_bluetooth` :
/// la liaison passe réellement par le Bluetooth, le son est alors compressé pour
/// tenir dans son débit.
pub async fn run_on<S>(
    stream: S,
    peer_ip: Option<std::net::IpAddr>,
    via_bluetooth: bool,
    mac: Option<(Vec<[u8; 6]>, bool)>,
    role: Role,
    local: &mut Local,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    let (mut reader, mut writer, peer, udp_crypto) = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        crypto::handshake(
            reader,
            writer,
            role,
            &local.psk,
            &PeerInfo {
                name: local.name.clone(),
                main: local.audio.main,
            },
        ),
    )
    .await
    .context("l'autre PC n'a pas répondu à temps")??;
    log::info!(
        "connecté à « {} » (situé à {}{})",
        peer.name,
        side_name(local.peer_side),
        if peer.main { ", PC principal" } else { "" }
    );
    if via_bluetooth {
        log::info!("liaison Bluetooth : son compressé (Opus) pour tenir dans son débit");
    }
    let link = (local.audio.quality, via_bluetooth);

    let (out_tx, mut out_rx) = mpsc::channel::<Message>(1024);
    // Son passé par la connexion (UDP bloqué, Bluetooth) : file courte et moins
    // prioritaire. Si la liaison est trop lente, le son en trop est perdu au lieu
    // de retarder la souris et le clavier.
    let (audio_tx, mut audio_rx) = mpsc::channel::<Message>(STREAM_AUDIO_QUEUE);
    // Fichiers : file la moins prioritaire ; l'envoi attend qu'elle se vide.
    let (files_tx, mut files_rx) = mpsc::channel::<Message>(4);
    let (in_tx, mut in_rx) = mpsc::channel::<Message>(1024);
    let write_task = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                biased;
                msg = out_rx.recv() => msg,
                Some(msg) = audio_rx.recv() => Some(msg),
                Some(msg) = files_rx.recv() => Some(msg),
            };
            let Some(msg) = msg else { break };
            writer.send(&msg).await?;
        }
        anyhow::Ok(())
    });
    let read_task = tokio::spawn(async move {
        loop {
            let msg = tokio::time::timeout(PEER_TIMEOUT, reader.recv())
                .await
                .context("l'autre PC ne répond plus")??;
            if in_tx.send(msg).await.is_err() {
                return anyhow::Ok(());
            }
        }
    });

    if let Some(clipboard) = &mut local.clipboard {
        clipboard.discard_pending();
    }
    if let Some(capture) = &mut local.capture {
        capture.create(PEER, local.peer_side).await?;
    }
    local.emulation.create(PEER).await;

    // Le son passe par UDP (un paquet perdu ne bloque rien), en repli par TCP.
    let local_ip = if peer_ip.is_some_and(|ip| ip.is_ipv6()) {
        std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
    } else {
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
    };
    let socket = UdpSocket::bind((local_ip, 0)).await?;
    if peer_ip.is_some() {
        send(&out_tx, Message::UdpPort(socket.local_addr()?.port())).await?;
    }
    let udp = Udp {
        socket,
        crypto: udp_crypto,
        stream_audio: audio_tx,
        peer_ip,
        peer_addr: None,
        send_ok: false,
        recv_ok: false,
    };

    // Adresse Bluetooth de ce PC : l'autre la mémorise pour pouvoir s'y relier
    // en Bluetooth plus tard, sans recherche.
    if let Ok(Some(address)) =
        tokio::time::timeout(Duration::from_secs(2), crate::bluetooth::local_address()).await
    {
        send(&out_tx, Message::BluetoothAddress(address)).await?;
    }
    // Adresse MAC de ce PC : l'autre pourra le réveiller à distance.
    if let Some((macs, wired)) = mac
        && !macs.is_empty()
    {
        send(&out_tx, Message::WakeAddresses { macs, wired }).await?;
    }
    CONNECTED.store(true, std::sync::atomic::Ordering::Relaxed);
    status::emit(Status::Connected(peer.name.clone()));
    status::emit_link(if via_bluetooth {
        LinkStatus::Bluetooth
    } else {
        LinkStatus::Tcp
    });
    // Côtés : chaque PC dit où il voit l'autre. S'ils ne sont pas opposés, le PC
    // principal fait foi (ou, sans PC principal, celui qui s'est connecté).
    send(
        &out_tx,
        Message::Layout {
            side: side_code(local.peer_side),
            offset: local.peer_offset,
        },
    )
    .await?;
    send(&out_tx, Message::Screens(crate::screens::local_monitors())).await?;
    let side_authority = if local.audio.main != peer.main {
        local.audio.main
    } else {
        role == Role::Connector
    };
    let link = Link {
        quality: link.0,
        via_bluetooth,
        side_authority,
        files: files_tx,
    };
    let result = event_loop(local, peer, udp, &out_tx, &mut in_rx, link).await;

    // Toujours rendre le curseur et relâcher les touches, même en cas d'erreur.
    if let Some(capture) = &mut local.capture {
        let _ = capture.release().await;
        let _ = capture.destroy(PEER).await;
    }
    local.emulation.destroy(PEER).await;
    write_task.abort();

    match result {
        Ok(()) => match read_task.await {
            Ok(Err(e)) => Err(e),
            _ => Ok(()),
        },
        Err(e) => {
            read_task.abort();
            Err(e)
        }
    }
}

async fn event_loop(
    local: &mut Local,
    peer: PeerInfo,
    mut udp: Udp,
    out: &mpsc::Sender<Message>,
    incoming: &mut mpsc::Receiver<Message>,
    link: Link,
) -> Result<()> {
    let Link {
        quality,
        via_bluetooth,
        side_authority,
        files: files_out,
    } = link;
    let mut next_file_id: u32 = 0;
    let link = (quality, via_bluetooth);
    let mut state = State {
        peer_name: peer.name,
        peer_main: peer.main,
        pointer_speed: local.pointer_speed,
        side_authority,
        coalesce_motion: via_bluetooth,
        ..State::default()
    };
    if local.audio.main && state.peer_main {
        log::warn!(
            "les deux PC sont cochés « PC principal » : le son ne suit que le contrôle. \
             Cochez-le seulement sur le PC où sont branchés la souris et le casque"
        );
    }
    let mut audio = AudioState::default();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    let mut probe = tokio::time::interval(PROBE_INTERVAL);
    let mut motion_flush = tokio::time::interval(MOTION_FLUSH);
    motion_flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut datagram = vec![0u8; 65_536];
    let mut defaults = audio::watch_defaults();
    // Verrouillage commun et anti-veille pendant la connexion.
    let power = crate::power::Power::new(local.keep_awake).await;
    let mut was_locked = power.is_locked().await;
    let mut locked_by_peer: Option<Instant> = None;
    let mut lock_check = tokio::time::interval(LOCK_CHECK);
    // Un seul curseur visible : caché ici quand la souris est sur l'autre écran.
    let mut cursor = crate::cursor::CursorHider::new();

    loop {
        audio.sync(&local.audio, &state);
        cursor.set_hidden(cursor_hidden(&state));
        tokio::select! {
            event = next_capture(&mut local.capture) => {
                let Some(event) = event else { bail!("la capture des entrées s'est arrêtée") };
                let (_, event) = event?;
                let side = local.peer_side;
                let capture = local.capture.as_mut().expect("événement sans capture");
                on_capture(capture, &mut state, out, side, event).await?;
            }
            msg = incoming.recv() => {
                // `None` : la tâche de lecture s'est arrêtée (déconnexion).
                let Some(msg) = msg else { return Ok(()) };
                match msg {
                    Message::UdpPort(port) => {
                        udp.peer_addr = udp.peer_ip.map(|ip| std::net::SocketAddr::new(ip, port));
                    }
                    Message::Lock => {
                        if local.lock_sync && !was_locked {
                            log::info!("l'autre PC est verrouillé : ce PC se verrouille aussi");
                            locked_by_peer = Some(Instant::now());
                            power.lock().await;
                        }
                    }
                    Message::UdpOk => {
                        if !udp.send_ok {
                            log::info!("son envoyé en UDP (paquets indépendants, pas d'attente en cas de perte)");
                            if !via_bluetooth {
                                status::emit_link(LinkStatus::Udp);
                            }
                        }
                        udp.send_ok = true;
                    }
                    msg => on_message(local, &mut state, &mut audio, out, msg).await?,
                }
            }
            received = udp.socket.recv_from(&mut datagram) => {
                if let Ok((len, from)) = received
                    && Some(from.ip()) == udp.peer_ip
                    && let Some(plain) = udp.crypto.open(&datagram[..len])
                    && let Ok(msg) = postcard::from_bytes::<Message>(&plain)
                {
                    if !udp.recv_ok {
                        udp.recv_ok = true;
                        send(out, Message::UdpOk).await?;
                    }
                    if let msg @ Message::Audio { .. } = msg {
                        on_message(local, &mut state, &mut audio, out, msg).await?;
                    }
                }
            }
            _ = probe.tick() => udp.probe(),
            _ = lock_check.tick(), if local.lock_sync => {
                let locked = power.is_locked().await;
                // Verrouillé ici (pas à la demande de l'autre PC) : l'autre suit.
                let from_peer = locked_by_peer.is_some_and(|at| at.elapsed() < LOCK_ECHO);
                if locked && !was_locked && !from_peer {
                    log::info!("ce PC est verrouillé : l'autre PC se verrouille aussi");
                    send(out, Message::Lock).await?;
                }
                was_locked = locked;
            }
            _ = motion_flush.tick(), if state.pending_motion != (0.0, 0.0) => {
                flush_motion(&mut state, out).await?;
            }
            Ok(()) = defaults.changed() => {
                let now = defaults.borrow_and_update().clone();
                audio.follow_defaults(&now, &local.audio);
            }
            frame = recv_frame(&mut audio.sound) => match frame {
                Some(frame) => send_audio(&mut audio.sound, AudioKind::Sound, link, &frame, &mut udp),
                None => audio.sound.capture_stopped(),
            },
            frame = recv_frame(&mut audio.mic) => match frame {
                Some(frame) => send_audio(&mut audio.mic, AudioKind::Mic, link, &frame, &mut udp),
                None => audio.mic.capture_stopped(),
            },
            Some(path) = recv_path(&mut local.files) => {
                // Identifiants espacés : chaque fichier d'un dossier a le sien.
                let id = next_file_id;
                next_file_id = next_file_id.wrapping_add(1 << 16);
                tokio::spawn(crate::transfer::send_path(path, id, files_out.clone()));
            }
            Some(clip) = recv_clipboard(&mut local.clipboard) => {
                if local.clipboard_history {
                    record_history(local.name.clone(), clip.clone());
                }
                let batch = next_file_id;
                next_file_id = next_file_id.wrapping_add(1 << 16);
                send_clip(clip, batch, files_out.clone(), out).await?;
            }
            _ = ping.tick() => send(out, Message::Ping).await?,
        }
    }
}

async fn next_capture(capture: &mut Option<Capture>) -> Option<Result<(u64, CaptureEvent)>> {
    match capture {
        Some(c) => c.next().await,
        None => std::future::pending().await,
    }
}

async fn recv_frame(link: &mut AudioLink) -> Option<AudioFrame> {
    match &mut link.capture {
        Some(c) => c.frames.recv().await,
        None => std::future::pending().await,
    }
}

/// Transport du son : UDP chiffré dès que les deux sens sont vérifiés, sinon TCP.
struct Udp {
    socket: UdpSocket,
    crypto: UdpCrypto,
    /// Repli : son envoyé par la connexion, dans la file moins prioritaire.
    stream_audio: mpsc::Sender<Message>,
    /// Absente sur un lien Bluetooth direct : pas d'UDP.
    peer_ip: Option<std::net::IpAddr>,
    /// Adresse UDP de l'autre PC (son port arrive par la connexion TCP).
    peer_addr: Option<std::net::SocketAddr>,
    /// L'autre PC reçoit nos datagrammes (il l'a confirmé).
    send_ok: bool,
    /// Nous recevons ses datagrammes (et le lui avons confirmé).
    recv_ok: bool,
}

impl Udp {
    fn send_datagram(&mut self, msg: &Message) {
        let Some(addr) = self.peer_addr else { return };
        let Ok(plain) = postcard::to_stdvec(msg) else {
            return;
        };
        let packet = self.crypto.seal(&plain);
        // Socket pleine : le paquet est perdu plutôt que de retarder la suite.
        let _ = self.socket.try_send_to(&packet, addr);
    }

    /// Vérifie que l'UDP passe, jusqu'à confirmation de l'autre PC.
    fn probe(&mut self) {
        if !self.send_ok {
            self.send_datagram(&Message::Probe);
        }
    }

    fn send_audio(&mut self, msg: Message) {
        if self.send_ok {
            self.send_datagram(&msg);
        } else {
            // Repli par la connexion (UDP bloqué, Bluetooth) ; si elle sature, le
            // paquet est perdu : le son passe après la souris et le clavier.
            let _ = self.stream_audio.try_send(msg);
        }
    }
}

/// Met en paquets le son capturé (sans perte ou Opus) et l'envoie.
fn send_audio(
    link: &mut AudioLink,
    kind: AudioKind,
    (quality, bluetooth): (Quality, bool),
    frame: &AudioFrame,
    udp: &mut Udp,
) {
    if link.encoder.is_none() {
        match AudioEncoder::for_link(kind, quality, bluetooth) {
            Ok(encoder) => link.encoder = Some(encoder),
            Err(e) => {
                log::warn!("compression du son impossible : {e:#}");
                link.capture = None;
                link.encoder_failed = true;
                return;
            }
        }
    }
    let Some(encoder) = &mut link.encoder else {
        return;
    };
    match encoder.push(frame) {
        Ok(packets) => {
            for packet in packets {
                let previous = if udp.send_ok {
                    std::mem::replace(&mut link.last_sent, packet.data.clone())
                } else {
                    link.last_sent.clear();
                    Vec::new()
                };
                udp.send_audio(Message::Audio {
                    kind,
                    seq: packet.seq,
                    format: packet.format,
                    rate: packet.rate,
                    channels: packet.channels,
                    // En UDP, chaque paquet transporte aussi le précédent : une
                    // perte isolée est réparée exactement. Inutile en TCP (fiable).
                    previous,
                    data: packet.data,
                });
            }
        }
        Err(e) => log::debug!("compression du son : {e:#}"),
    }
}

/// Un sens de partage audio (son du système ou micro) : capture locale et lecture
/// de ce que l'autre PC envoie.
#[derive(Default)]
struct AudioLink {
    capture: Option<AudioCapture>,
    /// Capture en cours d'ouverture, hors de la boucle principale : ouvrir un
    /// périphérique peut prendre du temps (jusqu'à une seconde sous Windows),
    /// pendant lequel le son reçu et la souris ne doivent pas attendre.
    capture_pending: Option<tokio::task::JoinHandle<Result<AudioCapture>>>,
    /// Pas de nouvel essai de capture avant cet instant (échec récent).
    capture_retry: Option<Instant>,
    /// Échecs de capture d'affilée (seul le premier est signalé).
    capture_failures: u32,
    /// Compression impossible : partage abandonné pour cette connexion.
    encoder_failed: bool,
    encoder: Option<AudioEncoder>,
    /// Dernier paquet envoyé, copié dans le suivant (réparation des pertes UDP).
    last_sent: Vec<u8>,
    decoder: Option<AudioDecoder>,
    player: Option<AudioPlayer>,
    /// Lecture en cours d'ouverture, hors de la boucle principale.
    player_pending: Option<tokio::task::JoinHandle<Result<AudioPlayer>>>,
    player_retry: Option<Instant>,
    player_failures: u32,
}

/// Délai avant de réessayer une capture ou une lecture qui a échoué (périphérique
/// débranché, occupé…) : le son revient tout seul quand il est de nouveau là.
const AUDIO_RETRY: Duration = Duration::from_secs(3);
/// Délai avant de relancer un flux interrompu (laisse le système se stabiliser).
const AUDIO_RESTART: Duration = Duration::from_millis(500);

impl AudioLink {
    /// Décode un paquet reçu (pertes comblées).
    fn decode(
        &mut self,
        seq: u32,
        format: AudioFormat,
        rate: u32,
        channels: u8,
        data: &[u8],
        previous: &[u8],
    ) -> Option<AudioFrame> {
        if self
            .decoder
            .as_ref()
            .is_none_or(|d| !d.matches(format, channels))
        {
            self.decoder = AudioDecoder::new(format, channels).ok();
        }
        match self
            .decoder
            .as_mut()?
            .decode_with(seq, rate, data, previous)
        {
            Ok(frame) => frame,
            Err(e) => {
                log::debug!("paquet audio illisible : {e:#}");
                None
            }
        }
    }
}

impl AudioLink {
    fn sync_capture(&mut self, wanted: bool, source: Source, device: Option<&str>) {
        if !wanted {
            self.capture = None;
            // Une ouverture en cours se termine seule ; son résultat est abandonné.
            self.capture_pending = None;
            return;
        }
        if let Some(result) = take_finished(&mut self.capture_pending) {
            self.capture_started(result, source);
        }
        if self.capture.as_ref().is_some_and(AudioCapture::is_broken) {
            log::info!("capture audio interrompue ({source:?}), relance");
            self.capture = None;
            self.capture_retry = Some(Instant::now() + AUDIO_RESTART);
        }
        if self.capture.is_some()
            || self.capture_pending.is_some()
            || self.encoder_failed
            || self.capture_retry.is_some_and(|at| Instant::now() < at)
        {
            return;
        }
        let device = device.map(str::to_owned);
        self.capture_pending = Some(tokio::task::spawn_blocking(move || {
            AudioCapture::start(source, device.as_deref())
        }));
    }

    fn capture_started(&mut self, result: Result<AudioCapture>, source: Source) {
        match result {
            Ok(capture) => {
                if self.capture_failures > 0 {
                    log::info!("capture audio rétablie ({source:?})");
                }
                self.capture_failures = 0;
                self.capture_retry = None;
                if source == Source::Microphone {
                    status::emit_mic(MicStatus::Sending(capture.label.clone()));
                }
                self.capture = Some(capture);
            }
            Err(e) => {
                if self.capture_failures == 0 {
                    log::warn!(
                        "capture audio impossible pour l'instant ({source:?}) : {e:#} — nouvel essai \
                         toutes les {} s",
                        AUDIO_RETRY.as_secs()
                    );
                } else {
                    log::debug!("capture audio ({source:?}) : {e:#}");
                }
                self.capture_failures += 1;
                self.capture_retry = Some(Instant::now() + AUDIO_RETRY);
            }
        }
    }

    fn capture_stopped(&mut self) {
        log::warn!("la capture audio s'est arrêtée, relance");
        self.capture = None;
        self.capture_retry = Some(Instant::now() + AUDIO_RESTART);
    }

    fn play(&mut self, frame: &AudioFrame, output: Option<&str>) {
        if self.player.as_ref().is_some_and(AudioPlayer::is_broken) {
            log::info!("lecture audio interrompue, relance");
            self.player = None;
            self.player_retry = Some(Instant::now() + AUDIO_RESTART);
        }
        if let Some(result) = take_finished(&mut self.player_pending) {
            self.player_started(result);
        }
        if self.player.is_none()
            && self.player_pending.is_none()
            && self.player_retry.is_none_or(|at| Instant::now() >= at)
        {
            let output = output.map(str::to_owned);
            self.player_pending = Some(tokio::task::spawn_blocking(move || {
                AudioPlayer::start(output.as_deref())
            }));
        }
        if let Some(player) = &mut self.player {
            player.push(frame);
        }
    }

    fn player_started(&mut self, result: Result<AudioPlayer>) {
        match result {
            Ok(player) => {
                if self.player_failures > 0 {
                    log::info!("lecture du son reçu rétablie");
                }
                self.player_failures = 0;
                self.player_retry = None;
                self.player = Some(player);
            }
            Err(e) => {
                if self.player_failures == 0 {
                    log::warn!(
                        "lecture du son reçu impossible pour l'instant : {e:#} — nouvel essai \
                         toutes les {} s",
                        AUDIO_RETRY.as_secs()
                    );
                }
                self.player_failures += 1;
                self.player_retry = Some(Instant::now() + AUDIO_RETRY);
            }
        }
    }
}

/// Résultat d'une ouverture de périphérique terminée (sans attendre si elle ne
/// l'est pas encore).
fn take_finished<T>(pending: &mut Option<tokio::task::JoinHandle<Result<T>>>) -> Option<Result<T>> {
    use futures::FutureExt;
    if !pending.as_ref()?.is_finished() {
        return None;
    }
    let handle = pending.take()?;
    match handle.now_or_never()? {
        Ok(result) => Some(result),
        Err(e) => Some(Err(anyhow::anyhow!(
            "ouverture du périphérique audio interrompue : {e}"
        ))),
    }
}

#[derive(Default)]
struct AudioState {
    sound: AudioLink,
    mic: AudioLink,
    /// Sortie où est joué le micro reçu (micro virtuel), une fois choisie.
    mic_output: Option<String>,
    /// Aucun micro virtuel pour recevoir le micro de l'autre PC.
    mic_unavailable: bool,
    /// Micro virtuel mis par défaut pendant la connexion (rétabli à la fin).
    _default_mic: Option<default_mic::DefaultMicGuard>,
}

impl AudioState {
    /// Le périphérique par défaut a changé (casque branché, sortie choisie dans
    /// Windows…) : les flux qui l'utilisaient sont relancés sur le nouveau.
    fn follow_defaults(&mut self, defaults: &audio::DefaultDevices, config: &AudioConfig) {
        let changed = |was: &Option<String>, now: &Option<String>| was.is_some() && was != now;
        if config.sound_device.is_none()
            && self
                .sound
                .capture
                .as_ref()
                .is_some_and(|c| changed(&c.default_id, &defaults.output))
        {
            log::info!("sortie audio par défaut changée : capture du son relancée");
            self.sound.capture = None;
        }
        if config.mic_device.is_none()
            && self
                .mic
                .capture
                .as_ref()
                .is_some_and(|c| changed(&c.default_id, &defaults.input))
        {
            log::info!("micro par défaut changé : capture du micro relancée");
            self.mic.capture = None;
        }
        if config.sound_output.is_none()
            && self
                .sound
                .player
                .as_ref()
                .is_some_and(|p| changed(&p.default_id, &defaults.output))
        {
            log::info!("sortie audio par défaut changée : le son de l'autre PC y est joué");
            self.sound.player = None;
        }
    }

    /// Démarre ou arrête les captures selon qui contrôle qui.
    fn sync(&mut self, config: &AudioConfig, state: &State) {
        let (sound, mic) = audio_directions(config, state);
        self.sound
            .sync_capture(sound, Source::System, config.sound_device.as_deref());
        self.mic
            .sync_capture(mic, Source::Microphone, config.mic_device.as_deref());
    }
}

/// Retient le PC qui vient de prendre la main (PC principal implicite).
fn note_controller(state: &mut State, we: bool) {
    if state.we_controlled_last != Some(we) {
        if !state.peer_main {
            log::info!(
                "{} : le son de l'autre PC y arrive désormais en permanence",
                if we {
                    "ce PC contrôle l'autre, il fait office de PC principal"
                } else {
                    "l'autre PC contrôle celui-ci, il fait office de PC principal"
                }
            );
        }
        state.we_controlled_last = Some(we);
    }
}

/// Faut-il envoyer maintenant (son du système, micro) ?
///
/// Le son va de l'autre PC vers le PC principal, le micro du PC principal vers
/// l'autre. Sans PC principal coché, le dernier PC à avoir pris la main en tient
/// lieu. Avec deux PC principaux (ou avant toute prise de main), le son n'est
/// envoyé que par le PC contrôlé et le micro que par celui qui contrôle : un seul
/// sens à la fois, donc jamais de boucle d'écho.
fn audio_directions(config: &AudioConfig, state: &State) -> (bool, bool) {
    use audio::Mode;
    let auto = |mode: Mode, active: bool| mode != Mode::Off && active;
    // Aucun PC principal coché : le dernier PC à avoir pris la main en tient lieu.
    let (local_main, peer_main) = match (config.main, state.peer_main, state.we_controlled_last) {
        (false, false, Some(we)) => (we, !we),
        roles => (roles.0, roles.1),
    };
    match (local_main, peer_main) {
        (false, true) => (config.sound.wanted(state.receiving), false),
        (true, false) => (false, config.mic.wanted(state.sending)),
        _ => (
            auto(config.sound, state.receiving),
            auto(config.mic, state.sending),
        ),
    }
}

async fn recv_path(
    files: &mut Option<mpsc::Receiver<std::path::PathBuf>>,
) -> Option<std::path::PathBuf> {
    match files {
        Some(files) => files.recv().await,
        None => std::future::pending().await,
    }
}

/// Ajoute une copie à l'historique, hors de la boucle principale (fichiers).
fn record_history(from: String, clip: crate::clipboard::Clip) {
    tokio::task::spawn_blocking(move || crate::history::record(&from, &clip));
}

/// Envoie un contenu copié localement : le texte directement, les images et les
/// fichiers par la file des fichiers (ils sont mis dans le presse-papier de
/// l'autre PC une fois tous reçus).
async fn send_clip(
    clip: crate::clipboard::Clip,
    batch: u32,
    files: mpsc::Sender<Message>,
    out: &mpsc::Sender<Message>,
) -> Result<()> {
    use crate::clipboard::Clip;
    match clip {
        Clip::Text(text) => {
            log::debug!("presse-papier local modifié ({} octets), envoi", text.len());
            send(out, Message::Clipboard(text)).await?;
        }
        Clip::Files(items) => {
            log::info!("fichiers copiés : envoi au presse-papier de l'autre PC");
            tokio::spawn(crate::transfer::send_clipboard(
                items, batch, false, batch, files,
            ));
        }
        Clip::Image {
            width,
            height,
            rgba,
        } => {
            tokio::spawn(async move {
                let png = tokio::task::spawn_blocking(move || {
                    let png = crate::clipboard::encode_png(width, height, &rgba)?;
                    let path = std::env::temp_dir().join(format!(
                        "one-mouse-image-{}-{batch}.png",
                        std::process::id()
                    ));
                    std::fs::write(&path, png)?;
                    anyhow::Ok(path)
                })
                .await;
                match png {
                    Ok(Ok(path)) => {
                        log::info!("image copiée : envoi au presse-papier de l'autre PC");
                        crate::transfer::send_clipboard(vec![path], batch, true, batch, files)
                            .await;
                    }
                    Ok(Err(e)) => log::warn!("image du presse-papier non envoyée : {e:#}"),
                    Err(e) => log::warn!("image du presse-papier non envoyée : {e}"),
                }
            });
        }
    }
    Ok(())
}

async fn recv_clipboard(clipboard: &mut Option<ClipboardSync>) -> Option<crate::clipboard::Clip> {
    match clipboard {
        Some(c) => c.changes.recv().await,
        None => std::future::pending().await,
    }
}

async fn send(out: &mpsc::Sender<Message>, msg: Message) -> Result<()> {
    out.send(msg)
        .await
        .map_err(|_| anyhow::anyhow!("connexion perdue"))
}

async fn on_capture(
    capture: &mut Capture,
    state: &mut State,
    out: &mpsc::Sender<Message>,
    side: Position,
    event: CaptureEvent,
) -> Result<()> {
    match event {
        CaptureEvent::Begin if state.receiving && recent(state.entered_at) => {
            // Bord de retour touché juste après l'arrivée : rebond, ignoré.
            capture.release().await?;
        }
        CaptureEvent::Begin if state.receiving => {
            // Le curseur rejoué a atteint le bord de retour : l'autre PC reprend la main.
            log::info!("retour du curseur vers l'autre PC");
            let along = exit_along(capture, side);
            capture.release().await?;
            send(out, Message::Leave { along }).await?;
        }
        CaptureEvent::Begin if recent(state.returned_at) => {
            // Bord franchi juste après le retour du curseur : rebond, ignoré.
            capture.release().await?;
        }
        CaptureEvent::Begin => {
            log::info!("le curseur passe sur l'autre PC");
            state.sending = true;
            note_controller(state, true);
            status::emit(Status::Sending);
            let along = exit_along(capture, side);
            send(out, Message::Enter { along }).await?;
        }
        CaptureEvent::Input(_) if !state.sending => {}
        CaptureEvent::Input(event) => {
            if capture.keys_pressed(&RELEASE_KEYS) {
                log::info!("raccourci de secours : reprise du curseur");
                stop_sending(capture, state, out, None).await?;
                return Ok(());
            }
            let Some(event) = scale_motion(state, event) else {
                return Ok(());
            };
            if state.coalesce_motion {
                if let Event::Pointer(PointerEvent::Motion { dx, dy, .. }) = event {
                    // Liaison lente : regroupé et envoyé toutes les 8 ms.
                    state.pending_motion.0 += dx;
                    state.pending_motion.1 += dy;
                    return Ok(());
                }
                // Clic, touche… : après les mouvements en attente, dans l'ordre.
                flush_motion(state, out).await?;
            }
            send(out, Message::Input(event.into())).await?;
        }
    }
    Ok(())
}

/// Où le curseur vient de quitter l'écran, le long du bord partagé.
fn exit_along(capture: &Capture, side: Position) -> Option<i32> {
    crate::screens::along(
        &crate::screens::local_monitors(),
        side,
        capture.exit_point()?,
    )
}

/// Point d'arrivée du curseur venant de l'autre PC, à la même hauteur.
fn arrival(local: &Local, along: Option<i32>) -> Option<(i32, i32)> {
    crate::screens::entry_point(
        &crate::screens::local_monitors(),
        local.peer_side,
        along?,
        local.peer_offset,
    )
}

/// Informe la fenêtre des écrans des deux PC et de leur alignement.
fn emit_screens(local: &Local, state: &State) {
    status::emit_screens(&status::ScreensStatus {
        local: crate::screens::local_monitors(),
        peer: state.peer_monitors.clone(),
        peer_name: state.peer_name.clone(),
        side: side_code(local.peer_side),
        offset: local.peer_offset,
    });
}

/// Le curseur de ce PC doit-il être caché ? Oui quand ce PC contrôle l'autre
/// (la souris est sur l'autre écran), et quand l'autre PC, qui l'a contrôlé en
/// dernier, a repris sa souris : seul le curseur du PC utilisé reste visible.
fn cursor_hidden(state: &State) -> bool {
    state.sending || (!state.receiving && state.we_controlled_last == Some(false))
}

/// Envoie le mouvement de souris accumulé (liaison lente).
async fn flush_motion(state: &mut State, out: &mpsc::Sender<Message>) -> Result<()> {
    let (dx, dy) = std::mem::take(&mut state.pending_motion);
    if (dx, dy) != (0.0, 0.0) {
        let motion = Event::Pointer(PointerEvent::Motion { time: 0, dx, dy });
        send(out, Message::Input(motion.into())).await?;
    }
    Ok(())
}

/// L'instant est-il assez récent pour qu'un passage inverse soit un rebond ?
fn recent(at: Option<Instant>) -> bool {
    at.is_some_and(|at| at.elapsed() < EDGE_COOLDOWN)
}

/// Applique la vitesse choisie et n'envoie que des pixels entiers : les fractions
/// sont reportées au mouvement suivant. Sans cela, les petits mouvements (moins
/// d'un pixel, fréquents sous macOS et Wayland) seraient perdus par les PC qui
/// tronquent à l'entier, comme Windows : la souris y paraîtrait plus lente.
fn scale_motion(state: &mut State, event: Event) -> Option<Event> {
    let Event::Pointer(PointerEvent::Motion { time, dx, dy }) = event else {
        return Some(event);
    };
    let x = dx * state.pointer_speed + state.motion_rest.0;
    let y = dy * state.pointer_speed + state.motion_rest.1;
    let (sx, sy) = (x.round(), y.round());
    state.motion_rest = (x - sx, y - sy);
    (sx != 0.0 || sy != 0.0).then_some(Event::Pointer(PointerEvent::Motion {
        time,
        dx: sx,
        dy: sy,
    }))
}

async fn on_message(
    local: &mut Local,
    state: &mut State,
    audio: &mut AudioState,
    out: &mpsc::Sender<Message>,
    msg: Message,
) -> Result<()> {
    match msg {
        Message::Enter { along } => {
            log::info!("l'autre PC prend le contrôle de celui-ci");
            // Le curseur apparaît à la hauteur où il a quitté l'autre écran.
            if let Some(point) = arrival(local, along) {
                crate::screens::warp_cursor(point);
            }
            state.receiving = true;
            state.entered_at = Some(Instant::now());
            note_controller(state, false);
            status::emit(Status::Receiving);
        }
        Message::Input(event) if state.receiving => {
            // Un événement impossible à rejouer (touche inconnue…) ne coupe pas la connexion.
            if let Err(e) = local.emulation.consume(event.into(), PEER).await {
                log::warn!("événement non rejoué : {e}");
            }
        }
        Message::Input(_) => {}
        Message::Exit => {
            state.receiving = false;
            status::emit(Status::Connected(state.peer_name.clone()));
            if let Err(e) = local.emulation.release_keys(PEER).await {
                log::warn!("relâchement des touches impossible : {e}");
            }
        }
        Message::Leave { along } => {
            let point = arrival(local, along);
            if let Some(capture) = &mut local.capture
                && state.sending
            {
                log::info!("le curseur revient sur ce PC");
                stop_sending(capture, state, out, point).await?;
            }
        }
        Message::Clipboard(text) => {
            if let Some(clipboard) = &local.clipboard
                && text.len() <= MAX_CLIPBOARD_LEN
            {
                let clip = crate::clipboard::Clip::Text(text);
                if local.clipboard_history {
                    record_history(state.peer_name.clone(), clip.clone());
                }
                clipboard.set(clip);
            }
        }
        Message::Audio {
            kind,
            seq,
            format,
            rate,
            channels,
            data,
            previous,
        } => {
            let link = match kind {
                AudioKind::Sound => &mut audio.sound,
                AudioKind::Mic => &mut audio.mic,
            };
            let Some(frame) = link.decode(seq, format, rate, channels, &data, &previous) else {
                return Ok(());
            };
            match kind {
                AudioKind::Sound => audio
                    .sound
                    .play(&frame, local.audio.sound_output.as_deref()),
                AudioKind::Mic => {
                    if audio.mic_output.is_none() && !audio.mic_unavailable {
                        // Le micro de l'autre PC devient un micro de ce PC : il est joué
                        // dans un micro virtuel, jamais sur les haut-parleurs (écho).
                        let output = local
                            .audio
                            .mic_output
                            .clone()
                            .or_else(audio::virtual_mic_output);
                        match output {
                            Some(output) => {
                                log::info!("micro de l'autre PC disponible via « {output} »");
                                audio._default_mic = default_mic::switch_to_virtual(&output);
                                let name = default_mic::virtual_mic_name(&output);
                                status::emit_mic(if audio._default_mic.is_some() {
                                    MicStatus::Default(name)
                                } else {
                                    MicStatus::NotDefault(name)
                                });
                                audio.mic_output = Some(output);
                            }
                            None => {
                                log::warn!(
                                    "micro de l'autre PC ignoré : aucun micro virtuel. Installez VB-Cable \
                                     (Windows, vb-audio.com/Cable) ou BlackHole (macOS), puis redémarrez le partage"
                                );
                                status::emit_mic(MicStatus::NoVirtual);
                                audio.mic_unavailable = true;
                            }
                        }
                    }
                    if let Some(output) = audio.mic_output.clone() {
                        audio.mic.play(&frame, Some(&output));
                    }
                }
            }
        }
        Message::ClipboardBatch {
            batch,
            count,
            image,
        } => state.incoming.start_batch(batch, count, image),
        Message::FileStart {
            id,
            path,
            size,
            batch,
        } => state.incoming.start(id, &path, size, batch),
        Message::FileChunk { id, data } => state.incoming.chunk(id, &data),
        Message::FileEnd { id } => {
            if let Some(clip) = state.incoming.end(id)
                && let Some(clipboard) = &local.clipboard
            {
                if local.clipboard_history {
                    record_history(state.peer_name.clone(), clip.clone());
                }
                clipboard.set(clip);
            }
        }
        Message::Layout { side, offset } => {
            let Some(expected) = mirrored_side(side) else {
                return Ok(());
            };
            // Vu de ce PC, le décalage de l'autre bureau est l'opposé.
            let offset = -offset;
            if local.peer_side != expected || local.peer_offset != offset {
                if state.side_authority {
                    if local.peer_side != expected {
                        log::warn!(
                            "côtés incohérents : l'autre PC s'aligne sur ce PC (autre PC : {})",
                            side_name(local.peer_side)
                        );
                    }
                } else {
                    if local.peer_side != expected {
                        log::warn!(
                            "côtés incohérents : ce PC s'aligne sur l'autre (autre PC : {}) — \
                             corrigez « Où se trouve l'autre PC ? » dans l'onglet Écrans",
                            side_name(expected)
                        );
                        local.peer_side = expected;
                        if let Some(capture) = &mut local.capture {
                            let _ = capture.destroy(PEER).await;
                            capture.create(PEER, expected).await?;
                        }
                    }
                    local.peer_offset = offset;
                }
            }
            emit_screens(local, state);
        }
        Message::Screens(monitors) => {
            state.peer_monitors = monitors;
            emit_screens(local, state);
        }
        Message::WakeAddresses { macs, wired } => {
            crate::wake::remember_peer(macs, wired, &state.peer_name);
        }
        Message::BluetoothAddress(address) => {
            crate::bluetooth::remember_peer(address, &state.peer_name);
        }
        Message::Ping | Message::Probe | Message::UdpPort(_) | Message::UdpOk | Message::Lock => {}
        Message::Hello { .. } => bail!("message Hello inattendu"),
    }
    Ok(())
}

/// Rend la main localement : relâche les touches encore enfoncées chez l'autre PC.
async fn stop_sending(
    capture: &mut Capture,
    state: &mut State,
    out: &mpsc::Sender<Message>,
    point: Option<(i32, i32)>,
) -> Result<()> {
    state.sending = false;
    state.returned_at = Some(Instant::now());
    // Mouvement pas encore envoyé : le curseur revient ici, il n'a plus lieu d'être.
    state.pending_motion = (0.0, 0.0);
    status::emit(Status::Connected(state.peer_name.clone()));
    let pressed = capture.take_pressed_keys();
    capture.release_at(point).await?;
    for key in pressed {
        let event = Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key: key as u32,
            state: 0,
        });
        send(out, Message::Input(event.into())).await?;
    }
    send(out, Message::Exit).await
}

pub fn side_name(side: Position) -> &'static str {
    match side {
        Position::Left => "gauche",
        Position::Right => "droite",
        Position::Top => "en haut",
        Position::Bottom => "en bas",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Mode;

    fn config(main: bool, sound: Mode, mic: Mode) -> AudioConfig {
        AudioConfig {
            main,
            sound,
            mic,
            sound_device: None,
            mic_device: None,
            sound_output: None,
            mic_output: None,
            quality: Quality::Lossless,
            bluetooth: false,
        }
    }

    fn state(peer_main: bool, sending: bool, receiving: bool) -> State {
        State {
            peer_main,
            sending,
            receiving,
            ..State::default()
        }
    }

    fn motion(dx: f64, dy: f64) -> Event {
        Event::Pointer(PointerEvent::Motion { time: 0, dx, dy })
    }

    #[test]
    fn small_motions_are_not_lost() {
        let mut st = State {
            pointer_speed: 1.0,
            ..State::default()
        };
        // 10 mouvements de 0,6 pixel = 6 pixels envoyés au total.
        let mut total = 0.0;
        for _ in 0..10 {
            if let Some(Event::Pointer(PointerEvent::Motion { dx, .. })) =
                scale_motion(&mut st, motion(0.6, 0.0))
            {
                assert_eq!(dx.fract(), 0.0, "pixels entiers uniquement");
                total += dx;
            }
        }
        assert_eq!(total, 6.0);
    }

    #[test]
    fn speed_multiplies_motion_and_keeps_direction() {
        let mut st = State {
            pointer_speed: 2.5,
            ..State::default()
        };
        assert_eq!(
            scale_motion(&mut st, motion(4.0, -2.0)),
            Some(motion(10.0, -5.0))
        );
        // Les autres événements passent tels quels.
        let key = Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key: 30,
            state: 1,
        });
        assert_eq!(scale_motion(&mut st, key), Some(key));
    }

    #[test]
    fn secondary_pc_always_sends_sound_to_main() {
        let cfg = config(false, Mode::Always, Mode::Auto);
        assert_eq!(
            audio_directions(&cfg, &state(true, false, false)),
            (true, false)
        );
        // Mode « auto » : seulement quand le PC principal le contrôle.
        let cfg = config(false, Mode::Auto, Mode::Auto);
        assert_eq!(
            audio_directions(&cfg, &state(true, false, false)),
            (false, false)
        );
        assert_eq!(
            audio_directions(&cfg, &state(true, false, true)),
            (true, false)
        );
    }

    #[test]
    fn main_pc_never_sends_sound_but_sends_mic() {
        let cfg = config(true, Mode::Always, Mode::Always);
        assert_eq!(
            audio_directions(&cfg, &state(false, false, false)),
            (false, true)
        );
        let cfg = config(true, Mode::Always, Mode::Auto);
        assert_eq!(
            audio_directions(&cfg, &state(false, true, false)),
            (false, true)
        );
        assert_eq!(
            audio_directions(&cfg, &state(false, false, false)),
            (false, false)
        );
    }

    #[test]
    fn only_the_cursor_in_use_is_visible() {
        let mut st = state(false, false, false);
        // Avant toute prise de main : curseur visible.
        assert!(!cursor_hidden(&st));
        // Ce PC contrôle l'autre : son curseur est caché.
        st.sending = true;
        note_controller(&mut st, true);
        assert!(cursor_hidden(&st));
        // Revenu chez lui : visible.
        st.sending = false;
        assert!(!cursor_hidden(&st));
        // Côté contrôlé : visible pendant le contrôle, caché après le départ.
        let mut st = state(false, false, true);
        note_controller(&mut st, false);
        assert!(!cursor_hidden(&st));
        st.receiving = false;
        assert!(cursor_hidden(&st));
    }

    #[test]
    fn two_main_pcs_follow_control_to_avoid_echo() {
        let cfg = config(true, Mode::Always, Mode::Always);
        // Jamais les deux PC en même temps : chacun ne peut être que contrôlé ou contrôleur.
        assert_eq!(
            audio_directions(&cfg, &state(true, false, false)),
            (false, false)
        );
        assert_eq!(
            audio_directions(&cfg, &state(true, false, true)),
            (true, false)
        );
        assert_eq!(
            audio_directions(&cfg, &state(true, true, false)),
            (false, true)
        );
    }

    #[test]
    fn without_main_pc_last_controller_acts_as_main() {
        let cfg = config(false, Mode::Always, Mode::Always);
        let mut st = state(false, false, false);
        // Avant toute prise de main : rien.
        assert_eq!(audio_directions(&cfg, &st), (false, false));
        // L'autre PC a pris la main puis l'a rendue : son envoyé en permanence.
        note_controller(&mut st, false);
        assert_eq!(audio_directions(&cfg, &st), (true, false));
        st.receiving = true;
        assert_eq!(audio_directions(&cfg, &st), (true, false));
        // Ce PC a pris la main : il reçoit le son et envoie son micro, même revenu chez lui.
        st.receiving = false;
        note_controller(&mut st, true);
        assert_eq!(audio_directions(&cfg, &st), (false, true));
    }
}
