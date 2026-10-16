#!/bin/sh
# Brings Muxin's AUR package (muxin-bin: `yay -S muxin-bin`) up to a
# published release: its PKGBUILD and .SRCINFO, with the release's version and
# the Linux tarballs' checksums from its SHA256SUMS.txt. It installs the app
# like the .pkg.tar.zst on the release does: /usr/lib/muxin, `muxin` and
# `muxen` in /usr/bin, the app menu entry and icon.
#
#   scripts/aur.sh 0.5.2           # write it in a clone of the AUR repo and commit
#   scripts/aur.sh 0.5.2 --push    # and push it (your AUR ssh key)
#
# AUR_DIR=<folder> uses a clone you already have. MUXIN_SUMS_FILE replaces
# the release's checksums (a local test). See RELEASING.md.
set -eu

cd "$(dirname "$0")/.."
usage="usage: scripts/aur.sh <version> [--push]"
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
x86_64=$(sha "Muxin-$version-linux-x86_64.tar.gz")
aarch64=$(sha "Muxin-$version-linux-aarch64.tar.gz")
[ -n "$x86_64" ] && [ -n "$aarch64" ] || {
    echo "error: v$version's SHA256SUMS.txt doesn't have the Linux tarballs" >&2
    exit 1
}
release=https://github.com/LumaryLabsLLC/muxin/releases/download/v$version
desc="All your machines in one app, for you and your AI agents"
depends="glibc libxcb libxkbcommon libxkbcommon-x11 vulkan-icd-loader"

aur=${AUR_DIR:-}
if [ -z "$aur" ]; then
    aur=$(mktemp -d)
    # An empty repository the first time: pushing to it creates the package.
    git clone --quiet ssh://aur@aur.archlinux.org/muxin-bin.git "$aur" 2>/dev/null || git -C "$aur" init --quiet -b master
    git -C "$aur" remote get-url origin >/dev/null 2>&1 || git -C "$aur" remote add origin ssh://aur@aur.archlinux.org/muxin-bin.git
fi

cat > "$aur/PKGBUILD" <<'EOF'
# Maintainer: Adib Hanna <adibhanna@gmail.com>
#
# Muxin (yay -S muxin-bin): written by scripts/aur.sh in
# https://github.com/LumaryLabsLLC/muxin from each release.

pkgname=muxin-bin
pkgver=@VERSION@
pkgrel=1
pkgdesc="@DESC@"
arch=('x86_64' 'aarch64')
url="https://muxin.dev"
license=('Apache-2.0')
depends=(@DEPENDS@)
optdepends=('wayland: to run in a Wayland session'
            'vulkan-driver: to draw its window (Mesa or your GPU maker'"'"'s)')
provides=('muxin')
conflicts=('muxin')
options=('!strip' '!debug')
source_x86_64=("Muxin-${pkgver}-linux-x86_64.tar.gz::https://github.com/LumaryLabsLLC/muxin/releases/download/v${pkgver}/Muxin-${pkgver}-linux-x86_64.tar.gz")
source_aarch64=("Muxin-${pkgver}-linux-aarch64.tar.gz::https://github.com/LumaryLabsLLC/muxin/releases/download/v${pkgver}/Muxin-${pkgver}-linux-aarch64.tar.gz")
sha256sums_x86_64=('@SHA_X86_64@')
sha256sums_aarch64=('@SHA_AARCH64@')

package() {
  cd "${srcdir}/Muxin"

  # The app, beside the agent builds it sets up on your machines.
  install -Dm755 muxin "${pkgdir}/usr/lib/muxin/muxin"
  install -dm755 "${pkgdir}/usr/lib/muxin/agents"
  install -m755 agents/* "${pkgdir}/usr/lib/muxin/agents/"

  # The command, and its name from before (Muxen).
  install -dm755 "${pkgdir}/usr/bin"
  ln -s /usr/lib/muxin/muxin "${pkgdir}/usr/bin/muxin"
  ln -s /usr/lib/muxin/muxin "${pkgdir}/usr/bin/muxen"

  install -Dm644 muxin.desktop "${pkgdir}/usr/share/applications/muxin.desktop"
  install -Dm644 muxin.png "${pkgdir}/usr/share/icons/hicolor/512x512/apps/muxin.png"
  install -Dm644 LICENSE NOTICE THIRD_PARTY_NOTICES.md -t "${pkgdir}/usr/share/licenses/${pkgname}/"
}
EOF
quoted=$(for d in $depends; do printf "'%s' " "$d"; done | sed 's/ $//')
sed -i.bak -e "s|@VERSION@|$version|" -e "s|@DESC@|$desc|" -e "s|@DEPENDS@|$quoted|" \
    -e "s|@SHA_X86_64@|$x86_64|" -e "s|@SHA_AARCH64@|$aarch64|" "$aur/PKGBUILD"
rm -f "$aur/PKGBUILD.bak"

# What `makepkg --printsrcinfo` writes for that PKGBUILD (no Arch tools needed).
{
    printf 'pkgbase = muxin-bin\n'
    printf '\tpkgdesc = %s\n\tpkgver = %s\n\tpkgrel = 1\n\turl = https://muxin.dev\n' "$desc" "$version"
    printf '\tarch = x86_64\n\tarch = aarch64\n\tlicense = Apache-2.0\n'
    for d in $depends; do printf '\tdepends = %s\n' "$d"; done
    printf '\toptdepends = wayland: to run in a Wayland session\n'
    printf "\toptdepends = vulkan-driver: to draw its window (Mesa or your GPU maker's)\n"
    printf '\tprovides = muxin\n\tconflicts = muxin\n\toptions = !strip\n\toptions = !debug\n'
    for a in x86_64 aarch64; do
        eval "sum=\$$a"
        printf '\tsource_%s = Muxin-%s-linux-%s.tar.gz::%s/Muxin-%s-linux-%s.tar.gz\n' "$a" "$version" "$a" "$release" "$version" "$a"
        printf '\tsha256sums_%s = %s\n' "$a" "$sum"
    done
    printf '\npkgname = muxin-bin\n'
} > "$aur/.SRCINFO"
echo "▸ $aur: muxin-bin $version-1"

cd "$aur"
git add PKGBUILD .SRCINFO
if git diff --cached --quiet; then
    echo "▸ already at $version"
    exit 0
fi
git commit --quiet -m "muxin-bin $version-1"
if [ -n "$push" ]; then
    git push --quiet origin HEAD:master
    echo "▸ pushed: yay -S muxin-bin gets it"
else
    echo "Push it from $aur: git push origin HEAD:master"
fi
