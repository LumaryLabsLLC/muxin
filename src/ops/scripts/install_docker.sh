# OpenRunner: install GitHub Actions runners as Docker containers (Linux, or any
# machine with a Docker daemon that runs Linux containers).
# OpenRunner prepends these variables:
#   OR_IMAGE           runner image, e.g. ghcr.io/actions/actions-runner:2.329.0
#   OR_IMAGE_FALLBACK  image to try when OR_IMAGE can't be pulled (may be empty)
#   OR_URL             https://github.com/<org> or https://github.com/<owner>/<repo>
#   OR_SCOPE           the org or owner/repo (stored as a container label)
#   OR_TOKEN           registration token (short-lived)
#   OR_NAMES           runner names, space separated
#   OR_LABELS          labels, comma separated (may be empty)
#   OR_GROUP           runner group (may be empty)
#   OR_SOCKET          1 to give the runner the host's Docker socket
#   OR_STUB            tests only: 1 skips GitHub registration and runs a stand-in listener
#
# Each runner gets a named volume mounted at /home/runner (Docker fills it from
# the image the first time), a one-off container that registers it, and then a
# long-running container `openrunner-<name>` that runs run.sh and restarts with
# the Docker daemon. The token reaches config.sh only through stdin and the
# environment of a throwaway container: never argv, never `docker inspect`.

set -u
say() { printf '%s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

command -v docker >/dev/null 2>&1 || fail "Docker isn't installed on this machine."
if ! docker version --format '{{.Server.Version}}' >/dev/null 2>&1; then
  why=$(docker version 2>&1 | grep -i -m1 -E 'permission denied|cannot connect|error')
  fail "Docker is installed but $(id -un) can't use it (${why:-no daemon}). Add $(id -un) to the docker group: sudo usermod -aG docker $(id -un), then log in again."
fi

for name in $OR_NAMES; do
  if docker container inspect "openrunner-$name" >/dev/null 2>&1; then
    fail "A container called openrunner-$name already exists. Remove it first or pick another name."
  fi
done

# Shows the useful lines of `docker pull` and reports whether the image is now here.
pull() {
  docker pull "$1" 2>&1 | while IFS= read -r line; do
    case "$line" in
      *"Pulling from"* | Digest:* | Status:* | *"Pull complete"* | *"Error"* | *"error"* | *"denied"* | *"not found"*) say "$line" ;;
    esac
  done
  docker image inspect "$1" >/dev/null 2>&1
}

image=$OR_IMAGE
if docker image inspect "$image" >/dev/null 2>&1; then
  say "Using $image (already on this machine)"
else
  say "Pulling $image (about 2 GB the first time)…"
  if ! pull "$image"; then
    if [ -n "$OR_IMAGE_FALLBACK" ] && [ "$OR_IMAGE_FALLBACK" != "$image" ]; then
      say "warning: couldn't pull $image; trying $OR_IMAGE_FALLBACK"
      image=$OR_IMAGE_FALLBACK
      pull "$image" || fail "Couldn't pull $image."
    else
      fail "Couldn't pull $image."
    fi
  fi
fi

socket_args=""
if [ "$OR_SOCKET" = 1 ]; then
  sock=/var/run/docker.sock
  [ -S "$sock" ] || fail "There's no Docker socket at $sock to share with the runners."
  gid=$(stat -c %g "$sock" 2>/dev/null || stat -f %g "$sock" 2>/dev/null)
  socket_args="-v $sock:$sock --group-add ${gid:-0}"
  say "warning: the runners get this machine's Docker socket, which is root-equivalent access to the machine. Only run trusted workflows on them."
fi

for name in $OR_NAMES; do
  c="openrunner-$name"
  vol="openrunner-$name"
  say "Setting up $name (container $c, volume $vol)"
  docker volume create --label "openrunner.runner=$name" "$vol" >/dev/null || fail "Couldn't create the volume $vol."

  if [ "$OR_STUB" = 1 ]; then
    # Tests only: write a .runner like config.sh would, without talking to GitHub.
    docker run --rm -v "$vol:/home/runner" --entrypoint bash "$image" -c \
      'printf "{\"agentName\":\"%s\",\"gitHubUrl\":\"%s\"}" "$1" "$2" > /home/runner/.runner' openrunner "$name" "$OR_URL" \
      || fail "Couldn't prepare $vol."
    say "√ Runner successfully added ($name, stubbed)"
  else
    set -- --unattended --url "$OR_URL" --name "$name" --work _work --replace
    [ -n "$OR_LABELS" ] && set -- "$@" --labels "$OR_LABELS"
    [ -n "$OR_GROUP" ] && set -- "$@" --runnergroup "$OR_GROUP"
    # The token travels on stdin into the throwaway container's environment.
    if ! printf '%s' "$OR_TOKEN" | docker run --rm -i -e ACTIONS_RUNNER_PRINT_LOG_TO_STDOUT=0 -v "$vol:/home/runner" --entrypoint bash "$image" \
      -c 'export ACTIONS_RUNNER_INPUT_TOKEN="$(cat)"; exec ./config.sh "$@"' openrunner "$@"; then
      docker volume rm "$vol" >/dev/null 2>&1
      fail "config.sh failed for $name; nothing was left behind."
    fi
  fi

  set -- -d --name "$c" --hostname "$name" --restart unless-stopped \
    --label "openrunner.runner=$name" --label "openrunner.scope=$OR_SCOPE" --label "openrunner.volume=$vol" \
    -v "$vol:/home/runner"
  if [ "$OR_STUB" = 1 ]; then
    # Tests only: a process named like the runner's listener, so checks see it.
    set -- "$@" --entrypoint bash
    # shellcheck disable=SC2086
    docker run "$@" $socket_args "$image" -c 'trap "exit 0" TERM INT; (exec -a /home/runner/bin/Runner.Listener sleep infinity) & wait' >/dev/null \
      || fail "Couldn't start the $c container."
  else
    set -- "$@" --entrypoint /home/runner/run.sh
    # shellcheck disable=SC2086
    docker run "$@" $socket_args "$image" >/dev/null || fail "Couldn't start the $c container."
  fi

  # It should stay up: a container that exits right away (bad credentials, missing
  # libraries) would otherwise restart forever.
  sleep 8
  state=$(docker inspect -f '{{.State.Status}} {{.RestartCount}}' "$c" 2>/dev/null)
  case "$state" in
    "running 0") ;;
    *)
      say "The $c container isn't staying up ($state). Its last output:"
      docker logs --tail 20 "$c" 2>&1
      fail "$name didn't start. Remove it with: docker rm -f $c && docker volume rm $vol"
      ;;
  esac
  say "✓ $name is installed"
done
