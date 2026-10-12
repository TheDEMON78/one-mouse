//! one-mouse : une seule souris et un seul clavier pour deux PC posés côte à côte.
//!
//! Sans argument, ouvre la fenêtre de réglages ; avec `listen` / `connect`,
//! lance directement le moteur (c'est aussi ce que fait la fenêtre, dans un
//! processus séparé).

// Sous Windows : pas de console noire au lancement de l'interface.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod audio;
mod autostart;
mod capture;
mod clipboard;
mod crypto;
mod firewall;
mod gui;
mod protocol;
mod session;
mod settings;
mod status;

use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use input_emulation::InputEmulation;
use tokio::net::{TcpListener, TcpStream};

use crate::audio::AudioConfig;
use crate::capture::Capture;
use crate::clipboard::ClipboardSync;
use crate::crypto::{Psk, Role};
use crate::session::Local;
use crate::settings::Side;
use crate::status::Status;

const DEFAULT_PORT: u16 = 24850;
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

#[derive(Parser)]
#[command(
    version,
    about = "Une seule souris et un seul clavier pour deux PC (Windows, Linux, macOS)"
)]
struct Cli {
    /// Sans commande : ouvre la fenêtre de réglages.
    #[command(subcommand)]
    mode: Option<Mode>,

    /// Écrit les changements d'état sur la sortie standard (utilisé par la fenêtre).
    #[arg(long, global = true, hide = true)]
    status: bool,

    /// Position de l'autre PC par rapport à celui-ci.
    #[arg(long, value_enum, global = true, default_value = "right")]
    side: Side,

    /// Mot de passe partagé (identique sur les deux PC, 8 caractères minimum).
    #[arg(
        long,
        env = "ONE_MOUSE_PASSWORD",
        global = true,
        hide_env_values = true
    )]
    password: Option<String>,

    /// Nom de ce PC affiché chez l'autre.
    #[arg(long, global = true)]
    name: Option<String>,

    /// Désactive le partage du presse-papier.
    #[arg(long, global = true)]
    no_clipboard: bool,

    /// Envoi du son de ce PC à l'autre (auto : quand l'autre PC contrôle celui-ci).
    #[arg(long, value_enum, global = true, default_value = "auto")]
    sound: audio::Mode,

    /// Envoi du micro de ce PC à l'autre (auto : quand ce PC contrôle l'autre).
    #[arg(long, value_enum, global = true, default_value = "off")]
    mic: audio::Mode,

    /// Source du son à envoyer (par défaut : la sortie audio principale).
    #[arg(long, global = true, value_name = "PÉRIPHÉRIQUE")]
    sound_device: Option<String>,

    /// Micro à envoyer (par défaut : le micro principal).
    #[arg(long, global = true, value_name = "PÉRIPHÉRIQUE")]
    mic_device: Option<String>,

    /// Sortie où jouer le son reçu de l'autre PC (par défaut : la sortie principale).
    #[arg(long, global = true, value_name = "PÉRIPHÉRIQUE")]
    sound_output: Option<String>,

    /// Sortie où jouer le micro reçu de l'autre PC, par exemple un câble audio virtuel
    /// (« CABLE Input », « BlackHole »…) pour l'utiliser comme micro dans les applications.
    #[arg(long, global = true, value_name = "PÉRIPHÉRIQUE")]
    mic_output: Option<String>,
}

#[derive(Subcommand)]
enum Mode {
    /// Attend la connexion de l'autre PC.
    Listen {
        /// Adresse d'écoute.
        #[arg(default_value_t = format!("0.0.0.0:{DEFAULT_PORT}"))]
        bind: String,
    },
    /// Se connecte à l'autre PC (reconnexion automatique).
    Connect {
        /// Adresse de l'autre PC, par exemple 192.168.1.20 ou 192.168.1.20:24850.
        host: String,
    },
    /// Liste les périphériques audio (pour --sound-device, --mic-output…).
    AudioDevices,
}

fn main() -> Result<()> {
    #[cfg(windows)]
    attach_console();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut cli = Cli::parse();
    let Some(mode) = cli.mode.take() else {
        return gui::run();
    };
    if cli.status {
        status::enable();
    }
    // Les backends de capture (notamment macOS) attendent un runtime mono-thread.
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run(cli, mode));
    if let Err(e) = &result {
        status::emit(Status::Error(format!("{e:#}")));
    }
    result
}

/// Application graphique sous Windows : en ligne de commande, la sortie est
/// redirigée vers la console qui l'a lancée.
#[cfg(windows)]
fn attach_console() {
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    if std::env::args_os().len() > 1 {
        // SAFETY : appel Win32 sans pointeur ; échoue sans effet s'il n'y a pas de console.
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
}

/// Se termine quand la fenêtre qui a lancé le moteur ferme son entrée standard
/// (arrêt demandé, ou fenêtre fermée / plantée).
async fn parent_closed() {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        let _ = tx.send(());
    });
    let _ = rx.await;
}

async fn run(cli: Cli, mode: Mode) -> Result<()> {
    if let Mode::AudioDevices = mode {
        return audio::list_devices();
    }
    let password = cli
        .password
        .context("mot de passe manquant : utilisez --password ou ONE_MOUSE_PASSWORD")?;
    let psk = Psk::from_password(&password)?;
    let name = cli.name.unwrap_or_else(default_name);

    let capture = Capture::new().await;
    let emulation = InputEmulation::new(None)
        .await
        .context("impossible de simuler la souris et le clavier")?;
    let clipboard = (!cli.no_clipboard).then(ClipboardSync::start);

    let mut local = Local {
        capture,
        emulation,
        clipboard,
        psk,
        name,
        peer_side: cli.side.into(),
        audio: AudioConfig {
            sound: cli.sound,
            mic: cli.mic,
            sound_device: cli.sound_device,
            mic_device: cli.mic_device,
            sound_output: cli.sound_output,
            mic_output: cli.mic_output,
        },
    };

    let watch_parent = cli.status;
    let result = tokio::select! {
        r = serve(&mode, &mut local) => r,
        _ = tokio::signal::ctrl_c() => {
            log::info!("arrêt demandé");
            Ok(())
        }
        _ = parent_closed(), if watch_parent => {
            log::info!("arrêt demandé par la fenêtre");
            Ok(())
        }
    };

    if let Some(capture) = &mut local.capture {
        let _ = capture.terminate().await;
    }
    local.emulation.terminate().await;
    result
}

async fn serve(mode: &Mode, local: &mut Local) -> Result<()> {
    match mode {
        // Traité au démarrage, avant d'initialiser la capture.
        Mode::AudioDevices => Ok(()),
        Mode::Listen { bind } => {
            let listener = TcpListener::bind(bind)
                .await
                .with_context(|| format!("impossible d'écouter sur {bind}"))?;
            log::info!(
                "en attente de l'autre PC sur {bind} (autre PC à {})",
                session::side_name(local.peer_side)
            );
            loop {
                status::emit(Status::Listening);
                let (stream, addr) = listener.accept().await?;
                log::info!("connexion entrante de {addr}");
                match session::run(stream, Role::Listener, local).await {
                    Ok(()) => log::info!("{addr} s'est déconnecté"),
                    Err(e) => log::warn!("connexion avec {addr} terminée : {e:#}"),
                }
            }
        }
        Mode::Connect { host } => {
            let addr = if host.contains(':') && !host.ends_with(']') {
                host.clone()
            } else {
                format!("{host}:{DEFAULT_PORT}")
            };
            loop {
                status::emit(Status::Connecting);
                match TcpStream::connect(&addr).await {
                    Ok(stream) => match session::run(stream, Role::Connector, local).await {
                        Ok(()) => log::info!("{addr} s'est déconnecté"),
                        Err(e) => log::warn!("connexion avec {addr} terminée : {e:#}"),
                    },
                    Err(e) => log::warn!("connexion à {addr} impossible : {e}{}", connect_hint(&e)),
                }
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
}

/// Explication de l'échec d'une connexion, pour l'utilisateur.
fn connect_hint(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::HostUnreachable => {
            " — l'autre PC ne répond pas : vérifiez son adresse, et que son pare-feu autorise \
             one-mouse (bouton « Autoriser dans le pare-feu » de one-mouse sous Windows, port TCP 24850)"
        }
        std::io::ErrorKind::ConnectionRefused => {
            " — one-mouse n'attend pas de connexion sur l'autre PC : démarrez-y le partage \
             en mode « Ce PC attend l'autre »"
        }
        _ => "",
    }
}

fn default_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|n| !n.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|n| n.trim().to_owned())
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "pc".to_owned())
}
