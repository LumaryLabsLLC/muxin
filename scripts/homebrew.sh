#!/bin/sh
# Brings Muxin's Homebrew cask up to a published release: Casks/muxin.rb in
# LumaryLabsLLC/homebrew-tap, with the release's version and the checksums
# from its SHA256SUMS.txt. One cask for macOS (the app, in Applications) and
# Linux (the app in Homebrew's folder, with an entry in the app menu); both
# put `muxin` (and `muxen`, its name from before) in the PATH.
#
#   scripts/homebrew.sh 0.5.1           # write it in a clone of the tap and commit
#   scripts/homebrew.sh 0.5.1 --push    # and push it
#
# HOMEBREW_TAP_DIR=<folder> uses a clone you already have. For a local test,
# MUXIN_CASK_URL replaces the download folder and MUXIN_SUMS_FILE the
# checksums. See RELEASING.md.
set -eu

cd "$(dirname "$0")/.."
usage="usage: scripts/homebrew.sh <version> [--push]"
version=${1:?$usage}
version=${version#v}
push=
case "${2:-}" in
    --push) push=1 ;;
    "") ;;
    *) echo "$usage" >&2; exit 2 ;;
esac

sums=$(if [ -n "${MUXIN_SUMS_FILE:-}" ]; then cat "$MUXIN_SUMS_FILE"; else gh release download "v$version" -p SHA256SUMS.txt -O -; fi)
sha() { echo "$sums" | awk -v f="$1" '$2 == f || $2 == "*" f { print $1 }'; }
macos=$(sha "Muxin-$version-macos.dmg")
x86_64=$(sha "Muxin-$version-linux-x86_64.tar.gz")
aarch64=$(sha "Muxin-$version-linux-aarch64.tar.gz")
[ -n "$macos" ] && [ -n "$x86_64" ] && [ -n "$aarch64" ] || {
    echo "error: v$version's SHA256SUMS.txt doesn't have the macOS and Linux files" >&2
    exit 1
}
base=${MUXIN_CASK_URL:-'https://github.com/LumaryLabsLLC/muxin/releases/download/v#{version}'}

tap=${HOMEBREW_TAP_DIR:-}
if [ -z "$tap" ]; then
    tap=$(mktemp -d)
    gh repo clone LumaryLabsLLC/homebrew-tap "$tap" -- --quiet
fi
mkdir -p "$tap/Casks"
cat > "$tap/Casks/muxin.rb" <<EOF
cask "muxin" do
  arch arm: "aarch64", intel: "x86_64"

  version "$version"
  sha256 arm:          "$macos",
         intel:        "$macos",
         arm64_linux:  "$aarch64",
         x86_64_linux: "$x86_64"

  on_macos do
    url "$base/Muxin-#{version}-macos.dmg"

    app "Muxin.app"
    binary "#{appdir}/Muxin.app/Contents/MacOS/muxin"
    binary "#{appdir}/Muxin.app/Contents/MacOS/muxin", target: "muxen"
  end
  on_linux do
    url "$base/Muxin-#{version}-linux-#{arch}.tar.gz"

    binary "Muxin/muxin"
    binary "Muxin/muxin", target: "muxen"
    artifact "Muxin/muxin.desktop", target: "~/.local/share/applications/muxin.desktop"

    preflight_steps do
      inreplace "Muxin/muxin.desktop", "Exec=muxin", "Exec={{HOMEBREW_PREFIX}}/bin/muxin"
      inreplace "Muxin/muxin.desktop", "Icon=muxin", "Icon={{staged_path}}/Muxin/muxin.png"
    end
  end

  name "Muxin"
  desc "All your machines in one app, for you and your AI agents"
  homepage "https://muxin.dev/"

  livecheck do
    url :url
    strategy :github_latest
  end

  zap trash: [
    "~/.cache/muxen",
    "~/.config/muxen",
    "~/.config/muxin",
    "~/.muxen",
    "~/Library/Caches/muxen",
    "~/Library/Preferences/com.lumarylabs.muxen.plist",
  ]
end
EOF
echo "▸ $tap/Casks/muxin.rb: $version"

cd "$tap"
if git diff --quiet -- Casks/muxin.rb && git ls-files --error-unmatch Casks/muxin.rb >/dev/null 2>&1; then
    echo "▸ already at $version"
    exit 0
fi
git add Casks/muxin.rb
git commit --quiet -m "muxin $version"
if [ -n "$push" ]; then
    git push --quiet origin HEAD
    echo "▸ pushed: brew upgrade --cask muxin gets it"
else
    echo "Push it from $tap: git push origin HEAD"
fi
