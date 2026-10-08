# Runs one command in a workspace. Prepended: OR_DIR OR_CWD OR_CMD OR_TIMEOUT OR_BG OR_LOG OR_ID
# and `export NAME=...` lines for the caller's environment variables.

or_fail() { echo "error: $1" >&2; printf '%s:exit=97\n' "$OR_MARK" >&2; exit 0; }
cd "$OR_DIR" 2>/dev/null || or_fail "the workspace folder is gone: $OR_DIR"
if [ -n "$OR_CWD" ]; then cd "$OR_CWD" 2>/dev/null || or_fail "there's no folder $OR_CWD in the workspace"; fi
export MUXEN_WORKSPACE="$OR_ID" NO_COLOR=1 GIT_TERMINAL_PROMPT=0 GIT_PAGER=cat PAGER=cat

# Run the command with the user's own shell when it's an sh-family one.
or_shell=sh
case "$SHELL" in */bash|*/zsh|*/ksh) or_shell=$SHELL ;; esac

if [ "$OR_BG" = 1 ]; then
  mkdir -p "$OR_DIR/.muxen/logs"
  nohup "$or_shell" -c "$OR_CMD" > "$OR_DIR/$OR_LOG" 2>&1 < /dev/null &
  printf 'pid=%s\n' "$!"
  printf '%s:exit=0\n' "$OR_MARK" >&2
  exit 0
fi

"$or_shell" -c "$OR_CMD" < /dev/null &
or_pid=$!
# Watchdog: on timeout, kill the whole process tree (not just the shell).
(
  sleep "$OR_TIMEOUT"
  printf '%s:timeout\n' "$OR_MARK" >&2
  or_kill_tree "$or_pid" TERM
  sleep 3
  or_kill_tree "$or_pid" KILL
) < /dev/null > /dev/null &
or_watch=$!
wait "$or_pid"
or_code=$?
or_kill_tree "$or_watch" KILL
printf '%s:exit=%s\n' "$OR_MARK" "$or_code" >&2
exit 0
