//! Fenêtre de one-mouse : réglages, démarrage / arrêt du partage et état.
//!
//! Le moteur (capture, émulation, son) tourne dans un processus séparé, lancé
//! avec les réglages en arguments : un plantage du moteur ne ferme pas la
//! fenêtre, et l'arrêt libère proprement souris et clavier.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::net::{IpAddr, UdpSocket};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use eframe::egui;

use crate::audio::{self, Mode};
use crate::codec::Quality;
use crate::screens::Monitor;
use crate::settings::{Settings, Side};
use crate::status::{
    BtStatus, DragEdge, FileDirection, FileStatus, LinkStatus, MicStatus, ScreensStatus, Status,
};
use crate::tray::Tray;
use crate::{autostart, crypto, firewall, network};

mod help;
mod wizard;

const ICON_PNG: &[u8] = include_bytes!("../assets/icons/256x256.png");
const MAX_LOG_LINES: usize = 300;
/// Transferts de fichiers affichés.
const MAX_FILES_SHOWN: usize = 6;

/// `hidden` : démarre réduit dans la zone de notification (lancement automatique).
pub fn run(hidden: bool) -> Result<()> {
    if crate::tray::activate_existing() {
        return Ok(());
    }
    let icon = eframe::icon_data::from_png_bytes(ICON_PNG).context("icône invalide")?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("one-mouse")
            .with_icon(icon)
            .with_inner_size([600.0, 720.0])
            .with_min_inner_size([460.0, 520.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "one-mouse",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, hidden)))),
    )
    .map_err(|e| anyhow::anyhow!("ouverture de la fenêtre impossible : {e}"))
}

/// Ce que le moteur a signalé, lu par des threads et affiché par la fenêtre.
#[derive(Default)]
struct Shared {
    status: Option<Status>,
    /// État du micro (envoyé, reçu…) pendant la connexion.
    mic: Option<MicStatus>,
    /// Chemin de la liaison établie (Bluetooth, UDP, TCP).
    link: Option<LinkStatus>,
    /// État du Bluetooth direct avant la connexion.
    bt: Option<BtStatus>,
    /// Derniers fichiers envoyés ou reçus (le plus récent en dernier).
    files: VecDeque<FileStatus>,
    /// Écrans des deux PC annoncés par le moteur, pas encore enregistrés.
    screens: Option<ScreensStatus>,
    /// Fichiers glissés contre le bord : zone de dépôt à afficher.
    drag_edge: Option<(DragEdge, Instant)>,
    /// Problème courant repéré dans le journal (mot de passe différent…).
    hint: Option<String>,
    /// L'autre PC a une autre version : chercher tout de suite une mise à jour.
    version_mismatch: bool,
    log: VecDeque<String>,
}

impl Shared {
    fn push_log(&mut self, line: String) {
        if self.log.len() == MAX_LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }
}

/// Processus du moteur en cours d'exécution.
struct Engine {
    child: Child,
    /// Fermer l'entrée standard demande au moteur de s'arrêter proprement.
    stdin: Option<ChildStdin>,
}

impl Engine {
    fn start(settings: &Settings, shared: Arc<Mutex<Shared>>, ctx: egui::Context) -> Result<Self> {
        let exe = std::env::current_exe().context("chemin de l'application introuvable")?;
        let mut command = Command::new(exe);
        command
            .args(settings.engine_args())
            .env("ONE_MOUSE_PASSWORD", &settings.password)
            .env(
                "RUST_LOG",
                "info,input_capture=error,input_emulation=error,pulseaudio=warn,winit=warn",
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().context("lancement du moteur impossible")?;

        let stdout = child.stdout.take().expect("sortie standard redirigée");
        let stderr = child.stderr.take().expect("sortie d'erreur redirigée");
        {
            let shared = shared.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    let mut shared = shared.lock().expect("verrou");
                    if let Some(status) = Status::parse(&line) {
                        if matches!(
                            status,
                            Status::Listening | Status::Connecting | Status::Error(_)
                        ) {
                            shared.mic = None;
                            shared.link = None;
                        }
                        if matches!(status, Status::Connected(_)) {
                            shared.hint = None;
                        }
                        shared.status = Some(status);
                    } else if let Some(mic) = MicStatus::parse(&line) {
                        shared.mic = Some(mic);
                    } else if let Some(link) = LinkStatus::parse(&line) {
                        shared.link = Some(link);
                    } else if let Some(edge) = DragEdge::parse(&line) {
                        shared.drag_edge = Some((edge, Instant::now()));
                    } else if let Some(screens) = ScreensStatus::parse(&line) {
                        shared.screens = Some(screens);
                    } else if let Some(bt) = BtStatus::parse(&line) {
                        shared.bt = Some(bt);
                    } else if let Some(file) = FileStatus::parse(&line) {
                        // Mise à jour du transfert en cours, sinon nouveau transfert.
                        match shared.files.iter_mut().find(|f| {
                            f.direction == file.direction && f.name == file.name && f.percent < 100
                        }) {
                            Some(existing) => *existing = file,
                            None => {
                                if shared.files.len() == MAX_FILES_SHOWN {
                                    shared.files.pop_front();
                                }
                                shared.files.push_back(file);
                            }
                        }
                    } else {
                        shared.push_log(line);
                    }
                    ctx.request_repaint();
                }
            });
        }
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut shared = shared.lock().expect("verrou");
                if let Some(hint) = hint_for(&line) {
                    shared.hint = Some(hint);
                }
                if is_version_mismatch(&line) {
                    shared.version_mismatch = true;
                }
                shared.push_log(simplify_log(&line));
                drop(shared);
                ctx.request_repaint();
            }
        });

        let stdin = child.stdin.take();
        Ok(Self { child, stdin })
    }

    /// Demande au moteur d'envoyer ce fichier (ou dossier) à l'autre PC.
    fn send_file(&mut self, path: &std::path::Path) -> Result<()> {
        use std::io::Write;
        let stdin = self.stdin.as_mut().context("moteur arrêté")?;
        writeln!(stdin, "@send {}", path.display())?;
        stdin.flush()?;
        Ok(())
    }

    /// Fichiers lâchés sur la zone de dépôt : envoyés comme un lot glissé.
    fn send_drop(&mut self, paths: &[std::path::PathBuf]) -> Result<()> {
        use std::io::Write;
        let stdin = self.stdin.as_mut().context("moteur arrêté")?;
        writeln!(stdin, "@drop {}", serde_json::to_string(paths)?)?;
        stdin.flush()?;
        Ok(())
    }

    /// `true` tant que le processus tourne.
    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn stop(&mut self) {
        self.stdin = None;
        // Le moteur rétablit le micro par défaut avant de quitter (PowerShell).
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            if !self.is_running() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `[2026-10-12T21:04:19Z INFO  one_mouse::session] message` → `21:04:19 message`.
fn simplify_log(line: &str) -> String {
    let Some(rest) = line.strip_prefix('[') else {
        return line.to_owned();
    };
    let Some((header, message)) = rest.split_once("] ") else {
        return line.to_owned();
    };
    let time = header
        .split_whitespace()
        .next()
        .and_then(|ts| ts.split('T').nth(1))
        .map(|t| t.trim_end_matches('Z'))
        .unwrap_or_default();
    let level = if header.contains("ERROR") {
        "erreur : "
    } else if header.contains("WARN") {
        "attention : "
    } else {
        ""
    };
    format!("{time} {level}{message}")
}

/// Explication claire d'un problème du moteur, avec ce qu'il faut faire.
fn friendly_error(message: &str) -> String {
    hint_for(message).unwrap_or_else(|| {
        let mut text: String = message.to_owned();
        if let Some(first) = text.get(..1) {
            text.replace_range(..1, &first.to_uppercase());
        }
        if !text.ends_with('.') {
            text.push('.');
        }
        text
    })
}

/// Message du moteur indiquant que les deux PC n'ont pas la même version.
fn is_version_mismatch(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("mettez one-mouse à jour") || m.contains("version incompatible")
}

/// Problème courant reconnu dans un message du moteur, expliqué simplement.
fn hint_for(message: &str) -> Option<String> {
    let m = message.to_lowercase();
    let text = if m.contains("mots de passe sont-ils identiques") {
        "Le mot de passe n'est pas le même sur les deux PC : saisissez exactement le même \
         des deux côtés."
    } else if m.contains("mettez one-mouse à jour") || m.contains("version incompatible") {
        "Les deux PC n'ont pas la même version de one-mouse : mettez-les à jour (onglet \
         Réglages, « Mises à jour »)."
    } else if m.contains("impossible d'écouter") {
        "one-mouse semble déjà ouvert sur ce PC (peut-être réduit près de l'horloge). \
         Fermez l'autre fenêtre puis réessayez."
    } else if m.contains("simuler la souris") {
        "one-mouse n'a pas l'autorisation de piloter la souris de ce PC. Sur Mac : Réglages \
         Système, Confidentialité et sécurité, Accessibilité, puis cochez one-mouse."
    } else if m.contains("bluetooth") && (m.contains("indisponible") || m.contains("activé")) {
        "Le Bluetooth semble désactivé sur ce PC : activez-le puis réessayez."
    } else if m.contains("n'est pas one-mouse") {
        "Un autre logiciel a répondu à la place de one-mouse : vérifiez l'adresse saisie, ou \
         laissez-la vide."
    } else {
        return None;
    };
    Some(text.to_owned())
}

/// Adresse IP de ce PC sur le réseau local (aucun paquet n'est envoyé).
fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

struct App {
    settings: Settings,
    saved: Settings,
    shared: Arc<Mutex<Shared>>,
    engine: Option<Engine>,
    show_password: bool,
    autostart: bool,
    /// `Some(false)` : le pare-feu Windows risque de bloquer l'autre PC.
    firewall_allowed: Option<bool>,
    outputs: Vec<String>,
    inputs: Vec<String>,
    local_ip: Option<IpAddr>,
    /// Adresse de ce PC sur la liaison Bluetooth, revérifiée régulièrement.
    bluetooth_ip: Option<IpAddr>,
    bluetooth_checked: Instant,
    /// Autre PC dont l'adresse Bluetooth a été apprise par le réseau.
    bluetooth_peer: Option<crate::bluetooth::KnownPeer>,
    error: Option<String>,
    /// Message de réussite d'une action (effacé par la suivante).
    notice: Option<String>,
    /// Assistant de premier démarrage en cours (étape affichée).
    wizard: Option<wizard::Step>,
    /// Réponse à « souris branchée sur cet ordinateur ? » (aucune au départ).
    wizard_main: Option<bool>,
    /// Des fichiers survolent la zone de dépôt.
    drop_zone_hovered: bool,
    /// Dernière recherche de mise à jour déclenchée par des versions différentes.
    mismatch_checked: Option<Instant>,
    logo: egui::TextureHandle,
    tab: Tab,
    /// Icône de la zone de notification (Windows).
    tray: Option<Tray>,
    /// Images restant à afficher avant de cacher la fenêtre (démarrage réduit).
    hide_after_frames: Option<u8>,
    /// Fermeture voulue (menu « Quitter ») : ne pas cacher la fenêtre.
    quitting: bool,
    /// Vérification et installation des nouvelles versions.
    updater: crate::update::Updater,
    /// Installation demandée à la main (bouton).
    install_requested: bool,
    /// Historique du presse-papier affiché, relu régulièrement.
    history: Vec<crate::history::Entry>,
    history_loaded: Instant,
    /// Presse-papier de la fenêtre (pour « Copier » depuis l'historique). Gardé
    /// ouvert : sous Linux, son contenu disparaît avec lui.
    clipboard: Option<arboard::Clipboard>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, hidden: bool) -> Self {
        apply_theme(&cc.egui_ctx);
        let settings = Settings::load();
        let settings_auto_update = settings.auto_update;
        let wizard = (!settings.setup_done).then_some(wizard::Step::Welcome);
        let logo = {
            let image = image::load_from_memory(ICON_PNG)
                .expect("icône intégrée valide")
                .to_rgba8();
            let size = [image.width() as usize, image.height() as usize];
            let color = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
            cc.egui_ctx
                .load_texture("logo", color, egui::TextureOptions::LINEAR)
        };
        let mut app = Self {
            saved: settings.clone(),
            settings,
            shared: Arc::default(),
            engine: None,
            show_password: false,
            autostart: autostart::is_enabled(),
            firewall_allowed: firewall::is_allowed(),
            outputs: audio::output_names(),
            inputs: audio::input_names(),
            local_ip: local_ip(),
            bluetooth_ip: network::bluetooth_ip(),
            bluetooth_peer: crate::bluetooth::known_peer(),
            bluetooth_checked: Instant::now(),
            error: None,
            notice: None,
            wizard,
            wizard_main: None,
            drop_zone_hovered: false,
            mismatch_checked: None,
            logo,
            tab: Tab::Connection,
            tray: Tray::new(cc, ICON_PNG),
            hide_after_frames: None,
            quitting: false,
            updater: {
                let ctx = cc.egui_ctx.clone();
                crate::update::Updater::start(settings_auto_update, move || ctx.request_repaint())
            },
            install_requested: false,
            history: Vec::new(),
            history_loaded: Instant::now(),
            clipboard: None,
        };
        // Historique d'une session précédente (fermeture brutale) : effacé.
        crate::history::clear();
        if hidden && app.tray.is_some() {
            // eframe montre la fenêtre après sa première image : on la cache ensuite.
            app.hide_after_frames = Some(2);
        }
        if cfg!(windows) && app.autostart {
            // Met à jour une ancienne inscription (lancement sans --tray).
            let _ = autostart::set_enabled(true);
        }
        if app.settings.start_sharing && app.validate().is_ok() {
            app.start(&cc.egui_ctx);
        }
        app
    }

    fn validate(&self) -> Result<(), String> {
        if self.settings.password.chars().count() < crypto::MIN_PASSWORD_LEN {
            return Err(format!(
                "Le mot de passe doit faire au moins {} caractères.",
                crypto::MIN_PASSWORD_LEN
            ));
        }

        Ok(())
    }

    fn start(&mut self, ctx: &egui::Context) {
        self.error = None;
        if let Err(e) = self.validate() {
            self.error = Some(e);
            return;
        }
        if self.settings != self.saved {
            match self.settings.save() {
                Ok(()) => self.saved = self.settings.clone(),
                Err(e) => {
                    self.error = Some(format!("Enregistrement des réglages impossible : {e:#}"))
                }
            }
        }
        {
            let mut shared = self.shared.lock().expect("verrou");
            shared.status = None;
            shared.hint = None;
            shared.mic = None;
            shared.link = None;
            shared.bt = None;
            shared.log.clear();
        }
        match Engine::start(&self.settings, self.shared.clone(), ctx.clone()) {
            Ok(engine) => self.engine = Some(engine),
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    fn stop(&mut self) {
        if let Some(mut engine) = self.engine.take() {
            engine.stop();
            // Curseur laissé caché par un moteur arrêté de force : rétabli.
            crate::cursor::restore_at_startup();
        }
        self.shared.lock().expect("verrou").status = None;
    }

    fn running(&mut self) -> bool {
        let running = self.engine.as_mut().is_some_and(Engine::is_running);
        if !running && self.engine.take().is_some() {
            // Moteur arrêté (erreur, plantage) : curseur rétabli s'il était caché.
            crate::cursor::restore_at_startup();
        }
        running
    }

    /// État affiché : couleur, titre court (pastille) et explication.
    fn status_view(&mut self, running: bool) -> (egui::Color32, String, String) {
        let status = self.shared.lock().expect("verrou").status.clone();
        match (running, status) {
            (_, Some(Status::Error(msg))) => (RED, "Problème".into(), friendly_error(&msg)),
            (false, _) => (
                GREY,
                "Partage arrêté".into(),
                "Cliquez sur « Démarrer le partage », en bas de la fenêtre, sur les deux PC."
                    .into(),
            ),
            (true, None) => (ORANGE, "Démarrage…".into(), "Lancement du partage.".into()),
            (true, Some(Status::Listening)) => (
                ORANGE,
                "En attente de l'autre PC".into(),
                "Démarrez maintenant le partage sur l'autre PC, avec le même mot de passe : \
                 les deux PC se trouveront tout seuls."
                    .into(),
            ),
            (true, Some(Status::Connecting)) => (
                ORANGE,
                "Recherche de l'autre PC".into(),
                if self.settings.bluetooth {
                    "Recherche en Bluetooth… Vérifiez que le partage est démarré sur l'autre PC."
                        .into()
                } else if self.settings.address.trim().is_empty() {
                    "Recherche sur votre réseau… Vérifiez que le partage est démarré sur l'autre \
                     PC et que les deux PC sont sur la même box ou le même Wi-Fi."
                        .into()
                } else {
                    format!(
                        "Recherche de {}… Vérifiez que le partage est démarré sur l'autre PC.",
                        self.settings.address.trim()
                    )
                },
            ),
            (true, Some(Status::Connected(name))) => (
                GREEN,
                "Connecté".into(),
                format!(
                    "Relié à « {name} ». Poussez la souris vers le bord de l'écran, du côté \
                     de l'autre PC, pour passer dessus."
                ),
            ),
            (true, Some(Status::Sending)) => (
                ACCENT,
                "Vous utilisez l'autre PC".into(),
                "Votre souris et votre clavier pilotent l'autre PC. Revenez en ramenant la \
                 souris vers ce PC."
                    .into(),
            ),
            (true, Some(Status::Receiving)) => (
                ACCENT,
                "Piloté par l'autre PC".into(),
                "La souris et le clavier de l'autre PC pilotent celui-ci.".into(),
            ),
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, running: bool) {
        let (color, short, _) = self.status_view(running);
        ui.horizontal(|ui| {
            ui.add(egui::Image::new(&self.logo).fit_to_exact_size(egui::vec2(40.0, 40.0)));
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new("one-mouse")
                        .strong()
                        .size(22.0)
                        .color(TEXT),
                );
                ui.label(
                    egui::RichText::new("Une souris et un clavier pour deux ordinateurs")
                        .size(12.0)
                        .color(MUTED),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                status_pill(ui, color, &short);
            });
        });
    }

    fn tabs(&mut self, ui: &mut egui::Ui) {
        // Onglets de même largeur sur toute la fenêtre ; sur deux lignes si elle
        // est trop étroite.
        let width = (ui.available_width() - 5.0 * ui.spacing().item_spacing.x) / 6.0;
        let draw = |ui: &mut egui::Ui, tab: Tab, selected: bool, width: Option<f32>| {
            let text = egui::RichText::new(tab.label())
                .size(13.5)
                .color(if selected {
                    egui::Color32::WHITE
                } else {
                    MUTED
                });
            let button = egui::Button::new(text)
                .fill(if selected {
                    ACCENT
                } else {
                    egui::Color32::TRANSPARENT
                })
                .stroke(egui::Stroke::NONE)
                .corner_radius(8);
            match width {
                Some(width) => ui.add_sized([width, 28.0], button),
                None => ui.add(button),
            }
            .clicked()
        };
        let fits = width >= 84.0;
        let layout = |ui: &mut egui::Ui, current: &mut Tab| {
            for tab in Tab::ALL {
                if draw(ui, tab, *current == tab, fits.then_some(width)) {
                    *current = tab;
                }
            }
        };
        if fits {
            ui.horizontal(|ui| layout(ui, &mut self.tab));
        } else {
            ui.horizontal_wrapped(|ui| layout(ui, &mut self.tab));
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        let connected = matches!(
            self.shared.lock().expect("verrou").status,
            Some(Status::Connected(_) | Status::Sending | Status::Receiving)
        );
        let Some(engine) = self.engine.as_mut().filter(|_| connected) else {
            self.error = Some(
                "Pour envoyer des fichiers, démarrez le partage et attendez d'être connecté à \
                 l'autre PC."
                    .into(),
            );
            return;
        };
        for path in dropped {
            if let Err(e) = engine.send_file(&path) {
                self.error = Some(format!("Envoi de {} impossible : {e:#}", path.display()));
            }
        }
        self.tab = Tab::Connection;
    }

    /// Onglet « Presse-papier » : les dernières copies des deux PC.
    fn clipboard_tab(&mut self, ui: &mut egui::Ui) {
        use crate::history::Content;
        if self.history_loaded.elapsed() > Duration::from_secs(1) || self.history.is_empty() {
            self.history = crate::history::load();
            self.history_loaded = Instant::now();
        }
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        card(ui, |ui| {
            section_title(ui, "Historique du presse-papier");
            ui.label(
                egui::RichText::new(
                    "Les 20 dernières copies des deux PC. « Copier » remet l'élément dans le \
                     presse-papier (et dans celui de l'autre PC). Effacé à la fermeture de one-mouse.",
                )
                .color(MUTED),
            );
            if !self.settings.clipboard_history {
                ui.label(
                    egui::RichText::new("Historique désactivé dans l'onglet Réglages.")
                        .color(ORANGE),
                );
            } else if self.history.is_empty() {
                ui.label(egui::RichText::new("Rien de copié pour l'instant.").color(MUTED));
            } else if ui.small_button("Vider l'historique").clicked() {
                crate::history::clear();
                self.history.clear();
            }
        });
        let mut copy = None;
        for (index, entry) in self.history.iter().enumerate() {
            card(ui, |ui| {
                let time = chrono_time(entry.at);
                ui.label(
                    egui::RichText::new(format!("Copié sur « {} » à {time}", entry.from))
                        .small()
                        .color(MUTED),
                );
                let preview = match &entry.content {
                    Content::Text { text } => {
                        let mut short: String = text.chars().take(300).collect();
                        if short.len() < text.len() {
                            short.push('…');
                        }
                        short
                    }
                    Content::Image { width, height, .. } => format!("Image {width} × {height}"),
                    Content::Files { paths } => paths
                        .iter()
                        .map(|p| {
                            p.file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                };
                ui.label(egui::RichText::new(preview).color(TEXT));
                if ui.small_button("Copier").clicked() {
                    copy = Some(index);
                }
            });
        }
        if let Some(index) = copy {
            self.copy_from_history(index);
        }
    }

    fn copy_from_history(&mut self, index: usize) {
        use crate::clipboard::Clip;
        let Some(clip) = self
            .history
            .get(index)
            .and_then(|entry| crate::history::to_clip(&entry.content))
        else {
            self.error = Some("Cet élément n'est plus disponible.".into());
            return;
        };
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        let Some(clipboard) = self.clipboard.as_mut() else {
            self.error = Some("Presse-papier indisponible.".into());
            return;
        };
        let result = match clip {
            Clip::Text(text) => clipboard.set_text(text),
            Clip::Files(files) => clipboard.set().file_list(&files),
            Clip::Image {
                width,
                height,
                rgba,
            } => clipboard.set_image(arboard::ImageData {
                width,
                height,
                bytes: rgba.into(),
            }),
        };
        if let Err(e) = result {
            self.error = Some(format!("Copie impossible : {e}"));
        }
    }

    /// Carte « Fichiers » : envois et réceptions récents.
    fn files_card(&mut self, ui: &mut egui::Ui) {
        let files = self.shared.lock().expect("verrou").files.clone();
        card(ui, |ui| {
            section_title(ui, "Fichiers");
            ui.label(
                egui::RichText::new(
                    "Glissez des fichiers ou des dossiers sur cette fenêtre pour les copier sur \
                     l'autre PC (rangés dans Téléchargements/one-mouse).",
                )
                .color(MUTED),
            );
            for file in files.iter().rev() {
                let arrow = match file.direction {
                    FileDirection::Sending => "Envoi",
                    FileDirection::Receiving => "Réception",
                };
                let name = std::path::Path::new(&file.name)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| file.name.clone());
                if file.failed {
                    ui.label(egui::RichText::new(format!("{arrow} échoué : {name}")).color(RED));
                } else if file.percent >= 100 {
                    ui.label(egui::RichText::new(format!("{arrow} terminé : {name}")).color(GREEN));
                } else {
                    ui.add(
                        egui::ProgressBar::new(f32::from(file.percent) / 100.0)
                            .text(format!("{arrow} : {name} ({} %)", file.percent)),
                    );
                }
            }
            if ui
                .small_button("Ouvrir le dossier des fichiers reçus")
                .clicked()
            {
                let dir = crate::transfer::received_dir();
                let _ = std::fs::create_dir_all(&dir);
                open_url(&dir.display().to_string());
            }
        });
    }

    fn connection_tab(&mut self, ui: &mut egui::Ui, running: bool) {
        let (color, short, detail) = self.status_view(running);
        card(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 6.0, color);
                ui.label(egui::RichText::new(short).strong().size(17.0).color(TEXT));
            });
            ui.label(egui::RichText::new(detail).color(MUTED));
            let hint = self.shared.lock().expect("verrou").hint.clone();
            if running && let Some(hint) = hint {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(hint).color(ORANGE));
            }
            if self.settings.bluetooth && self.bluetooth_checked.elapsed() > Duration::from_secs(3)
            {
                // La liaison Bluetooth peut s'établir pendant que la fenêtre est ouverte.
                self.bluetooth_ip = network::bluetooth_ip();
                self.bluetooth_peer = crate::bluetooth::known_peer();
                self.bluetooth_checked = Instant::now();
                ui.ctx().request_repaint_after(Duration::from_secs(3));
            }
            let address = if self.settings.bluetooth {
                self.bluetooth_ip
            } else {
                self.local_ip
            };
            if running {
                let (link, bt, mic) = {
                    let shared = self.shared.lock().expect("verrou");
                    (shared.link, shared.bt.clone(), shared.mic.clone())
                };
                if let Some(link) = link {
                    ui.add_space(4.0);
                    link_line(ui, link);
                } else if self.settings.bluetooth
                    && let Some(bt) = bt
                {
                    ui.add_space(4.0);
                    bluetooth_line(ui, &bt);
                }
                if let Some(mic) = mic {
                    ui.add_space(4.0);
                    mic_line(ui, &mic);
                }
            }
            // Adresse réseau : utile seulement si les PC ne se trouvent pas seuls.
            if let Some(ip) = address {
                ui.add_space(4.0);
                egui::CollapsingHeader::new(
                    egui::RichText::new("Les PC ne se trouvent pas ?")
                        .small()
                        .color(MUTED),
                )
                .id_salt("adresse")
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(
                            "Sur l'autre PC, choisissez « Ce PC rejoint l'autre » et saisissez \
                             cette adresse :",
                        )
                        .small()
                        .color(MUTED),
                    );
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(ip.to_string())
                                .monospace()
                                .size(16.0)
                                .color(TEXT),
                        );
                        if ui.small_button("Copier").clicked() {
                            ui.ctx().copy_text(ip.to_string());
                        }
                    });
                });
            }
        });

        ui.add_enabled_ui(!running, |ui| {
            let s = &mut self.settings;
            card(ui, |ui| {
                section_title(ui, "Qui attend qui ?");
                ui.columns(2, |cols| {
                    choice_tile(
                        &mut cols[0],
                        &mut s.connect,
                        false,
                        "Ce PC attend l'autre",
                        "Conseillé pour le PC où est branchée la souris",
                    );
                    choice_tile(
                        &mut cols[1],
                        &mut s.connect,
                        true,
                        "Ce PC rejoint l'autre",
                        "Choisissez-le sur un seul des deux PC",
                    );
                });
                if s.connect && s.bluetooth {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Rien à saisir : l'autre PC est trouvé en Bluetooth.")
                            .color(MUTED),
                    );
                } else if s.connect {
                    ui.add_space(4.0);
                    field(
                        ui,
                        "Adresse de l'autre PC (à laisser vide en général)",
                        |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut s.address)
                                    .hint_text("vide : l'autre PC est trouvé tout seul")
                                    .desired_width(f32::INFINITY),
                            );
                        },
                    );
                }
            });

            card(ui, |ui| {
                section_title(ui, "Comment les deux PC communiquent");
                ui.columns(2, |cols| {
                    choice_tile(
                        &mut cols[0],
                        &mut s.bluetooth,
                        false,
                        "Par la box ou le Wi-Fi",
                        "Recommandé : le plus rapide, son parfait",
                    );
                    choice_tile(
                        &mut cols[1],
                        &mut s.bluetooth,
                        true,
                        "Par Bluetooth",
                        "Si les PC ne sont pas sur le même réseau",
                    );
                });
                if s.bluetooth {
                    ui.add_space(6.0);
                    bluetooth_steps(ui, self.bluetooth_peer.as_ref());
                }
            });

            card(ui, |ui| {
                section_title(ui, "Mot de passe");
                ui.label(
                    egui::RichText::new(
                        "Inventez un mot de passe et saisissez le même sur les deux PC : il \
                         empêche tout autre ordinateur de prendre le contrôle.",
                    )
                    .small()
                    .color(MUTED),
                );
                field(ui, "Mot de passe (identique sur les deux PC)", |ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut s.password)
                                .password(!self.show_password)
                                .hint_text("8 caractères minimum")
                                .desired_width(ui.available_width() - 90.0),
                        );
                        let eye = if self.show_password {
                            "Masquer"
                        } else {
                            "Afficher"
                        };
                        if ui.small_button(eye).clicked() {
                            self.show_password = !self.show_password;
                        }
                    });
                });
                field(ui, "Nom de ce PC (affiché sur l'autre PC)", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut s.name)
                            .hint_text("nom de l'ordinateur")
                            .desired_width(f32::INFINITY),
                    );
                });
            });
        });
    }

    fn screens_tab(&mut self, ui: &mut egui::Ui, running: bool) {
        ui.add_enabled_ui(!running, |ui| {
            let s = &mut self.settings;
            card(ui, |ui| {
                section_title(ui, "Où se trouve l'autre PC ?");
                ui.label(
                    egui::RichText::new(
                        "Cliquez sur l'emplacement de l'autre écran par rapport à celui-ci.",
                    )
                    .color(MUTED),
                );
                ui.add_space(6.0);
                side_picker(ui, &mut s.side);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "L'autre PC est {} de celui-ci",
                            s.side.label()
                        ))
                        .color(TEXT),
                    );
                });
            });
            card(ui, |ui| {
                section_title(ui, "Alignement des écrans");
                ui.label(
                    egui::RichText::new(if matches!(s.side, Side::Left | Side::Right) {
                        "Faites glisser les écrans de l'autre PC vers le haut ou le bas : le \
                         curseur passe d'un PC à l'autre à la même hauteur, comme dans les \
                         paramètres d'affichage."
                    } else {
                        "Faites glisser les écrans de l'autre PC vers la gauche ou la droite : \
                         le curseur passe d'un PC à l'autre au même endroit, comme dans les \
                         paramètres d'affichage."
                    })
                    .color(MUTED),
                );
                ui.add_space(6.0);
                let mut local = crate::screens::local_monitors();
                if local.is_empty() {
                    local = s.local_monitors.clone();
                }
                layout_canvas(
                    ui,
                    &local,
                    &s.peer_monitors,
                    &s.peer_name,
                    s.side,
                    &mut s.peer_offset,
                );
                ui.horizontal(|ui| {
                    let along = |m: &[Monitor]| {
                        crate::screens::bounds(m).map(|b| {
                            if matches!(s.side, Side::Left | Side::Right) {
                                b.height
                            } else {
                                b.width
                            }
                        })
                    };
                    let (start, end) = if matches!(s.side, Side::Left | Side::Right) {
                        ("Aligner en haut", "Aligner en bas")
                    } else {
                        ("Aligner à gauche", "Aligner à droite")
                    };
                    if ui.small_button(start).clicked() {
                        s.peer_offset = 0;
                    }
                    if let (Some(this), Some(other)) = (along(&local), along(&s.peer_monitors)) {
                        if ui.small_button("Centrer").clicked() {
                            s.peer_offset = (this - other) / 2;
                        }
                        if ui.small_button(end).clicked() {
                            s.peer_offset = this - other;
                        }
                    }
                });
                if s.peer_monitors.is_empty() {
                    ui.label(
                        egui::RichText::new(
                            "Connectez-vous une fois : les écrans de l'autre PC s'afficheront ici.",
                        )
                        .small()
                        .color(MUTED),
                    );
                }
                if local.is_empty() {
                    ui.label(
                        egui::RichText::new(
                            "Les écrans de ce PC ne peuvent pas être lus ici (Wayland, macOS) : \
                             le curseur garde sa position en changeant de PC.",
                        )
                        .small()
                        .color(MUTED),
                    );
                }
            });
            card(ui, |ui| {
                section_title(ui, "Vitesse de la souris sur l'autre PC");
                ui.horizontal(|ui| {
                    ui.spacing_mut().slider_width = (ui.available_width() - 190.0).max(120.0);
                    ui.add(
                        egui::Slider::new(&mut s.pointer_speed, 0.25..=4.0)
                            .logarithmic(true)
                            .fixed_decimals(2)
                            .prefix("×"),
                    );
                    if ui.small_button("Réinitialiser").clicked() {
                        s.pointer_speed = 1.0;
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "À ajuster si la souris va plus vite ou moins vite sur l'autre PC.",
                    )
                    .small()
                    .color(MUTED),
                );
            });
        });
    }

    fn sound_tab(&mut self, ui: &mut egui::Ui, running: bool) {
        ui.add_enabled_ui(!running, |ui| {
            let outputs = &self.outputs;
            let inputs = &self.inputs;
            let s = &mut self.settings;
            card(ui, |ui| {
                section_title(ui, "Votre PC principal");
                ui.checkbox(
                    &mut s.main,
                    "Ma souris, mon clavier et mon casque sont branchés sur ce PC",
                );
                ui.label(
                    egui::RichText::new(
                        "À cocher sur un seul des deux PC. Vous entendez alors le son de l'autre PC \
                         dans votre casque, et votre micro fonctionne aussi sur l'autre PC.",
                    )
                    .small()
                    .color(MUTED),
                );
            });
            card(ui, |ui| {
                if s.main {
                    section_title(ui, "Son et micro");
                    output_combo(ui, "sound_output", "Où écouter le son de l'autre PC", &mut s.sound_output, outputs);
                    mode_combo(ui, "mic", "Utiliser mon micro sur l'autre PC", &mut s.mic, "quand je l'utilise");
                    if s.mic != Mode::Off {
                        device_combo(ui, "mic_device", "Quel micro", "micro habituel", &mut s.mic_device, inputs);
                        ui.label(
                            egui::RichText::new(
                                "Casque Bluetooth : son micro dégrade fortement le son (qualité \
                                 téléphone). Avec « micro habituel », one-mouse prend de lui-même un \
                                 autre micro (celui du PC, de la webcam…) s'il y en a un.",
                            )
                            .small()
                            .color(ORANGE),
                        );
                    }
                } else {
                    section_title(ui, "Son et micro");
                    if audio::virtual_mic_installed(outputs) == Some(false) {
                        ui.label(
                            egui::RichText::new(
                                "Pour que vos applications (Discord, Teams…) de ce PC entendent le \
                                 micro de votre PC principal, installez ce petit logiciel gratuit \
                                 une seule fois, puis redémarrez ce PC.",
                            )
                            .color(ORANGE),
                        );
                        let (name, url) = if cfg!(target_os = "macos") {
                            ("BlackHole", "https://existential.audio/blackhole/")
                        } else {
                            ("VB-Cable", "https://vb-audio.com/Cable/")
                        };
                        if ui.button(format!("Installer {name} (gratuit)")).clicked() {
                            open_url(url);
                        }
                    }
                    mode_combo(ui, "sound", "Envoyer le son de ce PC", &mut s.sound, "quand l'autre PC le pilote");
                    output_combo(ui, "mic_output", "Micro de l'autre PC (réglage avancé)", &mut s.mic_output, outputs);
                    ui.label(
                        egui::RichText::new(
                            "Pendant le partage, le micro de votre PC principal devient le micro de ce \
                             PC ; l'ancien revient à l'arrêt.",
                        )
                        .small()
                        .color(MUTED),
                    );
                }
            });
        });
        ui.add_enabled_ui(!running, |ui| {
            card(ui, |ui| {
                section_title(ui, "Qualité du son");
                let label = |q: Quality| match q {
                    Quality::Lossless => "Parfaite (recommandé)",
                    Quality::Compressed => "Économie (Wi-Fi lent ou chargé)",
                };
                let quality = &mut self.settings.sound_quality;
                egui::ComboBox::from_id_salt("quality")
                    .selected_text(label(*quality))
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for q in Quality::ALL {
                            ui.selectable_value(quality, q, label(q));
                        }
                    });
                ui.label(
                    egui::RichText::new(
                        "Parfaite : qualité studio, sans aucune perte audible. Économie : \
                         utilise six fois moins le réseau, si le son coupe en Wi-Fi.",
                    )
                    .small()
                    .color(MUTED),
                );
                if self.settings.bluetooth {
                    ui.label(
                        egui::RichText::new(
                            "En Bluetooth, le son est toujours légèrement compressé, le \
                             Bluetooth étant plus lent.",
                        )
                        .small()
                        .color(ORANGE),
                    );
                }
            });
        });
        if ui
            .small_button("Actualiser la liste des casques et micros")
            .clicked()
        {
            self.outputs = audio::output_names();
            self.inputs = audio::input_names();
        }
    }

    /// Fermer la fenêtre la cache dans la zone de notification ; « Quitter » ferme.
    fn handle_tray(&mut self, ctx: &egui::Context) {
        let Some(tray) = &self.tray else {
            return;
        };
        if let Some(frames) = self.hide_after_frames {
            if frames == 0 {
                self.hide_after_frames = None;
                tray.hide();
            } else {
                self.hide_after_frames = Some(frames - 1);
                ctx.request_repaint();
            }
        }
        if tray.quit_requested() && !self.quitting {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && self.settings.tray {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            tray.hide();
        }
    }

    /// Installe une mise à jour prête : seule (option) quand la souris ne contrôle
    /// aucun PC, ou à la demande. one-mouse se ferme, l'installateur le relance.
    /// Zone « Déposer ici » contre le bord de l'écran, quand des fichiers y sont
    /// glissés (le moteur Windows a la sienne). Elle disparaît après le dépôt, ou
    /// peu après si rien n'est glissé dessus.
    fn show_drop_zone(&mut self, ctx: &egui::Context) {
        let Some((edge, at)) = self.shared.lock().expect("verrou").drag_edge.clone() else {
            return;
        };
        if at.elapsed() > Duration::from_secs(3) && !self.drop_zone_hovered {
            self.shared.lock().expect("verrou").drag_edge = None;
            self.drop_zone_hovered = false;
            return;
        }
        // Position du moteur en pixels de l'écran ; la fenêtre se place en points.
        let ppp = ctx.native_pixels_per_point().unwrap_or(1.0).max(0.5);
        let size = (
            (crate::dropzone::WIDTH as f32 * ppp) as i32,
            (crate::dropzone::HEIGHT as f32 * ppp) as i32,
        );
        let (x, y, _, _) = crate::dropzone::zone_rect(
            (edge.x, edge.y),
            edge.side,
            size,
            (i32::MIN / 2, i32::MIN / 2, i32::MAX / 2, i32::MAX / 2),
        );
        let builder = egui::ViewportBuilder::default()
            .with_title("one-mouse")
            .with_decorations(false)
            .with_resizable(false)
            .with_taskbar(false)
            .with_active(false)
            .with_always_on_top()
            .with_position(egui::pos2(x as f32 / ppp, y as f32 / ppp))
            .with_inner_size(egui::vec2(
                crate::dropzone::WIDTH as f32,
                crate::dropzone::HEIGHT as f32,
            ));
        let (dropped, hovered) = ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("zone-depot"),
            builder,
            |ui, _| {
                let input = ui.ctx().input(|i| {
                    (
                        i.raw
                            .dropped_files
                            .iter()
                            .map(|f| f.path().to_path_buf())
                            .filter(|p| !p.as_os_str().is_empty())
                            .collect::<Vec<_>>(),
                        !i.raw.hovered_files.is_empty(),
                    )
                });
                let stroke = if input.1 { GREEN } else { ACCENT };
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(SURFACE)
                            .stroke(egui::Stroke::new(3.0, stroke))
                            .inner_margin(12),
                    )
                    .show(ui, |ui| {
                        ui.centered_and_justified(|ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Déposer ici\npour envoyer à « {} »",
                                    edge.peer
                                ))
                                .size(17.0)
                                .strong()
                                .color(TEXT),
                            );
                        });
                    });
                input
            },
        );
        self.drop_zone_hovered = hovered;
        ctx.request_repaint_after(Duration::from_millis(200));
        if dropped.is_empty() {
            return;
        }
        self.shared.lock().expect("verrou").drag_edge = None;
        self.drop_zone_hovered = false;
        if let Some(engine) = self.engine.as_mut()
            && let Err(e) = engine.send_drop(&dropped)
        {
            self.error = Some(format!("Envoi des fichiers impossible : {e:#}"));
        }
    }

    /// Enregistre les écrans de l'autre PC et l'alignement retenu par le moteur
    /// (après harmonisation avec l'autre PC).
    fn handle_screens(&mut self) {
        let Some(screens) = self.shared.lock().expect("verrou").screens.take() else {
            return;
        };
        for settings in [&mut self.settings, &mut self.saved] {
            if !screens.peer.is_empty() {
                settings.peer_monitors = screens.peer.clone();
                settings.peer_name = screens.peer_name.clone();
            }
            if !screens.local.is_empty() {
                settings.local_monitors = screens.local.clone();
            }
            if let Some(side) = Side::from_code(screens.side) {
                settings.side = side;
            }
            settings.peer_offset = screens.offset;
        }
        if let Err(e) = self.saved.save() {
            self.error = Some(format!("Enregistrement des réglages impossible : {e:#}"));
        }
    }

    fn handle_update(&mut self, ctx: &egui::Context) {
        // Versions différentes : l'autre PC vient sans doute de se mettre à jour.
        // On cherche aussitôt (au plus toutes les 10 min) au lieu d'attendre 6 h.
        if std::mem::take(&mut self.shared.lock().expect("verrou").version_mismatch)
            && self
                .mismatch_checked
                .is_none_or(|at| at.elapsed() > Duration::from_secs(600))
        {
            self.mismatch_checked = Some(Instant::now());
            log::info!("versions différentes : recherche d'une mise à jour");
            self.updater.check_now();
        }
        if !matches!(self.updater.state(), crate::update::UpdateState::Ready(_)) {
            return;
        }
        // Jamais pendant une connexion : installer redémarre one-mouse, ce qui
        // couperait la souris et le son. La mise à jour attend que le partage
        // soit arrêté, ou que l'autre PC ne soit pas joignable (par exemple
        // parce qu'il vient lui-même de se mettre à jour).
        let connected = self.running()
            && matches!(
                self.shared.lock().expect("verrou").status,
                Some(Status::Connected(_) | Status::Sending | Status::Receiving)
            );
        if !(self.install_requested || self.settings.auto_update && !connected) {
            return;
        }
        let Some(prepared) = self.updater.take_ready() else {
            return;
        };
        self.install_requested = false;
        self.stop();
        match crate::update::launch(prepared) {
            Ok(()) => {
                log::info!("installation de la mise à jour, one-mouse va redémarrer");
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(e) => self.error = Some(format!("Mise à jour : {e:#}")),
        }
    }

    /// Carte « Mises à jour » de l'onglet Réglages.
    fn update_card(&mut self, ui: &mut egui::Ui) {
        use crate::update::UpdateState;
        card(ui, |ui| {
            section_title(ui, "Mises à jour");
            ui.label(
                egui::RichText::new(format!(
                    "Version installée : {}",
                    crate::update::current_version()
                ))
                .color(MUTED),
            );
            let (color, text) = match self.updater.state() {
                UpdateState::Checking => (MUTED, "Recherche d'une nouvelle version…".to_owned()),
                UpdateState::UpToDate => (GREEN, "one-mouse est à jour.".to_owned()),
                UpdateState::Available(release) if release.automatic() => {
                    (ORANGE, format!("Version {} disponible.", release.version))
                }
                UpdateState::Available(release) => (
                    ORANGE,
                    format!(
                        "Version {} disponible : téléchargez-la (pas d'installation automatique \
                         sur ce système).",
                        release.version
                    ),
                ),
                UpdateState::Downloading(version) => {
                    (MUTED, format!("Téléchargement de la version {version}…"))
                }
                UpdateState::Ready(version) => {
                    (GREEN, format!("Version {version} prête à être installée."))
                }
                UpdateState::Failed(error) => (RED, format!("Mise à jour impossible : {error}")),
            };
            ui.label(egui::RichText::new(text).color(color));
            if ui
                .checkbox(
                    &mut self.settings.auto_update,
                    "Installer les nouvelles versions automatiquement",
                )
                .on_hover_text(
                    "Dès qu'une version sort, elle est téléchargée, puis installée quand les deux \
                     PC ne sont pas connectés (partage arrêté, ou autre PC injoignable) : jamais \
                     de coupure en pleine utilisation. Gardez-le sur les deux PC : ils ont ainsi \
                     toujours la même version.",
                )
                .changed()
            {
                self.updater.set_auto(self.settings.auto_update);
                self.saved.auto_update = self.settings.auto_update;
                if let Err(e) = self.saved.save() {
                    self.error = Some(format!("Enregistrement des réglages impossible : {e:#}"));
                }
            }
            ui.horizontal(|ui| {
                if ui.button("Vérifier maintenant").clicked() {
                    self.updater.check_now();
                }
                match self.updater.state() {
                    UpdateState::Available(release) if release.automatic() => {
                        if ui.button("Télécharger et installer").clicked() {
                            self.install_requested = true;
                            self.updater.download_now();
                        }
                    }
                    UpdateState::Available(release) => {
                        if ui.button("Ouvrir la page de téléchargement").clicked() {
                            open_url(&release.page);
                        }
                    }
                    UpdateState::Ready(_) if ui.button("Installer maintenant").clicked() => {
                        self.install_requested = true;
                    }
                    _ => {}
                }
            });
        });
    }

    fn options_tab(&mut self, ui: &mut egui::Ui, running: bool) {
        ui.add_enabled_ui(!running, |ui| {
            let s = &mut self.settings;
            card(ui, |ui| {
                section_title(ui, "Partage");
                ui.checkbox(
                    &mut s.clipboard,
                    "Copier sur un PC, coller sur l'autre (texte, images, fichiers)",
                )
                .on_hover_text(
                    "Copiez sur un PC, collez sur l'autre avec Ctrl+V. Les fichiers copiés sont \
                     transférés (1 Go au plus par copie) dans Téléchargements/one-mouse/Presse-papier.",
                );
                ui.checkbox(
                    &mut s.clipboard_history,
                    "Garder les 20 dernières copies (effacées à la fermeture)",
                );
                ui.checkbox(
                    &mut s.start_sharing,
                    "Démarrer le partage dès l'ouverture de one-mouse",
                );
            });
            card(ui, |ui| {
                section_title(ui, "Les deux PC ensemble");
                ui.checkbox(&mut s.lock_sync, "Verrouiller les deux PC en même temps")
                    .on_hover_text(
                        "Verrouiller un PC (Windows+L…) verrouille aussi l'autre. Chacun se \
                         déverrouille ensuite avec son propre mot de passe.",
                    );
                ui.checkbox(
                    &mut s.keep_awake,
                    "Empêcher les PC de se mettre en veille pendant le partage",
                )
                .on_hover_text(
                    "Aucun des deux PC ne s'endort tant qu'ils sont connectés : son, micro et \
                     souris restent disponibles. L'écran peut toujours s'éteindre.",
                );
            });
            card(ui, |ui| {
                section_title(ui, "Allumer l'autre PC à distance");
                ui.checkbox(&mut s.wake, "Sortir l'autre PC de veille au démarrage du partage")
                    .on_hover_text(
                        "Fonctionne si l'autre PC est branché par câble à la box et a été préparé \
                         (bouton ci-dessous, sur l'autre PC). Il faut s'être connecté une fois.",
                    );
                ui.label(
                    egui::RichText::new(
                        "À faire une fois sur le PC à allumer : « Préparer ce PC ». Il doit être \
                         branché par câble à la box.",
                    )
                    .small()
                    .color(MUTED),
                );
            if let Some(peer) = crate::wake::known_peer() {
                if ui
                    .button(format!("Allumer « {} » maintenant", peer.name))
                    .clicked()
                    && crate::wake::send(&peer.macs).is_err()
                {
                    self.error = Some("Impossible d'envoyer le signal d'allumage.".into());
                }
                if !peer.wired {
                    ui.label(
                        egui::RichText::new(format!(
                            "« {} » n'est relié qu'en Wi-Fi : il ne pourra sans doute pas être \
                             allumé à distance. Branchez-le par câble à la box.",
                            peer.name
                        ))
                        .small()
                        .color(ORANGE),
                    );
                }
            }
            if ui
                .button("Préparer ce PC pour être allumé à distance")
                .on_hover_text(
                    "Demande l'autorisation de l'administrateur. Active le réveil par le \
                     réseau sur ce PC (et, sous Windows, désactive le « démarrage rapide », \
                     qui l'empêche). Sur certains PC, il faut aussi activer « Wake on LAN » \
                     dans le BIOS.",
                )
                .clicked()
            {
                match crate::wake::prepare_this_pc() {
                    Some(Ok(())) => {
                        self.error = None;
                        self.notice = Some("Ce PC peut maintenant être allumé à distance.".into());
                    }
                    Some(Err(e)) => {
                        self.notice = None;
                        self.error = Some(format!("Allumage à distance : {e:#}"));
                    }
                    None => {
                        self.error = Some(
                            "Sur ce système, activez le réveil par le réseau dans les réglages \
                             de la carte réseau."
                                .into(),
                        )
                    }
                }
            }
            });
        });
        self.update_card(ui);
        card(ui, |ui| {
            section_title(ui, "Démarrage");
            if ui
                .add_enabled(
                    !running,
                    egui::Button::new("Relancer l'assistant de démarrage"),
                )
                .on_hover_text("Les 3 questions du premier démarrage, pour tout régler à nouveau.")
                .clicked()
            {
                self.open_wizard();
            }
            let mut autostart = self.autostart;
            if ui
                .checkbox(&mut autostart, "Lancer one-mouse à l'ouverture de session")
                .changed()
            {
                match autostart::set_enabled(autostart) {
                    Ok(()) => self.autostart = autostart,
                    Err(e) => self.error = Some(format!("{e:#}")),
                }
            }
            if self.tray.is_some()
                && ui
                    .checkbox(
                        &mut self.settings.tray,
                        "Fermer la fenêtre garde one-mouse dans la barre des tâches",
                    )
                    .on_hover_text(
                        "L'icône one-mouse reste en bas à droite, à côté de l'heure : un clic \
                         rouvre la fenêtre, clic droit → Quitter pour fermer vraiment.",
                    )
                    .changed()
            {
                // Enregistré tout de suite, sans attendre le prochain démarrage du partage.
                self.saved.tray = self.settings.tray;
                if let Err(e) = self.saved.save() {
                    self.error = Some(format!("Enregistrement des réglages impossible : {e:#}"));
                }
            }
        });
        if let Some(allowed) = self.firewall_allowed {
            card(ui, |ui| {
                section_title(ui, "Pare-feu Windows");
                if allowed {
                    ui.label(
                        egui::RichText::new("one-mouse est autorisé à recevoir les connexions.")
                            .color(GREEN),
                    );
                } else {
                    ui.label(
                        egui::RichText::new(
                            "Le pare-feu peut empêcher l'autre PC de se connecter à celui-ci.",
                        )
                        .color(ORANGE),
                    );
                    if ui.button("Autoriser one-mouse dans le pare-feu").clicked() {
                        match firewall::allow() {
                            Ok(()) => {
                                self.firewall_allowed = Some(true);
                                self.error = None;
                            }
                            Err(e) => self.error = Some(format!("Pare-feu : {e:#}")),
                        }
                    }
                }
            });
        }
    }
}

/// Ouvre une page dans le navigateur.
/// Par où passe la liaison établie.
fn link_line(ui: &mut egui::Ui, link: LinkStatus) {
    let (color, text) = match link {
        LinkStatus::Bluetooth => (
            ACCENT,
            "Relié en Bluetooth — son légèrement compressé pour passer en Bluetooth",
        ),
        LinkStatus::Udp => (GREEN, "Relié par le réseau — son en qualité parfaite"),
        LinkStatus::Tcp => (
            ORANGE,
            "Relié par le réseau, son en mode de secours (un peu plus de retard) : \
             autorisez one-mouse dans le pare-feu (onglet Réglages)",
        ),
    };
    ui.label(egui::RichText::new(text).color(color));
}

/// Où en est le Bluetooth direct avant la connexion.
fn bluetooth_line(ui: &mut egui::Ui, bt: &BtStatus) {
    let (color, text) = match bt {
        BtStatus::Visible => (
            GREEN,
            "Bluetooth : ce PC est prêt, il attend l'autre PC".to_owned(),
        ),
        BtStatus::Searching(_) => (MUTED, "Bluetooth : recherche de l'autre PC…".to_owned()),
        BtStatus::Error(message) => (RED, format!("Bluetooth : {message}")),
    };
    ui.label(egui::RichText::new(text).color(color));
}

/// Ligne d'état du micro dans l'onglet Accueil.
fn mic_line(ui: &mut egui::Ui, mic: &MicStatus) {
    let (color, text) = match mic {
        MicStatus::Sending(name) => (
            GREEN,
            format!("Votre micro « {name} » est utilisable sur l'autre PC"),
        ),
        MicStatus::Default(name) => (
            GREEN,
            format!(
                "Le micro de l'autre PC est utilisable ici (vos applications le voient comme \
                 « {name} »)"
            ),
        ),
        MicStatus::NotDefault(name) => (
            ORANGE,
            format!(
                "Le micro de l'autre PC arrive dans « {name} » : choisissez « {name} » comme \
                 micro dans les réglages de son de ce PC ou de votre application"
            ),
        ),
        MicStatus::NoVirtual => (
            RED,
            "Le micro de l'autre PC ne peut pas encore être utilisé ici : installez le \
             petit logiciel gratuit ci-dessous (une seule fois)."
                .to_owned(),
        ),
    };
    ui.label(egui::RichText::new(text).color(color));
    if *mic == MicStatus::NoVirtual {
        let (name, url) = if cfg!(target_os = "macos") {
            ("BlackHole", "https://existential.audio/blackhole/")
        } else {
            ("VB-Cable", "https://vb-audio.com/Cable/")
        };
        if ui
            .button(format!("Installer {name} (gratuit), puis redémarrer ce PC"))
            .clicked()
        {
            open_url(url);
        }
    }
}

/// Heure locale « HH:MM:SS » d'un instant (secondes depuis 1970).
fn chrono_time(at: u64) -> String {
    // Décalage horaire local : différence entre l'heure locale et UTC, connue du
    // système via la fenêtre (egui n'en fournit pas) ; à défaut, heure UTC.
    let offset = local_offset_seconds();
    let secs = (at as i64 + offset).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

#[cfg(unix)]
fn local_offset_seconds() -> i64 {
    // SAFETY : localtime_r remplit la structure fournie.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm.tm_gmtoff
    }
}

#[cfg(windows)]
fn local_offset_seconds() -> i64 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    // SAFETY : structure remplie par l'appel.
    unsafe {
        let mut info: TIME_ZONE_INFORMATION = std::mem::zeroed();
        let mode = GetTimeZoneInformation(&mut info);
        // 2 : heure d'été en vigueur.
        let bias = info.Bias
            + if mode == 2 {
                info.DaylightBias
            } else {
                info.StandardBias
            };
        -(bias as i64) * 60
    }
}

#[cfg(not(any(unix, windows)))]
fn local_offset_seconds() -> i64 {
    0
}

fn open_url(url: &str) {
    let result = if cfg!(windows) {
        std::process::Command::new("explorer").arg(url).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(e) = result {
        log::warn!("ouverture de {url} impossible : {e}");
    }
}

const ACCENT: egui::Color32 = egui::Color32::from_rgb(88, 140, 255);
const GREEN: egui::Color32 = egui::Color32::from_rgb(70, 200, 120);
const ORANGE: egui::Color32 = egui::Color32::from_rgb(240, 160, 60);
const RED: egui::Color32 = egui::Color32::from_rgb(235, 85, 85);
const GREY: egui::Color32 = egui::Color32::from_rgb(130, 136, 150);
const TEXT: egui::Color32 = egui::Color32::from_rgb(230, 234, 242);
const MUTED: egui::Color32 = egui::Color32::from_rgb(145, 152, 168);
const BACKGROUND: egui::Color32 = egui::Color32::from_rgb(17, 20, 28);
const SURFACE: egui::Color32 = egui::Color32::from_rgb(26, 30, 41);
const SURFACE_LIGHT: egui::Color32 = egui::Color32::from_rgb(36, 41, 55);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Connection,
    Screens,
    Sound,
    Clipboard,
    Options,
    Help,
}

impl Tab {
    const ALL: [Tab; 6] = [
        Tab::Connection,
        Tab::Screens,
        Tab::Sound,
        Tab::Clipboard,
        Tab::Options,
        Tab::Help,
    ];

    fn label(self) -> &'static str {
        match self {
            Tab::Connection => "Accueil",
            Tab::Screens => "Écrans",
            Tab::Sound => "Son et micro",
            Tab::Clipboard => "Presse-papier",
            Tab::Options => "Réglages",
            Tab::Help => "Aide",
        }
    }
}

/// Thème sombre aux couleurs du logo.
fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = SURFACE;
    visuals.extreme_bg_color = egui::Color32::from_rgb(12, 14, 20);
    visuals.selection.bg_fill = ACCENT;
    visuals.hyperlink_color = ACCENT;
    let radius = egui::CornerRadius::same(8);
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = radius;
    }
    visuals.widgets.inactive.weak_bg_fill = SURFACE_LIGHT;
    visuals.widgets.inactive.bg_fill = SURFACE_LIGHT;
    visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(48, 55, 74);
    // Contours visibles pour les cases à cocher et champs non sélectionnés.
    visuals.widgets.inactive.bg_stroke =
        egui::Stroke::new(1.0, egui::Color32::from_rgb(72, 80, 100));
    ctx.set_visuals(visuals);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.interact_size.y = 26.0;
        // Textes plus grands que ceux d'egui par défaut : lisibles par tous.
        use egui::{FontFamily, FontId, TextStyle};
        style.text_styles = [
            (
                TextStyle::Small,
                FontId::new(12.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(14.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Heading,
                FontId::new(20.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(13.0, FontFamily::Monospace),
            ),
        ]
        .into();
    });
}

fn card<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let inner = egui::Frame::new()
        .fill(SURFACE)
        .stroke(egui::Stroke::new(1.0, SURFACE_LIGHT))
        .corner_radius(12)
        .inner_margin(14)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui)
        })
        .inner;
    ui.add_space(6.0);
    inner
}

fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.label(egui::RichText::new(title).strong().size(15.0).color(TEXT));
}

fn field(ui: &mut egui::Ui, label: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    ui.label(egui::RichText::new(label).size(12.5).color(MUTED));
    add_contents(ui);
}

/// Pastille d'état (point de couleur + texte), dessinée à sa taille exacte.
fn status_pill(ui: &mut egui::Ui, color: egui::Color32, text: &str) {
    let galley =
        ui.painter()
            .layout_no_wrap(text.to_owned(), egui::FontId::proportional(14.0), color);
    let size = egui::vec2(galley.size().x + 38.0, 28.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 14, color.gamma_multiply(0.18));
    painter.rect_stroke(
        rect,
        14,
        egui::Stroke::new(1.0, color.gamma_multiply(0.6)),
        egui::StrokeKind::Inside,
    );
    painter.circle_filled(egui::pos2(rect.left() + 15.0, rect.center().y), 4.5, color);
    painter.galley(
        egui::pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
}

/// Grande vignette cliquable pour un choix entre deux options.
/// Étapes pour relier les deux PC par Bluetooth.
fn bluetooth_steps(ui: &mut egui::Ui, peer: Option<&crate::bluetooth::KnownPeer>) {
    let step = |ui: &mut egui::Ui, n: &str, text: &str| {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(n).strong().color(ACCENT));
            ui.label(egui::RichText::new(text).color(TEXT));
        });
    };
    if cfg!(target_os = "macos") {
        ui.label(
            egui::RichText::new(
                "Le Bluetooth n'est pas encore disponible sur Mac : choisissez « Par la box ou \
                 le Wi-Fi ».",
            )
            .color(ORANGE),
        );
        return;
    }
    match peer {
        Some(peer) => ui.label(
            egui::RichText::new(format!(
                "« {} » est déjà connu : les deux PC se retrouveront directement en Bluetooth. \
                 S'ils ne sont pas encore associés, le système le proposera (un clic sur \
                 chaque PC).",
                peer.name
            ))
            .color(GREEN),
        ),
        None => ui.label(
            egui::RichText::new(
                "Astuce : connectez-vous une fois par la box ou le Wi-Fi ; les deux PC se \
                 retrouveront ensuite tout seuls en Bluetooth.",
            )
            .small()
            .color(MUTED),
        ),
    };
    step(
        ui,
        "1.",
        "Une seule fois : démarrez le partage sur le PC qui attend, puis sur l'autre PC \
         cliquez sur « Associer en Bluetooth », choisissez le premier PC dans la liste et \
         confirmez le code affiché sur les deux écrans.",
    );
    if ui.button("Associer en Bluetooth").clicked() {
        crate::bluetooth::open_settings();
    }
    step(
        ui,
        "2.",
        "Ensuite, il suffit de démarrer le partage sur les deux PC : ils se retrouvent tout \
         seuls.",
    );
    ui.label(
        egui::RichText::new(
            "Le Bluetooth est plus lent que la box : le son y est légèrement compressé et \
             arrive avec un peu plus de retard, et les PC doivent rester à moins d'une dizaine \
             de mètres. La box ou le Wi-Fi restent le meilleur choix.",
        )
        .small()
        .color(MUTED),
    );
}

fn choice_tile(ui: &mut egui::Ui, value: &mut bool, option: bool, title: &str, detail: &str) {
    let selected = *value == option;
    let response = egui::Frame::new()
        .fill(if selected {
            ACCENT.gamma_multiply(0.22)
        } else {
            SURFACE_LIGHT
        })
        .stroke(egui::Stroke::new(
            if selected { 2.0 } else { 1.0 },
            if selected { ACCENT } else { SURFACE_LIGHT },
        ))
        .corner_radius(10)
        .inner_margin(10)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(title).strong().color(TEXT));
            ui.label(egui::RichText::new(detail).small().color(MUTED));
        })
        .response
        .interact(egui::Sense::click());
    if response.clicked() {
        *value = option;
    }
}

/// Écrans des deux PC à l'échelle, ceux de l'autre PC déplaçables le long du
/// bord partagé (`offset`, en pixels).
fn layout_canvas(
    ui: &mut egui::Ui,
    local: &[Monitor],
    peer: &[Monitor],
    peer_name: &str,
    side: Side,
    offset: &mut i32,
) {
    // Écrans inconnus : un écran 1920 × 1080 pour la représentation.
    let placeholder = [Monitor {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    }];
    let local = if local.is_empty() {
        &placeholder[..]
    } else {
        local
    };
    let peer_known = !peer.is_empty();
    let peer = if peer_known { peer } else { &placeholder[..] };
    let (Some(lb), Some(pb)) = (crate::screens::bounds(local), crate::screens::bounds(peer)) else {
        return;
    };
    let vertical = matches!(side, Side::Left | Side::Right);
    let (local_len, peer_len) = if vertical {
        (lb.height, pb.height)
    } else {
        (lb.width, pb.width)
    };
    // Les deux bureaux restent en contact.
    let min_offset = -peer_len + 1;
    let max_offset = local_len - 1;
    *offset = (*offset).clamp(min_offset, max_offset);

    // Origine du bureau de l'autre PC dans celui de ce PC.
    let peer_origin = |offset: i32| match side {
        Side::Right => (lb.x + lb.width, lb.y + offset),
        Side::Left => (lb.x - pb.width, lb.y + offset),
        Side::Bottom => (lb.x + offset, lb.y + lb.height),
        Side::Top => (lb.x + offset, lb.y - pb.height),
    };
    // Échelle fixe (indépendante du décalage) : le dessin ne bouge pas pendant
    // le glissement. Étendue maximale : toutes les positions possibles.
    let (span_w, span_h) = if vertical {
        (
            (lb.width + pb.width) as f32,
            (local_len + 2 * peer_len) as f32,
        )
    } else {
        (
            (local_len + 2 * peer_len) as f32,
            (lb.height + pb.height) as f32,
        )
    };
    let height = 200.0;
    let (response, painter) = ui.allocate_painter(
        egui::vec2(ui.available_width(), height),
        egui::Sense::drag(),
    );
    let area = response.rect.shrink(8.0);
    let scale = (area.width() / span_w).min(area.height() / span_h);
    // Centre du dessin : centre du bureau de ce PC, décalé vers l'autre PC.
    let (half_w, half_h) = match side {
        Side::Right => (pb.width as f32 / 2.0, 0.0),
        Side::Left => (-pb.width as f32 / 2.0, 0.0),
        Side::Bottom => (0.0, pb.height as f32 / 2.0),
        Side::Top => (0.0, -pb.height as f32 / 2.0),
    };
    let center_x = lb.x as f32 + lb.width as f32 / 2.0 + half_w;
    let center_y = lb.y as f32 + lb.height as f32 / 2.0 + half_h;
    let to_screen = |x: i32, y: i32| {
        area.center() + egui::vec2((x as f32 - center_x) * scale, (y as f32 - center_y) * scale)
    };
    let rect_of = |m: &Monitor, dx: i32, dy: i32| {
        egui::Rect::from_two_pos(
            to_screen(m.x + dx, m.y + dy),
            to_screen(m.x + m.width + dx, m.y + m.height + dy),
        )
        .shrink(1.0)
    };

    // Glissement : converti en pixels du bureau, avec aimantation aux
    // alignements usuels (haut, centre, bas).
    let drag_id = ui.id().with("decalage");
    if response.drag_started() {
        ui.data_mut(|d| d.insert_temp(drag_id, *offset as f32));
    }
    if response.dragged() {
        let delta = response.drag_delta();
        let moved = if vertical { delta.y } else { delta.x } / scale;
        let raw = ui.data_mut(|d| {
            let value = d.get_temp_mut_or(drag_id, *offset as f32);
            *value += moved;
            *value
        });
        let snap = (12.0 / scale) as i32;
        let mut value = raw.round() as i32;
        for target in [0, (local_len - peer_len) / 2, local_len - peer_len] {
            if (value - target).abs() <= snap {
                value = target;
            }
        }
        *offset = value.clamp(min_offset, max_offset);
    }
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(if vertical {
            egui::CursorIcon::ResizeVertical
        } else {
            egui::CursorIcon::ResizeHorizontal
        });
    }

    for m in local {
        let rect = rect_of(m, 0, 0);
        painter.rect_filled(rect, 4, SURFACE_LIGHT);
        painter.rect_stroke(
            rect,
            4,
            egui::Stroke::new(1.5, TEXT),
            egui::StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("Ce PC\n{} × {}", m.width, m.height),
            egui::FontId::proportional(11.0),
            TEXT,
        );
    }
    let (ox, oy) = peer_origin(*offset);
    let (dx, dy) = (ox - pb.x, oy - pb.y);
    let name = if peer_name.is_empty() {
        "Autre PC"
    } else {
        peer_name
    };
    for m in peer {
        let rect = rect_of(m, dx, dy);
        let stroke = if peer_known {
            egui::Stroke::new(2.0, ACCENT)
        } else {
            egui::Stroke::new(1.5, GREY)
        };
        painter.rect_filled(rect, 4, ACCENT.gamma_multiply(0.25));
        painter.rect_stroke(rect, 4, stroke, egui::StrokeKind::Inside);
        let label = if peer_known {
            format!("{name}\n{} × {}", m.width, m.height)
        } else {
            format!("{name}\n(écrans inconnus)")
        };
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(11.0),
            egui::Color32::WHITE,
        );
    }
}

/// Disposition des écrans : ce PC au centre, l'autre PC à cliquer autour.
fn side_picker(ui: &mut egui::Ui, side: &mut Side) {
    let size = egui::vec2(ui.available_width(), 190.0);
    let (area, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let center = area.center();
    let painter = ui.painter_at(area);
    let screen = |c: egui::Pos2, w: f32, h: f32| egui::Rect::from_center_size(c, egui::vec2(w, h));

    let this = screen(center, 96.0, 60.0);
    painter.rect_filled(this, 8, SURFACE_LIGHT);
    painter.rect_stroke(
        this,
        8,
        egui::Stroke::new(2.0, TEXT),
        egui::StrokeKind::Inside,
    );
    painter.text(
        this.center(),
        egui::Align2::CENTER_CENTER,
        "Ce PC",
        egui::FontId::proportional(14.0),
        TEXT,
    );

    let slots = [
        (
            Side::Left,
            screen(center - egui::vec2(116.0, 0.0), 84.0, 52.0),
        ),
        (
            Side::Right,
            screen(center + egui::vec2(116.0, 0.0), 84.0, 52.0),
        ),
        (
            Side::Top,
            screen(center - egui::vec2(0.0, 64.0), 84.0, 52.0).translate(egui::vec2(0.0, -6.0)),
        ),
        (
            Side::Bottom,
            screen(center + egui::vec2(0.0, 64.0), 84.0, 52.0).translate(egui::vec2(0.0, 6.0)),
        ),
    ];
    for (slot, rect) in slots {
        let id = ui.id().with(("side", slot.arg()));
        let response = ui.interact(rect, id, egui::Sense::click());
        let selected = *side == slot;
        let (fill, stroke, label) = if selected {
            (
                ACCENT.gamma_multiply(0.35),
                egui::Stroke::new(2.0, ACCENT),
                "Autre PC",
            )
        } else if response.hovered() {
            (SURFACE_LIGHT, egui::Stroke::new(1.5, MUTED), "Ici ?")
        } else {
            (
                egui::Color32::TRANSPARENT,
                egui::Stroke::new(1.0, GREY.gamma_multiply(0.6)),
                "",
            )
        };
        painter.rect_filled(rect, 8, fill);
        painter.rect_stroke(rect, 8, stroke, egui::StrokeKind::Inside);
        if !label.is_empty() {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(13.0),
                if selected {
                    egui::Color32::WHITE
                } else {
                    MUTED
                },
            );
        }
        if response.clicked() {
            *side = slot;
        }
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }
}

fn mode_combo(ui: &mut egui::Ui, id: &str, label: &str, mode: &mut Mode, auto: &str) {
    let text = |m: Mode| match m {
        Mode::Off => "jamais".to_owned(),
        Mode::Auto => auto.to_owned(),
        Mode::Always => "toujours".to_owned(),
    };
    field(ui, label, |ui| {
        egui::ComboBox::from_id_salt(id)
            .selected_text(text(*mode))
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for m in Mode::ALL {
                    ui.selectable_value(mode, m, text(m));
                }
            });
    });
}

fn device_combo(
    ui: &mut egui::Ui,
    id: &str,
    label: &str,
    automatic: &str,
    value: &mut String,
    devices: &[String],
) {
    let shown = if value.is_empty() {
        automatic.to_owned()
    } else {
        value.clone()
    };
    field(ui, label, |ui| {
        egui::ComboBox::from_id_salt(id)
            .selected_text(shown)
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                ui.selectable_value(value, String::new(), automatic);
                for name in devices {
                    ui.selectable_value(value, name.clone(), name);
                }
            });
    });
}

fn output_combo(ui: &mut egui::Ui, id: &str, label: &str, value: &mut String, outputs: &[String]) {
    // Vide = choix automatique : sortie principale pour le son, micro virtuel pour le micro.
    let automatic = if id == "mic_output" {
        "micro virtuel (automatique)"
    } else {
        "sortie par défaut"
    };
    let shown = if value.is_empty() {
        automatic.to_owned()
    } else {
        value.clone()
    };
    field(ui, label, |ui| {
        egui::ComboBox::from_id_salt(id)
            .selected_text(shown)
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                ui.selectable_value(value, String::new(), automatic);
                for name in outputs {
                    ui.selectable_value(value, name.clone(), name);
                }
            });
    });
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_tray(&ctx);
        self.handle_update(&ctx);
        self.handle_dropped_files(&ctx);
        self.handle_screens();
        self.show_drop_zone(&ctx);
        let running = self.running();
        if running {
            // L'état change sans action de l'utilisateur : rafraîchissement régulier.
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        if let Some(step) = self.wizard
            && !running
        {
            egui::Panel::top("entete")
                .frame(
                    egui::Frame::new()
                        .fill(SURFACE)
                        .inner_margin(egui::Margin::symmetric(16, 12)),
                )
                .show(ui, |ui| self.header(ui, running));
            egui::Panel::bottom("actions")
                .frame(
                    egui::Frame::new()
                        .fill(SURFACE)
                        .inner_margin(egui::Margin::symmetric(16, 12)),
                )
                .show(ui, |ui| {
                    if let Some(error) = &self.error {
                        ui.label(egui::RichText::new(error).color(RED));
                    }
                    self.wizard_buttons(ui, &ctx, step);
                });
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(BACKGROUND)
                        .inner_margin(egui::Margin::symmetric(16, 12)),
                )
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| self.wizard_ui(ui, step));
                });
            return;
        }

        egui::Panel::top("entete")
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .inner_margin(egui::Margin::symmetric(16, 12)),
            )
            .show(ui, |ui| {
                self.header(ui, running);
                ui.add_space(6.0);
                self.tabs(ui);
            });

        egui::Panel::bottom("actions")
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .inner_margin(egui::Margin::symmetric(16, 12)),
            )
            .show(ui, |ui| {
                if let Some(error) = &self.error {
                    ui.label(egui::RichText::new(error).color(RED));
                } else if let Some(notice) = &self.notice {
                    ui.label(egui::RichText::new(notice).color(GREEN));
                }
                if running {
                    ui.label(
                        egui::RichText::new("Arrêtez le partage pour modifier les réglages.")
                            .small()
                            .color(MUTED),
                    );
                }
                let (label, fill) = if running {
                    ("Arrêter le partage", egui::Color32::from_rgb(190, 70, 70))
                } else {
                    ("Démarrer le partage", ACCENT)
                };
                let button = egui::Button::new(
                    egui::RichText::new(label)
                        .size(16.0)
                        .strong()
                        .color(egui::Color32::WHITE),
                )
                .fill(fill)
                .corner_radius(10);
                // add_sized centre le texte dans toute la largeur.
                if ui.add_sized([ui.available_width(), 42.0], button).clicked() {
                    if running {
                        self.stop();
                    } else {
                        self.start(&ctx);
                    }
                }
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::symmetric(16, 12)),
            )
            .show(ui, |ui| {
                // Fichiers survolés : invitation à les lâcher.
                if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
                    let rect = ui.max_rect();
                    ui.painter()
                        .rect_filled(rect, 12.0, ACCENT.gamma_multiply(0.25));
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "Lâchez pour envoyer à l'autre PC",
                        egui::FontId::proportional(20.0),
                        TEXT,
                    );
                }
                egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                    Tab::Connection => {
                        self.connection_tab(ui, running);
                        if running {
                            self.files_card(ui);
                        }
                    }
                    Tab::Screens => self.screens_tab(ui, running),
                    Tab::Sound => self.sound_tab(ui, running),
                    Tab::Options => self.options_tab(ui, running),
                    Tab::Clipboard => self.clipboard_tab(ui),
                    Tab::Help => self.help_tab(ui),
                });
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop();
        // L'historique peut contenir des données sensibles : effacé en partant.
        crate::history::clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lines_are_simplified() {
        assert_eq!(
            simplify_log("[2026-10-12T21:04:19Z WARN  one_mouse] capture indisponible"),
            "21:04:19 attention : capture indisponible"
        );
        assert_eq!(simplify_log("texte brut"), "texte brut");
    }
}
