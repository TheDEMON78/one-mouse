//! Onglet Aide : réponses aux questions courantes, journal technique replié
//! et informations sur l'application.

use eframe::egui;

use super::{ACCENT, App, MUTED, TEXT, card, open_url, section_title};

/// Page des signalements de problèmes.
const ISSUES_URL: &str = "https://github.com/TheDEMON78/one-mouse/issues/new/choose";
/// Page du projet.
const PROJECT_URL: &str = "https://github.com/TheDEMON78/one-mouse";

/// Questions courantes : titre, puis étapes à suivre.
const FAQ: &[(&str, &[&str])] = &[
    (
        "Les deux PC ne se trouvent pas",
        &[
            "Le partage doit être démarré sur les deux PC.",
            "Le mot de passe doit être exactement le même des deux côtés (majuscules comprises).",
            "Les deux PC doivent être sur la même box ou le même Wi-Fi (pas un Wi-Fi « invité »).",
            "Sous Windows : onglet Réglages, « Autoriser one-mouse dans le pare-feu ».",
            "En dernier recours : dans l'Accueil, ouvrez « Les PC ne se trouvent pas ? » et \
             saisissez l'adresse indiquée sur l'autre PC.",
        ],
    ),
    (
        "Le curseur ne passe pas sur l'autre écran",
        &[
            "Vérifiez dans l'onglet Écrans de quel côté se trouve l'autre PC.",
            "Poussez franchement la souris contre ce bord de l'écran.",
            "Sur Mac : Réglages Système, Confidentialité et sécurité, Accessibilité, puis \
             cochez one-mouse.",
            "Si un bouton de la souris est enfoncé (glisser en cours), le curseur reste sur ce \
             PC : relâchez-le d'abord.",
        ],
    ),
    (
        "Le curseur arrive trop haut ou trop bas sur l'autre écran",
        &[
            "Onglet Écrans : faites glisser les écrans de l'autre PC pour les aligner sur les \
           vôtres, comme ils sont posés sur le bureau.",
        ],
    ),
    (
        "Je suis bloqué sur l'autre PC",
        &[
            "Appuyez en même temps sur Ctrl + Maj + Alt + la touche Windows (Cmd sur Mac) : la \
           souris revient immédiatement sur ce PC.",
        ],
    ),
    (
        "Je n'entends pas le son de l'autre PC",
        &[
            "Onglet Son et micro : sur le PC où est branché votre casque, cochez « Ma souris, mon \
             clavier et mon casque sont branchés sur ce PC ».",
            "Vérifiez « Où écouter le son de l'autre PC » (casque ou haut-parleurs).",
        ],
    ),
    (
        "Mon micro ne fonctionne pas sur l'autre PC",
        &[
            "Sur l'autre PC, onglet Son et micro : installez le petit logiciel gratuit proposé \
             (une seule fois), puis redémarrez ce PC.",
            "Avec un casque Bluetooth, préférez le micro du PC ou de la webcam : celui du casque \
             dégrade fortement le son.",
        ],
    ),
    (
        "Le son coupe ou grésille",
        &[
            "Reliez si possible un des deux PC à la box par un câble.",
            "Onglet Son et micro : choisissez la qualité « Économie ».",
        ],
    ),
    (
        "Envoyer des fichiers à l'autre PC",
        &[
            "Glissez-les jusqu'au bord de l'écran, du côté de l'autre PC, et lâchez-les sur la \
             zone « Déposer ici » qui apparaît.",
            "Ou glissez-les sur la fenêtre one-mouse, ou copiez-les (Ctrl+C) puis collez-les sur \
             l'autre PC (Ctrl+V).",
            "Ils arrivent dans le dossier Téléchargements, sous « one-mouse ».",
        ],
    ),
    (
        "Allumer l'autre PC à distance ne marche pas",
        &[
            "L'autre PC doit être relié à la box par un câble (pas en Wi-Fi).",
            "Sur l'autre PC : onglet Réglages, « Préparer ce PC pour être allumé à distance ».",
            "Sur certains PC, il faut aussi activer « Wake on LAN » dans le BIOS.",
        ],
    ),
];

impl App {
    pub(super) fn help_tab(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            section_title(ui, "Problèmes fréquents");
            ui.label(
                egui::RichText::new("Cliquez sur une question pour voir la solution.")
                    .small()
                    .color(MUTED),
            );
            for (question, steps) in FAQ {
                egui::CollapsingHeader::new(egui::RichText::new(*question).color(TEXT))
                    .id_salt(question)
                    .show(ui, |ui| {
                        for step in *steps {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(egui::RichText::new("•").color(ACCENT));
                                ui.label(egui::RichText::new(*step).color(TEXT));
                            });
                        }
                    });
            }
        });

        card(ui, |ui| {
            section_title(ui, "Raccourci de secours");
            ui.label(
                egui::RichText::new(
                    "Ctrl + Maj + Alt + touche Windows (Cmd sur Mac) : la souris et le clavier \
                     reviennent immédiatement sur ce PC.",
                )
                .color(TEXT),
            );
        });

        card(ui, |ui| {
            section_title(ui, "Toujours un problème ?");
            ui.label(
                egui::RichText::new(
                    "Décrivez-le en joignant le journal technique (bouton « Copier le journal » \
                     ci-dessous, puis collez-le dans votre message).",
                )
                .color(MUTED),
            );
            ui.horizontal(|ui| {
                if ui.button("Signaler un problème").clicked() {
                    open_url(ISSUES_URL);
                }
                if ui.button("Copier le journal").clicked() {
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
                    self.notice = Some("Journal copié : collez-le dans votre message.".into());
                }
            });
            egui::CollapsingHeader::new(
                egui::RichText::new("Journal technique")
                    .small()
                    .color(MUTED),
            )
            .id_salt("journal")
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(260.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        let shared = self.shared.lock().expect("verrou");
                        if shared.log.is_empty() {
                            ui.label(
                                egui::RichText::new(
                                    "Vide pour l'instant : il se remplit pendant le partage.",
                                )
                                .color(MUTED),
                            );
                        }
                        for line in &shared.log {
                            ui.label(egui::RichText::new(line).monospace().size(11.5));
                        }
                    });
            });
        });

        card(ui, |ui| {
            section_title(ui, "À propos");
            ui.label(
                egui::RichText::new(format!(
                    "one-mouse {} · BloopStudio · logiciel libre (GPL 3)",
                    crate::update::current_version()
                ))
                .color(MUTED),
            );
            ui.label(
                egui::RichText::new(
                    "Les échanges entre les deux PC sont chiffrés par votre mot de passe et \
                     restent sur votre réseau.",
                )
                .small()
                .color(MUTED),
            );
            if ui.button("Page du projet").clicked() {
                open_url(PROJECT_URL);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_question_has_an_answer() {
        assert!(FAQ.iter().all(|(question, steps)| {
            !question.is_empty() && !steps.is_empty() && steps.iter().all(|s| s.ends_with('.'))
        }));
    }
}
