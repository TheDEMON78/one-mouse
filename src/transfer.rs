//! Envoi de fichiers à l'autre PC.
//!
//! Les fichiers (ou dossiers) glissés sur la fenêtre one-mouse sont envoyés par
//! la connexion chiffrée, en morceaux, après la souris, le clavier et le son.
//! L'autre PC les enregistre dans « Téléchargements/one-mouse », en gardant
//! l'arborescence des dossiers. Un fichier incomplet (connexion coupée) est
//! supprimé.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use crate::protocol::{BatchKind, Message};
use crate::status::{self, FileDirection, FileStatus};

/// Taille des morceaux envoyés.
const CHUNK: usize = 64 * 1024;
/// Progression signalée tous les ~2 %.
const PROGRESS_STEP: u8 = 2;

/// Dossier où sont enregistrés les fichiers reçus.
pub fn received_dir() -> PathBuf {
    let base = directories::UserDirs::new()
        .and_then(|dirs| {
            dirs.download_dir()
                .map(Path::to_path_buf)
                .or_else(|| Some(dirs.home_dir().to_path_buf()))
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("one-mouse")
}

/// Chemin relatif reçu, rendu sûr : jamais de chemin absolu ni de « .. ».
fn sanitize(path: &str) -> Option<PathBuf> {
    let mut clean = PathBuf::new();
    for part in path.split(['/', '\\']) {
        let part = part.trim();
        if part.is_empty() || part == "." || part == ".." || part.contains(':') {
            continue;
        }
        // Caractères interdits dans les noms de fichiers Windows.
        let part: String = part
            .chars()
            .map(|c| {
                if "<>\"|?*".contains(c) || c.is_control() {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        clean.push(part);
    }
    (clean.components().count() > 0
        && clean
            .components()
            .all(|c| matches!(c, Component::Normal(_))))
    .then_some(clean)
}

/// Chemin libre : « nom (2).ext », « nom (3).ext »… si le fichier existe déjà.
fn free_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| path.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|candidate| !candidate.exists())
        .expect("un nom libre finit par exister")
}

/// Fichiers à envoyer pour ce chemin : lui-même, ou le contenu d'un dossier
/// (chemins relatifs commençant par le nom du dossier).
fn expand(path: &Path) -> Vec<(PathBuf, String)> {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Vec::new();
    };
    if path.is_file() {
        return vec![(path.to_path_buf(), name)];
    }
    let mut files = Vec::new();
    let mut pending = vec![(path.to_path_buf(), name)];
    while let Some((dir, rel)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let child = entry.path();
            let child_rel = format!("{rel}/{}", entry.file_name().to_string_lossy());
            if child.is_dir() {
                pending.push((child, child_rel));
            } else if child.is_file() {
                files.push((child, child_rel));
            }
        }
    }
    files
}

/// Envoie les fichiers d'un chemin (fichier ou dossier). `out` est la file des
/// fichiers, la moins prioritaire de la connexion : l'envoi attend quand elle
/// est pleine, sans jamais retarder la souris.
pub async fn send_path(path: PathBuf, first_id: u32, out: mpsc::Sender<Message>) {
    let files = tokio::task::spawn_blocking(move || expand(&path))
        .await
        .unwrap_or_default();
    send_files(files, first_id, None, &out).await;
}

/// Montre des fichiers reçus dans l'explorateur de fichiers (le premier
/// sélectionné quand c'est possible), d'où ils peuvent être glissés ailleurs.
pub fn reveal(paths: &[PathBuf]) {
    let Some(first) = paths.first() else {
        return;
    };
    #[cfg(windows)]
    let result = {
        use std::os::windows::process::CommandExt;
        // Chemin entre guillemets après « /select, » : forme attendue par l'Explorateur.
        std::process::Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", first.display()))
            .spawn()
    };
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open")
        .arg("-R")
        .arg(first)
        .spawn();
    #[cfg(not(any(windows, target_os = "macos")))]
    let result = std::process::Command::new("xdg-open")
        .arg(first.parent().unwrap_or(first))
        .spawn();
    if let Err(e) = result {
        log::debug!("explorateur de fichiers non ouvert : {e}");
    }
}

/// Taille maximale d'une copie de fichiers envoyée par le presse-papier : au-delà,
/// glisser les fichiers sur la fenêtre (évite d'envoyer un énorme dossier copié
/// par mégarde).
const MAX_CLIPBOARD_FILES: u64 = 1 << 30;

/// À envoyer à l'autre PC, à la demande de la fenêtre ou de la zone de dépôt.
#[derive(Debug)]
pub enum Outgoing {
    /// Fichier ou dossier glissé sur la fenêtre one-mouse.
    File(PathBuf),
    /// Fichiers glissés jusqu'au bord de l'écran.
    Drop(Vec<PathBuf>),
}

/// Envoie des fichiers copiés, glissés ou une image pour le presse-papier de
/// l'autre PC : un en-tête de lot, puis les fichiers.
pub async fn send_clipboard(
    items: Vec<PathBuf>,
    batch: u32,
    kind: BatchKind,
    first_id: u32,
    out: mpsc::Sender<Message>,
) {
    let image = kind == BatchKind::Image;
    let expanded = items.clone();
    let files: Vec<(PathBuf, String)> = tokio::task::spawn_blocking(move || {
        expanded.iter().flat_map(|item| expand(item)).collect()
    })
    .await
    .unwrap_or_default();
    let total: u64 = files
        .iter()
        .filter_map(|(path, _)| std::fs::metadata(path).ok())
        .map(|m| m.len())
        .sum();
    if kind == BatchKind::Files && total > MAX_CLIPBOARD_FILES {
        log::warn!(
            "copie de {} non envoyée par le presse-papier (plus de {}) : glissez ces fichiers \
             sur la fenêtre one-mouse",
            human_size(total),
            human_size(MAX_CLIPBOARD_FILES)
        );
    } else if !files.is_empty() {
        if kind == BatchKind::Drop {
            log::info!(
                "fichiers glissés vers l'autre PC : {} élément(s), {}",
                files.len(),
                human_size(total)
            );
        }
        let header = Message::ClipboardBatch {
            batch,
            count: files.len() as u32,
            kind,
        };
        if out.send(header).await.is_ok() {
            send_files(files, first_id, Some(batch), &out).await;
        }
    }
    if image {
        // Image écrite dans un fichier temporaire pour l'envoi.
        for item in items {
            let _ = std::fs::remove_file(item);
        }
    }
}

async fn send_files(
    files: Vec<(PathBuf, String)>,
    first_id: u32,
    batch: Option<u32>,
    out: &mpsc::Sender<Message>,
) {
    for (index, (file, rel)) in files.into_iter().enumerate() {
        let id = first_id.wrapping_add(index as u32);
        if let Err(e) = send_file(&file, &rel, id, batch, out).await {
            log::warn!("envoi de « {rel} » impossible : {e:#}");
            status::emit_file(&FileStatus {
                direction: FileDirection::Sending,
                percent: 0,
                name: rel,
                failed: true,
            });
        }
    }
}

async fn send_file(
    file: &Path,
    rel: &str,
    id: u32,
    batch: Option<u32>,
    out: &mpsc::Sender<Message>,
) -> Result<()> {
    let mut source = tokio::fs::File::open(file)
        .await
        .with_context(|| format!("lecture de {} impossible", file.display()))?;
    let size = source.metadata().await?.len();
    let closed = || anyhow::anyhow!("connexion fermée");
    out.send(Message::FileStart {
        id,
        path: rel.to_owned(),
        size,
        batch,
    })
    .await
    .map_err(|_| closed())?;
    log::info!("envoi de « {rel} » ({})", human_size(size));
    let mut progress = Progress::new(FileDirection::Sending, rel.to_owned(), size);
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = source.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        out.send(Message::FileChunk {
            id,
            data: buf[..n].to_vec(),
        })
        .await
        .map_err(|_| closed())?;
        progress.add(n as u64);
    }
    out.send(Message::FileEnd { id })
        .await
        .map_err(|_| closed())?;
    progress.done();
    log::info!("« {rel} » envoyé");
    Ok(())
}

/// Taille lisible (« 3,2 Mo »).
fn human_size(bytes: u64) -> String {
    let (value, unit) = match bytes {
        b if b >= 1 << 30 => (b as f64 / (1u64 << 30) as f64, "Go"),
        b if b >= 1 << 20 => (b as f64 / (1u64 << 20) as f64, "Mo"),
        b if b >= 1 << 10 => (b as f64 / (1u64 << 10) as f64, "Ko"),
        b => (b as f64, "octets"),
    };
    format!("{value:.1} {unit}").replace('.', ",")
}

/// Progression signalée à la fenêtre.
struct Progress {
    direction: FileDirection,
    name: String,
    size: u64,
    done: u64,
    shown: u8,
}

impl Progress {
    fn new(direction: FileDirection, name: String, size: u64) -> Self {
        let progress = Self {
            direction,
            name,
            size,
            done: 0,
            shown: 0,
        };
        progress.emit(0);
        progress
    }

    fn add(&mut self, bytes: u64) {
        self.done += bytes;
        let percent = (self.done * 100 / self.size.max(1)).min(99) as u8;
        if percent >= self.shown + PROGRESS_STEP {
            self.shown = percent;
            self.emit(percent);
        }
    }

    fn done(&self) {
        self.emit(100);
    }

    fn emit(&self, percent: u8) {
        status::emit_file(&FileStatus {
            direction: self.direction,
            percent,
            name: self.name.clone(),
            failed: false,
        });
    }
}

/// Fichier en cours de réception.
struct Partial {
    file: std::fs::File,
    path: PathBuf,
    progress: Progress,
    /// Lot du presse-papier auquel il appartient.
    batch: Option<u32>,
}

/// Lot destiné au presse-papier, en cours de réception.
struct Batch {
    id: u32,
    dir: PathBuf,
    remaining: u32,
    kind: BatchKind,
    /// Éléments copiés (fichiers ou dossiers du premier niveau).
    items: std::collections::BTreeSet<PathBuf>,
}

/// Fichiers en cours de réception.
#[derive(Default)]
pub struct Incoming {
    files: HashMap<u32, Partial>,
    batch: Option<Batch>,
}

impl Incoming {
    /// Un lot pour le presse-papier commence : ses fichiers vont dans un dossier
    /// à lui, « Téléchargements/one-mouse/Presse-papier/… », ou directement dans
    /// « Téléchargements/one-mouse » s'ils ont été glissés.
    pub fn start_batch(&mut self, id: u32, count: u32, kind: BatchKind) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let dir = match kind {
            BatchKind::Drop => received_dir(),
            _ => received_dir()
                .join("Presse-papier")
                .join(format!("{stamp}-{id}")),
        };
        self.batch = Some(Batch {
            id,
            dir,
            remaining: count,
            kind,
            items: Default::default(),
        });
    }

    pub fn start(&mut self, id: u32, path: &str, size: u64, batch: Option<u32>) {
        let base = match (&mut self.batch, batch) {
            (Some(current), Some(b)) if current.id == b => current.dir.clone(),
            _ => received_dir(),
        };
        let result = (|| -> Result<Partial> {
            let rel = sanitize(path).context("nom de fichier invalide")?;
            let first = rel.components().next().map(|c| base.join(c));
            // Premier niveau déjà reçu dans ce lot (fichier d'un dossier glissé) :
            // pas de renommage, il complète ce dossier.
            let in_batch_dir = match (&self.batch, batch, &first) {
                (Some(current), Some(b), Some(first)) if current.id == b => {
                    current.items.contains(first) && rel.components().count() > 1
                }
                _ => false,
            };
            let target = if in_batch_dir {
                base.join(&rel)
            } else {
                free_path(base.join(&rel))
            };
            if let (Some(current), Some(b)) = (&mut self.batch, batch)
                && current.id == b
                && let Ok(inside) = target.strip_prefix(&base)
                && let Some(top) = inside.components().next()
            {
                current.items.insert(base.join(top));
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = std::fs::File::create(&target)
                .with_context(|| format!("création de {} impossible", target.display()))?;
            log::info!(
                "réception de « {} » ({})",
                target.display(),
                human_size(size)
            );
            let progress =
                Progress::new(FileDirection::Receiving, target.display().to_string(), size);
            Ok(Partial {
                file,
                path: target,
                progress,
                batch,
            })
        })();
        match result {
            Ok(partial) => {
                self.files.insert(id, partial);
            }
            Err(e) => log::warn!("fichier reçu ignoré : {e:#}"),
        }
    }

    pub fn chunk(&mut self, id: u32, data: &[u8]) {
        let Some(partial) = self.files.get_mut(&id) else {
            return;
        };
        if let Err(e) = partial.file.write_all(data) {
            log::warn!("écriture de {} impossible : {e}", partial.path.display());
            self.fail(id);
            return;
        }
        partial.progress.add(data.len() as u64);
    }

    /// Fichier complet. Renvoie le contenu à mettre dans le presse-papier quand
    /// c'était le dernier fichier d'un lot, avec l'origine du lot.
    pub fn end(&mut self, id: u32) -> Option<(crate::clipboard::Clip, BatchKind)> {
        let partial = self.files.remove(&id)?;
        let _ = partial.file.sync_all();
        partial.progress.done();
        log::info!("fichier reçu : {}", partial.path.display());
        let batch = self
            .batch
            .as_mut()
            .filter(|b| Some(b.id) == partial.batch)?;
        batch.remaining = batch.remaining.saturating_sub(1);
        if batch.remaining > 0 {
            return None;
        }
        let batch = self.batch.take()?;
        if batch.kind == BatchKind::Image {
            // Image : remise dans le presse-papier, fichier temporaire supprimé.
            let clip = std::fs::read(&partial.path)
                .ok()
                .and_then(|png| crate::clipboard::decode_png(&png).ok());
            let _ = std::fs::remove_file(&partial.path);
            let _ = std::fs::remove_dir(&batch.dir);
            log::info!("image copiée sur l'autre PC : collez-la ici avec Ctrl+V");
            clip.map(|clip| (clip, BatchKind::Image))
        } else {
            if batch.kind == BatchKind::Drop {
                log::info!(
                    "fichiers glissés depuis l'autre PC : dans {}, et collables ici avec Ctrl+V",
                    batch.dir.display()
                );
            } else {
                log::info!("fichiers copiés sur l'autre PC : collez-les ici avec Ctrl+V");
            }
            Some((
                crate::clipboard::Clip::Files(batch.items.into_iter().collect()),
                batch.kind,
            ))
        }
    }

    fn fail(&mut self, id: u32) {
        if let Some(partial) = self.files.remove(&id) {
            drop(partial.file);
            let _ = std::fs::remove_file(&partial.path);
            status::emit_file(&FileStatus {
                direction: FileDirection::Receiving,
                percent: 0,
                name: partial.path.display().to_string(),
                failed: true,
            });
        }
    }
}

impl Drop for Incoming {
    /// Connexion coupée : les fichiers incomplets sont supprimés.
    fn drop(&mut self) {
        let ids: Vec<u32> = self.files.keys().copied().collect();
        for id in ids {
            log::warn!("réception interrompue : fichier incomplet supprimé");
            self.fail(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn received_paths_stay_inside_the_folder() {
        assert_eq!(sanitize("photo.jpg"), Some(PathBuf::from("photo.jpg")));
        assert_eq!(
            sanitize("Dossier/sous/doc.txt"),
            Some(PathBuf::from("Dossier").join("sous").join("doc.txt"))
        );
        // Tentatives de sortir du dossier : neutralisées.
        assert_eq!(
            sanitize("../../etc/passwd"),
            Some(PathBuf::from("etc").join("passwd"))
        );
        assert_eq!(
            sanitize("/etc/passwd"),
            Some(PathBuf::from("etc").join("passwd"))
        );
        assert_eq!(
            sanitize("C:\\Windows\\x.dll"),
            Some(PathBuf::from("Windows").join("x.dll"))
        );
        assert_eq!(sanitize(".."), None);
        assert_eq!(sanitize("a?b.txt"), Some(PathBuf::from("a_b.txt")));
    }

    #[test]
    fn existing_files_are_not_overwritten() {
        let dir = std::env::temp_dir().join(format!("one-mouse-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("note.txt");
        std::fs::write(&first, "a").unwrap();
        assert_eq!(free_path(first.clone()), dir.join("note (2).txt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn folders_are_sent_with_their_tree() {
        let dir = std::env::temp_dir().join(format!("one-mouse-arbo-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sous")).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("sous").join("b.txt"), "b").unwrap();
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let mut files: Vec<String> = expand(&dir).into_iter().map(|(_, rel)| rel).collect();
        files.sort();
        assert_eq!(
            files,
            vec![format!("{name}/a.txt"), format!("{name}/sous/b.txt")]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(human_size(512), "512,0 octets");
        assert_eq!(human_size(3 * 1024 * 1024 + 300_000), "3,3 Mo");
    }
}
