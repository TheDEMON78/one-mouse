//! one-mouse : une seule souris et un seul clavier pour deux PC posés côte à côte.

mod clipboard;
mod crypto;
mod protocol;
mod session;

use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use input_capture::{InputCapture, Position};
use input_emulation::InputEmulation;
use tokio::net::{TcpListener, TcpStream};

use crate::clipboard::ClipboardSync;
use crate::crypto::{Psk, Role};
use crate::session::Local;

const DEFAULT_PORT: u16 = 24850;
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

#[derive(Parser)]
#[command(
    version,
    about = "Une seule souris et un seul clavier pour deux PC (Windows, Linux, macOS)"
)]
struct Cli {
    #[command(subcommand)]
    mode: Mode,

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
}

#[derive(Clone, Copy, ValueEnum)]
enum Side {
    Left,
    Right,
    Top,
    Bottom,
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

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    // Les backends de capture (notamment macOS) attendent un runtime mono-thread.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    let password = cli
        .password
        .context("mot de passe manquant : utilisez --password ou ONE_MOUSE_PASSWORD")?;
    let psk = Psk::from_password(&password)?;
    let name = cli.name.unwrap_or_else(default_name);

    let capture = match InputCapture::new(None).await {
        Ok(capture) => Some(capture),
        Err(e) => {
            // Ex. Linux sous X11 : ce PC pourra être contrôlé, mais pas contrôler l'autre.
            log::warn!("capture indisponible ({e}) : ce PC fonctionnera en réception seule");
            None
        }
    };
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
    };

    let result = tokio::select! {
        r = serve(&cli.mode, &mut local) => r,
        _ = tokio::signal::ctrl_c() => {
            log::info!("arrêt demandé");
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
        Mode::Listen { bind } => {
            let listener = TcpListener::bind(bind)
                .await
                .with_context(|| format!("impossible d'écouter sur {bind}"))?;
            log::info!(
                "en attente de l'autre PC sur {bind} (autre PC à {})",
                session::side_name(local.peer_side)
            );
            loop {
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
                match TcpStream::connect(&addr).await {
                    Ok(stream) => match session::run(stream, Role::Connector, local).await {
                        Ok(()) => log::info!("{addr} s'est déconnecté"),
                        Err(e) => log::warn!("connexion avec {addr} terminée : {e:#}"),
                    },
                    Err(e) => log::warn!("connexion à {addr} impossible : {e}"),
                }
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
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
