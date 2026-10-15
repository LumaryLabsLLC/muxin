# Deletes a VM Muxin made here: its container and its folder (a Windows
# VM's disk). Prepended: OR_NAME (always "muxen-...").
set -u
PATH="$PATH:/usr/local/bin:/opt/homebrew/bin"
case "$OR_NAME" in muxen-*) ;; *) echo "error: $OR_NAME isn't a container Muxin made." >&2; exit 1 ;; esac
if docker inspect "$OR_NAME" >/dev/null 2>&1; then
  echo "▸ Removing the container $OR_NAME"
  docker rm -f -v "$OR_NAME" >/dev/null || { echo "error: Docker couldn't remove $OR_NAME." >&2; exit 1; }
fi
if [ -d "$HOME/.muxen/vms/$OR_NAME" ]; then
  echo "▸ Deleting its disk ($(du -sh "$HOME/.muxen/vms/$OR_NAME" 2>/dev/null | cut -f1))"
  rm -rf "$HOME/.muxen/vms/$OR_NAME"
fi
echo "✓ Gone"
