#!/usr/bin/env bash
# Builds the Linux packages for this machine's architecture: the desktop
# app with the terminal app (redcrab-tui), and the terminal app alone, for
# servers, which then need none of the GUI's libraries:
#   dist/redcrab-<version>-linux-<arch>.tar.gz       binaries, .desktop file, icons
#   dist/redcrab_<version>_<debarch>.deb             for Debian and Ubuntu
#   dist/redcrab-tui-<version>-linux-<arch>.tar.gz   redcrab-tui alone
#   dist/redcrab-tui_<version>_<debarch>.deb         redcrab-tui alone
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
cargo build -p redcrab-gui -p redcrab-tui --release --locked
binary="$TARGET_DIR/release/redcrab"
tui="$TARGET_DIR/release/redcrab-tui"
answers_version "$binary"
answers_version "$tui" redcrab-tui

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
install -m 755 "$tui" "$tree/redcrab-tui"
install -m 644 "$desktop" "$tree/share/applications/redcrab.desktop"
cp -R "$icons/hicolor" "$tree/share/icons/"
install -m 644 LICENSE-MIT LICENSE-APACHE README.md "$tree/"
tarball="$DIST/$name.tar.gz"
tar -C "$WORK" -czf "$tarball" "$name"

# --- .deb
root="$WORK/deb"
install -D -m 755 "$binary" "$root/usr/bin/redcrab"
install -D -m 755 "$tui" "$root/usr/bin/redcrab-tui"
install -D -m 644 "$desktop" "$root/usr/share/applications/redcrab.desktop"
mkdir -p "$root/usr/share/icons"
cp -R "$icons/hicolor" "$root/usr/share/icons/"
# The machine-readable copyright file of the .deb named $1, in its root $2.
deb_copyright() {
    local doc="$2/usr/share/doc/$1"
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
    find "$2" -type d -exec chmod 755 {} +
}

# The libraries the given binaries link against, as dpkg-shlibdeps derives
# them, for a Depends field.
shlib_depends() {
    local dir="$WORK/shlibs"
    mkdir -p "$dir/debian"
    printf 'Source: redcrab\n\nPackage: redcrab\nArchitecture: any\n' >"$dir/debian/control"
    (cd "$dir" && dpkg-shlibdeps -O "$@" | sed -n 's/^shlibs:Depends=//p')
}

deb_copyright redcrab "$root"
# Plus the libraries GPUI opens at runtime with dlopen: Vulkan to draw,
# fontconfig to find fonts, and Wayland on Wayland sessions (X11 works
# without it).
depends="$(shlib_depends "$root/usr/bin/redcrab" "$root/usr/bin/redcrab-tui"), libvulkan1, libfontconfig1"
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
Provides: redcrab-tui
Conflicts: redcrab-tui
Replaces: redcrab-tui
Homepage: https://github.com/muratmirgun/redcrab
Description: fast, native Redis GUI
 redcrab is a GPU-rendered client for Redis and Valkey that stays responsive
 on keyspaces with millions of keys. It browses keys as a tree, edits values
 of every type, and connects to standalone servers, clusters and Sentinel,
 over TLS, Unix sockets or SSH tunnels. redcrab-tui does the same in a
 terminal.
EOF
deb="$DIST/redcrab_${VERSION}_${deb_arch}.deb"
dpkg-deb --root-owner-group --build "$root" "$deb" >&2
dpkg-deb --info "$deb" >&2

# --- the terminal app alone: what a server needs, none of the GUI's
# libraries. The desktop .deb holds redcrab-tui too, so it takes this one's
# place (Provides, Conflicts, Replaces above).
tui_name="redcrab-tui-$VERSION-linux-$arch"
tui_tree="$WORK/$tui_name"
mkdir -p "$tui_tree"
install -m 755 "$tui" "$tui_tree/redcrab-tui"
install -m 644 LICENSE-MIT LICENSE-APACHE README.md "$tui_tree/"
tui_tarball="$DIST/$tui_name.tar.gz"
tar -C "$WORK" -czf "$tui_tarball" "$tui_name"

tui_root="$WORK/tui-deb"
install -D -m 755 "$tui" "$tui_root/usr/bin/redcrab-tui"
deb_copyright redcrab-tui "$tui_root"
tui_depends=$(shlib_depends "$tui_root/usr/bin/redcrab-tui")
mkdir -p "$tui_root/DEBIAN"
cat >"$tui_root/DEBIAN/control" <<EOF
Package: redcrab-tui
Version: $VERSION
Section: database
Priority: optional
Architecture: $deb_arch
Maintainer: Murat Mirgün Ercan <muratmirgunercan225@gmail.com>
Installed-Size: $(du -sk "$tui_root/usr" | cut -f1)
Depends: $tui_depends
Homepage: https://github.com/muratmirgun/redcrab
Description: fast Redis and Valkey client for the terminal
 redcrab-tui browses keys as a tree, edits values of every type, runs
 commands in a console, and shows the server's figures, a memory analysis
 and Pub/Sub, in a terminal: for a server reached over SSH or a machine
 without a GPU. It reads the connections saved by the redcrab desktop app.
EOF
tui_deb="$DIST/redcrab-tui_${VERSION}_${deb_arch}.deb"
dpkg-deb --root-owner-group --build "$tui_root" "$tui_deb" >&2
dpkg-deb --info "$tui_deb" >&2

checksum "$tarball" "$deb" "$tui_tarball" "$tui_deb"
echo "$tarball"
echo "$deb"
echo "$tui_tarball"
echo "$tui_deb"
