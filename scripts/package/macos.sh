#!/usr/bin/env bash
# Builds redcrab.app and wraps it in a disk image:
#   dist/redcrab-<version>-macos-<arch>.dmg
#
#   scripts/package/macos.sh                       # this Mac's architecture
#   TARGETS="aarch64-apple-darwin x86_64-apple-darwin" scripts/package/macos.sh
#                                                   # a universal app (lipo)
#
# Signing, all optional; without them the app is ad-hoc signed, which runs
# locally, while downloaded copies need a right-click > Open the first time:
#   APPLE_SIGNING_IDENTITY  a "Developer ID Application: ..." identity in the
#                           keychain: hardened runtime + secure timestamp
#   APPLE_NOTARY_KEY        path to an App Store Connect API key (.p8)
#   APPLE_NOTARY_KEY_ID     its key id
#   APPLE_NOTARY_ISSUER     its issuer id
# With all three notary variables (and a signing identity) the disk image is
# notarized and the ticket stapled to it.

source "$(dirname "$0")/common.sh"

BUNDLE_ID="io.github.muratmirgun.redcrab"
# Apple Silicon starts at macOS 11; keep Intel builds on the same floor.
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"

read -r -a targets <<<"${TARGETS:-$(rustc -vV | sed -n 's/^host: //p')}"

# Without Xcode's Metal compiler the shaders are compiled at startup instead
# (the default `runtime-shaders` feature); see the CI workflow.
features=()
if xcrun -sdk macosx metal --version >/dev/null 2>&1 &&
    xcrun -sdk macosx --find metallib >/dev/null 2>&1; then
    features=(--no-default-features)
fi

binaries=()
for target in "${targets[@]}"; do
    log "building $target"
    cargo build -p redcrab-gui --release --locked --target "$target" ${features[@]+"${features[@]}"}
    binaries+=("$TARGET_DIR/$target/release/redcrab")
done

case "${#targets[@]}:${targets[0]}" in
    1:aarch64-*) arch=arm64 ;;
    1:x86_64-*) arch=x86_64 ;;
    *) arch=universal ;;
esac

app="$WORK/redcrab.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
if [ "${#binaries[@]}" -gt 1 ]; then
    lipo -create -output "$app/Contents/MacOS/redcrab" "${binaries[@]}"
else
    cp "${binaries[0]}" "$app/Contents/MacOS/redcrab"
fi
lipo -info "$app/Contents/MacOS/redcrab" >&2
cp assets/logo/icon.icns "$app/Contents/Resources/redcrab.icns"
cp LICENSE-MIT LICENSE-APACHE "$app/Contents/Resources/"
printf 'APPL????' >"$app/Contents/PkgInfo"
cat >"$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>redcrab</string>
    <key>CFBundleExecutable</key>
    <string>redcrab</string>
    <key>CFBundleIconFile</key>
    <string>redcrab</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>redcrab</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
    <key>LSMinimumSystemVersion</key>
    <string>$MACOSX_DEPLOYMENT_TARGET</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>Copyright © 2026 Murat Mirgün Ercan. MIT OR Apache-2.0.</string>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
</dict>
</plist>
EOF
plutil -lint "$app/Contents/Info.plist" >&2

identity="${APPLE_SIGNING_IDENTITY:-}"
if [ -n "$identity" ]; then
    log "signing with $identity"
    codesign --force --options runtime --timestamp --sign "$identity" "$app"
else
    log "ad-hoc signing (no APPLE_SIGNING_IDENTITY)"
    codesign --force --sign - "$app"
fi
codesign --verify --strict --verbose=2 "$app"

# The bundle must run: the binary answers --version without opening a window.
"$app/Contents/MacOS/redcrab" --version

dmg="$DIST/redcrab-$VERSION-macos-$arch.dmg"
staging="$WORK/dmg"
mkdir -p "$staging"
cp -R "$app" "$staging/"
ln -s /Applications "$staging/Applications"
rm -f "$dmg"
hdiutil create -quiet -volname "redcrab $VERSION" -srcfolder "$staging" \
    -fs HFS+ -format UDZO -ov "$dmg"
hdiutil verify -quiet "$dmg"
if [ -n "$identity" ]; then
    codesign --force --timestamp --sign "$identity" "$dmg"
fi

if [ -n "$identity" ] && [ -n "${APPLE_NOTARY_KEY:-}" ] &&
    [ -n "${APPLE_NOTARY_KEY_ID:-}" ] && [ -n "${APPLE_NOTARY_ISSUER:-}" ]; then
    log "notarizing"
    xcrun notarytool submit "$dmg" --wait \
        --key "$APPLE_NOTARY_KEY" --key-id "$APPLE_NOTARY_KEY_ID" \
        --issuer "$APPLE_NOTARY_ISSUER"
    xcrun stapler staple "$dmg"
    xcrun stapler validate "$dmg"
fi

checksum "$dmg"
echo "$dmg"
