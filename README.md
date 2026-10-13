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

Ouvrez **one-mouse** sur les deux PC (même réseau local) :

1. Sur le premier PC, laissez « Ce PC attend l'autre ». Notez l'**adresse de ce PC** affichée en haut de la fenêtre.
2. Sur le second PC, choisissez « Se connecter à l'autre PC » et saisissez cette adresse.
3. Sur les deux PC : saisissez le **même mot de passe** (8 caractères minimum), indiquez de quel côté se trouve
   l'autre PC, puis cliquez sur **Démarrer le partage**.

Poussez ensuite le curseur contre le bord de l'écran situé du côté de l'autre PC pour passer dessus, et contre le bord
opposé pour revenir. **Raccourci de secours** pour reprendre la main : `Ctrl` + `Maj` + `Alt` + `Méta`
(touche Windows / Cmd).

Les réglages sont enregistrés. Les options « Démarrer le partage à l'ouverture de one-mouse » et « Lancer one-mouse à
l'ouverture de session » rendent le partage automatique.

**Pare-feu** : le PC qui attend la connexion doit accepter le port TCP `24850`. Sous Windows, cliquez sur
« Autoriser one-mouse dans le pare-feu » dans sa fenêtre (une confirmation Windows s'affiche). Sous Linux avec ufw :
`sudo ufw allow 24850/tcp`. Si la connexion échoue avec « l'autre PC ne répond pas » (erreur 10060 sous Windows),
c'est presque toujours le pare-feu du PC qui attend.

### En ligne de commande

```sh
one-mouse --side right --password "mon mot de passe" listen          # premier PC
one-mouse --side left --password "mon mot de passe" connect 192.168.1.10   # second PC
```

Le mot de passe peut aussi passer par la variable `ONE_MOUSE_PASSWORD`. `one-mouse --help` liste toutes les options.

### Son, micro et vitesse de la souris

- **PC principal** : cochez « Ce PC est mon PC principal » sur le PC où sont branchés la souris, le clavier et le
  casque (un seul des deux, option `--main`). L'autre PC lui envoie alors son son **en permanence**, que la souris
  soit dessus ou non.
- **Micro** : le micro du PC qui contrôle (PC principal) est envoyé à l'autre PC et y devient **automatiquement le micro
  par défaut** pendant la connexion : Discord, Teams… l'utilisent sans réglage, et l'ancien micro est rétabli à
  l'arrêt. Sous Linux, one-mouse crée lui-même le micro virtuel « Micro-one-mouse ». Sous Windows et macOS, un micro
  virtuel demande un pilote gratuit : [VB-Cable](https://vb-audio.com/Cable/) ou
  [BlackHole](https://existential.audio/blackhole/) (bouton « Installer le micro virtuel » dans l'onglet Son).
  Le micro par défaut est basculé automatiquement sous Linux et Windows. Sans micro virtuel, le micro reçu n'est pas
  joué sur les haut-parleurs (il reviendrait en écho).
- Sans PC principal, le son n'est envoyé que par le PC contrôlé, et le micro que par celui qui contrôle : un seul sens
  à la fois, donc jamais d'écho.
- Qualité : son transmis sans perte (flottants 32 bits, non compressé), rééchantillonnage par sinc fenêtré
  (32 points, filtre anti-repliement), tampon réseau adaptatif, fondus anti-clic et compensation de la dérive entre
  les cartes son. Mesuré de bout en bout : 1 kHz et 7 kHz restitués exactement, rapport signal/bruit 71,6 dB,
  aucune coupure.
- **Casque Bluetooth** : utiliser son micro le fait passer en « mode téléphone » (son mono, faible qualité). Choisissez
  le micro intégré du PC dans l'onglet Son (« Micro utilisé »), ou n'envoyez pas le micro.
- **Vitesse de la souris** sur l'autre PC : réglage de ×0,25 à ×4 (`--pointer-speed`), si les deux PC n'ont pas la
  même sensibilité.
- `one-mouse audio-devices` liste les périphériques (`--sound-device`, `--mic-device`, `--sound-output`,
  `--mic-output`). Capture du son système : Windows, macOS 14.6 et plus, Linux avec PulseAudio ou PipeWire.

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
