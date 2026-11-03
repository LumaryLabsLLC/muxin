# Muxin: control one runner that lives in a Docker container.
# Muxin prepends these variables:
#   OR_ACTION     start | stop | restart | logs | remove | update
#   OR_CONTAINER  the container, e.g. muxen-beelink-01
#   OR_TOKEN      remove token (remove only; empty skips deregistration)
#   OR_DELETE     1 to delete the runner's volume after removing
#   OR_VERSION    runner release to update to (update only), e.g. 2.329.0
#   OR_SHA256     each package's SHA-256 (update only), space separated <platform>=<hex>

set -u
say() { printf '%s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

c=$OR_CONTAINER
[ -n "$c" ] || fail "No container given."
command -v docker >/dev/null 2>&1 || fail "Docker isn't installed on this machine."
docker container inspect "$c" >/dev/null 2>&1 || fail "There's no container called $c on this machine."

state() { docker inspect -f '{{.State.Status}}' "$c" 2>/dev/null; }

# The volume with the runner's folder.
volume() {
  v=$(docker inspect -f '{{index .Config.Labels "muxen.volume"}}' "$c" 2>/dev/null)
  if [ -z "$v" ]; then
    v=$(docker inspect -f '{{range .Mounts}}{{if eq .Destination "/home/runner"}}{{.Name}}{{end}}{{end}}' "$c" 2>/dev/null)
  fi
  printf '%s' "$v"
}

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
  update)
    [ -n "$OR_VERSION" ] || fail "No version to update to."
    image=$(docker inspect -f '{{.Config.Image}}' "$c")
    vol=$(volume)
    [ -n "$vol" ] || fail "$c keeps no runner folder in a volume, so its runner can't be updated here."
    if [ "$(state)" = running ] && docker exec "$c" sh -c "grep -qa '[R]unner\.Worker' /proc/[0-9]*/cmdline" 2>/dev/null; then
      fail "It's running a job. Update it when it's idle."
    fi
    # The runner's programs live in the volume, so a newer image alone wouldn't
    # change them. A throwaway container of the same image gets the new ones
    # ready beside them, with the runner still running: what can fail fails first.
    # shellcheck disable=SC2016
    prepare='cd /home/runner || exit 1
case "$(uname -m)" in
  x86_64 | amd64) a=x64 ;;
  aarch64 | arm64) a=arm64 ;;
  armv7l | armv6l | arm) a=arm ;;
  *) echo "error: Unsupported CPU architecture: $(uname -m)" >&2; exit 1 ;;
esac
pkg="actions-runner-linux-$a-$OR_VERSION.tar.gz"
sha=""
for e in $OR_SHA256; do
  case "$e" in "linux-$a="*) sha=${e#*=} ;; esac
done
[ -n "$sha" ] || { echo "error: GitHub'"'"'s release notes for actions-runner $OR_VERSION don'"'"'t list a SHA-256 for $pkg, so it can'"'"'t be checked. Not updating." >&2; exit 1; }
rm -rf .muxin-update
mkdir -p .muxin-update/unpacked || exit 1
url="https://github.com/actions/runner/releases/download/v$OR_VERSION/$pkg"
echo "Downloading actions-runner $OR_VERSION for linux-$a…"
curl -fsSL --retry 3 -o ".muxin-update/$pkg" "$url" || { rm -rf .muxin-update; echo "error: Download failed: $url" >&2; exit 1; }
got=$(sha256sum ".muxin-update/$pkg" | cut -d" " -f1)
[ "$got" = "$sha" ] || { rm -rf .muxin-update; echo "error: The downloaded $pkg doesn'"'"'t match the SHA-256 GitHub published for it (expected $sha, got ${got:-nothing}), so it was deleted. Nothing changed." >&2; exit 1; }
echo "Checked $pkg against GitHub'"'"'s SHA-256"
tar xzf ".muxin-update/$pkg" -C .muxin-update/unpacked || { rm -rf .muxin-update; echo "error: Couldn'"'"'t unpack $pkg. Nothing changed." >&2; exit 1; }
[ -f .muxin-update/unpacked/bin/Runner.Listener ] || { rm -rf .muxin-update; echo "error: $pkg has no runner in it. Nothing changed." >&2; exit 1; }'
    docker run --rm -e "OR_VERSION=$OR_VERSION" -e "OR_SHA256=$OR_SHA256" -v "$vol:/home/runner" --entrypoint bash "$image" -c "$prepare" || exit 1
    was_running=0
    if [ "$(state)" = running ]; then
      was_running=1
      say "Stopping ${c}…"
      docker stop -t 30 "$c" >/dev/null || fail "docker stop $c failed. Nothing changed."
    fi
    say "Replacing the runner's programs (its registration and work folder stay)…"
    docker run --rm -v "$vol:/home/runner" --entrypoint bash "$image" -c 'cd /home/runner && rm -rf bin externals bin.* externals.* && cp -Rp .muxin-update/unpacked/. . && rm -rf .muxin-update' \
      || fail "Couldn't replace the runner's programs in $vol. Update it again: the download is still there."
    if [ "$was_running" = 1 ]; then
      say "Starting ${c}…"
      docker start "$c" >/dev/null || fail "docker start $c failed."
    fi
    say "✓ Updated to $OR_VERSION"
    ;;
  remove)
    image=$(docker inspect -f '{{.Config.Image}}' "$c")
    vol=$(volume)
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
