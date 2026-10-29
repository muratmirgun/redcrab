#!/usr/bin/env bash
# Builds, with the licenses:
#   dist/redcrab-<version>-windows-<arch>.zip       redcrab.exe (icon and version
#       information embedded by crates/redcrab-gui/build.rs) and the terminal
#       app, redcrab-tui.exe
#   dist/redcrab-tui-<version>-windows-<arch>.zip   redcrab-tui.exe alone
# Runs in Git Bash on Windows (the release workflow's shell). The executables
# are not code-signed, so SmartScreen may warn on first start.

source "$(dirname "$0")/common.sh"

case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

log "building"
cargo build -p redcrab-gui -p redcrab-tui --release --locked
exe="$TARGET_DIR/release/redcrab.exe"
tui="$TARGET_DIR/release/redcrab-tui.exe"
answers_version "$tui" redcrab-tui
# A GUI program on Windows, yet it answers --version on the pipe it is
# given, as the updater needs.
answers_version "$exe"

# Zips the folder $1 with the given files into dist/$1.zip, and prints its
# path.
package() {
    local folder="$WORK/$1" zip="$DIST/$1.zip"
    shift
    mkdir -p "$folder"
    cp "$@" LICENSE-MIT LICENSE-APACHE README.md "$folder/"
    rm -f "$zip"
    # Compress-Archive ships with every Windows; zip(1) does not.
    powershell.exe -NoProfile -NonInteractive -Command \
        "Compress-Archive -Path '$(cygpath -w "$folder")' -DestinationPath '$(cygpath -w "$zip")'"
    checksum "$zip"
    echo "$zip"
}

package "redcrab-$VERSION-windows-$arch" "$exe" "$tui"
package "redcrab-tui-$VERSION-windows-$arch" "$tui"
