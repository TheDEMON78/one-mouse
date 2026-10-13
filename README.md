<p align="center">
  <img src="assets/icons/128x128.png" alt="Logo one-mouse" width="96">
</p>

<h1 align="center">one-mouse</h1>

<p align="center">
  <b>Une seule souris et un seul clavier pour deux PC posés côte à côte</b><br>
  Windows · Linux · macOS — par <b>BloopStudio</b>
</p>

<p align="center">
  <a href="https://github.com/TheDEMON78/one-mouse/releases/latest"><img alt="Dernière version" src="https://img.shields.io/github/v/release/TheDEMON78/one-mouse?label=version"></a>
  <a href="https://github.com/TheDEMON78/one-mouse/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/TheDEMON78/one-mouse/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/TheDEMON78/one-mouse/releases"><img alt="Téléchargements" src="https://img.shields.io/github/downloads/TheDEMON78/one-mouse/total?label=t%C3%A9l%C3%A9chargements"></a>
  <a href="LICENSE"><img alt="Licence GPL-3.0" src="https://img.shields.io/badge/licence-GPL--3.0-blue"></a>
</p>

Poussez le curseur contre le bord de l'écran : il passe sur l'autre PC, comme sur un écran supplémentaire. Le clavier
suit, le presse-papier est partagé, et le son de l'autre PC arrive dans votre casque.

Ce n'est **pas** du bureau à distance : aucune image n'est transmise, chaque PC garde son propre écran.

<p align="center">
  <img src="docs/interface.png" alt="Fenêtre de one-mouse : onglets Connexion et Écrans" width="760">
</p>

## Fonctionnalités

- 🖱️ **Souris et clavier partagés** : le curseur franchit le bord de l'écran, dans les deux sens, entre Windows, Linux
  et macOS (n'importe quelle combinaison).
- 📋 **Presse-papier partagé** (texte) : copiez sur un PC, collez sur l'autre.
- 🔊 **Son de l'autre PC dans votre casque, en permanence**, sans perte (flottants 32 bits), en UDP chiffré.
- 🎙️ **Votre micro devient celui de l'autre PC** : Discord, Teams… l'utilisent sans réglage.
- 🎯 **Vitesse de la souris réglable** sur l'autre PC, si les deux n'ont pas la même sensibilité.
- 🔒 **Connexion chiffrée** par mot de passe (Argon2id, ChaCha20-Poly1305).
- 📶 **Par le réseau ou par Bluetooth** : câble ou Wi-Fi (recommandé), ou liaison Bluetooth directe entre les deux PC.
- 🪟 **Reste dans la barre des tâches** (Windows) : fermer la fenêtre garde one-mouse près de l'horloge.
- 🚀 **Démarrage automatique** à l'ouverture de session, partage lancé tout seul.
- 📦 **Installateurs** générés automatiquement à chaque version.

## Installation

Téléchargez l'installateur de la [dernière version](https://github.com/TheDEMON78/one-mouse/releases/latest) et
installez **la même version sur les deux PC** :

| Système | Fichier |
| --- | --- |
| Windows | `one-mouse_X.Y.Z_x64-setup.exe` ou `.msi` |
| macOS Apple Silicon (M1, M2…) | `one-mouse_X.Y.Z_aarch64.dmg` |
| macOS Intel | `one-mouse_X.Y.Z_x64.dmg` |
| Linux | `.deb` (Debian, Ubuntu…) ou `.AppImage` |

## Démarrage rapide

Ouvrez **one-mouse** sur les deux PC (même réseau local) :

1. Sur le premier PC, laissez « Attendre l'autre PC ». Notez l'**adresse de ce PC** affichée dans l'onglet Connexion.
2. Sur le second PC, choisissez « Se connecter » et saisissez cette adresse.
3. Sur les deux PC : saisissez le **même mot de passe** (8 caractères minimum), indiquez dans l'onglet Écrans de quel
   côté se trouve l'autre PC, puis cliquez sur **Démarrer le partage**.
4. Sur le PC où sont branchés la souris, le clavier et le casque, cochez « Ce PC est mon PC principal » (onglet Son).

Poussez ensuite le curseur contre le bord de l'écran situé du côté de l'autre PC pour passer dessus, et contre le bord
opposé pour revenir.

> **Raccourci de secours** pour reprendre la main : `Ctrl` + `Maj` + `Alt` + `Méta` (touche Windows / Cmd).

Les réglages sont enregistrés. Les options « Démarrer le partage à l'ouverture de one-mouse » et « Lancer one-mouse à
l'ouverture de session » rendent tout automatique. Sous Windows, one-mouse démarre alors directement dans la zone de
notification ; fermer la fenêtre l'y garde (clic sur l'icône pour la rouvrir, clic droit → **Quitter** pour fermer).

## Liaison par Bluetooth

Sans réseau commun, les deux PC peuvent se relier **directement en Bluetooth**, sans adresse à saisir (Windows et
Linux). Choisissez **Bluetooth** dans « Liaison entre les deux PC » (onglet Connexion) sur les deux PC.

1. **Une seule fois** : démarrez le partage sur le PC qui attend (il devient visible en Bluetooth), puis sur l'autre
   PC cliquez sur **Associer en Bluetooth**, choisissez le premier PC et confirmez le code affiché sur les deux.
   Cette confirmation est imposée par le système, pour la sécurité : aucune application ne peut la faire à votre place.
2. **Ensuite** : démarrez simplement le partage sur les deux PC. Celui qui se connecte cherche tout seul, parmi les PC
   associés, celui qui attend avec one-mouse, et se reconnecte automatiquement.

Le débit du Bluetooth (1 à 2 Mbit/s) ne permet pas le son sans perte : il est compressé automatiquement (Opus
256 kbit/s, micro 64 kbit/s), avec un peu plus de latence et une portée d'une dizaine de mètres. Le réseau local reste
le meilleur choix pour un son parfait.

Sous macOS, le Bluetooth direct n'est pas encore disponible : un réseau personnel Bluetooth (PAN) fonctionne en
saisissant l'adresse de l'autre PC. En ligne de commande : `--bluetooth`, et `connect` sans adresse.

## Son et micro

- **PC principal** : cochez « Ce PC est mon PC principal » sur le PC où sont branchés la souris, le clavier et le
  casque (un seul des deux, option `--main`). L'autre PC lui envoie alors son son **en permanence**, que la souris
  soit dessus ou non.
- **Micro** : le micro du PC principal est envoyé à l'autre PC et y devient **automatiquement le micro par défaut**
  pendant la connexion. L'ancien micro est rétabli à l'arrêt.
  - Linux : one-mouse crée lui-même le micro virtuel « Micro-one-mouse ».
  - Windows et macOS : il faut un pilote gratuit, [VB-Cable](https://vb-audio.com/Cable/) ou
    [BlackHole](https://existential.audio/blackhole/) (bouton « Installer le micro virtuel » dans l'onglet Son).
  - Sans micro virtuel, le micro reçu n'est pas joué sur les haut-parleurs (il reviendrait en écho).
  - L'onglet Connexion indique en direct le micro envoyé, et sur l'autre PC si le micro reçu est bien devenu le
    micro par défaut (sinon : quoi installer ou choisir).
  - Le PC principal n'envoie jamais un micro virtuel resté par défaut : il prend alors son vrai micro.
- Sans PC principal, le son n'est envoyé que par le PC contrôlé, et le micro que par celui qui contrôle : un seul sens
  à la fois, donc jamais d'écho.
- **Qualité** : par défaut, son **sans perte** (~3 Mbit/s en stéréo), mesuré de bout en bout à 71,6 dB de rapport
  signal/bruit, sans coupure. Option « Compressé » (Opus 510 kbit/s, six fois moins de données) pour un Wi-Fi saturé.
  - Le son passe en **UDP chiffré**, par paquets de 2,5 ms : un paquet perdu est comblé au lieu de bloquer le flux.
    Repli automatique en TCP si l'UDP est bloqué.
  - Traitement : rééchantillonnage par sinc fenêtré, tampon adaptatif (qui ne grossit pas pendant les silences),
    fondus anti-clic, compensation de la dérive entre les cartes son.
- **Casques 5.1 / 7.1** : vrai mixage multicanal. La stéréo va sur l'avant gauche et droite. En ramenant du 5.1/7.1 en
  stéréo, le centre (les voix) est conservé à -3 dB.
- **Casque Bluetooth** : utiliser son micro le fait passer en « mode téléphone » (son mono, qualité radio). Avec
  « micro par défaut », one-mouse prend automatiquement un autre micro (intégré au PC, webcam…) s'il en existe un.
- **Windows** : pour un son fidèle, désactivez « Son spatial » et les « Améliorations audio » du casque.

## Pare-feu

Le PC qui attend la connexion doit accepter le port TCP `24850` (et l'UDP pour le son, sinon il passe en TCP).

- **Windows** : cliquez sur « Autoriser one-mouse dans le pare-feu », dans l'onglet Options (une confirmation
  Windows s'affiche).
- **Linux avec ufw** : `sudo ufw allow 24850/tcp`.

Si la connexion échoue avec « l'autre PC ne répond pas » (erreur 10060 sous Windows), c'est presque toujours le
pare-feu du PC qui attend.

## Permissions par système

- **macOS** : autoriser l'application dans *Réglages Système → Confidentialité et sécurité → Accessibilité*.
- **Linux Wayland** (GNOME, KDE, Sway, Hyprland…) : le système demande l'autorisation au premier lancement.
- **Linux X11** : aucune autorisation nécessaire. La capture X11 est propre à one-mouse (XInput2) : un PC sous X11
  peut contrôler l'autre comme être contrôlé.

## En ligne de commande

```sh
one-mouse --side right --password "mon mot de passe" listen                 # premier PC
one-mouse --side left --password "mon mot de passe" connect 192.168.1.10    # second PC
```

Le mot de passe peut aussi passer par la variable `ONE_MOUSE_PASSWORD`.

- `one-mouse --help` liste toutes les options : `--main`, `--pointer-speed`, `--sound`, `--mic`, `--sound-quality`…
- `one-mouse audio-devices` liste les périphériques audio, à utiliser avec `--sound-device`, `--mic-device`,
  `--sound-output` et `--mic-output`.
- Capture du son système : Windows, macOS 14.6 et plus, Linux avec PulseAudio ou PipeWire.

## Sécurité

La connexion est chiffrée et authentifiée par le mot de passe partagé : Argon2id pour dériver la clé, HKDF-SHA256
pour une clé par session et par sens, et ChaCha20-Poly1305 pour chaque message (TCP comme UDP, avec protection contre
le rejeu). Sans le bon mot de passe, impossible de lire les frappes clavier ou le son, ni d'envoyer de faux événements.

## Versions

La version est calculée à partir du nombre de commits : `1.X.Y` avec `X = commits / 100` et `Y = commits % 100`.
Par exemple, 1 commit donne `1.0.1`, 48 commits `1.0.48` et 101 commits `1.1.1`. Voir `scripts/version.sh`.

À chaque push sur `main`, la GitHub Action [Installateurs](.github/workflows/release.yml) compile les installateurs
pour les trois systèmes et les publie dans une release `v1.X.Y`.

## Développement

```sh
# Linux : dépendances de compilation
sudo apt install libx11-dev libxtst-dev libxkbcommon-dev libxi-dev libasound2-dev cmake

cargo test
cargo run              # fenêtre
cargo run -- --help    # ligne de commande

# Installateurs locaux (cargo install cargo-packager)
cargo build --release && cargo packager --release
```

Organisation du code (`src/`) :

| Fichier | Rôle |
| --- | --- |
| `gui.rs`, `tray.rs` | Fenêtre de réglages, icône de la zone de notification |
| `session.rs` | Connexion, passage du curseur, son, micro, presse-papier |
| `crypto.rs`, `protocol.rs` | Poignée de main chiffrée et messages échangés |
| `bluetooth/`, `network.rs` | Liaison Bluetooth directe (Winsock RFCOMM, BlueZ), interfaces réseau |
| `capture/` | Capture de la souris et du clavier (X11 maison, sinon lan-mouse) |
| `audio.rs`, `codec.rs` | Capture, lecture, mixage, rééchantillonnage, sans perte / Opus |
| `default_mic/`, `firewall.rs`, `autostart.rs` | Intégration au système |

## Licence

GPL-3.0-or-later. Sous Windows, macOS et Wayland, la capture et l'émulation des entrées reposent sur les
bibliothèques `input-capture` et `input-emulation` du projet [lan-mouse](https://github.com/feschber/lan-mouse).
La capture X11 (`src/capture/x11.rs`), le protocole chiffré, le son et le presse-papier sont propres à one-mouse.
