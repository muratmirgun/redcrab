#!/bin/sh
# Renders icon.svg / icon-small.svg into the committed app icons:
#   icon.icns (macOS, needs iconutil), icon.ico (Windows), icon.png (1024 px).
# The PNGs come from resvg via the `icons` cargo example; Linux packages
# render their hicolor PNGs at packaging time with the same example.
set -eu

cd "$(dirname "$0")"
tmp=$(mktemp -d)
trap 'rm -rf "${tmp:?}"' EXIT

icons() { cargo run -q -p redcrab-gui --example icons -- "$@"; }

icons ico icon.ico
icons iconset "$tmp/redcrab.iconset"
cp "$tmp/redcrab.iconset/icon_512x512@2x.png" icon.png
if command -v iconutil >/dev/null 2>&1; then
    iconutil -c icns -o icon.icns "$tmp/redcrab.iconset"
else
    echo "iconutil not found (macOS only): icon.icns left as it is" >&2
fi
echo "wrote icon.ico, icon.png and icon.icns"
