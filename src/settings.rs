//! Réglages de l'interface graphique, enregistrés dans le dossier de
//! configuration de l'utilisateur.

use std::path::PathBuf;

use anyhow::{Context, Result};
use input_capture::Position;
use serde::{Deserialize, Serialize};

use crate::audio;

/// Position de l'autre PC par rapport à celui-ci.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub const ALL: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];

    pub fn arg(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
            Side::Top => "top",
            Side::Bottom => "bottom",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Side::Left => "à gauche",
            Side::Right => "à droite",
            Side::Top => "au-dessus",
            Side::Bottom => "en dessous",
        }
    }
}

impl From<Side> for Position {
    fn from(side: Side) -> Self {
        match side {
            Side::Left => Position::Left,
            Side::Right => Position::Right,
            Side::Top => Position::Top,
            Side::Bottom => Position::Bottom,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `true` : ce PC se connecte à `address` ; `false` : il attend l'autre.
    pub connect: bool,
    /// Adresse de l'autre PC.
    pub address: String,
    pub side: Side,
    pub password: String,
    pub name: String,
    pub clipboard: bool,
    /// PC principal : souris, clavier et casque branchés dessus.
    pub main: bool,
    /// Vitesse de la souris sur l'autre PC.
    pub pointer_speed: f64,
    pub sound: audio::Mode,
    pub mic: audio::Mode,
    pub sound_output: String,
    pub mic_output: String,
    /// Micro à envoyer (vide = micro par défaut du système).
    pub mic_device: String,
    /// Qualité du son envoyé.
    pub sound_quality: crate::codec::Quality,
    /// Démarre le partage dès l'ouverture de la fenêtre.
    pub start_sharing: bool,
    /// Liaison par Bluetooth (réseau personnel) plutôt que par le réseau local.
    pub bluetooth: bool,
    /// Windows : fermer la fenêtre la garde dans la zone de notification.
    pub tray: bool,
    /// Version du format, pour adapter les anciens réglages (absent = ancien format 0).
    #[serde(default)]
    pub format: u32,
}

/// Format actuel des réglages.
const FORMAT: u32 = 3;

impl Default for Settings {
    fn default() -> Self {
        Self {
            connect: false,
            address: String::new(),
            side: Side::Right,
            password: String::new(),
            name: String::new(),
            clipboard: true,
            main: false,
            pointer_speed: 1.0,
            sound: audio::Mode::Always,
            mic: audio::Mode::Always,
            sound_output: String::new(),
            mic_output: String::new(),
            mic_device: String::new(),
            sound_quality: crate::codec::Quality::Lossless,
            start_sharing: false,
            bluetooth: false,
            tray: true,
            format: FORMAT,
        }
    }
}

impl Settings {
    fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("com", "BloopStudio", "one-mouse")
            .map(|dirs| dirs.config_dir().join("settings.json"))
    }

    pub fn load() -> Self {
        let mut settings: Self = Self::path()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default();
        settings.migrate();
        settings
    }

    /// Adapte les réglages enregistrés par une version précédente.
    fn migrate(&mut self) {
        if self.format < 2 && self.sound == audio::Mode::Auto {
            // Avant le « PC principal », « auto » était le défaut : le son est maintenant
            // envoyé en permanence vers le PC principal.
            self.sound = audio::Mode::Always;
        }
        if self.format < 3 && self.mic == audio::Mode::Off {
            // Le micro du PC principal est maintenant envoyé par défaut à l'autre PC.
            self.mic = audio::Mode::Always;
        }
        self.format = FORMAT;
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path().context("dossier de configuration introuvable")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, serde_json::to_vec_pretty(self)?)?;
        // Le mot de passe est stocké en clair : fichier lisible par l'utilisateur seul.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    /// Arguments de ligne de commande du moteur correspondant à ces réglages
    /// (le mot de passe passe par une variable d'environnement, pas par les arguments).
    pub fn engine_args(&self) -> Vec<String> {
        let mut args = vec![
            "--status".to_owned(),
            "--side".to_owned(),
            self.side.arg().to_owned(),
            "--sound".to_owned(),
            self.sound.arg().to_owned(),
            "--mic".to_owned(),
            self.mic.arg().to_owned(),
        ];
        let optional = [
            ("--name", &self.name),
            ("--sound-output", &self.sound_output),
            ("--mic-output", &self.mic_output),
            ("--mic-device", &self.mic_device),
        ];
        for (flag, value) in optional {
            if !value.trim().is_empty() {
                args.push(flag.to_owned());
                args.push(value.trim().to_owned());
            }
        }
        if !self.clipboard {
            args.push("--no-clipboard".to_owned());
        }
        if self.main {
            args.push("--main".to_owned());
        }
        if self.bluetooth {
            args.push("--bluetooth".to_owned());
        }
        if self.sound_quality != crate::codec::Quality::Lossless {
            args.push("--sound-quality".to_owned());
            args.push(self.sound_quality.arg().to_owned());
        }
        if self.pointer_speed != 1.0 {
            args.push("--pointer-speed".to_owned());
            args.push(format!("{:.2}", self.pointer_speed));
        }
        if self.connect {
            args.push("connect".to_owned());
            // Sans adresse (et toujours en Bluetooth) : l'autre PC est trouvé tout seul.
            if !self.bluetooth && !self.address.trim().is_empty() {
                args.push(self.address.trim().to_owned());
            }
        } else {
            args.push("listen".to_owned());
        }
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_args_listen() {
        let settings = Settings::default();
        assert_eq!(
            settings.engine_args(),
            [
                "--status", "--side", "right", "--sound", "always", "--mic", "always", "listen"
            ]
        );
    }

    #[test]
    fn engine_args_connect() {
        let settings = Settings {
            connect: true,
            address: " 192.168.1.20 ".into(),
            side: Side::Left,
            mic: audio::Mode::Auto,
            mic_output: "CABLE Input".into(),
            clipboard: false,
            main: true,
            ..Settings::default()
        };
        assert_eq!(
            settings.engine_args(),
            [
                "--status",
                "--side",
                "left",
                "--sound",
                "always",
                "--mic",
                "auto",
                "--mic-output",
                "CABLE Input",
                "--no-clipboard",
                "--main",
                "connect",
                "192.168.1.20"
            ]
        );
    }

    #[test]
    fn old_files_load_with_defaults() {
        let settings: Settings = serde_json::from_str(r#"{"password":"secret123"}"#).unwrap();
        assert_eq!(settings.password, "secret123");
        assert_eq!(settings.side, Side::Right);
    }

    #[test]
    fn old_auto_sound_becomes_always() {
        let mut settings: Settings = serde_json::from_str(r#"{"sound":"auto"}"#).unwrap();
        settings.migrate();
        assert_eq!(settings.sound, audio::Mode::Always);
        // Un choix fait avec le format actuel est respecté.
        let mut settings: Settings =
            serde_json::from_str(r#"{"sound":"auto","format":3}"#).unwrap();
        settings.migrate();
        assert_eq!(settings.sound, audio::Mode::Auto);
    }
}
