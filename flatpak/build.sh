#!/bin/sh
set -eu

cd "$(dirname "$0")/.."
manifest=flatpak/com.yokarion.oxiedraw.yml
out=target/flatpak
tools="$out/flatpak-builder-tools"

if [ ! -d "$tools" ]; then
  git clone --depth 1 https://github.com/flatpak/flatpak-builder-tools.git "$tools"
fi
python3 "$tools/cargo/flatpak-cargo-generator.py" Cargo.lock -o flatpak/cargo-sources.json
sh flatpak/changelog-to-metainfo.sh

source="$out/source"
rm -rf "$source"
mkdir -p "$source"
git ls-files -z --cached --others --exclude-standard |
  tar --null --files-from=- --ignore-failed-read -cf - |
  tar -C "$source" -xf -

flatpak run --env=FLATPAK_USER_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/flatpak" \
  --command=flatpak-builder org.flatpak.Builder \
  --force-clean --user --install --install-deps-from=flathub \
  --ccache --mirror-screenshots-url=https://dl.flathub.org/media --compose-url-policy=full \
  --state-dir="$out/state" --repo="$out/repo" "$out/build" "$manifest"

flatpak run --command=flatpak-builder-lint org.flatpak.Builder manifest "$manifest"
flatpak run --command=flatpak-builder-lint org.flatpak.Builder repo "$out/repo"
