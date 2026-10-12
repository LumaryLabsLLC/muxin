#!/bin/sh
# Builds muxen-agent for every platform Muxen can set up, into
# dist/agents/muxen-agent-<os>-<arch>. The app uploads the matching one
# to each machine (~/.muxen/bin) when it connects.
#
# Needs: rustup targets (x86_64-apple-darwin, aarch64-apple-darwin,
# x86_64-unknown-linux-musl, aarch64-unknown-linux-musl, x86_64-pc-windows-gnu),
# cargo-zigbuild and zig for the Linux builds (static musl binaries that run on
# any distro) and the Windows one.
set -eu
cd "$(dirname "$0")/.."
out=dist/agents
mkdir -p "$out"

build() { # <target> <name> <cargo subcommand> [<.exe>]
  echo "▸ $2"
  cargo "$3" --release -p muxen-agent --target "$1" --quiet
  cp "target/$1/release/muxen-agent${4:-}" "$out/muxen-agent-$2${4:-}"
}

case "$(uname -s)" in
  Darwin)
    build aarch64-apple-darwin darwin-aarch64 build
    build x86_64-apple-darwin darwin-x86_64 build
    # Signed with a stable identity, a Mac keeps the agent's Screen Recording
    # and Accessibility permissions across updates (an ad-hoc signature
    # changes with every build, and the permissions go with it).
    identity=${MUXEN_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -n1)}
    if [ -n "$identity" ]; then
      for f in "$out"/muxen-agent-darwin-*; do
        codesign --force --timestamp=none --identifier com.lumarylabs.muxen-agent -s "$identity" "$f"
      done
      echo "▸ signed the macOS agents ($identity)"
    else
      echo "▸ no Developer ID certificate: the macOS agents are ad-hoc signed"
    fi
    ;;
esac
build x86_64-unknown-linux-musl linux-x86_64 zigbuild
build aarch64-unknown-linux-musl linux-aarch64 zigbuild
build x86_64-pc-windows-gnu windows-x86_64 zigbuild .exe
ls -lh "$out"
