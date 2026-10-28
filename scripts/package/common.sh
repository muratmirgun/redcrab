# shellcheck shell=bash
# Shared by the packaging scripts; sourced, not run.
#
# Every script runs from the repository root, writes its packages to dist/
# and prints their paths. They behave the same on a developer machine and in
# .github/workflows/release.yml, so a failing release step can be reproduced
# by running the script locally.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

# The workspace version, e.g. 0.1.0 (the first `version = ` line of the root
# Cargo.toml is [workspace.package]'s).
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[ -n "$VERSION" ] || {
    echo "could not read the version from Cargo.toml" >&2
    exit 1
}
DIST="$PWD/dist"
# shellcheck disable=SC2034 # used by the scripts that source this file
TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
mkdir -p "$DIST"

# A scratch directory removed when the script exits.
WORK=$(mktemp -d)
trap 'rm -rf "${WORK:?}"' EXIT

log() { printf '==> %s\n' "$*" >&2; }

# Writes a SHA-256 line for each given file next to it, as `<file>.sha256`,
# in the format `sha256sum -c` reads.
checksum() {
    local file
    for file in "$@"; do
        (
            cd "$(dirname "$file")"
            if command -v sha256sum >/dev/null 2>&1; then
                sha256sum "$(basename "$file")"
            else
                shasum -a 256 "$(basename "$file")"
            fi
        ) >"$file.sha256"
    done
}

# The release profile's fat LTO links the whole GUI as one LLVM module, which
# needs more than 6 GiB on Linux. On machines with less than 12 GiB (GitHub's
# 7 GB macOS runners, small VMs) fall back to thin LTO, with a warning, unless
# the caller chose an LTO mode already.
fit_lto_to_memory() {
    [ -z "${CARGO_PROFILE_RELEASE_LTO:-}" ] || return 0
    local mib
    case "$(uname -s)" in
        Darwin) mib=$(($(sysctl -n hw.memsize) / 1048576)) ;;
        Linux) mib=$(awk '/^MemTotal:/ { print int($2 / 1024) }' /proc/meminfo) ;;
        *) return 0 ;;
    esac
    if [ "$mib" -lt 12288 ]; then
        echo "::warning title=Thin LTO::${mib} MiB of memory is too little for fat LTO; building with thin LTO." >&2
        export CARGO_PROFILE_RELEASE_LTO=thin
    fi
}
fit_lto_to_memory
