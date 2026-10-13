//! État du moteur, transmis à la fenêtre qui l'a lancé.
//!
//! Avec `--status`, chaque changement d'état est écrit sur la sortie standard
//! sous la forme `@status <état>`, lue par l'interface graphique.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

pub const PREFIX: &str = "@status ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// En attente de la connexion de l'autre PC.
    Listening,
    /// Tentative de connexion à l'autre PC.
    Connecting,
    /// Connecté, chaque PC contrôle le sien.
    Connected(String),
    /// Ce PC contrôle l'autre.
    Sending,
    /// L'autre PC contrôle celui-ci.
    Receiving,
    /// Arrêt sur une erreur.
    Error(String),
}

impl Status {
    fn encode(&self) -> String {
        match self {
            Status::Listening => "listening".into(),
            Status::Connecting => "connecting".into(),
            Status::Connected(name) => format!("connected {name}"),
            Status::Sending => "sending".into(),
            Status::Receiving => "receiving".into(),
            Status::Error(msg) => format!("error {msg}"),
        }
    }

    /// Décode une ligne de la sortie du moteur ; `None` si ce n'est pas un état.
    pub fn parse(line: &str) -> Option<Self> {
        let rest = line.strip_prefix(PREFIX)?;
        let (kind, arg) = rest.split_once(' ').unwrap_or((rest, ""));
        Some(match kind {
            "listening" => Status::Listening,
            "connecting" => Status::Connecting,
            "connected" => Status::Connected(arg.to_owned()),
            "sending" => Status::Sending,
            "receiving" => Status::Receiving,
            "error" => Status::Error(arg.to_owned()),
            _ => return None,
        })
    }
}

/// État du micro, sur des lignes `@mic <état>` distinctes.
pub const MIC_PREFIX: &str = "@mic ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicStatus {
    /// Ce PC envoie ce micro à l'autre PC.
    Sending(String),
    /// Micro de l'autre PC reçu dans ce micro virtuel, devenu le micro par défaut.
    Default(String),
    /// Reçu dans ce micro virtuel, mais le micro par défaut n'a pas pu être changé.
    NotDefault(String),
    /// Micro de l'autre PC reçu, mais aucun micro virtuel pour le recevoir.
    NoVirtual,
}

impl MicStatus {
    fn encode(&self) -> String {
        match self {
            MicStatus::Sending(name) => format!("sending {name}"),
            MicStatus::Default(name) => format!("default {name}"),
            MicStatus::NotDefault(name) => format!("notdefault {name}"),
            MicStatus::NoVirtual => "novirtual".into(),
        }
    }

    pub fn parse(line: &str) -> Option<Self> {
        let rest = line.strip_prefix(MIC_PREFIX)?;
        let (kind, arg) = rest.split_once(' ').unwrap_or((rest, ""));
        let arg = arg.to_owned();
        Some(match kind {
            "sending" => MicStatus::Sending(arg),
            "default" => MicStatus::Default(arg),
            "notdefault" => MicStatus::NotDefault(arg),
            "novirtual" => MicStatus::NoVirtual,
            _ => return None,
        })
    }
}

pub fn emit_mic(status: MicStatus) {
    if ENABLED.load(Ordering::Relaxed) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{MIC_PREFIX}{}", status.encode().replace('\n', " "));
        let _ = out.flush();
    }
}

/// Chemin de la liaison établie, sur des lignes `@link <chemin>`.
pub const LINK_PREFIX: &str = "@link ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStatus {
    /// Bluetooth (direct, ou réseau personnel Bluetooth).
    Bluetooth,
    /// Réseau local, son en UDP.
    Udp,
    /// Réseau local, son par la connexion TCP (UDP pas encore vérifié, ou bloqué).
    Tcp,
}

impl LinkStatus {
    fn encode(self) -> &'static str {
        match self {
            LinkStatus::Bluetooth => "bluetooth",
            LinkStatus::Udp => "udp",
            LinkStatus::Tcp => "tcp",
        }
    }

    pub fn parse(line: &str) -> Option<Self> {
        Some(match line.strip_prefix(LINK_PREFIX)? {
            "bluetooth" => LinkStatus::Bluetooth,
            "udp" => LinkStatus::Udp,
            "tcp" => LinkStatus::Tcp,
            _ => return None,
        })
    }
}

/// État du Bluetooth direct avant la connexion, sur des lignes `@bt <état> <texte>`.
pub const BT_PREFIX: &str = "@bt ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BtStatus {
    /// Service publié : ce PC attend en Bluetooth et est visible pour l'association.
    Visible,
    /// Recherche de l'autre PC parmi ces PC associés.
    Searching(String),
    /// Bluetooth inutilisable, ou autre PC introuvable : explication.
    Error(String),
}

impl BtStatus {
    fn encode(&self) -> String {
        match self {
            BtStatus::Visible => "visible".into(),
            BtStatus::Searching(names) => format!("searching {names}"),
            BtStatus::Error(msg) => format!("error {msg}"),
        }
    }

    pub fn parse(line: &str) -> Option<Self> {
        let rest = line.strip_prefix(BT_PREFIX)?;
        let (kind, arg) = rest.split_once(' ').unwrap_or((rest, ""));
        Some(match kind {
            "visible" => BtStatus::Visible,
            "searching" => BtStatus::Searching(arg.to_owned()),
            "error" => BtStatus::Error(arg.to_owned()),
            _ => return None,
        })
    }
}

/// Transferts de fichiers, sur des lignes `@file <état> <pourcentage> <nom>`.
pub const FILE_PREFIX: &str = "@file ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileDirection {
    Sending,
    Receiving,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStatus {
    pub direction: FileDirection,
    /// 0 à 100 ; 100 : terminé.
    pub percent: u8,
    /// Nom du fichier (envoi) ou chemin où il a été enregistré (réception).
    pub name: String,
    /// Échec : le fichier n'est pas complet.
    pub failed: bool,
}

impl FileStatus {
    fn encode(&self) -> String {
        let kind = match (self.direction, self.failed) {
            (_, true) => "failed",
            (FileDirection::Sending, false) => "sending",
            (FileDirection::Receiving, false) => "receiving",
        };
        let direction = match self.direction {
            FileDirection::Sending => "out",
            FileDirection::Receiving => "in",
        };
        format!("{kind} {direction} {} {}", self.percent, self.name)
    }

    pub fn parse(line: &str) -> Option<Self> {
        let mut parts = line.strip_prefix(FILE_PREFIX)?.splitn(4, ' ');
        let failed = match parts.next()? {
            "failed" => true,
            "sending" | "receiving" => false,
            _ => return None,
        };
        let direction = match parts.next()? {
            "out" => FileDirection::Sending,
            "in" => FileDirection::Receiving,
            _ => return None,
        };
        let percent = parts.next()?.parse().ok()?;
        let name = parts.next()?.to_owned();
        Some(Self {
            direction,
            percent,
            name,
            failed,
        })
    }
}

pub fn emit_file(status: &FileStatus) {
    emit_line(FILE_PREFIX, &status.encode());
}

/// Écrans des deux PC, sur des lignes `@screens <json>`.
pub const SCREENS_PREFIX: &str = "@screens ";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScreensStatus {
    pub local: Vec<crate::screens::Monitor>,
    pub peer: Vec<crate::screens::Monitor>,
    pub peer_name: String,
    /// Côté de l'autre PC (0 gauche, 1 droite, 2 haut, 3 bas).
    pub side: u8,
    /// Décalage du bureau de l'autre PC (après harmonisation).
    pub offset: i32,
}

impl ScreensStatus {
    pub fn parse(line: &str) -> Option<Self> {
        serde_json::from_str(line.strip_prefix(SCREENS_PREFIX)?).ok()
    }
}

pub fn emit_screens(status: &ScreensStatus) {
    if let Ok(json) = serde_json::to_string(status) {
        emit_line(SCREENS_PREFIX, &json);
    }
}

fn emit_line(prefix: &str, text: &str) {
    if ENABLED.load(Ordering::Relaxed) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{prefix}{}", text.replace('\n', " "));
        let _ = out.flush();
    }
}

pub fn emit_link(status: LinkStatus) {
    emit_line(LINK_PREFIX, status.encode());
}

pub fn emit_bt(status: BtStatus) {
    emit_line(BT_PREFIX, &status.encode());
}

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub fn emit(status: Status) {
    if ENABLED.load(Ordering::Relaxed) {
        let mut out = std::io::stdout().lock();
        // Une ligne par état ; les retours à la ligne d'un message sont aplatis.
        let _ = writeln!(out, "{PREFIX}{}", status.encode().replace('\n', " "));
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for status in [
            Status::Listening,
            Status::Connecting,
            Status::Connected("pc du salon".into()),
            Status::Sending,
            Status::Receiving,
            Status::Error("mot de passe refusé".into()),
        ] {
            let line = format!("{PREFIX}{}", status.encode());
            assert_eq!(Status::parse(&line), Some(status));
        }
        assert_eq!(Status::parse("autre chose"), None);
    }

    #[test]
    fn mic_roundtrip() {
        for status in [
            MicStatus::Sending("Micro (Realtek Audio)".into()),
            MicStatus::Default("CABLE Output".into()),
            MicStatus::NotDefault("CABLE Output".into()),
            MicStatus::NoVirtual,
        ] {
            let line = format!("{MIC_PREFIX}{}", status.encode());
            assert_eq!(MicStatus::parse(&line), Some(status));
        }
    }

    #[test]
    fn link_and_bluetooth_roundtrip() {
        for link in [LinkStatus::Bluetooth, LinkStatus::Udp, LinkStatus::Tcp] {
            assert_eq!(
                LinkStatus::parse(&format!("{LINK_PREFIX}{}", link.encode())),
                Some(link)
            );
        }
        for bt in [
            BtStatus::Visible,
            BtStatus::Searching("PC-SALON, PORTABLE".into()),
            BtStatus::Error("Bluetooth désactivé".into()),
        ] {
            let line = format!("{BT_PREFIX}{}", bt.encode());
            assert_eq!(BtStatus::parse(&line), Some(bt));
        }
    }

    #[test]
    fn file_roundtrip() {
        let status = FileStatus {
            direction: FileDirection::Receiving,
            percent: 42,
            name: "/home/moi/Téléchargements/one-mouse/mon fichier.txt".into(),
            failed: false,
        };
        let line = format!("{FILE_PREFIX}{}", status.encode());
        assert_eq!(FileStatus::parse(&line), Some(status));
    }
}
