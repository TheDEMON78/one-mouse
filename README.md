# one-mouse

**Une seule souris et un seul clavier pour deux PC posés côte à côte**, sous Windows, Linux et macOS.

Ce n'est pas du bureau à distance : aucune image n'est transmise. Chaque PC garde son écran. Quand le curseur
franchit le bord de l'écran, il passe sur l'autre PC, comme s'il s'agissait d'un écran supplémentaire. Le
clavier suit le curseur, et le presse-papier (texte) est partagé.

Éditeur : **BloopStudio**

## Installation

Les installateurs sont générés automatiquement à chaque version, dans les
[Releases](https://github.com/TheDEMON78/one-mouse-for-two-pc/releases) :

| Système | Fichier |
| --- | --- |
| Windows | `one-mouse_X.Y.Z_x64-setup.exe` ou `.msi` |
| macOS (Apple Silicon / Intel) | `one-mouse_X.Y.Z_aarch64.dmg` / `_x64.dmg` |
| Linux | `.deb` (Debian, Ubuntu…) ou `.AppImage` |

## Utilisation

Les deux PC doivent être sur le même réseau local et utiliser **le même mot de passe** (8 caractères minimum).

Sur le premier PC (exemple : l'autre PC est posé **à sa droite**) :

```sh
one-mouse --side right --password "mon mot de passe" listen
```

Sur le second PC (le premier est donc **à sa gauche**) :

```sh
one-mouse --side left --password "mon mot de passe" connect 192.168.1.10
```

- Pour passer sur l'autre PC, poussez le curseur contre le bord de l'écran situé de son côté. Pour revenir, poussez-le
  contre le bord opposé.
- **Raccourci de secours** pour reprendre la main : `Ctrl` + `Maj` + `Alt` + `Méta` (touche Windows / Cmd).
- Le mot de passe peut aussi être passé par la variable d'environnement `ONE_MOUSE_PASSWORD`.
- `--no-clipboard` désactive le partage du presse-papier. `one-mouse --help` liste toutes les options.
- Port par défaut : `24850` (TCP). Autorisez-le dans le pare-feu du PC qui écoute (`listen`).

### Son et micro

- **Son** : par défaut (`--sound auto`), le PC contrôlé envoie son son au PC qui le contrôle. Vous entendez donc
  l'autre PC dans votre casque dès que le curseur y passe. `--sound always` l'envoie en permanence,
  `--sound off` jamais.
- **Micro** : `--mic auto` envoie votre micro à l'autre PC quand vous le contrôlez, `--mic always` en permanence.
  Pour que les applications de l'autre PC (Discord, Teams…) l'utilisent comme micro, jouez-le dans un câble
  audio virtuel avec `--mic-output` : [VB-Cable](https://vb-audio.com/Cable/) sous Windows (`--mic-output "CABLE Input"`),
  [BlackHole](https://existential.audio/blackhole/) sous macOS, ou un `null-sink` PulseAudio/PipeWire sous Linux.
- `one-mouse audio-devices` liste les périphériques, à utiliser avec `--sound-device`, `--mic-device`,
  `--sound-output` et `--mic-output` (nom complet ou partiel).
- Capture du son système : Windows, macOS 14.6 et plus (autoriser l'enregistrement audio), Linux avec
  PulseAudio ou PipeWire.

### Permissions par système

- **macOS** : autoriser l'application dans *Réglages Système → Confidentialité et sécurité → Accessibilité*.
- **Linux Wayland** (GNOME, KDE, Sway, Hyprland…) : le système demande l'autorisation au premier lancement.
- **Linux X11** : aucune autorisation nécessaire. La capture X11 est propre à one-mouse (XInput2) : un PC sous X11
  peut contrôler l'autre comme être contrôlé.

## Sécurité

La connexion est chiffrée et authentifiée par le mot de passe partagé : Argon2id pour dériver la clé, HKDF-SHA256
pour une clé par session et par sens, et ChaCha20-Poly1305 pour chaque message. Sans le bon mot de passe, impossible
de lire les frappes clavier, ni d'envoyer de faux événements.

## Versions

La version est calculée à partir du nombre de commits : `1.X.Y` avec `X = commits / 100` et `Y = commits % 100`.
Par exemple, 1 commit donne `1.0.1`, 48 commits `1.0.48` et 101 commits `1.1.1`. Voir `scripts/version.sh`.

À chaque push sur `main`, la GitHub Action [Installateurs](.github/workflows/release.yml) compile les installateurs
pour les trois systèmes et les publie dans une release `v1.X.Y`.

## Développement

```sh
# Linux : dépendances de compilation
sudo apt install libx11-dev libxtst-dev libxkbcommon-dev libxi-dev libasound2-dev

cargo test
cargo run -- --help

# Installateurs locaux (cargo install cargo-packager)
cargo build --release && cargo packager --release
```

## Licence

GPL-3.0-or-later. Sous Windows, macOS et Wayland, la capture et l'émulation des entrées reposent sur les
bibliothèques `input-capture` et `input-emulation` du projet [lan-mouse](https://github.com/feschber/lan-mouse).
La capture X11 (`src/capture/x11.rs`), le protocole chiffré, le son et le presse-papier sont propres à one-mouse.
