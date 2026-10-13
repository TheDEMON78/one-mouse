//! Synchronisation du presse-papier : texte, images et fichiers.
//!
//! Un thread dédié possède le presse-papier du système : il le surveille
//! régulièrement et applique ce qui arrive de l'autre PC. Les images et les
//! fichiers voyagent comme des fichiers (voir `transfer`) puis sont remis dans
//! le presse-papier de l'autre PC : Ctrl+V y colle l'image ou les fichiers.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::protocol::MAX_CLIPBOARD_LEN;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Lire une image du presse-papier coûte cher (conversion) : moins souvent.
const IMAGE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Contenu du presse-papier.
#[derive(Debug, Clone)]
pub enum Clip {
    Text(String),
    /// Image en RGBA 8 bits.
    Image {
        width: usize,
        height: usize,
        rgba: Vec<u8>,
    },
    /// Fichiers et dossiers copiés (Ctrl+C dans l'explorateur).
    Files(Vec<PathBuf>),
}

/// Empreinte légère, pour repérer un changement sans garder l'image.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fingerprint {
    Text(String),
    Image(u64),
    Files(Vec<PathBuf>),
}

fn image_hash(rgba: &[u8], width: usize, height: usize) -> u64 {
    let mut hasher = DefaultHasher::new();
    (width, height).hash(&mut hasher);
    rgba.hash(&mut hasher);
    hasher.finish()
}

impl Clip {
    fn fingerprint(&self) -> Fingerprint {
        match self {
            Clip::Text(text) => Fingerprint::Text(text.clone()),
            Clip::Image {
                width,
                height,
                rgba,
            } => Fingerprint::Image(image_hash(rgba, *width, *height)),
            Clip::Files(files) => Fingerprint::Files(files.clone()),
        }
    }
}

pub struct ClipboardSync {
    /// Contenus copiés localement, à envoyer à l'autre PC.
    pub changes: mpsc::Receiver<Clip>,
    setter: std_mpsc::Sender<Clip>,
    /// Garde le canal ouvert même si le presse-papier est indisponible,
    /// pour que `changes.recv()` attende au lieu de renvoyer `None` en boucle.
    _keepalive: mpsc::Sender<Clip>,
}

impl ClipboardSync {
    pub fn start() -> Self {
        let (changes_tx, changes) = mpsc::channel(8);
        let (setter, set_rx) = std_mpsc::channel::<Clip>();
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

    /// Remplace le presse-papier local par un contenu venu de l'autre PC.
    pub fn set(&self, clip: Clip) {
        let _ = self.setter.send(clip);
    }

    /// Ignore les copies faites pendant que l'autre PC était déconnecté.
    pub fn discard_pending(&mut self) {
        while self.changes.try_recv().is_ok() {}
    }
}

/// Numéro de version du presse-papier (Windows) : rien à lire s'il n'a pas changé.
#[cfg(windows)]
fn sequence() -> Option<u32> {
    // SAFETY : fonction sans argument.
    Some(unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() })
}

#[cfg(not(windows))]
fn sequence() -> Option<u32> {
    None
}

/// Lit le presse-papier : fichiers, sinon texte, sinon (si `with_image`) image.
fn read(clipboard: &mut arboard::Clipboard, with_image: bool) -> Option<Clip> {
    if let Ok(files) = clipboard.get().file_list()
        && !files.is_empty()
    {
        return Some(Clip::Files(files));
    }
    if let Ok(text) = clipboard.get_text()
        && !text.is_empty()
    {
        return Some(Clip::Text(text));
    }
    if with_image && let Ok(image) = clipboard.get_image() {
        return Some(Clip::Image {
            width: image.width,
            height: image.height,
            rgba: image.bytes.into_owned(),
        });
    }
    None
}

fn write(clipboard: &mut arboard::Clipboard, clip: Clip) {
    let result = match clip {
        Clip::Text(text) => clipboard.set_text(text),
        Clip::Image {
            width,
            height,
            rgba,
        } => clipboard.set_image(arboard::ImageData {
            width,
            height,
            bytes: rgba.into(),
        }),
        Clip::Files(files) => clipboard.set().file_list(&files),
    };
    if let Err(e) = result {
        log::warn!("impossible d'écrire dans le presse-papier : {e}");
    }
}

fn run(changes: mpsc::Sender<Clip>, set_rx: std_mpsc::Receiver<Clip>) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            log::warn!("presse-papier indisponible, synchronisation désactivée : {e}");
            return;
        }
    };
    // Ce qui est déjà dans le presse-papier au démarrage n'est pas envoyé.
    let mut last = read(&mut clipboard, true).map(|c| c.fingerprint());
    let mut last_sequence = sequence();
    let mut last_image_check = Instant::now();
    loop {
        match set_rx.recv_timeout(POLL_INTERVAL) {
            Ok(clip) => {
                last = Some(clip.fingerprint());
                write(&mut clipboard, clip);
                last_sequence = sequence();
                continue;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        }
        // Windows : presse-papier inchangé, rien à lire.
        if let Some(current) = sequence() {
            if Some(current) == last_sequence {
                continue;
            }
            last_sequence = Some(current);
        }
        let with_image = sequence().is_some() || last_image_check.elapsed() >= IMAGE_POLL_INTERVAL;
        if with_image {
            last_image_check = Instant::now();
        }
        let Some(clip) = read(&mut clipboard, with_image) else {
            continue;
        };
        let fingerprint = clip.fingerprint();
        if last.as_ref() == Some(&fingerprint) {
            continue;
        }
        last = Some(fingerprint);
        if let Clip::Text(text) = &clip
            && text.len() > MAX_CLIPBOARD_LEN
        {
            continue;
        }
        if changes.blocking_send(clip).is_err() {
            return;
        }
    }
}

/// Encode une image du presse-papier en PNG (pour l'envoyer comme un fichier).
pub fn encode_png(width: usize, height: usize, rgba: &[u8]) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context;
    let image = image::RgbaImage::from_raw(width as u32, height as u32, rgba.to_vec())
        .context("image du presse-papier invalide")?;
    let mut png = Vec::new();
    image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
    Ok(png)
}

/// Décode une image PNG reçue pour la mettre dans le presse-papier.
pub fn decode_png(png: &[u8]) -> anyhow::Result<Clip> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)?.to_rgba8();
    Ok(Clip::Image {
        width: image.width() as usize,
        height: image.height() as usize,
        rgba: image.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_roundtrip_keeps_the_image() {
        let (width, height) = (3, 2);
        let rgba: Vec<u8> = (0..width * height * 4).map(|i| (i * 7) as u8).collect();
        let png = encode_png(width, height, &rgba).unwrap();
        let Clip::Image {
            width: w,
            height: h,
            rgba: back,
        } = decode_png(&png).unwrap()
        else {
            panic!("image attendue");
        };
        assert_eq!((w, h), (width, height));
        assert_eq!(back, rgba);
    }
}
