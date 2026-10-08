# Runs one command inside a workspace's container. Prepended: OR_DIR OR_CWD OR_CMD
# OR_TIMEOUT OR_BG OR_LOG OR_ID OR_CONTAINER OR_IMAGE OR_SOCKET OR_ENV (names of
# exported variables to pass in), plus the docker.sh and lib.sh helpers.

or_fail() { echo "error: $1" >&2; printf '%s:exit=97\n' "$OR_MARK" >&2; exit 0; }
or_setup_failed() { printf '%s:exit=97\n' "$OR_MARK" >&2; exit 0; }
[ -d "$OR_DIR" ] || or_fail "the workspace folder is gone: $OR_DIR"
if [ -n "$OR_CWD" ] && [ ! -d "$OR_DIR/$OR_CWD" ]; then or_fail "there's no folder $OR_CWD in the workspace"; fi
or_docker_check || or_setup_failed
or_docker_up "$OR_DIR" quiet || or_setup_failed
mkdir -p "$OR_DIR/.openrunner/run" "$OR_DIR/.openrunner/logs"

export OPENRUNNER_WORKSPACE="$OR_ID" NO_COLOR=1 GIT_TERMINAL_PROMPT=0 GIT_PAGER=cat PAGER=cat
or_envs="-e OPENRUNNER_WORKSPACE -e NO_COLOR -e GIT_TERMINAL_PROMPT -e GIT_PAGER -e PAGER"
for or_n in $OR_ENV; do or_envs="$or_envs -e $or_n"; done
or_wd=/workspace${OR_CWD:+/$OR_CWD}
# The command's pid (inside the container) is written here, so a timeout can kill it.
or_pidfile=".openrunner/run/$OR_MARK.pid"

if [ "$OR_BG" = 1 ]; then
  # shellcheck disable=SC2086
  docker exec -d -w "$or_wd" $or_envs "$OR_CONTAINER" \
    sh -c 'echo $$ > "/workspace/$1"; exec sh -c "$2" > "/workspace/$3" 2>&1 < /dev/null' sh "$or_pidfile" "$OR_CMD" "$OR_LOG"
  or_i=0
  while [ ! -s "$OR_DIR/$or_pidfile" ] && [ "$or_i" -lt 50 ]; do sleep 0.1; or_i=$((or_i + 1)); done
  printf 'pid=%s\n' "$(cat "$OR_DIR/$or_pidfile" 2>/dev/null)"
  rm -f "$OR_DIR/$or_pidfile"
  printf '%s:exit=0\n' "$OR_MARK" >&2
  exit 0
fi

# The command gets its own process group (setsid; sh job control needs a tty),
# so a timeout kills everything it started, not just the shell.
# shellcheck disable=SC2086
docker exec -w "$or_wd" $or_envs "$OR_CONTAINER" \
  sh -c 'if command -v setsid >/dev/null 2>&1; then setsid sh -c "$2" < /dev/null & else sh -c "$2" < /dev/null & fi
         p=$!; echo "$p" > "/workspace/$1"; wait "$p"; c=$?; rm -f "/workspace/$1"; exit "$c"' \
  sh "$or_pidfile" "$OR_CMD" < /dev/null &
or_pid=$!
# Inside the container: signal the command's group, or (no setsid in the image)
# walk /proc for its descendants.
or_killer='t=$1; s=$2
kill -"$s" -"$t" 2>/dev/null && exit 0
all=" $t "; more=1
while [ "$more" = 1 ]; do more=0
  for f in /proc/[0-9]*/stat; do
    read -r pid _ _ ppid _ < "$f" 2>/dev/null || continue
    case "$all" in *" $pid "*) continue ;; esac
    case "$all" in *" $ppid "*) all="$all$pid "; more=1 ;; esac
  done
done
kill -"$s" $all 2>/dev/null'

(
  sleep "$OR_TIMEOUT"
  printf '%s:timeout\n' "$OR_MARK" >&2
  or_cpid=$(cat "$OR_DIR/$or_pidfile" 2>/dev/null)
  if [ -n "$or_cpid" ]; then
    docker exec "$OR_CONTAINER" sh -c "$or_killer" sh "$or_cpid" TERM >/dev/null 2>&1
    sleep 3
    docker exec "$OR_CONTAINER" sh -c "$or_killer" sh "$or_cpid" KILL >/dev/null 2>&1
  fi
  or_kill_tree "$or_pid" KILL
) < /dev/null > /dev/null &
or_watch=$!
wait "$or_pid"
or_code=$?
or_kill_tree "$or_watch" KILL
rm -f "$OR_DIR/$or_pidfile"
printf '%s:exit=%s\n' "$OR_MARK" "$or_code" >&2
exit 0
