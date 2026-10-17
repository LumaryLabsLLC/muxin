# Muxin: control one installed runner (macOS / Linux).
# Muxin prepends these variables:
#   OR_ACTION  start | stop | restart | logs | remove
#   OR_DIR     the runner folder (contains config.sh and .runner)
#   OR_TOKEN   remove token (remove only; empty skips deregistration)
#   OR_DELETE  1 to delete the folder after removing

set -u
say() { printf '%s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

[ -n "$OR_DIR" ] || fail "No runner folder given."
# Removing a runner whose folder is already gone: nothing left to do here.
if [ "$OR_ACTION" = remove ] && [ ! -e "$OR_DIR" ]; then
  say "$OR_DIR is already gone."
  say "✓ Removed"
  exit 0
fi
cd "$OR_DIR" 2>/dev/null || fail "$OR_DIR doesn't exist."

os=$(uname -s)
SUDO=""
if [ "$os" = Linux ] && [ "$(id -u)" != 0 ]; then
  SUDO="sudo -n"
fi
[ "$os" = Linux ] && [ "$(id -u)" = 0 ] && export RUNNER_ALLOW_RUNASROOT=1

has_service() { [ -f .service ]; }

svc() {
  if [ -n "$SUDO" ]; then
    $SUDO ./svc.sh "$@" || fail "svc.sh $1 failed. Linux services need root: allow passwordless sudo for $(id -un), or run on the machine: cd $OR_DIR && sudo ./svc.sh $1"
  else
    ./svc.sh "$@" || fail "svc.sh $1 failed."
  fi
}

# Snapshot the process list before filtering it: a filter running alongside ps
# would see itself (macOS awk even rewrites its argv so `-v x=path` shows as `x path`).
processes() {
  procs=$(ps -A -o pid= -o command= 2>/dev/null)
  printf '%s\n' "$procs"
}

# PIDs of this runner's Listener and the run-helper.sh loop that restarts it.
# run.sh itself exits once run-helper.sh does.
runner_pids() {
  processes | OR_D="$OR_DIR" awk '
    { for (i = 2; i <= NF; i++) if ($i == ENVIRON["OR_D"] "/bin/Runner.Listener" || $i == ENVIRON["OR_D"] "/run-helper.sh") { print $1; break } }'
}

listener_pid() {
  processes | OR_D="$OR_DIR" awk '{ for (i = 2; i <= NF; i++) if ($i == ENVIRON["OR_D"] "/bin/Runner.Listener") { print $1; exit } }'
}

start_runner() {
  if has_service; then
    say "Starting service $(head -n1 .service)…"
    svc start
  elif [ -n "$(listener_pid)" ]; then
    say "Already running."
  else
    say "Starting run.sh in the background (no service installed)…"
    nohup ./run.sh </dev/null >/dev/null 2>&1 &
  fi
}

stop_runner() {
  if has_service; then
    say "Stopping service $(head -n1 .service)…"
    svc stop
    return
  fi
  pids=$(runner_pids)
  if [ -z "$pids" ]; then
    say "Not running."
    return
  fi
  say "Stopping $(printf '%s' "$pids" | wc -w | tr -d ' ') process(es)…"
  # shellcheck disable=SC2086
  kill $pids 2>/dev/null
  i=0
  while [ -n "$(listener_pid)" ] && [ $i -lt 20 ]; do
    sleep 0.5
    i=$((i + 1))
  done
  pid=$(listener_pid)
  if [ -n "$pid" ]; then
    kill -9 "$pid" 2>/dev/null
  fi
  say "Stopped."
}

case "$OR_ACTION" in
  start)
    start_runner
    ;;
  stop)
    stop_runner
    ;;
  restart)
    stop_runner
    sleep 1
    start_runner
    ;;
  logs)
    log=$(ls -t _diag/Runner_*.log 2>/dev/null | head -n1)
    if [ -z "$log" ]; then
      say "No runner logs yet in $OR_DIR/_diag."
    else
      say "==> $OR_DIR/$log (last 200 lines)"
      tail -n 200 "$log"
    fi
    ;;
  remove)
    stop_runner
    if has_service; then
      say "Uninstalling the service…"
      svc uninstall
    fi
    if [ -f .runner ] && [ -n "$OR_TOKEN" ]; then
      say "Removing the runner from GitHub…"
      # The token goes in config.sh's environment, not its arguments: any account
      # on the machine can read those with ps.
      ACTIONS_RUNNER_INPUT_TOKEN=$OR_TOKEN ./config.sh remove || fail "config.sh remove failed."
    elif [ -f .runner ]; then
      say "Not removing it from GitHub (no removal token); dropping its registration here."
      rm -f .runner .credentials .credentials_rsaparams
    fi
    if [ "$OR_DELETE" = 1 ]; then
      # Only ever delete something that looks like a runner folder.
      case "$OR_DIR" in
        / | "$HOME" | "$HOME/" | "") fail "Refusing to delete $OR_DIR." ;;
      esac
      if [ -f "$OR_DIR/config.sh" ] && [ -d "$OR_DIR/bin" ]; then
        cd / && rm -rf "$OR_DIR" || fail "Couldn't delete $OR_DIR."
        say "Deleted $OR_DIR"
      else
        fail "$OR_DIR doesn't look like a runner folder; not deleting it."
      fi
    fi
    say "✓ Removed"
    ;;
  *)
    fail "Unknown action: $OR_ACTION"
    ;;
esac
