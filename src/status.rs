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
}
