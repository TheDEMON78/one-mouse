#!/usr/bin/env bash
# Version calculée à partir du nombre de commits :
#   1 commit -> 1.0.1, 99 -> 1.0.99, 100 -> 1.1.0, 101 -> 1.1.1, 250 -> 1.2.50
# Usage : scripts/version.sh            affiche la version
#         scripts/version.sh --apply    l'écrit aussi dans Cargo.toml
set -euo pipefail
cd "$(dirname "$0")/.."

count=$(git rev-list --count HEAD)
version="1.$((count / 100)).$((count % 100))"

if [ "${1:-}" = "--apply" ]; then
  # Remplace uniquement la version du paquet (première ligne `version = ` du fichier).
  sed -i.bak "0,/^version = \".*\"/s//version = \"$version\"/" Cargo.toml
  rm -f Cargo.toml.bak
fi
echo "$version"
