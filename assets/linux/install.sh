#!/bin/sh
# Installs Muxin for you (no root needed): the app in ~/.local/share/muxin-app,
# the `muxin` command in ~/.local/bin (and `muxen`, its name from before), and
# Muxin in your desktop's app menu. Run it again to update; it replaces an
# install from when it was called Muxen. `./install.sh --uninstall` takes it
# all out again (your settings in ~/.config/muxin stay).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
data=${XDG_DATA_HOME:-$HOME/.local/share}
dest=$data/muxin-app
bin=$HOME/.local/bin
entry=$data/applications/muxin.desktop
icon=$data/icons/hicolor/512x512/apps/muxin.png

# From before the rename: the app's folder, its menu entry and icon, and the
# muxen command when it ran that copy.
old=$data/muxen-app
[ "$(readlink "$bin/muxen" 2>/dev/null)" = "$old/muxen" ] && rm -f "$bin/muxen"
rm -rf "$old" "$data/applications/muxen.desktop" "$data/icons/hicolor/512x512/apps/muxen.png"

if [ "${1:-}" = --uninstall ]; then
    for name in muxin muxen; do
        [ "$(readlink "$bin/$name" 2>/dev/null)" = "$dest/muxin" ] && rm -f "$bin/$name"
    done
    rm -rf "$dest" "$entry" "$icon"
    echo "Muxin is uninstalled. Your settings are still in ~/.config/muxin."
    exit 0
fi

rm -rf "$dest"
mkdir -p "$dest" "$bin" "$(dirname "$entry")" "$(dirname "$icon")"
cp -R "$here/muxin" "$here/agents" "$dest/"
ln -sf "$dest/muxin" "$bin/muxin"
# muxen too, unless something else has that name.
case "$(readlink "$bin/muxen" 2>/dev/null)" in
    */muxin | */muxen) ln -sf "$dest/muxin" "$bin/muxen" ;;
    "") [ -e "$bin/muxen" ] || ln -s "$dest/muxin" "$bin/muxen" ;;
esac
cp "$here/muxin.png" "$icon"
sed "s|^Exec=.*|Exec=$dest/muxin|" "$here/muxin.desktop" > "$entry"
command -v update-desktop-database >/dev/null && update-desktop-database "$(dirname "$entry")" 2>/dev/null || true

echo "Muxin is installed: open it from your app menu, or run \`muxin\`."
case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "To use \`muxin\` in a terminal, add $bin to your PATH." ;;
esac
