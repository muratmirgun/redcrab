#!/usr/bin/env bash
# Builds the Linux packages for this machine's architecture:
#   dist/redcrab-<version>-linux-<arch>.tar.gz   binary, .desktop file, icons
#   dist/redcrab_<version>_<debarch>.deb         for Debian and Ubuntu
#
# Build on the oldest distribution the packages should run on: the binary
# needs at least the glibc it was linked against (the release workflow uses
# Ubuntu 22.04, glibc 2.35). Needs dpkg-deb and dpkg-shlibdeps (dpkg-dev).

source "$(dirname "$0")/common.sh"

case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 deb_arch=amd64 ;;
    aarch64 | arm64) arch=aarch64 deb_arch=arm64 ;;
    *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

log "building"
cargo build -p redcrab-gui --release --locked
binary="$TARGET_DIR/release/redcrab"
answers_version "$binary"

log "rendering icons"
icons="$WORK/icons"
# --release reuses the dependencies the binary was just built with.
cargo run -q --release --locked -p redcrab-gui --example icons -- pngs "$icons/hicolor"

desktop="$WORK/redcrab.desktop"
cat >"$desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=redcrab
GenericName=Redis Client
Comment=Fast, native Redis GUI
Exec=redcrab %u
Icon=redcrab
Terminal=false
Categories=Development;Database;
Keywords=redis;valkey;database;cache;key-value;
StartupWMClass=redcrab
EOF
if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$desktop"
fi

# --- tarball: everything in one folder, usable without installing
name="redcrab-$VERSION-linux-$arch"
tree="$WORK/$name"
mkdir -p "$tree/share/applications" "$tree/share/icons"
install -m 755 "$binary" "$tree/redcrab"
install -m 644 "$desktop" "$tree/share/applications/redcrab.desktop"
cp -R "$icons/hicolor" "$tree/share/icons/"
install -m 644 LICENSE-MIT LICENSE-APACHE README.md "$tree/"
tarball="$DIST/$name.tar.gz"
tar -C "$WORK" -czf "$tarball" "$name"

# --- .deb
root="$WORK/deb"
install -D -m 755 "$binary" "$root/usr/bin/redcrab"
install -D -m 644 "$desktop" "$root/usr/share/applications/redcrab.desktop"
mkdir -p "$root/usr/share/icons"
cp -R "$icons/hicolor" "$root/usr/share/icons/"
doc="$root/usr/share/doc/redcrab"
mkdir -p "$doc"
{
    cat <<'EOF'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: redcrab
Source: https://github.com/muratmirgun/redcrab

Files: *
Copyright: 2026 Murat Mirgün Ercan
License: MIT or Apache-2.0

License: MIT
EOF
    sed -e 's/^$/./' -e 's/^/ /' LICENSE-MIT
    printf '\nLicense: Apache-2.0\n'
    sed -e 's/^$/./' -e 's/^/ /' LICENSE-APACHE
} >"$doc/copyright"
find "$root" -type d -exec chmod 755 {} +

# Libraries the binary links against, as dpkg-shlibdeps derives them, plus
# the ones GPUI opens at runtime with dlopen: Vulkan to draw, fontconfig to
# find fonts, and Wayland on Wayland sessions (X11 works without it).
mkdir -p "$WORK/shlibs/debian"
printf 'Source: redcrab\n\nPackage: redcrab\nArchitecture: any\n' >"$WORK/shlibs/debian/control"
depends=$(cd "$WORK/shlibs" && dpkg-shlibdeps -O "$root/usr/bin/redcrab" |
    sed -n 's/^shlibs:Depends=//p')
depends="$depends, libvulkan1, libfontconfig1"
installed_kb=$(du -sk "$root/usr" | cut -f1)

mkdir -p "$root/DEBIAN"
cat >"$root/DEBIAN/control" <<EOF
Package: redcrab
Version: $VERSION
Section: database
Priority: optional
Architecture: $deb_arch
Maintainer: Murat Mirgün Ercan <muratmirgunercan225@gmail.com>
Installed-Size: $installed_kb
Depends: $depends
Recommends: libwayland-client0, mesa-vulkan-drivers
Homepage: https://github.com/muratmirgun/redcrab
Description: fast, native Redis GUI
 redcrab is a GPU-rendered client for Redis and Valkey that stays responsive
 on keyspaces with millions of keys. It browses keys as a tree, edits values
 of every type, and connects to standalone servers, clusters and Sentinel,
 over TLS, Unix sockets or SSH tunnels.
EOF
deb="$DIST/redcrab_${VERSION}_${deb_arch}.deb"
dpkg-deb --root-owner-group --build "$root" "$deb" >&2
dpkg-deb --info "$deb" >&2

checksum "$tarball" "$deb"
echo "$tarball"
echo "$deb"
