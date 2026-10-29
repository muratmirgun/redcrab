#!/usr/bin/env bash
# Writes redcrab's files in the Homebrew tap (github.com/muratmirgun/homebrew-tap)
# for a published release:
#   Casks/redcrab.rb          the desktop app, from the macOS disk image
#   Formula/redcrab-tui.rb    the terminal app, from the macOS and Linux tarballs
#
#   scripts/homebrew.sh <version> <SHA256SUMS> <tap checkout>
#
# The checksums are the release's SHA256SUMS; check its signature first, as
# .github/workflows/homebrew.yml does. A version older than the tap's is left
# alone, so publishing an old draft never takes the tap back.

set -euo pipefail

usage="usage: scripts/homebrew.sh <version> <SHA256SUMS> <tap checkout>"
version=${1:?$usage}
sums=${2:?$usage}
tap=${3:?$usage}

if ! [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "not a release version: $version" >&2
    exit 1
fi

cask="$tap/Casks/redcrab.rb"
formula="$tap/Formula/redcrab-tui.rb"

current=$(sed -n 's/^  version "\(.*\)"$/\1/p' "$cask" 2>/dev/null || true)
if [ -n "$current" ] &&
    [ "$(printf '%s\n' "$current" "$version" | sort -V | tail -n 1)" != "$version" ]; then
    echo "the tap already has redcrab $current, newer than $version; left alone" >&2
    exit 0
fi

# The SHA-256 that SHA256SUMS gives for the release file $1.
sha() {
    local hex
    hex=$(awk -v file="$1" '$2 == file || $2 == "*" file { print $1 }' "$sums")
    if ! [[ $hex =~ ^[0-9a-f]{64}$ ]]; then
        echo "$sums has no SHA-256 for $1" >&2
        exit 1
    fi
    echo "$hex"
}

# Assigned first: a failure inside the heredocs below would go unnoticed.
dmg=$(sha "redcrab-$version-macos-universal.dmg")
tui_macos=$(sha "redcrab-tui-$version-macos-universal.tar.gz")
tui_arm=$(sha "redcrab-tui-$version-linux-aarch64.tar.gz")
tui_intel=$(sha "redcrab-tui-$version-linux-x86_64.tar.gz")
download="https://github.com/muratmirgun/redcrab/releases/download/v$version"

mkdir -p "$tap/Casks" "$tap/Formula"

cat >"$cask" <<EOF
# Written by redcrab's scripts/homebrew.sh for each release; edit it there.
cask "redcrab" do
  version "$version"
  sha256 "$dmg"

  url "https://github.com/muratmirgun/redcrab/releases/download/v#{version}/redcrab-#{version}-macos-universal.dmg"
  name "redcrab"
  desc "Fast native Redis and Valkey client"
  homepage "https://github.com/muratmirgun/redcrab"

  livecheck do
    url :url
    strategy :github_latest
  end

  # It asks before installing a new release, checked against its own key.
  auto_updates true
  depends_on :macos

  app "redcrab.app"

  zap trash: [
    "~/Library/Application Support/redcrab",
    "~/Library/Preferences/io.github.muratmirgun.redcrab.plist",
  ]

  caveats <<~EOS
    redcrab is not notarized yet, so macOS stops it the first time it opens.
    Open it once, then choose Open Anyway in System Settings > Privacy & Security,
    or run:
      xattr -dr com.apple.quarantine /Applications/redcrab.app
  EOS
end
EOF

cat >"$formula" <<EOF
# Written by redcrab's scripts/homebrew.sh for each release; edit it there.
class RedcrabTui < Formula
  desc "Fast Redis and Valkey client for the terminal"
  homepage "https://github.com/muratmirgun/redcrab"
  license any_of: ["MIT", "Apache-2.0"]

  # One universal binary; Homebrew takes a url only per architecture.
  on_macos do
    on_arm do
      url "$download/redcrab-tui-$version-macos-universal.tar.gz"
      sha256 "$tui_macos"
    end
    on_intel do
      url "$download/redcrab-tui-$version-macos-universal.tar.gz"
      sha256 "$tui_macos"
    end
  end

  on_linux do
    on_arm do
      url "$download/redcrab-tui-$version-linux-aarch64.tar.gz"
      sha256 "$tui_arm"
    end
    on_intel do
      url "$download/redcrab-tui-$version-linux-x86_64.tar.gz"
      sha256 "$tui_intel"
    end
  end

  def install
    bin.install "redcrab-tui"
  end

  test do
    assert_equal "redcrab-tui #{version}", shell_output("#{bin}/redcrab-tui --version").strip
  end
end
EOF

echo "$cask"
echo "$formula"
