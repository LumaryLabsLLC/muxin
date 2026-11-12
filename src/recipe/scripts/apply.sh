# Installs one component (recipe::apply), in the user's login shell like its
# check. Prepended:
#   OR_ID       the component's id, for messages
#   OR_PRELUDE  what every step can use (prelude.sh)
#   OR_STEP     its install step, run by sh -e after the prelude, with nothing on stdin
#   OR_ROOT     1 when the step needs root here (root = true, on Linux)
#   OR_UNLESS   its root_unless test: when it exits 0, the step runs as the user
# Ends with "$OR_MARK:exit=N" on stderr, N being the step's exit status, or 77
# when it needs root and sudo would ask for a password (then nothing ran).
or_install() {
  if [ "$OR_ROOT" = 1 ] && [ "$(id -u)" != 0 ]; then
    if [ -n "$OR_UNLESS" ] && sh -c "$OR_UNLESS" </dev/null >/dev/null 2>&1; then
      echo "As $(id -un): $OR_UNLESS"
    elif sudo -n true 2>/dev/null; then
      echo "As root (sudo -n)"
      sudo -n sh -ec "$OR_PRELUDE
$OR_STEP" </dev/null
      return
    else
      echo "error: $OR_ID needs root here, and sudo won't run without a password." >&2
      return 77
    fi
  fi
  sh -ec "$OR_PRELUDE
$OR_STEP" </dev/null
}
or_install
printf '%s:exit=%s\n' "$OR_MARK" "$?" >&2
