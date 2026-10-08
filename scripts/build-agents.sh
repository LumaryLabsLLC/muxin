#!/bin/sh
# Builds muxen-agent for every platform Muxen can set up, into
# dist/agents/muxen-agent-<os>-<arch>. The app uploads the matching one
# to each machine (~/.muxen/bin) when it connects.
#
# Needs: rustup targets (x86_64-apple-darwin, aarch64-apple-darwin,
# x86_64-unknown-linux-musl, aarch64-unknown-linux-musl), cargo-zigbuild and zig
# for the Linux builds (static musl binaries that run on any distro).
set -eu
cd "$(dirname "$0")/.."
out=dist/agents
mkdir -p "$out"

build() { # <target> <name> <cargo subcommand>
  echo "▸ $2"
  cargo "$3" --release -p muxen-agent --target "$1" --quiet
  cp "target/$1/release/muxen-agent" "$out/muxen-agent-$2"
}

case "$(uname -s)" in
  Darwin)
    build aarch64-apple-darwin darwin-aarch64 build
    build x86_64-apple-darwin darwin-x86_64 build
    ;;
esac
build x86_64-unknown-linux-musl linux-x86_64 zigbuild
build aarch64-unknown-linux-musl linux-aarch64 zigbuild
ls -lh "$out"
