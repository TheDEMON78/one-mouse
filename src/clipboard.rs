//! Synchronisation du presse-papier (texte uniquement).
//!
//! Un thread dédié possède le presse-papier du système : il le surveille
//! régulièrement et applique les textes reçus de l'autre PC.

use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::protocol::MAX_CLIPBOARD_LEN;

const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub struct ClipboardSync {
    /// Textes copiés localement, à envoyer à l'autre PC.
    pub changes: mpsc::Receiver<String>,
    setter: std_mpsc::Sender<String>,
    /// Garde le canal ouvert même si le presse-papier est indisponible,
    /// pour que `changes.recv()` attende au lieu de renvoyer `None` en boucle.
    _keepalive: mpsc::Sender<String>,
}

impl ClipboardSync {
    pub fn start() -> Self {
        let (changes_tx, changes) = mpsc::channel(8);
        let (setter, set_rx) = std_mpsc::channel::<String>();
        let keepalive = changes_tx.clone();
        std::thread::Builder::new()
            .name("presse-papier".into())
            .spawn(move || run(changes_tx, set_rx))
            .expect("création du thread presse-papier");
        Self {
            changes,
            setter,
            _keepalive: keepalive,
        }
    }

    /// Remplace le presse-papier local par un texte venu de l'autre PC.
    pub fn set(&self, text: String) {
        let _ = self.setter.send(text);
    }

    /// Ignore les copies faites pendant que l'autre PC était déconnecté.
    pub fn discard_pending(&mut self) {
        while self.changes.try_recv().is_ok() {}
    }
}

fn run(changes: mpsc::Sender<String>, set_rx: std_mpsc::Receiver<String>) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("presse-papier indisponible, synchronisation désactivée : {e}");
            return;
        }
    };
    let mut last = clipboard.get_text().ok();
    loop {
        match set_rx.recv_timeout(POLL_INTERVAL) {
            Ok(text) => {
                if let Err(e) = clipboard.set_text(text.clone()) {
                    log::warn!("impossible d'écrire dans le presse-papier : {e}");
                }
                last = Some(text);
                continue;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        }
        let Ok(text) = clipboard.get_text() else {
            continue;
        };
        if last.as_ref() == Some(&text) {
            continue;
        }
        last = Some(text.clone());
        if text.is_empty() || text.len() > MAX_CLIPBOARD_LEN {
            continue;
        }
        if changes.blocking_send(text).is_err() {
            return;
        }
    }
}
