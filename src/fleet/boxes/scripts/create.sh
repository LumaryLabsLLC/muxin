# Creates a box on this machine: Muxin's box image (built here the first
# time), a checkout of the repository, the agent's home and a container
# running as this user, then the repository's .agents/setup in it.
# Prepended: OR_ID OR_CONTAINER OR_HOSTNAME OR_IMAGE OR_DOCKERFILE OR_REPO
# OR_LOCAL OR_REPO_SHOWN OR_REF OR_BRANCH OR_AUTH OR_FILES OR_COPY OR_MOUNTS
# OR_ENSURE OR_DB_DIRS OR_DB_COPY OR_CPUS OR_MEM. OR_AUTH is an
# http.extraHeader value; it only ever lives in this process's environment.
# OR_LOCAL: OR_REPO is a checkout here (a local project): it's cloned from
# that folder with its uncommitted changes (and new files that aren't
# ignored), and pushes where the checkout does. OR_FILES: one file of the agent's home per
# line, "<path>\t<mode>\t<base64>". OR_COPY: files of this user's home copied
# into the agent's when they're here. OR_MOUNTS: this user's agent folders
# (~/.claude, ~/.codex, ...) mounted into the agent's home when they're here,
# so the box's agent is signed in as it is here; OR_ENSURE: the ones made
# first (the box's agent's), so a sign-in in the box stays. OR_DB_DIRS: those
# with databases, not shared where Docker runs in a VM (a Mac); the box gets
# copies of OR_DB_COPY (their sign-in) instead.
# Prints docker=<path> and path=<the box's folder>.
set -u
PATH="$PATH:/usr/local/bin:/opt/homebrew/bin:$HOME/.orbstack/bin"
fail() { echo "error: $*" >&2; exit 1; }

docker=$(command -v docker) || fail "Docker isn't installed on $(hostname). Boxes run in Docker: install it there (on a Mac: Docker Desktop, OrbStack or Colima)."
docker info >/dev/null 2>&1 </dev/null || fail "Docker is installed on $(hostname), but $(id -un) can't use it: start Docker, or add $(id -un) to the docker group."
printf 'docker=%s\n' "$docker"
command -v git >/dev/null 2>&1 || fail "git isn't installed on $(hostname)."
command -v base64 >/dev/null 2>&1 || fail "base64 isn't installed on $(hostname)."

root="$HOME/muxen/boxes"
dir="$root/$OR_ID"
[ -e "$dir" ] && fail "$dir is there already."
mkdir -p "$dir/repo" "$dir/home/.muxin" || fail "Couldn't make $dir."
printf 'path=%s\n' "$dir"

# The image: built once per machine (a few minutes), shared by its boxes.
if ! docker image inspect "$OR_IMAGE" >/dev/null 2>&1 </dev/null; then
  echo "▸ Building the box image (the first time on this machine: a few minutes)"
  mkdir -p "$root/.image" || fail "Couldn't make $root/.image."
  printf '%s\n' "$OR_DOCKERFILE" > "$root/.image/Dockerfile"
  # Its steps as they start, each once.
  docker build -t "$OR_IMAGE" --label muxen.box-image=1 --build-arg UID="$(id -u)" --build-arg GID="$(id -g)" "$root/.image" </dev/null 2>&1 \
    | grep --line-buffered -E '^#[0-9]+ \[[0-9]+/|^Step [0-9]+/|ERROR|[Ee]rror:' | awk '!seen[$0]++ { print; fflush() }'
  docker image inspect "$OR_IMAGE" >/dev/null 2>&1 </dev/null || fail "The box image didn't build (see above)."
  echo "✓ Built the box image"
fi

# The checkout.
cd "$dir/repo" || fail "Couldn't open $dir/repo."
export GIT_TERMINAL_PROMPT=0
if [ -n "$OR_AUTH" ]; then
  export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.extraHeader GIT_CONFIG_VALUE_0="$OR_AUTH"
fi
if [ "$OR_LOCAL" = 1 ]; then
  src=$OR_REPO
  case "$src" in "~") src=$HOME ;; "~/"*) src="$HOME/${src#\~/}" ;; esac
  git -C "$src" rev-parse --is-inside-work-tree >/dev/null 2>&1 </dev/null || fail "$OR_REPO_SHOWN isn't a git checkout on $(hostname)."
  echo "▸ Copying the checkout $OR_REPO_SHOWN"
  git clone --quiet "$src" . </dev/null 2>&1 || { echo "error: git clone failed" >&2; exit 4; }
  if [ -n "$OR_REF" ]; then
    git -c advice.detachedHead=false checkout --quiet "$OR_REF" 2>&1 || fail "Couldn't check out $OR_REF."
  else
    # Where the checkout is now: its uncommitted changes, and its new files.
    git -C "$src" diff HEAD --binary > "$dir/.changes" 2>/dev/null </dev/null
    if [ -s "$dir/.changes" ]; then
      git apply --whitespace=nowarn "$dir/.changes" 2>&1 </dev/null || echo "! Some of its uncommitted changes didn't carry over"
    fi
    rm -f "$dir/.changes"
    (cd "$src" && git ls-files --others --exclude-standard -z </dev/null | tar cf - --null -T -) | tar xf - 2>/dev/null
    or_n=$(git status --porcelain 2>/dev/null </dev/null | wc -l | tr -d ' ')
    [ "${or_n:-0}" -gt 0 ] && echo "✓ Brought along its $or_n uncommitted files"
  fi
  # It pushes where the checkout does; the checkout itself stays a remote ("checkout").
  or_origin=$(git -C "$src" remote get-url origin 2>/dev/null </dev/null)
  if [ -n "$or_origin" ]; then
    git remote set-url origin "$or_origin"
    git remote add checkout "$src"
  else
    git remote rename origin checkout
  fi
else
echo "▸ Cloning $OR_REPO_SHOWN"
case "$OR_REF" in
  pull/*/head)
    or_pr=${OR_REF#pull/}; or_pr=pr-${or_pr%/head}
    git clone --quiet "$OR_REPO" . </dev/null 2>&1 || { echo "error: git clone failed" >&2; exit 4; }
    git fetch --quiet origin "+refs/$OR_REF:refs/heads/$or_pr" </dev/null 2>&1 || fail "Couldn't fetch $OR_REF."
    git checkout --quiet "$or_pr" 2>&1 || fail "Couldn't check out $OR_REF."
    ;;
  "")
    git clone --quiet "$OR_REPO" . </dev/null 2>&1 || { echo "error: git clone failed" >&2; exit 4; }
    ;;
  *)
    git clone --quiet "$OR_REPO" . </dev/null 2>&1 || { echo "error: git clone failed" >&2; exit 4; }
    git -c advice.detachedHead=false checkout --quiet "$OR_REF" 2>&1 || fail "Couldn't check out $OR_REF."
    ;;
esac
fi
unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0
if [ -n "$OR_BRANCH" ]; then
  git checkout --quiet -b "$OR_BRANCH" 2>&1 || fail "Couldn't make the branch $OR_BRANCH."
fi
echo "✓ Checked out $(git rev-parse --abbrev-ref HEAD 2>/dev/null) at $(git rev-parse --short HEAD 2>/dev/null)"

# The agent's home: what Muxin wrote, then the sign-ins found here.
home="$dir/home"
umask 077
tab=$(printf '\t')
printf '%s\n' "$OR_FILES" | while IFS="$tab" read -r f_path f_mode f_data; do
  [ -n "$f_path" ] || continue
  mkdir -p "$(dirname "$home/$f_path")"
  printf '%s' "$f_data" | base64 -d > "$home/$f_path" 2>/dev/null || printf '%s' "$f_data" | base64 -D > "$home/$f_path"
  chmod "$f_mode" "$home/$f_path"
done
[ -f "$home/.muxin/start" ] || fail "Couldn't write the box's files."
# Docker in a VM (a Mac, Docker Desktop) can't lock files here for the
# container, which databases need.
or_vm=
[ "$(uname -s)" = Darwin ] && or_vm=1
docker info --format '{{.OperatingSystem}}' 2>/dev/null </dev/null | grep -q "Docker Desktop" && or_vm=1
[ -n "$or_vm" ] && OR_COPY="$OR_COPY $OR_DB_COPY"
for f in $OR_COPY; do
  if [ -f "$HOME/$f" ] && [ ! -e "$home/$f" ]; then
    mkdir -p "$(dirname "$home/$f")"
    cp "$HOME/$f" "$home/$f"
  fi
done
umask 022
for d in $OR_ENSURE; do
  mkdir -p "$HOME/$d"
done
or_mounts=""
for d in $OR_MOUNTS; do
  if [ -n "$or_vm" ]; then
    case " $OR_DB_DIRS " in *" $d "*) continue ;; esac
  fi
  if [ -d "$HOME/$d" ]; then
    # Made here first, so Docker doesn't make them (as root) in the box's home.
    mkdir -p "$home/$d"
    or_mounts="$or_mounts -v $HOME/$d:/home/box/$d"
    echo "✓ Shares this machine's ~/$d"
  fi
done

# The container: this user, the checkout at the same path as here (agents
# keep their conversations by folder), the home at /home/box.
echo "▸ Starting the box"
docker rm -f "$OR_CONTAINER" >/dev/null 2>&1 </dev/null
# shellcheck disable=SC2086
docker run -d --name "$OR_CONTAINER" --label muxen.box="$OR_ID" --hostname "$OR_HOSTNAME" --init \
  --user "$(id -u):$(id -g)" -e HOME=/home/box -e MUXIN_BOX="$OR_ID" -e MUXIN_WORKDIR="$dir/repo" \
  -v "$dir/repo:$dir/repo" -v "$home:/home/box" $or_mounts -w "$dir/repo" \
  ${OR_CPUS:+--cpus "$OR_CPUS"} ${OR_MEM:+--memory "$OR_MEM"} \
  --entrypoint sh "$OR_IMAGE" -c 'trap "exit 0" TERM INT; while :; do sleep 3600 & wait $!; done' </dev/null >/dev/null \
  || fail "Docker couldn't start the box's container."

if [ -f "$dir/repo/.agents/setup" ]; then
  echo "▸ Running .agents/setup"
  docker exec -w "$dir/repo" "$OR_CONTAINER" sh -c 'set -a; . "$HOME/.muxin/env" 2>/dev/null; set +a; if [ -x .agents/setup ]; then exec ./.agents/setup; else exec sh .agents/setup; fi' </dev/null 2>&1
  or_setup=$?
  [ "$or_setup" -eq 0 ] && echo "✓ .agents/setup finished" || echo "! .agents/setup exited with $or_setup; the agent starts anyway"
fi
echo "ready"
