# Muxin: control one runner that lives in a Docker container.
# Muxin prepends these variables:
#   OR_ACTION     start | stop | restart | logs | remove
#   OR_CONTAINER  the container, e.g. muxen-beelink-01
#   OR_TOKEN      remove token (remove only; empty skips deregistration)
#   OR_DELETE     1 to delete the runner's volume after removing

set -u
say() { printf '%s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

c=$OR_CONTAINER
[ -n "$c" ] || fail "No container given."
command -v docker >/dev/null 2>&1 || fail "Docker isn't installed on this machine."
docker container inspect "$c" >/dev/null 2>&1 || fail "There's no container called $c on this machine."

state() { docker inspect -f '{{.State.Status}}' "$c" 2>/dev/null; }

case "$OR_ACTION" in
  start)
    if [ "$(state)" = running ]; then
      say "Already running."
    else
      say "Starting ${c}…"
      docker start "$c" >/dev/null || fail "docker start $c failed."
      say "Started."
    fi
    ;;
  stop)
    if [ "$(state)" = running ]; then
      say "Stopping $c (jobs get 30 seconds to finish)…"
      docker stop -t 30 "$c" >/dev/null || fail "docker stop $c failed."
      say "Stopped."
    else
      say "Not running."
    fi
    ;;
  restart)
    say "Restarting ${c}…"
    docker restart -t 30 "$c" >/dev/null || fail "docker restart $c failed."
    say "Restarted."
    ;;
  logs)
    say "==> docker logs $c (last 200 lines)"
    docker logs --tail 200 "$c" 2>&1
    ;;
  remove)
    image=$(docker inspect -f '{{.Config.Image}}' "$c")
    vol=$(docker inspect -f '{{index .Config.Labels "muxen.volume"}}' "$c" 2>/dev/null)
    if [ -z "$vol" ]; then
      vol=$(docker inspect -f '{{range .Mounts}}{{if eq .Destination "/home/runner"}}{{.Name}}{{end}}{{end}}' "$c" 2>/dev/null)
    fi
    if [ "$(state)" = running ]; then
      say "Stopping ${c}…"
      docker stop -t 30 "$c" >/dev/null 2>&1
    fi
    if [ -n "$vol" ] && docker run --rm -v "$vol:/home/runner" --entrypoint test "$image" -f /home/runner/.runner 2>/dev/null; then
      if [ -n "$OR_TOKEN" ]; then
        say "Removing the runner from GitHub…"
        printf '%s' "$OR_TOKEN" | docker run --rm -i -e ACTIONS_RUNNER_PRINT_LOG_TO_STDOUT=0 -v "$vol:/home/runner" --entrypoint bash "$image" \
          -c 'export ACTIONS_RUNNER_INPUT_TOKEN="$(cat)"; exec ./config.sh remove' || fail "config.sh remove failed; the container was left stopped."
      else
        say "warning: no removal token, so the runner stays registered on GitHub."
      fi
    fi
    docker rm -f "$c" >/dev/null || fail "docker rm $c failed."
    say "Removed container $c"
    if [ "$OR_DELETE" = 1 ] && [ -n "$vol" ]; then
      docker volume rm "$vol" >/dev/null && say "Deleted volume $vol"
    fi
    say "✓ Removed"
    ;;
  *)
    fail "Unknown action: $OR_ACTION"
    ;;
esac
