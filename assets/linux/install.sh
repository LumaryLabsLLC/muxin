#!/bin/sh
# Installs Muxen for you (no root needed): the app in ~/.local/share/muxen-app,
# the `muxen` command in ~/.local/bin, and Muxen in your desktop's app menu.
# Run it again to update. `./install.sh --uninstall` takes it all out again
# (your settings in ~/.config/muxin stay).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
data=${XDG_DATA_HOME:-$HOME/.local/share}
dest=$data/muxen-app
bin=$HOME/.local/bin
entry=$data/applications/muxen.desktop
icon=$data/icons/hicolor/512x512/apps/muxen.png

if [ "${1:-}" = --uninstall ]; then
    [ "$(readlink "$bin/muxen" 2>/dev/null)" = "$dest/muxen" ] && rm -f "$bin/muxen"
    rm -rf "$dest" "$entry" "$icon"
    echo "Muxen is uninstalled. Your settings are still in ~/.config/muxin."
    exit 0
fi

rm -rf "$dest"
mkdir -p "$dest" "$bin" "$(dirname "$entry")" "$(dirname "$icon")"
cp -R "$here/muxen" "$here/agents" "$dest/"
ln -sf "$dest/muxen" "$bin/muxen"
cp "$here/muxen.png" "$icon"
sed "s|^Exec=.*|Exec=$dest/muxen|" "$here/muxen.desktop" > "$entry"
command -v update-desktop-database >/dev/null && update-desktop-database "$(dirname "$entry")" 2>/dev/null || true

echo "Muxen is installed: open it from your app menu, or run \`muxen\`."
case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "To use \`muxen\` in a terminal, add $bin to your PATH." ;;
esac
