//! one-mouse : une seule souris et un seul clavier pour deux PC posés côte à côte.
//!
//! Sans argument, ouvre la fenêtre de réglages ; avec `listen` / `connect`,
//! lance directement le moteur (c'est aussi ce que fait la fenêtre, dans un
//! processus séparé).

// Sous Windows : pas de console noire au lancement de l'interface.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod audio;
mod autostart;
mod bluetooth;
mod capture;
mod clipboard;
mod codec;
mod crypto;
mod cursor;
mod default_mic;
mod default_output;
mod discovery;
mod firewall;
mod gui;
mod network;
mod powershell;
mod protocol;
mod session;
mod settings;
mod status;
mod tray;
mod update;

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
use crate::status::{BtStatus, Status};

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

    /// Ouvre la fenêtre directement réduite dans la zone de notification (Windows).
    #[arg(long, hide = true)]
    tray: bool,

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

    /// Vitesse de la souris sur l'autre PC (1 = identique, 2 = deux fois plus rapide).
    #[arg(long, global = true, default_value_t = 1.0, value_parser = parse_speed)]
    pointer_speed: f64,

    /// Ce PC est le PC principal (souris, clavier et casque) : il reçoit le son de l'autre
    /// en permanence et peut lui envoyer son micro.
    #[arg(long, global = true)]
    main: bool,

    /// Envoi du son de ce PC à l'autre (auto : quand l'autre PC contrôle celui-ci ;
    /// always : en permanence vers le PC principal).
    #[arg(long, value_enum, global = true, default_value = "always")]
    sound: audio::Mode,

    /// Envoi du micro de ce PC à l'autre (auto : quand ce PC contrôle l'autre ;
    /// always : en permanence depuis le PC principal).
    #[arg(long, value_enum, global = true, default_value = "always")]
    mic: audio::Mode,

    /// Qualité du son envoyé : lossless (sans perte, réseau local rapide) ou
    /// compressed (Opus, Wi-Fi faible).
    #[arg(long, value_enum, global = true, default_value = "lossless")]
    sound_quality: codec::Quality,

    /// Liaison Bluetooth : le PC qui attend publie aussi un service Bluetooth, et
    /// celui qui se connecte sans adresse trouve seul l'autre PC associé. Le son
    /// n'est compressé que si la liaison passe réellement par le Bluetooth.
    #[arg(long, global = true)]
    bluetooth: bool,

    /// Source du son à envoyer (par défaut : la sortie audio principale).
    #[arg(long, global = true, value_name = "PÉRIPHÉRIQUE")]
    sound_device: Option<String>,

    /// Micro à envoyer (par défaut : le micro principal).
    #[arg(long, global = true, value_name = "PÉRIPHÉRIQUE")]
    mic_device: Option<String>,

    /// Sortie où jouer le son reçu de l'autre PC (par défaut : la sortie principale).
    #[arg(long, global = true, value_name = "PÉRIPHÉRIQUE")]
    sound_output: Option<String>,

    /// Sortie où jouer le micro reçu de l'autre PC (par défaut : micro virtuel détecté
    /// ou créé automatiquement — VB-Cable, BlackHole, « Micro-one-mouse » sous Linux).
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
        /// Sans adresse : l'autre PC est trouvé tout seul sur le réseau (ou en
        /// Bluetooth avec --bluetooth).
        host: Option<String>,
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
        return gui::run(cli.tray);
    };
    if cli.status {
        status::enable();
    }
    // Curseurs laissés cachés par un arrêt brutal précédent : rétablis.
    cursor::restore_at_startup();
    let result = block_on(run(cli, mode)).and_then(|r| r);
    if let Err(e) = &result {
        status::emit(Status::Error(format!("{e:#}")));
    }
    result
}

/// Exécute le moteur. Les backends de capture et d'émulation (Windows, macOS,
/// Wayland) lancent des tâches avec `tokio::task::spawn_local` (répétition des
/// touches, boucle de capture) : il leur faut un runtime mono-thread *et* un
/// `LocalSet`, sinon le moteur plante dès la première touche.
fn block_on<F: std::future::Future>(future: F) -> Result<F::Output> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    Ok(runtime.block_on(tokio::task::LocalSet::new().run_until(future)))
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

/// Ctrl+C, ou SIGTERM sous Unix (arrêt du système, `kill`) : dans les deux cas
/// l'arrêt est propre (curseur rendu, micro par défaut rétabli…).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
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
        pointer_speed: cli.pointer_speed,
        audio: AudioConfig {
            main: cli.main,
            sound: cli.sound,
            mic: cli.mic,
            sound_device: cli.sound_device,
            mic_device: cli.mic_device,
            sound_output: cli.sound_output,
            mic_output: cli.mic_output,
            quality: cli.sound_quality,
            bluetooth: cli.bluetooth,
        },
    };

    let watch_parent = cli.status;
    let result = tokio::select! {
        r = serve(&mode, &mut local) => r,
        _ = shutdown_signal() => {
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
            // Annonce sur le réseau : l'autre PC le trouve sans adresse.
            let announcer = tokio::spawn(discovery::announce(
                local.psk.clone(),
                local.name.clone(),
                listener.local_addr()?.port(),
            ));
            let _announcer = AbortOnDrop(announcer);
            let mut bt = None;
            if local.audio.bluetooth {
                match bluetooth::Server::start().await {
                    Ok(server) => {
                        status::emit_bt(BtStatus::Visible);
                        bt = Some(server);
                    }
                    Err(e) => {
                        log::warn!("Bluetooth direct indisponible : {e:#}");
                        status::emit_bt(BtStatus::Error(format!("{e:#}")));
                    }
                }
            }
            loop {
                status::emit(Status::Listening);
                // Connexion par le réseau ou, en liaison Bluetooth, directement.
                let incoming = tokio::select! {
                    accepted = listener.accept() => Incoming::Tcp(accepted?),
                    accepted = accept_bluetooth(bt.as_mut()) => Incoming::Bluetooth(accepted),
                };
                match incoming {
                    Incoming::Tcp((stream, addr)) => {
                        log::info!("connexion entrante de {addr}");
                        match session::run(stream, Role::Listener, local).await {
                            Ok(()) => log::info!("{addr} s'est déconnecté"),
                            Err(e) => log::warn!("connexion avec {addr} terminée : {e:#}"),
                        }
                    }
                    Incoming::Bluetooth(Ok((stream, name))) => {
                        log::info!("connexion Bluetooth entrante de « {name} »");
                        match session::run_on(stream, None, true, Role::Listener, local).await {
                            Ok(()) => log::info!("« {name} » s'est déconnecté"),
                            Err(e) => log::warn!("connexion Bluetooth terminée : {e:#}"),
                        }
                    }
                    Incoming::Bluetooth(Err(e)) => {
                        log::warn!("Bluetooth direct arrêté : {e:#}");
                        status::emit_bt(BtStatus::Error(format!("{e:#}")));
                        bt = None;
                    }
                }
            }
        }
        Mode::Connect { host: None } if !local.audio.bluetooth => {
            log::info!("recherche de l'autre PC sur le réseau (même mot de passe)");
            let mut last_error = String::new();
            loop {
                status::emit(Status::Connecting);
                let found = discovery::find(&local.psk, DISCOVERY_TIMEOUT).await;
                let attempt = match found {
                    Ok((addr, name)) => {
                        log::info!("« {name} » trouvé sur le réseau ({addr})");
                        match TcpStream::connect(addr).await {
                            Ok(stream) => {
                                last_error.clear();
                                match session::run(stream, Role::Connector, local).await {
                                    Ok(()) => log::info!("« {name} » s'est déconnecté"),
                                    Err(e) => {
                                        log::warn!("connexion avec « {name} » terminée : {e:#}")
                                    }
                                }
                                Ok(())
                            }
                            Err(e) => Err(format!(
                                "connexion à {addr} impossible : {e}{}",
                                connect_hint(&e)
                            )),
                        }
                    }
                    Err(e) => Err(format!("{e:#}")),
                };
                // Le même échec se répète à chaque essai : signalé une fois.
                if let Err(message) = attempt
                    && message != last_error
                {
                    log::warn!("{message}");
                    last_error = message;
                }
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
        Mode::Connect { host: None } => {
            log::info!("recherche de l'autre PC parmi les PC associés en Bluetooth");
            status::emit_bt(BtStatus::Searching(String::new()));
            let mut last_error = String::new();
            loop {
                status::emit(Status::Connecting);
                match bluetooth::connect_paired(bluetooth::known_peer()).await {
                    Ok((stream, name)) => {
                        last_error.clear();
                        log::info!("relié en Bluetooth à « {name} »");
                        match session::run_on(stream, None, true, Role::Connector, local).await {
                            Ok(()) => log::info!("« {name} » s'est déconnecté"),
                            Err(e) => log::warn!("connexion Bluetooth terminée : {e:#}"),
                        }
                    }
                    Err(e) => {
                        // Le même échec se répète à chaque essai : signalé une fois.
                        let message = format!("{e:#}");
                        if message != last_error {
                            log::warn!("{message}");
                            status::emit_bt(BtStatus::Error(message.clone()));
                            last_error = message;
                        }
                    }
                }
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
        Mode::Connect { host: Some(host) } => {
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

/// Durée d'écoute des annonces de l'autre PC à chaque essai.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(4);

/// Arrête une tâche quand il est détruit (fin de l'attente).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

enum Incoming {
    Tcp((TcpStream, std::net::SocketAddr)),
    Bluetooth(Result<(bluetooth::Stream, String)>),
}

/// Prochaine connexion Bluetooth entrante (jamais sans service Bluetooth).
async fn accept_bluetooth(
    server: Option<&mut bluetooth::Server>,
) -> Result<(bluetooth::Stream, String)> {
    match server {
        Some(server) => server.accept().await,
        None => std::future::pending().await,
    }
}

fn parse_speed(text: &str) -> Result<f64, String> {
    let speed: f64 = text
        .replace(',', ".")
        .parse()
        .map_err(|_| format!("« {text} » n'est pas un nombre"))?;
    if (0.1..=10.0).contains(&speed) {
        Ok(speed)
    } else {
        Err("la vitesse doit être comprise entre 0,1 et 10".into())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduit l'appel des backends à chaque touche (répétition automatique).
    #[test]
    fn engine_runtime_supports_spawn_local() {
        let value =
            block_on(async { tokio::task::spawn_local(async { 42 }).await.unwrap() }).unwrap();
        assert_eq!(value, 42);
    }
}
