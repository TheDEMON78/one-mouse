//! Assistant de premier démarrage : quelques questions simples qui règlent
//! one-mouse sans connaître le vocabulaire (PC principal, qui attend qui…).

use eframe::egui;

use super::{
    ACCENT, App, GREEN, MUTED, TEXT, card, choice_tile, crypto, section_title, side_picker,
};

/// Étapes de l'assistant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Welcome,
    Mouse,
    Side,
    Password,
    Done,
}

impl Step {
    /// Numéro affiché (« Étape 2 sur 4 ») ; l'accueil et la fin n'en ont pas.
    fn number(self) -> Option<usize> {
        match self {
            Step::Mouse => Some(1),
            Step::Side => Some(2),
            Step::Password => Some(3),
            Step::Welcome | Step::Done => None,
        }
    }

    fn next(self) -> Self {
        match self {
            Step::Welcome => Step::Mouse,
            Step::Mouse => Step::Side,
            Step::Side => Step::Password,
            Step::Password | Step::Done => Step::Done,
        }
    }

    fn previous(self) -> Self {
        match self {
            Step::Welcome | Step::Mouse => Step::Welcome,
            Step::Side => Step::Mouse,
            Step::Password => Step::Side,
            Step::Done => Step::Password,
        }
    }
}

const STEPS: usize = 3;

impl App {
    /// Contenu de l'assistant (à la place des onglets).
    pub(super) fn wizard_ui(&mut self, ui: &mut egui::Ui, step: Step) {
        if let Some(n) = step.number() {
            ui.label(
                egui::RichText::new(format!("Question {n} sur {STEPS}"))
                    .small()
                    .color(MUTED),
            );
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 3, super::SURFACE_LIGHT);
            let done = egui::Rect::from_min_size(
                rect.min,
                egui::vec2(rect.width() * n as f32 / STEPS as f32, rect.height()),
            );
            ui.painter().rect_filled(done, 3, ACCENT);
            ui.add_space(8.0);
        }
        let s = &mut self.settings;
        match step {
            Step::Welcome => card(ui, |ui| {
                ui.label(
                    egui::RichText::new("Bienvenue dans one-mouse")
                        .size(22.0)
                        .strong()
                        .color(TEXT),
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "Une seule souris et un seul clavier pour deux ordinateurs posés côte \
                         à côte : le curseur passe d'un écran à l'autre comme s'il n'y avait \
                         qu'un seul ordinateur. Le son, le micro et le copier-coller suivent \
                         aussi.",
                    )
                    .color(TEXT),
                );
                ui.add_space(10.0);
                ui.label(egui::RichText::new("Pour commencer :").strong().color(TEXT));
                for (n, text) in [
                    ("1", "Installez one-mouse sur les deux ordinateurs."),
                    (
                        "2",
                        "Répondez à 3 questions sur chacun (moins d'une minute).",
                    ),
                    (
                        "3",
                        "Cliquez sur « Démarrer le partage » sur les deux. C'est tout.",
                    ),
                ] {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(n).strong().color(ACCENT));
                        ui.label(egui::RichText::new(text).color(TEXT));
                    });
                }
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Les deux ordinateurs doivent être sur la même box ou le même Wi-Fi \
                         (ou reliés en Bluetooth, à choisir ensuite dans l'Accueil).",
                    )
                    .small()
                    .color(MUTED),
                );
            }),
            Step::Mouse => card(ui, |ui| {
                section_title(
                    ui,
                    "Votre souris et votre clavier sont-ils branchés sur cet ordinateur ?",
                );
                ui.add_space(4.0);
                // Aucune réponse pré-choisie : il faut en cliquer une.
                let mut yes = self.wizard_main == Some(true);
                let mut not_no = self.wizard_main != Some(false);
                ui.columns(2, |cols| {
                    choice_tile(
                        &mut cols[0],
                        &mut yes,
                        true,
                        "Oui, sur celui-ci",
                        "C'est l'ordinateur que j'utilise le plus",
                    );
                    choice_tile(
                        &mut cols[1],
                        &mut not_no,
                        false,
                        "Non, sur l'autre",
                        "Celui-ci sera piloté depuis l'autre",
                    );
                });
                if yes && self.wizard_main != Some(true) {
                    self.wizard_main = Some(true);
                } else if !not_no && self.wizard_main != Some(false) {
                    self.wizard_main = Some(false);
                }
                let Some(main) = self.wizard_main else {
                    return;
                };
                // Le PC de la souris attend, l'autre le rejoint (trouvé tout seul).
                s.main = main;
                s.connect = !main;
                if !main {
                    s.address.clear();
                }
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(if main {
                        "Votre casque aussi, s'il y en a un : vous y entendrez le son de l'autre \
                         ordinateur, et votre micro y fonctionnera."
                    } else {
                        "Sur l'autre ordinateur, vous répondrez « Oui, sur celui-ci »."
                    })
                    .small()
                    .color(MUTED),
                );
            }),
            Step::Side => card(ui, |ui| {
                section_title(ui, "Où se trouve l'écran de l'autre ordinateur ?");
                ui.label(
                    egui::RichText::new(
                        "Par rapport à l'écran de celui-ci, tel que vous les voyez sur votre \
                         bureau. Cliquez sur l'emplacement.",
                    )
                    .color(MUTED),
                );
                ui.add_space(6.0);
                side_picker(ui, &mut s.side);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "L'autre écran est {} de celui-ci : poussez la souris de ce côté \
                             pour passer dessus.",
                            s.side.label()
                        ))
                        .color(TEXT),
                    );
                });
            }),
            Step::Password => card(ui, |ui| {
                section_title(ui, "Choisissez un mot de passe");
                ui.label(
                    egui::RichText::new(
                        "Inventez-le maintenant et saisissez exactement le même sur l'autre \
                         ordinateur. Il empêche tout autre appareil de prendre le contrôle.",
                    )
                    .color(MUTED),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut s.password)
                            .password(!self.show_password)
                            .hint_text(format!("{} caractères minimum", crypto::MIN_PASSWORD_LEN))
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
                let count = s.password.chars().count();
                if count > 0 && count < crypto::MIN_PASSWORD_LEN {
                    ui.label(
                        egui::RichText::new(format!(
                            "Encore {} caractère(s).",
                            crypto::MIN_PASSWORD_LEN - count
                        ))
                        .small()
                        .color(MUTED),
                    );
                }
            }),
            Step::Done => card(ui, |ui| {
                ui.label(
                    egui::RichText::new("Cet ordinateur est prêt")
                        .size(20.0)
                        .strong()
                        .color(GREEN),
                );
                ui.add_space(6.0);
                let other = if s.main {
                    "«\u{a0}Non,\u{a0}sur\u{a0}l'autre\u{a0}»"
                } else {
                    "«\u{a0}Oui,\u{a0}sur\u{a0}celui-ci\u{a0}»"
                };
                for (n, text) in [
                    (
                        "1",
                        format!(
                            "Sur l'autre ordinateur, ouvrez one-mouse et répondez aux mêmes \
                             questions : {other} à la première, et le même mot de passe."
                        ),
                    ),
                    (
                        "2",
                        "Cliquez sur « Démarrer le partage » sur les deux ordinateurs : ils se \
                         trouvent tout seuls."
                            .to_owned(),
                    ),
                    (
                        "3",
                        "Poussez la souris vers le bord de l'écran, du côté de l'autre : le \
                         curseur passe dessus."
                            .to_owned(),
                    ),
                ] {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(n).strong().color(ACCENT));
                        ui.label(egui::RichText::new(text).color(TEXT));
                    });
                }
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Tous les réglages restent modifiables ensuite dans les onglets, et l'onglet Aide \
                         répond aux questions courantes.",
                    )
                    .small()
                    .color(MUTED),
                );
            }),
        }
    }

    /// Boutons de l'assistant (en bas de la fenêtre).
    pub(super) fn wizard_buttons(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, step: Step) {
        let password_ok = self.settings.password.chars().count() >= crypto::MIN_PASSWORD_LEN;
        let (label, enabled) = match step {
            Step::Welcome => ("Commencer", true),
            Step::Password => ("Terminer", password_ok),
            Step::Mouse => ("Suivant", self.wizard_main.is_some()),
            Step::Done => ("Démarrer le partage", true),
            _ => ("Suivant", true),
        };
        let button = egui::Button::new(
            egui::RichText::new(label)
                .size(16.0)
                .strong()
                .color(egui::Color32::WHITE),
        )
        .fill(ACCENT)
        .corner_radius(10);
        if ui
            .add_enabled_ui(enabled, |ui| {
                ui.add_sized([ui.available_width(), 42.0], button)
            })
            .inner
            .clicked()
        {
            if step == Step::Done {
                self.finish_wizard();
                self.start(ctx);
            } else {
                if step.next() == Step::Done {
                    // Réglages enregistrés dès maintenant, même si la fenêtre est fermée.
                    self.save_wizard();
                }
                self.wizard = Some(step.next());
            }
        }
        ui.horizontal(|ui| {
            if step != Step::Welcome && ui.button("Retour").clicked() {
                self.wizard = Some(step.previous());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let skip = if step == Step::Done {
                    "Voir les réglages"
                } else {
                    "Passer l'assistant"
                };
                if ui.button(skip).clicked() {
                    self.finish_wizard();
                }
            });
        });
    }

    /// Ouvre l'assistant depuis le début.
    pub(super) fn open_wizard(&mut self) {
        self.wizard = Some(Step::Welcome);
        self.wizard_main = None;
    }

    /// Fin de l'assistant : réglages enregistrés, retour aux onglets.
    fn finish_wizard(&mut self) {
        self.wizard = None;
        self.tab = super::Tab::Connection;
        self.save_wizard();
    }

    fn save_wizard(&mut self) {
        self.settings.setup_done = true;
        match self.settings.save() {
            Ok(()) => self.saved = self.settings.clone(),
            Err(e) => {
                self.error = Some(format!("Enregistrement des réglages impossible : {e:#}"));
            }
        }
    }
}
