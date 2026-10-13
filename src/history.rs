//! Historique du presse-papier partagé : les dernières copies des deux PC.
//!
//! Le moteur y ajoute chaque copie (de ce PC ou reçue de l'autre) ; la fenêtre
//! l'affiche et permet de recopier un élément. Il peut contenir des données
//! sensibles (mots de passe copiés…) : il est effacé à la fermeture de
//! one-mouse et peut être désactivé.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::clipboard::Clip;

/// Nombre de copies gardées.
pub const MAX_ENTRIES: usize = 20;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Heure de la copie (secondes depuis 1970).
    pub at: u64,
    /// PC où la copie a été faite.
    pub from: String,
    pub content: Content,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Content {
    Text {
        text: String,
    },
    Image {
        width: usize,
        height: usize,
        png: PathBuf,
    },
    Files {
        paths: Vec<PathBuf>,
    },
}

fn dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("com", "BloopStudio", "one-mouse")
        .map(|dirs| dirs.config_dir().join("historique"))
}

fn index() -> Option<PathBuf> {
    dir().map(|d| d.join("presse-papier.json"))
}

/// Copies enregistrées, la plus récente en premier.
pub fn load() -> Vec<Entry> {
    index()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

/// Efface tout l'historique (à la fermeture, ou à la demande).
pub fn clear() {
    if let Some(dir) = dir() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Ajoute une copie en tête de l'historique (opération bloquante : fichiers).
pub fn record(from: &str, clip: &Clip) {
    let Some(dir) = dir() else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let content = match clip {
        Clip::Text(text) => Content::Text { text: text.clone() },
        Clip::Files(paths) => Content::Files {
            paths: paths.clone(),
        },
        Clip::Image {
            width,
            height,
            rgba,
        } => {
            let Ok(png) = crate::clipboard::encode_png(*width, *height, rgba) else {
                return;
            };
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let path = dir.join(format!("image-{nanos}.png"));
            if std::fs::write(&path, png).is_err() {
                return;
            }
            Content::Image {
                width: *width,
                height: *height,
                png: path,
            }
        }
    };
    let mut entries = load();
    // Même contenu que la dernière copie (aller-retour entre les PC) : ignoré.
    if entries
        .first()
        .is_some_and(|last| same_content(&last.content, &content))
    {
        if let Content::Image { png, .. } = &content {
            let _ = std::fs::remove_file(png);
        }
        return;
    }
    entries.insert(
        0,
        Entry {
            at,
            from: from.to_owned(),
            content,
        },
    );
    for old in entries.drain(MAX_ENTRIES.min(entries.len())..) {
        if let Content::Image { png, .. } = old.content {
            let _ = std::fs::remove_file(png);
        }
    }
    if let (Some(path), Ok(data)) = (index(), serde_json::to_vec(&entries)) {
        let _ = std::fs::write(path, data);
    }
}

fn same_content(a: &Content, b: &Content) -> bool {
    match (a, b) {
        (Content::Text { text: a }, Content::Text { text: b }) => a == b,
        (Content::Files { paths: a }, Content::Files { paths: b }) => a == b,
        (
            Content::Image {
                width: wa,
                height: ha,
                png: pa,
            },
            Content::Image {
                width: wb,
                height: hb,
                png: pb,
            },
        ) => wa == wb && ha == hb && std::fs::read(pa).ok() == std::fs::read(pb).ok(),
        _ => false,
    }
}

/// Contenu à remettre dans le presse-papier pour une copie de l'historique.
pub fn to_clip(content: &Content) -> Option<Clip> {
    match content {
        Content::Text { text } => Some(Clip::Text(text.clone())),
        Content::Files { paths } => Some(Clip::Files(paths.clone())),
        Content::Image { png, .. } => crate::clipboard::decode_png(&std::fs::read(png).ok()?).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_texts_are_detected() {
        let a = Content::Text { text: "x".into() };
        let b = Content::Text { text: "x".into() };
        let c = Content::Text { text: "y".into() };
        assert!(same_content(&a, &b));
        assert!(!same_content(&a, &c));
    }
}
