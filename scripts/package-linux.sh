#!/bin/sh
# Builds Muxin for Linux and packs it: dist/Muxin-<version>-linux-<arch>.tar.gz,
# with the app, the agent builds it sets up on each machine (agents/, made by
# scripts/build-agents.sh), a desktop entry and an install.sh that puts it in
# ~/.local for the user.
#
# Needs: Rust, and GPUI's libraries (Debian/Ubuntu: libxkbcommon-dev
# libxkbcommon-x11-dev libwayland-dev libvulkan-dev libfontconfig-dev
# libx11-xcb-dev libxcb1-dev). Build on the oldest distro you support: the app
# needs the glibc it was built with, or newer.
#
#   --skip-agents  use the agent builds already in dist/agents.
set -eu

cd "$(dirname "$0")/.."
skip_agents=
[ "${1:-}" = --skip-agents ] && skip_agents=1
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
arch=$(uname -m)

cargo build --release --locked
[ -n "$skip_agents" ] || ./scripts/build-agents.sh >/dev/null
ls dist/agents/muxen-agent-* >/dev/null 2>&1 || { echo "error: no agent builds in dist/agents" >&2; exit 1; }

root=dist/linux
out=$root/Muxin
rm -rf "$root"
mkdir -p "$out/agents"
cp target/release/muxin "$out/"
cp dist/agents/muxen-agent-* "$out/agents/"
cp assets/brand/icon-512.png "$out/muxin.png"
cp assets/linux/muxin.desktop assets/linux/install.sh "$out/"
cp README.md LICENSE NOTICE THIRD_PARTY_NOTICES.md "$out/"

tarball=dist/Muxin-$version-linux-$arch.tar.gz
tar -czf "$tarball" -C "$root" Muxin
echo "Built $tarball ($(du -h "$tarball" | cut -f1))"
