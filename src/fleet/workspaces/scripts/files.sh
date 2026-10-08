# Reads, writes or lists files in a workspace. Prepended: OR_ACTION OR_DIR OR_REL
# OR_MAX OR_APPEND OR_B64 OR_DEPTH. Paths were validated to stay inside the workspace.

cd "$OR_DIR" 2>/dev/null || { echo "error: the workspace folder is gone: $OR_DIR" >&2; exit 97; }
or_size() { ls -ldn "$1" 2>/dev/null | awk '{ print $5 }'; }

case "$OR_ACTION" in
read)
  [ -e "$OR_REL" ] || { echo "error: there's no file $OR_REL in the workspace" >&2; exit 3; }
  [ -d "$OR_REL" ] && { echo "error: $OR_REL is a folder" >&2; exit 3; }
  printf 'size=%s\n' "$(or_size "$OR_REL")"
  printf 'data='
  head -c "$OR_MAX" "$OR_REL" | base64 | tr -d '\n\r'
  printf '\n'
  ;;
write)
  case "$OR_REL" in */*) mkdir -p "${OR_REL%/*}" || { echo "error: couldn't create the folder for $OR_REL" >&2; exit 3; } ;; esac
  if printf 'YQ==' | base64 -d >/dev/null 2>&1; then or_dec='base64 -d'; else or_dec='base64 -D'; fi
  if [ "$OR_APPEND" = 1 ]; then
    printf '%s' "$OR_B64" | $or_dec >> "$OR_REL" || { echo "error: couldn't write $OR_REL" >&2; exit 3; }
  else
    printf '%s' "$OR_B64" | $or_dec > "$OR_REL" || { echo "error: couldn't write $OR_REL" >&2; exit 3; }
  fi
  printf 'size=%s\n' "$(or_size "$OR_REL")"
  ;;
list)
  or_target=${OR_REL:-.}
  [ -d "$or_target" ] || { echo "error: there's no folder $OR_REL in the workspace" >&2; exit 3; }
  cd "$or_target" || exit 3
  # Lines: kind<TAB>size<TAB>path, kind f (file), d (folder) or l (link).
  if find . -maxdepth 0 -printf '' >/dev/null 2>&1; then
    find . -mindepth 1 -maxdepth "$OR_DEPTH" \( -name .git -o -name .openrunner \) -prune -o -printf '%y\t%s\t%P\n' | head -n 2001
  else
    find . -mindepth 1 -maxdepth "$OR_DEPTH" \( -name .git -o -name .openrunner \) -prune -o -print | head -n 2001 | while IFS= read -r or_p; do
      if [ -L "$or_p" ]; then or_k=l; elif [ -d "$or_p" ]; then or_k=d; else or_k=f; fi
      printf '%s\t%s\t%s\n' "$or_k" "$(or_size "$or_p")" "${or_p#./}"
    done
  fi
  ;;
esac
