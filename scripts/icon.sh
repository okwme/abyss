#!/usr/bin/env bash
# Rebuilds assets/icon/abyss.icns from assets/icon/render.swift.
# Not part of the bundle build: abyss.icns is committed.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

swift "$root/assets/icon/render.swift" "$tmp/abyss.iconset"
iconutil -c icns "$tmp/abyss.iconset" -o "$root/assets/icon/abyss.icns"
echo "wrote $root/assets/icon/abyss.icns"
