# Docker helpers for container workspaces. Expects OR_ID OR_CONTAINER OR_IMAGE OR_SOCKET.
# The workspace folder stays on the host and is bind-mounted at /workspace; the
# container runs as the host user so files never end up owned by root.

# or_docker_check: docker is installed and the daemon answers.
or_docker_check() {
  if ! command -v docker >/dev/null 2>&1; then
    echo "error: docker isn't available on $(hostname): the docker command isn't installed" >&2
    return 1
  fi
  if ! or_dk_err=$(docker info --format '{{.ServerVersion}}' 2>&1 >/dev/null); then
    echo "error: docker isn't available on $(hostname): $(printf '%s' "$or_dk_err" | grep -v '^$' | tail -n 1)" >&2
    return 1
  fi
}

# or_docker_up DIR [quiet]: makes sure the workspace's container is running,
# creating it (and pulling the image) when it doesn't exist.
or_docker_up() {
  or_dk_state=$(docker inspect -f '{{.State.Running}}' "$OR_CONTAINER" 2>/dev/null) || or_dk_state=missing
  case "$or_dk_state" in
    true) return 0 ;;
    false)
      docker start "$OR_CONTAINER" >/dev/null 2>&1 && return 0
      docker rm -f "$OR_CONTAINER" >/dev/null 2>&1 ;;
  esac
  if ! docker image inspect "$OR_IMAGE" >/dev/null 2>&1; then
    [ -n "$2" ] || echo "Pulling $OR_IMAGE"
    if ! or_dk_out=$(docker pull "$OR_IMAGE" 2>&1); then
      echo "error: couldn't pull $OR_IMAGE: $(printf '%s' "$or_dk_out" | grep -v '^$' | tail -n 1)" >&2
      return 1
    fi
    [ -n "$2" ] || printf '%s\n' "$or_dk_out" | tail -n 2
  fi
  mkdir -p "$1/.openrunner/home" "$1/.openrunner/logs" "$1/.openrunner/run"
  or_dk_sock=""
  if [ "$OR_SOCKET" = 1 ]; then
    if [ -S /var/run/docker.sock ]; then
      or_dk_gid=$(ls -lnL /var/run/docker.sock | awk '{ print $4 }')
      or_dk_sock="-v /var/run/docker.sock:/var/run/docker.sock --group-add $or_dk_gid"
    else
      echo "warning: there's no /var/run/docker.sock to share with the container"
    fi
  fi
  [ -n "$2" ] || echo "Starting container $OR_CONTAINER from $OR_IMAGE"
  # A shell loop keeps it alive (works on any image with sh, unlike `sleep infinity`).
  # shellcheck disable=SC2086
  if ! or_dk_out=$(docker run -d --name "$OR_CONTAINER" --label "openrunner.workspace=$OR_ID" --init \
      --user "$(id -u):$(id -g)" -e HOME=/workspace/.openrunner/home \
      -v "$1:/workspace" -w /workspace $or_dk_sock \
      --entrypoint sh "$OR_IMAGE" -c 'trap "exit 0" TERM INT; while :; do sleep 3600 & wait $!; done' 2>&1); then
    echo "error: couldn't start a container from $OR_IMAGE: $(printf '%s' "$or_dk_out" | grep -v '^$' | tail -n 1)" >&2
    return 1
  fi
}
