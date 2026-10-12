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
use crate::settings::{Settings, Side};
use crate::status::Status;
use crate::{autostart, crypto, firewall};

const ICON_PNG: &[u8] = include_bytes!("../assets/icons/256x256.png");
const MAX_LOG_LINES: usize = 300;

pub fn run() -> Result<()> {
    let icon = eframe::icon_data::from_png_bytes(ICON_PNG).context("icône invalide")?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("one-mouse")
            .with_icon(icon)
            .with_inner_size([460.0, 720.0])
            .with_min_inner_size([400.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "one-mouse",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("ouverture de la fenêtre impossible : {e}"))
}

/// Ce que le moteur a signalé, lu par des threads et affiché par la fenêtre.
#[derive(Default)]
struct Shared {
    status: Option<Status>,
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
                    match Status::parse(&line) {
                        Some(status) => shared.status = Some(status),
                        None => shared.push_log(line),
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
    local_ip: Option<IpAddr>,
    error: Option<String>,
    logo: egui::TextureHandle,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = Settings::load();
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
            local_ip: local_ip(),
            error: None,
            logo,
        };
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
        if self.settings.connect && self.settings.address.trim().is_empty() {
            return Err(
                "Indiquez l'adresse de l'autre PC (affichée dans sa fenêtre one-mouse).".into(),
            );
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

    fn status_line(&mut self, running: bool) -> (egui::Color32, String) {
        let status = self.shared.lock().expect("verrou").status.clone();
        let grey = egui::Color32::from_rgb(140, 145, 160);
        let orange = egui::Color32::from_rgb(240, 160, 60);
        let green = egui::Color32::from_rgb(80, 200, 120);
        let blue = egui::Color32::from_rgb(88, 140, 255);
        let red = egui::Color32::from_rgb(235, 85, 85);
        match (running, status) {
            (_, Some(Status::Error(msg))) => (red, format!("Erreur : {msg}")),
            (false, _) => (grey, "Partage arrêté".into()),
            (true, None) => (orange, "Démarrage…".into()),
            (true, Some(Status::Listening)) => (orange, "En attente de l'autre PC…".into()),
            (true, Some(Status::Connecting)) => (orange, "Connexion à l'autre PC…".into()),
            (true, Some(Status::Connected(name))) => (green, format!("Connecté à « {name} »")),
            (true, Some(Status::Sending)) => (blue, "Vous contrôlez l'autre PC".into()),
            (true, Some(Status::Receiving)) => (blue, "L'autre PC contrôle celui-ci".into()),
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let s = &mut self.settings;

        ui.heading("Connexion");
        ui.horizontal(|ui| {
            ui.radio_value(&mut s.connect, false, "Ce PC attend l'autre");
            ui.radio_value(&mut s.connect, true, "Se connecter à l'autre PC");
        });
        if s.connect {
            ui.horizontal(|ui| {
                ui.label("Adresse de l'autre PC :");
                ui.add(egui::TextEdit::singleline(&mut s.address).hint_text("192.168.1.20"));
            });
        }
        ui.horizontal(|ui| {
            ui.label("Mot de passe :");
            ui.add(
                egui::TextEdit::singleline(&mut s.password)
                    .password(!self.show_password)
                    .hint_text("identique sur les deux PC"),
            );
            ui.checkbox(&mut self.show_password, "afficher");
        });
        ui.horizontal(|ui| {
            ui.label("L'autre PC est");
            egui::ComboBox::from_id_salt("side")
                .selected_text(s.side.label())
                .show_ui(ui, |ui| {
                    for side in Side::ALL {
                        ui.selectable_value(&mut s.side, side, side.label());
                    }
                });
            ui.label("de celui-ci");
        });
        ui.horizontal(|ui| {
            ui.label("Nom de ce PC :");
            ui.add(egui::TextEdit::singleline(&mut s.name).hint_text("nom de l'ordinateur"));
        });

        ui.add_space(8.0);
        ui.heading("Son et micro");
        mode_combo(
            ui,
            "sound",
            "Son de ce PC envoyé :",
            &mut s.sound,
            "quand l'autre PC le contrôle",
        );
        mode_combo(
            ui,
            "mic",
            "Micro de ce PC envoyé :",
            &mut s.mic,
            "quand il contrôle l'autre PC",
        );
        output_combo(
            ui,
            "sound_output",
            "Son reçu joué sur :",
            &mut s.sound_output,
            &self.outputs,
        );
        output_combo(
            ui,
            "mic_output",
            "Micro reçu joué sur :",
            &mut s.mic_output,
            &self.outputs,
        );
        ui.label(
            egui::RichText::new(
                "Pour utiliser le micro reçu dans Discord ou Teams, choisissez un câble audio \
                 virtuel (VB-Cable, BlackHole).",
            )
            .small()
            .weak(),
        );

        ui.add_space(8.0);
        ui.heading("Options");
        ui.checkbox(&mut s.clipboard, "Partager le presse-papier (texte)");
        ui.checkbox(
            &mut s.start_sharing,
            "Démarrer le partage à l'ouverture de one-mouse",
        );
    }
}

fn mode_combo(ui: &mut egui::Ui, id: &str, label: &str, mode: &mut Mode, auto: &str) {
    let text = |m: Mode| match m {
        Mode::Off => "jamais".to_owned(),
        Mode::Auto => auto.to_owned(),
        Mode::Always => "toujours".to_owned(),
    };
    ui.horizontal(|ui| {
        ui.label(label);
        egui::ComboBox::from_id_salt(id)
            .selected_text(text(*mode))
            .show_ui(ui, |ui| {
                for m in Mode::ALL {
                    ui.selectable_value(mode, m, text(m));
                }
            });
    });
}

fn output_combo(ui: &mut egui::Ui, id: &str, label: &str, value: &mut String, outputs: &[String]) {
    let shown = if value.is_empty() {
        "sortie par défaut".to_owned()
    } else {
        value.clone()
    };
    ui.horizontal(|ui| {
        ui.label(label);
        egui::ComboBox::from_id_salt(id)
            .selected_text(shown)
            .width(220.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(value, String::new(), "sortie par défaut");
                for name in outputs {
                    ui.selectable_value(value, name.clone(), name);
                }
            });
    });
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let running = self.running();
        if running {
            // L'état change sans action de l'utilisateur : rafraîchissement régulier.
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Image::new(&self.logo).fit_to_exact_size(egui::vec2(48.0, 48.0)));
                    ui.vertical(|ui| {
                        ui.heading(egui::RichText::new("one-mouse").strong().size(24.0));
                        ui.label(
                            egui::RichText::new(format!(
                                "Version {} — BloopStudio",
                                env!("CARGO_PKG_VERSION")
                            ))
                            .weak(),
                        );
                    });
                });
                ui.add_space(6.0);

                let (color, text) = self.status_line(running);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), 7.0, color);
                        ui.label(egui::RichText::new(text).strong().size(16.0));
                    });
                    if let Some(ip) = self.local_ip {
                        ui.horizontal(|ui| {
                            ui.label("Adresse de ce PC :");
                            ui.add(egui::Label::new(egui::RichText::new(ip.to_string()).monospace()).selectable(true));
                        });
                    }
                    ui.label(
                        egui::RichText::new(
                            "Reprendre la main à tout moment : Ctrl + Maj + Alt + touche Windows / Cmd",
                        )
                        .small()
                        .weak(),
                    );
                });
                ui.add_space(8.0);

                ui.add_enabled_ui(!running, |ui| self.settings_ui(ui));
                if running {
                    ui.label(egui::RichText::new("Arrêtez le partage pour modifier les réglages.").small().weak());
                }

                if self.firewall_allowed == Some(false) {
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.colored_label(
                            egui::Color32::from_rgb(240, 160, 60),
                            "Le pare-feu Windows peut empêcher l'autre PC de se connecter à celui-ci.",
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
                    });
                    ui.add_space(4.0);
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

                ui.add_space(10.0);
                let (label, fill) = if running {
                    ("Arrêter le partage", egui::Color32::from_rgb(170, 60, 60))
                } else {
                    ("Démarrer le partage", egui::Color32::from_rgb(60, 110, 230))
                };
                let button = egui::Button::new(egui::RichText::new(label).size(17.0).color(egui::Color32::WHITE))
                    .fill(fill)
                    .min_size(egui::vec2(ui.available_width(), 40.0));
                if ui.add(button).clicked() {
                    if running {
                        self.stop();
                    } else {
                        self.start(&ctx);
                    }
                }
                if let Some(error) = &self.error {
                    ui.colored_label(egui::Color32::from_rgb(235, 85, 85), error);
                }

                ui.add_space(8.0);
                egui::CollapsingHeader::new("Journal").show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(180.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            let shared = self.shared.lock().expect("verrou");
                            for line in &shared.log {
                                ui.label(egui::RichText::new(line).monospace().small());
                            }
                        });
                    if ui.small_button("Actualiser la liste des sorties audio").clicked() {
                        self.outputs = audio::output_names();
                    }
                });
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
