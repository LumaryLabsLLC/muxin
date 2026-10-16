#!/bin/sh
# Makes Muxin's Linux packages from what scripts/package-linux.sh staged in
# dist/linux/Muxin, for this machine's architecture, next to the tarball:
#
#   dist/Muxin-<version>-linux-<arch>.deb        Debian, Ubuntu
#   dist/Muxin-<version>-linux-<arch>.rpm        Fedora, openSUSE, RHEL 10
#   dist/Muxin-<version>-linux-<arch>.pkg.tar.zst  Arch
#   dist/Muxin-<version>-linux-<arch>.AppImage   any distribution
#
# The packages put the app in /usr/lib/muxin, `muxin` (and `muxen`, its name
# from before) in /usr/bin, and Muxin in the app menu; they ask for the
# libraries it loads (xcb, xkbcommon, Vulkan) and glibc 2.35 or newer. The
# AppImage carries the same folder and runs without FUSE 2.
#
# nfpm, appimagetool and the AppImage runtime are pinned and checked against
# their SHA-256; they're fetched into target/tools the first time.
set -eu

cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
arch=$(uname -m)
stage=dist/linux/Muxin
[ -x "$stage/muxin" ] || { echo "error: no app in $stage: run scripts/package-linux.sh first" >&2; exit 1; }

nfpm_version=2.47.0
appimagetool_version=1.9.1
runtime_version=20251108
case $arch in
    x86_64)
        goarch=amd64
        nfpm_sha=0660ca602b2d2d2ae4781a06c692b3eeb9d437ffea05b831d76e41f4a3188783
        appimagetool_sha=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
        runtime_sha=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
        nfpm_file=nfpm_${nfpm_version}_Linux_x86_64.tar.gz ;;
    aarch64)
        goarch=arm64
        nfpm_sha=1c0f5f2999b9a974bfb04fdb0cc3306096de530ac5dbb25d739cc5f5219c919c
        appimagetool_sha=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
        runtime_sha=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
        nfpm_file=nfpm_${nfpm_version}_Linux_arm64.tar.gz ;;
    *) echo "error: no packages for $arch" >&2; exit 1 ;;
esac

tools=target/tools
mkdir -p "$tools"
# fetch <url> <file> <sha256>: downloads once, and only keeps a file that matches.
fetch() {
    if [ -f "$2" ] && echo "$3  $2" | sha256sum -c --status; then
        return
    fi
    curl -fsSL --retry 3 -o "$2.part" "$1"
    echo "$3  $2.part" | sha256sum -c --status || { rm -f "$2.part"; echo "error: $1 doesn't match its checksum" >&2; exit 1; }
    mv "$2.part" "$2"
}
fetch "https://github.com/goreleaser/nfpm/releases/download/v$nfpm_version/$nfpm_file" "$tools/$nfpm_file" "$nfpm_sha"
fetch "https://github.com/AppImage/appimagetool/releases/download/$appimagetool_version/appimagetool-$arch.AppImage" "$tools/appimagetool-$arch.AppImage" "$appimagetool_sha"
fetch "https://github.com/AppImage/type2-runtime/releases/download/$runtime_version/runtime-$arch" "$tools/runtime-$arch" "$runtime_sha"
tar -xzf "$tools/$nfpm_file" -C "$tools" nfpm
chmod +x "$tools/nfpm" "$tools/appimagetool-$arch.AppImage"

# The .deb, .rpm and Arch package, from one description.
config=dist/linux/nfpm.yaml
cat > "$config" <<EOF
name: muxin
arch: $goarch
platform: linux
version: $version
release: 1
section: devel
priority: optional
maintainer: Lumary Labs <contact@lumarylabs.com>
vendor: Lumary Labs LLC
homepage: https://muxin.dev
license: Apache-2.0
description: |-
  All your machines in one app, for you and your AI agents.
  Muxin runs AI coding agents on your Macs, Linux servers and Windows PCs,
  shows their screens, moves files between them and looks after their
  services, from one window or from your agents through MCP.
contents:
  - src: $stage/muxin
    dst: /usr/lib/muxin/muxin
    file_info: { mode: 0755 }
  - src: $stage/agents/
    dst: /usr/lib/muxin/agents/
    file_info: { mode: 0755 }
  - src: /usr/lib/muxin/muxin
    dst: /usr/bin/muxin
    type: symlink
  - src: /usr/lib/muxin/muxin
    dst: /usr/bin/muxen
    type: symlink
  - src: $stage/muxin.desktop
    dst: /usr/share/applications/muxin.desktop
    file_info: { mode: 0644 }
  - src: assets/brand/icon-128.png
    dst: /usr/share/icons/hicolor/128x128/apps/muxin.png
    file_info: { mode: 0644 }
  - src: assets/brand/icon-512.png
    dst: /usr/share/icons/hicolor/512x512/apps/muxin.png
    file_info: { mode: 0644 }
  - src: LICENSE
    dst: /usr/share/doc/muxin/LICENSE
    packager: deb
  - src: NOTICE
    dst: /usr/share/doc/muxin/NOTICE
    packager: deb
  - src: THIRD_PARTY_NOTICES.md
    dst: /usr/share/doc/muxin/THIRD_PARTY_NOTICES.md
    packager: deb
  - src: LICENSE
    dst: /usr/share/licenses/muxin/LICENSE
    packager: rpm
  - src: NOTICE
    dst: /usr/share/licenses/muxin/NOTICE
    packager: rpm
  - src: THIRD_PARTY_NOTICES.md
    dst: /usr/share/licenses/muxin/THIRD_PARTY_NOTICES.md
    packager: rpm
  - src: LICENSE
    dst: /usr/share/licenses/muxin/LICENSE
    packager: archlinux
  - src: NOTICE
    dst: /usr/share/licenses/muxin/NOTICE
    packager: archlinux
  - src: THIRD_PARTY_NOTICES.md
    dst: /usr/share/licenses/muxin/THIRD_PARTY_NOTICES.md
    packager: archlinux
overrides:
  deb:
    depends:
      - libc6 (>= 2.35)
      - libxcb1
      - libxkbcommon0
      - libxkbcommon-x11-0
      - libvulkan1
    recommends:
      - mesa-vulkan-drivers
      - libwayland-client0
  rpm:
    depends:
      - libc.so.6(GLIBC_2.35)(64bit)
      - libxcb.so.1()(64bit)
      - libxkbcommon.so.0()(64bit)
      - libxkbcommon-x11.so.0()(64bit)
      - libvulkan.so.1()(64bit)
    recommends:
      - mesa-vulkan-drivers
  archlinux:
    depends:
      - glibc
      - libxcb
      - libxkbcommon
      - libxkbcommon-x11
      - vulkan-icd-loader
EOF
for packager in deb rpm archlinux; do
    case $packager in
        deb) ext=deb ;;
        rpm) ext=rpm ;;
        archlinux) ext=pkg.tar.zst ;;
    esac
    out=dist/Muxin-$version-linux-$arch.$ext
    "$tools/nfpm" package --config "$config" --packager "$packager" --target "$out" >/dev/null
    echo "Built $out ($(du -h "$out" | cut -f1))"
done

# The AppImage: the same folder, run by AppRun.
appdir=dist/linux/Muxin.AppDir
rm -rf "$appdir"
mkdir -p "$appdir/usr/lib/muxin" "$appdir/usr/share/applications" "$appdir/usr/share/icons/hicolor/512x512/apps" "$appdir/usr/share/doc/muxin"
cp -R "$stage/muxin" "$stage/agents" "$appdir/usr/lib/muxin/"
cp "$stage/muxin.desktop" "$appdir/muxin.desktop"
cp "$stage/muxin.desktop" "$appdir/usr/share/applications/muxin.desktop"
cp assets/brand/icon-512.png "$appdir/muxin.png"
cp assets/brand/icon-512.png "$appdir/usr/share/icons/hicolor/512x512/apps/muxin.png"
ln -s muxin.png "$appdir/.DirIcon"
cp LICENSE NOTICE THIRD_PARTY_NOTICES.md "$appdir/usr/share/doc/muxin/"
cat > "$appdir/AppRun" <<'EOF'
#!/bin/sh
# Muxin's AppImage: the app lives in usr/lib/muxin, beside its agents.
here=$(dirname "$(readlink -f "$0")")
exec "$here/usr/lib/muxin/muxin" "$@"
EOF
chmod +x "$appdir/AppRun"
out=dist/Muxin-$version-linux-$arch.AppImage
# --appimage-extract-and-run: appimagetool is an AppImage itself, and build
# machines (containers, CI) often have no FUSE.
ARCH=$arch "$tools/appimagetool-$arch.AppImage" --appimage-extract-and-run --no-appstream \
    --runtime-file "$tools/runtime-$arch" "$appdir" "$out" > dist/linux/appimagetool.log 2>&1 \
    || { cat dist/linux/appimagetool.log >&2; echo "error: appimagetool failed" >&2; exit 1; }
chmod +x "$out"
echo "Built $out ($(du -h "$out" | cut -f1))"
