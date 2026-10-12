//! Une connexion active avec l'autre PC : capture, émulation et presse-papier.
//!
//! Les deux PC jouent le même rôle. Celui dont le curseur franchit le bord de
//! l'écran du côté de l'autre PC devient « émetteur » : ses entrées locales sont
//! capturées et envoyées, l'autre PC les rejoue. Quand le curseur rejoué atteint
//! le bord du retour, le récepteur envoie `Leave` et l'émetteur rend la main.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use input_capture::{CaptureEvent, InputCapture, Position};
use input_emulation::InputEmulation;
use input_event::{Event, KeyboardEvent, scancode};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::clipboard::ClipboardSync;
use crate::crypto::{self, Psk, Role};
use crate::protocol::{MAX_CLIPBOARD_LEN, Message};

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
    pub capture: Option<InputCapture>,
    pub emulation: InputEmulation,
    pub clipboard: Option<ClipboardSync>,
    pub psk: Psk,
    pub name: String,
    /// Position de l'autre PC par rapport à celui-ci.
    pub peer_side: Position,
}

#[derive(Default)]
struct State {
    /// Nos entrées sont capturées et envoyées à l'autre PC.
    sending: bool,
    /// L'autre PC nous envoie ses entrées.
    receiving: bool,
}

pub async fn run(stream: TcpStream, role: Role, local: &mut Local) -> Result<()> {
    stream.set_nodelay(true)?;
    let (reader, writer) = stream.into_split();
    let (mut reader, mut writer, peer_name) = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        crypto::handshake(reader, writer, role, &local.psk, &local.name),
    )
    .await
    .context("l'autre PC n'a pas répondu à temps")??;
    log::info!(
        "connecté à « {peer_name} » (situé à {})",
        side_name(local.peer_side)
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

    let result = event_loop(local, &out_tx, &mut in_rx).await;

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
    out: &mpsc::Sender<Message>,
    incoming: &mut mpsc::Receiver<Message>,
) -> Result<()> {
    let mut state = State::default();
    let mut ping = tokio::time::interval(PING_INTERVAL);

    loop {
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
                on_message(local, &mut state, out, msg).await?;
            }
            Some(text) = recv_clipboard(&mut local.clipboard) => {
                log::debug!("presse-papier local modifié ({} octets), envoi", text.len());
                send(out, Message::Clipboard(text)).await?;
            }
            _ = ping.tick() => send(out, Message::Ping).await?,
        }
    }
}

async fn next_capture(
    capture: &mut Option<InputCapture>,
) -> Option<Result<(u64, CaptureEvent), input_capture::CaptureError>> {
    match capture {
        Some(c) => c.next().await,
        None => std::future::pending().await,
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
    capture: &mut InputCapture,
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
            send(out, Message::Enter).await?;
        }
        CaptureEvent::Input(_) if !state.sending => {}
        CaptureEvent::Input(event) => {
            if capture.keys_pressed(&RELEASE_KEYS) {
                log::info!("raccourci de secours : reprise du curseur");
                stop_sending(capture, state, out).await?;
                return Ok(());
            }
            send(out, Message::Input(event.into())).await?;
        }
    }
    Ok(())
}

async fn on_message(
    local: &mut Local,
    state: &mut State,
    out: &mpsc::Sender<Message>,
    msg: Message,
) -> Result<()> {
    match msg {
        Message::Enter => {
            log::info!("l'autre PC prend le contrôle de celui-ci");
            state.receiving = true;
        }
        Message::Input(event) if state.receiving => {
            local.emulation.consume(event.into(), PEER).await?;
        }
        Message::Input(_) => {}
        Message::Exit => {
            state.receiving = false;
            local.emulation.release_keys(PEER).await?;
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
        Message::Ping => {}
        Message::Hello { .. } => bail!("message Hello inattendu"),
    }
    Ok(())
}

/// Rend la main localement : relâche les touches encore enfoncées chez l'autre PC.
async fn stop_sending(
    capture: &mut InputCapture,
    state: &mut State,
    out: &mpsc::Sender<Message>,
) -> Result<()> {
    state.sending = false;
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
