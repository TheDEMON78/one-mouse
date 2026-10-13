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
}
