# Stops a workspace's background commands and deletes its folder.
# Prepended: OR_DIR OR_ID OR_PIDS (space separated) OR_KEEP, and for docker
# workspaces OR_CONTAINER and OR_IMAGE.
if [ -n "$OR_CONTAINER" ]; then
  if command -v docker >/dev/null 2>&1; then
    if docker rm -f "$OR_CONTAINER" >/dev/null 2>&1; then echo "Removed container $OR_CONTAINER"; else echo "Container $OR_CONTAINER was already gone"; fi
  else
    echo "warning: docker isn't available, so container $OR_CONTAINER couldn't be removed"
  fi
fi
for or_pid in $OR_PIDS; do
  if kill -0 "$or_pid" 2>/dev/null; then
    or_kill_tree "$or_pid" TERM
    echo "Stopped background process $or_pid"
  fi
done
if [ "$OR_KEEP" = 1 ]; then echo "Kept the files in $OR_DIR"; exit 0; fi
# Only ever delete a workspace folder.
case "$OR_DIR" in */"$OR_ID") ;; *) echo "error: refusing to delete $OR_DIR" >&2; exit 3 ;; esac
if [ -d "$OR_DIR" ]; then
  if ! rm -rf "$OR_DIR" 2>/dev/null; then
    # Files a container created as root (e.g. through a shared docker socket):
    # delete them from a throwaway container of the same image.
    if [ -n "$OR_IMAGE" ] && command -v docker >/dev/null 2>&1; then
      docker run --rm --user 0:0 --entrypoint sh -v "${OR_DIR%/*}:/parent" "$OR_IMAGE" -c "rm -rf /parent/$OR_ID" >/dev/null 2>&1
    fi
    [ ! -e "$OR_DIR" ] || { echo "error: couldn't delete $OR_DIR" >&2; exit 3; }
  fi
  echo "Deleted $OR_DIR"
else
  echo "$OR_DIR was already gone"
fi
