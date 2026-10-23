#!/usr/bin/env bash
# Zips your prepared resources for the web build (web/README.md), into the gitignored
# .local/horizon-web-resources.zip that web/serve.py hands to the page on this machine.
#
#   web/pack-resources.sh [--no-sounds] [RESOURCES_DIR]
#
# --no-sounds leaves the sounds out: most of the size, and the game runs silent without them.
#
# RESOURCES_DIR defaults to .local/horizon-client/<version> (what `horizon-prepare --prepare`
# writes); a release folder's `resources/` works too. The zip holds files derived from your
# own copy of Minecraft: keep it to yourself, never upload it anywhere public.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
version="$(grep -m1 '"version"' "$root/research-lock.json" | sed 's/.*: *"\([^"]*\)".*/\1/')"
sounds=1
if [ "${1:-}" = --no-sounds ]; then
  sounds=0
  shift
fi
src="${1:-$root/.local/horizon-client/$version}"
out="$root/.local/horizon-web-resources.zip"
if [ ! -f "$src/horizon-resources.json" ]; then
  echo "no prepared resources in $src (horizon-resources.json is missing)." >&2
  echo "Prepare them first: cargo run --release -p horizon-prepare -- --prepare --server-jar PATH" >&2
  exit 1
fi
mkdir -p "$(dirname "$out")"
rm -f "$out"
# Sounds are Ogg Vorbis already: stored, not compressed again.
if [ "$sounds" = 1 ]; then
  (cd "$src" && zip -qr -1 -n .ogg:.png "$out" .)
else
  (cd "$src" && zip -qr -1 -n .png "$out" . -x 'assets/*/sounds/*.ogg')
fi
echo "$out: $(du -h "$out" | cut -f1)"
