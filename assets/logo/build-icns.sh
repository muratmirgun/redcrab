#!/bin/sh
# Renders icon.svg / icon-small.svg into icon.icns and icon.png.
# macOS only (iconutil); the PNGs come from resvg via a cargo example.
set -eu

cd "$(dirname "$0")"
tmp=$(mktemp -d)
trap 'rm -rf "${tmp:?}"' EXIT
iconset="$tmp/redcrab.iconset"
mkdir "$iconset"

cargo run -q -p redcrab-gui --example icons -- "$iconset"
iconutil -c icns -o icon.icns "$iconset"
cp "$iconset/icon_512x512@2x.png" icon.png
echo "wrote icon.icns and icon.png"
