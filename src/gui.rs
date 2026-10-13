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
use crate::settings::{Settings, Side};
use crate::status::{BtStatus, LinkStatus, MicStatus, Status};
use crate::tray::Tray;
use crate::{autostart, crypto, firewall, network};

const ICON_PNG: &[u8] = include_bytes!("../assets/icons/256x256.png");
const MAX_LOG_LINES: usize = 300;

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
            .with_inner_size([520.0, 680.0])
            .with_min_inner_size([400.0, 480.0]),
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
                        shared.status = Some(status);
                    } else if let Some(mic) = MicStatus::parse(&line) {
                        shared.mic = Some(mic);
                    } else if let Some(link) = LinkStatus::parse(&line) {
                        shared.link = Some(link);
                    } else if let Some(bt) = BtStatus::parse(&line) {
                        shared.bt = Some(bt);
                    } else {
                        shared.push_log(line);
                    }
                    ctx.request_repaint();
                }
            });
        }
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                shared.lock().expect("verrou").push_log(simplify_log(&line));
                ctx.request_repaint();
            }
        });

        let stdin = child.stdin.take();
        Ok(Self { child, stdin })
    }

    /// `true` tant que le processus tourne.
    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn stop(&mut self) {
        self.stdin = None;
        let deadline = Instant::now() + Duration::from_secs(2);
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
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, hidden: bool) -> Self {
        apply_theme(&cc.egui_ctx);
        let settings = Settings::load();
        let settings_auto_update = settings.auto_update;
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
        };
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
        }
        self.shared.lock().expect("verrou").status = None;
    }

    fn running(&mut self) -> bool {
        let running = self.engine.as_mut().is_some_and(Engine::is_running);
        if !running {
            self.engine = None;
        }
        running
    }

    /// État affiché : couleur, titre court (pastille) et explication.
    fn status_view(&mut self, running: bool) -> (egui::Color32, String, String) {
        let status = self.shared.lock().expect("verrou").status.clone();
        match (running, status) {
            (_, Some(Status::Error(msg))) => (RED, "Erreur".into(), msg),
            (false, _) => (
                GREY,
                "Arrêté".into(),
                "Réglez la connexion puis démarrez le partage.".into(),
            ),
            (true, None) => (ORANGE, "Démarrage".into(), "Lancement du partage…".into()),
            (true, Some(Status::Listening)) => (
                ORANGE,
                "En attente".into(),
                "Démarrez le partage sur l'autre PC avec l'adresse ci-dessous.".into(),
            ),
            (true, Some(Status::Connecting)) => (
                ORANGE,
                "Connexion".into(),
                if self.settings.bluetooth {
                    "Recherche de l'autre PC en Bluetooth…".into()
                } else if self.settings.address.trim().is_empty() {
                    "Recherche de l'autre PC sur le réseau…".into()
                } else {
                    format!("Recherche de {}…", self.settings.address.trim())
                },
            ),
            (true, Some(Status::Connected(name))) => (
                GREEN,
                "Connecté".into(),
                format!("Relié à « {name} ». Poussez la souris vers l'autre écran."),
            ),
            (true, Some(Status::Sending)) => (
                ACCENT,
                "Sur l'autre PC".into(),
                "Votre souris et votre clavier contrôlent l'autre PC.".into(),
            ),
            (true, Some(Status::Receiving)) => (
                ACCENT,
                "Contrôlé".into(),
                "L'autre PC contrôle celui-ci.".into(),
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
                    egui::RichText::new(format!(
                        "BloopStudio · version {}",
                        env!("CARGO_PKG_VERSION")
                    ))
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
        ui.horizontal(|ui| {
            for tab in Tab::ALL {
                let selected = self.tab == tab;
                let text = egui::RichText::new(tab.label())
                    .size(14.0)
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
                if ui.add(button).clicked() {
                    self.tab = tab;
                }
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
            if self.settings.bluetooth && self.bluetooth_checked.elapsed() > Duration::from_secs(3)
            {
                // La liaison Bluetooth peut s'établir pendant que la fenêtre est ouverte.
                self.bluetooth_ip = network::bluetooth_ip();
                self.bluetooth_peer = crate::bluetooth::known_peer();
                self.bluetooth_checked = Instant::now();
                ui.ctx().request_repaint_after(Duration::from_secs(3));
            }
            let (address, label) = if self.settings.bluetooth {
                (self.bluetooth_ip, "Adresse Bluetooth de ce PC")
            } else {
                (self.local_ip, "Adresse de ce PC")
            };
            if self.settings.bluetooth && address.is_none() {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Bluetooth direct : aucune adresse à saisir, les deux PC se trouvent seuls.",
                    )
                    .color(MUTED),
                );
            }
            if let Some(ip) = address {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(label).color(MUTED));
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
            }
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
        });

        ui.add_enabled_ui(!running, |ui| {
            let s = &mut self.settings;
            card(ui, |ui| {
                section_title(ui, "Rôle de ce PC");
                ui.columns(2, |cols| {
                    choice_tile(
                        &mut cols[0],
                        &mut s.connect,
                        false,
                        "Attendre l'autre PC",
                        "L'autre PC se connecte à celui-ci",
                    );
                    choice_tile(
                        &mut cols[1],
                        &mut s.connect,
                        true,
                        "Se connecter",
                        "Ce PC rejoint l'autre",
                    );
                });
                if s.connect && s.bluetooth {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Liaison Bluetooth : aucune adresse à saisir, l'autre PC est trouvé \
                             en Bluetooth.",
                        )
                        .color(MUTED),
                    );
                } else if s.connect {
                    ui.add_space(4.0);
                    field(ui, "Adresse de l'autre PC (facultatif)", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut s.address)
                                .hint_text("vide : trouvé tout seul sur le réseau")
                                .desired_width(f32::INFINITY),
                        );
                    });
                }
            });

            card(ui, |ui| {
                section_title(ui, "Liaison entre les deux PC");
                ui.columns(2, |cols| {
                    choice_tile(
                        &mut cols[0],
                        &mut s.bluetooth,
                        false,
                        "Réseau (câble ou Wi-Fi)",
                        "Recommandé : son sans perte",
                    );
                    choice_tile(
                        &mut cols[1],
                        &mut s.bluetooth,
                        true,
                        "Bluetooth",
                        "Sans réseau, son compressé",
                    );
                });
                if s.bluetooth {
                    ui.add_space(6.0);
                    bluetooth_steps(ui, self.bluetooth_peer.as_ref());
                }
            });

            card(ui, |ui| {
                section_title(ui, "Sécurité");
                field(ui, "Mot de passe (identique sur les deux PC)", |ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut s.password)
                                .password(!self.show_password)
                                .hint_text("8 caractères minimum")
                                .desired_width(ui.available_width() - 70.0),
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
                field(ui, "Nom de ce PC (affiché chez l'autre)", |ui| {
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
                section_title(ui, "PC principal");
                ui.checkbox(
                    &mut s.main,
                    "Ce PC est mon PC principal (souris, clavier et casque branchés dessus)",
                );
                ui.label(
                    egui::RichText::new(
                        "À cocher sur un seul des deux PC : il reçoit le son de l'autre en permanence \
                         et lui prête son micro.",
                    )
                    .small()
                    .color(MUTED),
                );
            });
            card(ui, |ui| {
                if s.main {
                    section_title(ui, "Son et micro de ce PC principal");
                    output_combo(ui, "sound_output", "Son de l'autre PC joué sur", &mut s.sound_output, outputs);
                    mode_combo(ui, "mic", "Mon micro envoyé à l'autre PC", &mut s.mic, "quand je le contrôle");
                    if s.mic != Mode::Off {
                        device_combo(ui, "mic_device", "Micro utilisé", "micro par défaut", &mut s.mic_device, inputs);
                        ui.label(
                            egui::RichText::new(
                                "Casque Bluetooth : utiliser son micro le fait passer en « mode téléphone » \
                                 (son mono, qualité radio). Avec « micro par défaut », one-mouse prend \
                                 automatiquement un autre micro (intégré au PC, webcam…) s'il en existe un.",
                            )
                            .small()
                            .color(ORANGE),
                        );
                    }
                } else {
                    section_title(ui, "Son et micro de ce PC");
                    if audio::virtual_mic_installed(outputs) == Some(false) {
                        ui.label(
                            egui::RichText::new(
                                "Pour que les applications de ce PC utilisent le micro du PC qui contrôle, \
                                 installez le micro virtuel gratuit, puis redémarrez ce PC.",
                            )
                            .color(ORANGE),
                        );
                        let (name, url) = if cfg!(target_os = "macos") {
                            ("BlackHole", "https://existential.audio/blackhole/")
                        } else {
                            ("VB-Cable", "https://vb-audio.com/Cable/")
                        };
                        if ui.button(format!("Installer le micro virtuel ({name})")).clicked() {
                            open_url(url);
                        }
                    }
                    mode_combo(ui, "sound", "Son de ce PC envoyé", &mut s.sound, "quand l'autre PC le contrôle");
                    output_combo(ui, "mic_output", "Micro de l'autre PC envoyé vers", &mut s.mic_output, outputs);
                    ui.label(
                        egui::RichText::new(
                            "Pendant la connexion, le micro du PC qui contrôle devient automatiquement le \
                             micro par défaut de ce PC (Linux, Windows) ; l'ancien est rétabli à l'arrêt.",
                        )
                        .small()
                        .color(MUTED),
                    );
                }
            });
        });
        ui.add_enabled_ui(!running, |ui| {
            card(ui, |ui| {
                section_title(ui, "Qualité du son envoyé");
                let label = |q: Quality| match q {
                    Quality::Lossless => "Sans perte (réseau local rapide)",
                    Quality::Compressed => "Compressé, Opus (Wi-Fi faible)",
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
                        "Sans perte : le son arrive exactement tel qu'il sort du PC (~3 Mbit/s). \
                         Compressé : six fois moins de données, pour un Wi-Fi saturé.",
                    )
                    .small()
                    .color(MUTED),
                );
                if self.settings.bluetooth {
                    ui.label(
                        egui::RichText::new(
                            "Liaison Bluetooth choisie : quand la connexion passe vraiment par \
                             le Bluetooth, le son est compressé pour tenir dans son débit (par le \
                             réseau, il reste sans perte).",
                        )
                        .small()
                        .color(ORANGE),
                    );
                }
            });
        });
        if ui
            .small_button("Actualiser la liste des périphériques audio")
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
    fn handle_update(&mut self, ctx: &egui::Context) {
        if !matches!(self.updater.state(), crate::update::UpdateState::Ready(_)) {
            return;
        }
        let busy = matches!(
            self.shared.lock().expect("verrou").status,
            Some(Status::Sending | Status::Receiving)
        );
        if !(self.install_requested || self.settings.auto_update && !busy) {
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

    /// Carte « Mises à jour » de l'onglet Options.
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
                    "Dès qu'une version sort, elle est téléchargée puis installée quand la souris \
                     ne contrôle aucun PC (le partage redémarre seul). Gardez-le sur les deux PC : \
                     ils ont ainsi toujours la même version.",
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
                ui.checkbox(&mut s.clipboard, "Partager le presse-papier (texte)");
                ui.checkbox(
                    &mut s.start_sharing,
                    "Démarrer le partage à l'ouverture de one-mouse",
                );
            });
        });
        self.update_card(ui);
        card(ui, |ui| {
            section_title(ui, "Démarrage");
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
        card(ui, |ui| {
            section_title(ui, "Raccourci de secours");
            ui.label(
                egui::RichText::new(
                    "Ctrl + Maj + Alt + touche Windows / Cmd : reprendre la main à tout moment.",
                )
                .color(MUTED),
            );
        });
    }

    fn log_tab(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                section_title(ui, "Journal");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Copier").clicked() {
                        let text = self
                            .shared
                            .lock()
                            .expect("verrou")
                            .log
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("\n");
                        ui.ctx().copy_text(text);
                    }
                });
            });
            egui::ScrollArea::vertical()
                .max_height(ui.available_height().max(200.0))
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    let shared = self.shared.lock().expect("verrou");
                    if shared.log.is_empty() {
                        ui.label(egui::RichText::new("Rien pour le moment.").color(MUTED));
                    }
                    for line in &shared.log {
                        ui.label(egui::RichText::new(line).monospace().size(11.5));
                    }
                });
        });
    }
}

/// Ouvre une page dans le navigateur.
/// Par où passe la liaison établie.
fn link_line(ui: &mut egui::Ui, link: LinkStatus) {
    let (color, text) = match link {
        LinkStatus::Bluetooth => (
            ACCENT,
            "Liaison : Bluetooth — son compressé (Opus) pour tenir dans son débit",
        ),
        LinkStatus::Udp => (GREEN, "Liaison : réseau local — son sans perte en UDP"),
        LinkStatus::Tcp => (
            MUTED,
            "Liaison : réseau local — son par TCP (si cela dure, le pare-feu bloque l'UDP)",
        ),
    };
    ui.label(egui::RichText::new(text).color(color));
}

/// Où en est le Bluetooth direct avant la connexion.
fn bluetooth_line(ui: &mut egui::Ui, bt: &BtStatus) {
    let (color, text) = match bt {
        BtStatus::Visible => (
            GREEN,
            "Bluetooth : ce PC est visible et attend l'autre PC".to_owned(),
        ),
        BtStatus::Searching(_) => (
            MUTED,
            "Bluetooth : recherche de l'autre PC parmi les PC associés…".to_owned(),
        ),
        BtStatus::Error(message) => (RED, format!("Bluetooth : {message}")),
    };
    ui.label(egui::RichText::new(text).color(color));
}

/// Ligne d'état du micro dans l'onglet Connexion.
fn mic_line(ui: &mut egui::Ui, mic: &MicStatus) {
    let (color, text) = match mic {
        MicStatus::Sending(name) => (GREEN, format!("Micro envoyé à l'autre PC : « {name} »")),
        MicStatus::Default(name) => (
            GREEN,
            format!("Micro de l'autre PC reçu : « {name} » est le micro par défaut de ce PC"),
        ),
        MicStatus::NotDefault(name) => (
            ORANGE,
            format!(
                "Micro de l'autre PC reçu dans « {name} », mais il n'a pas pu devenir le micro \
                 par défaut : choisissez « {name} » comme micro dans Windows ou vos applications"
            ),
        ),
        MicStatus::NoVirtual => (
            RED,
            "Micro de l'autre PC reçu mais inutilisable : aucun micro virtuel sur ce PC. \
             Vos applications utilisent encore le micro de ce PC."
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
            .button(format!(
                "Installer le micro virtuel ({name}), puis redémarrer le PC"
            ))
            .clicked()
        {
            open_url(url);
        }
    }
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
    Options,
    Log,
}

impl Tab {
    const ALL: [Tab; 5] = [
        Tab::Connection,
        Tab::Screens,
        Tab::Sound,
        Tab::Options,
        Tab::Log,
    ];

    fn label(self) -> &'static str {
        match self {
            Tab::Connection => "Connexion",
            Tab::Screens => "Écrans",
            Tab::Sound => "Son",
            Tab::Options => "Options",
            Tab::Log => "Journal",
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
                "Bluetooth direct pas encore disponible sous macOS : choisissez la liaison par \
                 le réseau.",
            )
            .color(ORANGE),
        );
        return;
    }
    match peer {
        Some(peer) => ui.label(
            egui::RichText::new(format!(
                "Autre PC connu : « {} » ({}), appris lors d'une connexion par le réseau. Le \
                 Bluetooth le vise directement ; s'ils ne sont pas encore associés, le système \
                 propose l'association (un clic sur chaque PC).",
                peer.name,
                crate::bluetooth::format_address(peer.address)
            ))
            .color(GREEN),
        ),
        None => ui.label(
            egui::RichText::new(
                "Astuce : connectez-vous une fois par le réseau, les deux PC apprennent alors \
                 leurs adresses Bluetooth et se retrouvent ensuite directement.",
            )
            .small()
            .color(MUTED),
        ),
    };
    step(
        ui,
        "1.",
        "Une seule fois : démarrez le partage sur le PC qui attend (il devient visible), puis \
         sur l'autre PC cliquez sur « Associer en Bluetooth », choisissez le premier PC et \
         confirmez le code affiché sur les deux.",
    );
    if ui.button("Associer en Bluetooth").clicked() {
        crate::bluetooth::open_settings();
    }
    step(
        ui,
        "2.",
        "Ensuite, démarrez simplement le partage sur les deux PC : ils se retrouvent tout seuls \
         en Bluetooth, sans adresse à saisir.",
    );
    ui.label(
        egui::RichText::new(
            "Débit du Bluetooth limité (1 à 2 Mbit/s) : le son est compressé (Opus 128 kbit/s), \
             avec un peu plus de latence, et la portée est d'une dizaine de mètres. Le réseau \
             local reste le meilleur choix pour un son parfait.",
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
        let running = self.running();
        if running {
            // L'état change sans action de l'utilisateur : rafraîchissement régulier.
            ctx.request_repaint_after(Duration::from_millis(500));
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
                egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                    Tab::Connection => self.connection_tab(ui, running),
                    Tab::Screens => self.screens_tab(ui, running),
                    Tab::Sound => self.sound_tab(ui, running),
                    Tab::Options => self.options_tab(ui, running),
                    Tab::Log => self.log_tab(ui),
                });
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop();
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
