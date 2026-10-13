//! Une connexion active avec l'autre PC : capture, émulation et presse-papier.
//!
//! Les deux PC jouent le même rôle. Celui dont le curseur franchit le bord de
//! l'écran du côté de l'autre PC devient « émetteur » : ses entrées locales sont
//! capturées et envoyées, l'autre PC les rejoue. Quand le curseur rejoué atteint
//! le bord du retour, le récepteur envoie `Leave` et l'émetteur rend la main.
//!
//! Son : par défaut, le PC contrôlé envoie son son au PC qui le contrôle, et le
//! micro du PC qui contrôle peut être envoyé au PC contrôlé (voir `audio::Mode`).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use input_capture::{CaptureEvent, Position};
use input_emulation::InputEmulation;
use input_event::{Event, KeyboardEvent, PointerEvent, scancode};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::audio::{self, AudioCapture, AudioConfig, AudioFrame, AudioPlayer, Source};
use crate::capture::Capture;
use crate::clipboard::ClipboardSync;
use crate::crypto::{self, Psk, Role};
use crate::default_mic;
use crate::protocol::{self, AudioKind, MAX_CLIPBOARD_LEN, Message, PeerInfo};
use crate::status::{self, Status};

/// Identifiant de l'unique autre PC (la V1 gère une seule connexion).
const PEER: u64 = 0;
const PING_INTERVAL: Duration = Duration::from_secs(5);
/// Sans message de l'autre PC pendant cette durée, la connexion est considérée perdue.
const PEER_TIMEOUT: Duration = Duration::from_secs(15);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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
    pub audio: AudioConfig,
    /// Multiplicateur des mouvements de souris envoyés à l'autre PC.
    pub pointer_speed: f64,
}

#[derive(Default)]
struct State {
    /// Nom de l'autre PC.
    peer_name: String,
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
}

pub async fn run(stream: TcpStream, role: Role, local: &mut Local) -> Result<()> {
    stream.set_nodelay(true)?;
    let (reader, writer) = stream.into_split();
    let (mut reader, mut writer, peer) = tokio::time::timeout(
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

    let (out_tx, mut out_rx) = mpsc::channel::<Message>(1024);
    let (in_tx, mut in_rx) = mpsc::channel::<Message>(1024);
    let write_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
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

    status::emit(Status::Connected(peer.name.clone()));
    let result = event_loop(local, peer, &out_tx, &mut in_rx).await;

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
    out: &mpsc::Sender<Message>,
    incoming: &mut mpsc::Receiver<Message>,
) -> Result<()> {
    let mut state = State {
        peer_name: peer.name,
        peer_main: peer.main,
        pointer_speed: local.pointer_speed,
        ..State::default()
    };
    if local.audio.sound == audio::Mode::Always && local.audio.main == state.peer_main {
        log::info!(
            "son envoyé seulement quand l'autre PC contrôle celui-ci : pour l'entendre en permanence, \
             cochez « PC principal » sur le PC où vous écoutez (et seulement sur lui)"
        );
    }
    let mut audio = AudioState::default();
    let mut ping = tokio::time::interval(PING_INTERVAL);

    loop {
        audio.sync(&local.audio, &state);
        tokio::select! {
            event = next_capture(&mut local.capture) => {
                let Some(event) = event else { bail!("la capture des entrées s'est arrêtée") };
                let (_, event) = event?;
                let capture = local.capture.as_mut().expect("événement sans capture");
                on_capture(capture, &mut state, out, event).await?;
            }
            msg = incoming.recv() => {
                // `None` : la tâche de lecture s'est arrêtée (déconnexion).
                let Some(msg) = msg else { return Ok(()) };
                on_message(local, &mut state, &mut audio, out, msg).await?;
            }
            frame = recv_frame(&mut audio.sound) => match frame {
                Some(frame) => send_audio(out, AudioKind::Sound, &frame),
                None => audio.sound.capture_stopped(),
            },
            frame = recv_frame(&mut audio.mic) => match frame {
                Some(frame) => send_audio(out, AudioKind::Mic, &frame),
                None => audio.mic.capture_stopped(),
            },
            Some(text) = recv_clipboard(&mut local.clipboard) => {
                log::debug!("presse-papier local modifié ({} octets), envoi", text.len());
                send(out, Message::Clipboard(text)).await?;
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

/// Le son passe après les entrées : si la connexion sature, le paquet est perdu.
fn send_audio(out: &mpsc::Sender<Message>, kind: AudioKind, frame: &AudioFrame) {
    let _ = out.try_send(Message::audio(kind, frame));
}

/// Un sens de partage audio (son du système ou micro) : capture locale et lecture
/// de ce que l'autre PC envoie.
#[derive(Default)]
struct AudioLink {
    capture: Option<AudioCapture>,
    capture_failed: bool,
    player: Option<AudioPlayer>,
    player_failed: bool,
}

impl AudioLink {
    fn sync_capture(&mut self, wanted: bool, source: Source, device: Option<&str>) {
        if !wanted {
            self.capture = None;
            return;
        }
        if self.capture.is_some() || self.capture_failed {
            return;
        }
        match AudioCapture::start(source, device) {
            Ok(capture) => self.capture = Some(capture),
            Err(e) => {
                log::warn!("partage audio désactivé ({source:?}) : {e:#}");
                self.capture_failed = true;
            }
        }
    }

    fn capture_stopped(&mut self) {
        log::warn!("la capture audio s'est arrêtée");
        self.capture = None;
        self.capture_failed = true;
    }

    fn play(&mut self, frame: &AudioFrame, output: Option<&str>) {
        if self.player.is_none() && !self.player_failed {
            match AudioPlayer::start(output) {
                Ok(player) => self.player = Some(player),
                Err(e) => {
                    log::warn!("lecture du son reçu impossible : {e:#}");
                    self.player_failed = true;
                }
            }
        }
        if let Some(player) = &mut self.player {
            player.push(frame);
        }
    }
}

#[derive(Default)]
struct AudioState {
    sound: AudioLink,
    mic: AudioLink,
    /// Sortie où est joué le micro reçu (micro virtuel), une fois choisie.
    mic_output: Option<String>,
    /// Micro virtuel mis par défaut pendant la connexion (rétabli à la fin).
    _default_mic: Option<default_mic::DefaultMicGuard>,
}

impl AudioState {
    /// Démarre ou arrête les captures selon qui contrôle qui.
    fn sync(&mut self, config: &AudioConfig, state: &State) {
        let (sound, mic) = audio_directions(config, state);
        self.sound
            .sync_capture(sound, Source::System, config.sound_device.as_deref());
        self.mic
            .sync_capture(mic, Source::Microphone, config.mic_device.as_deref());
    }
}

/// Faut-il envoyer maintenant (son du système, micro) ?
///
/// Le son va de l'autre PC vers le PC principal, le micro du PC principal vers
/// l'autre. Sans PC principal défini (ou avec deux), le son n'est envoyé que par
/// le PC contrôlé et le micro que par celui qui contrôle : un seul sens à la fois,
/// donc jamais de boucle d'écho (le son joué serait recapturé et renvoyé).
fn audio_directions(config: &AudioConfig, state: &State) -> (bool, bool) {
    use audio::Mode;
    let auto = |mode: Mode, active: bool| mode != Mode::Off && active;
    match (config.main, state.peer_main) {
        (false, true) => (config.sound.wanted(state.receiving), false),
        (true, false) => (false, config.mic.wanted(state.sending)),
        _ => (
            auto(config.sound, state.receiving),
            auto(config.mic, state.sending),
        ),
    }
}

async fn recv_clipboard(clipboard: &mut Option<ClipboardSync>) -> Option<String> {
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
    event: CaptureEvent,
) -> Result<()> {
    match event {
        CaptureEvent::Begin if state.receiving => {
            // Le curseur rejoué a atteint le bord de retour : l'autre PC reprend la main.
            log::info!("retour du curseur vers l'autre PC");
            capture.release().await?;
            send(out, Message::Leave).await?;
        }
        CaptureEvent::Begin => {
            log::info!("le curseur passe sur l'autre PC");
            state.sending = true;
            status::emit(Status::Sending);
            send(out, Message::Enter).await?;
        }
        CaptureEvent::Input(_) if !state.sending => {}
        CaptureEvent::Input(event) => {
            if capture.keys_pressed(&RELEASE_KEYS) {
                log::info!("raccourci de secours : reprise du curseur");
                stop_sending(capture, state, out).await?;
                return Ok(());
            }
            let Some(event) = scale_motion(state, event) else {
                return Ok(());
            };
            send(out, Message::Input(event.into())).await?;
        }
    }
    Ok(())
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
        Message::Enter => {
            log::info!("l'autre PC prend le contrôle de celui-ci");
            state.receiving = true;
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
        Message::Leave => {
            if let Some(capture) = &mut local.capture
                && state.sending
            {
                log::info!("le curseur revient sur ce PC");
                stop_sending(capture, state, out).await?;
            }
        }
        Message::Clipboard(text) => {
            if let Some(clipboard) = &local.clipboard
                && text.len() <= MAX_CLIPBOARD_LEN
            {
                clipboard.set(text);
            }
        }
        Message::Audio {
            kind,
            rate,
            channels,
            pcm,
        } => {
            let Some(frame) = protocol::audio_frame(rate, channels, &pcm) else {
                return Ok(());
            };
            match kind {
                AudioKind::Sound => audio
                    .sound
                    .play(&frame, local.audio.sound_output.as_deref()),
                AudioKind::Mic => {
                    if audio.mic.player.is_none() && !audio.mic.player_failed {
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
                                audio.mic_output = Some(output);
                            }
                            None => {
                                log::warn!(
                                    "micro de l'autre PC ignoré : aucun micro virtuel. Installez VB-Cable \
                                     (Windows, vb-audio.com/Cable) ou BlackHole (macOS), puis redémarrez le partage"
                                );
                                audio.mic.player_failed = true;
                            }
                        }
                    }
                    if let Some(output) = audio.mic_output.clone() {
                        audio.mic.play(&frame, Some(&output));
                    }
                }
            }
        }
        Message::Ping => {}
        Message::Hello { .. } => bail!("message Hello inattendu"),
    }
    Ok(())
}

/// Rend la main localement : relâche les touches encore enfoncées chez l'autre PC.
async fn stop_sending(
    capture: &mut Capture,
    state: &mut State,
    out: &mpsc::Sender<Message>,
) -> Result<()> {
    state.sending = false;
    status::emit(Status::Connected(state.peer_name.clone()));
    let pressed = capture.take_pressed_keys();
    capture.release().await?;
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
    fn without_main_pc_sound_follows_control_to_avoid_echo() {
        for main in [false, true] {
            let cfg = config(main, Mode::Always, Mode::Always);
            // Jamais les deux PC en même temps : chacun ne peut être que contrôlé ou contrôleur.
            assert_eq!(
                audio_directions(&cfg, &state(main, false, false)),
                (false, false)
            );
            assert_eq!(
                audio_directions(&cfg, &state(main, false, true)),
                (true, false)
            );
            assert_eq!(
                audio_directions(&cfg, &state(main, true, false)),
                (false, true)
            );
        }
    }
}
