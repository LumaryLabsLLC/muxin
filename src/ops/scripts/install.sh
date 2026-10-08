# OpenRunner: install GitHub Actions runners (macOS / Linux).
# OpenRunner prepends these variables:
#   OR_VERSION  runner release, e.g. 2.329.0
#   OR_URL      https://github.com/<org> or https://github.com/<owner>/<repo>
#   OR_TOKEN    registration token (short-lived)
#   OR_NAMES    runner names, space separated
#   OR_LABELS   extra labels, comma separated (may be empty)
#   OR_GROUP    runner group (may be empty)
#   OR_SERVICE  1 to install as a service, 0 to start with run.sh
#   OR_BASE     install root (empty: $HOME/actions-runner)

set -u
say() { printf '%s\n' "$*"; }
fail() { printf 'error: %s\n' "$*" >&2; exit 1; }

os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Darwin) ros=osx ;;
  Linux) ros=linux ;;
  *) fail "Unsupported operating system: $os" ;;
esac
case "$arch" in
  x86_64 | amd64) rarch=x64 ;;
  arm64 | aarch64) rarch=arm64 ;;
  armv7l | armv6l | arm) rarch=arm ;;
  *) fail "Unsupported CPU architecture: $arch" ;;
esac

base="${OR_BASE:-$HOME/actions-runner}"
pkg="actions-runner-$ros-$rarch-$OR_VERSION.tar.gz"
url="https://github.com/actions/runner/releases/download/v$OR_VERSION/$pkg"
cache="$base/.cache"

SUDO=""
if [ "$ros" = linux ] && [ "$(id -u)" != 0 ]; then
  SUDO="sudo -n"
fi
if [ "$ros" = linux ] && [ "$(id -u)" = 0 ]; then
  # config.sh refuses to run as root unless told it's intended.
  export RUNNER_ALLOW_RUNASROOT=1
  say "Running as root; the runners will run as root too."
fi

# Fail before touching anything if the service can't be installed.
if [ "$OR_SERVICE" = 1 ] && [ -n "$SUDO" ]; then
  if ! sudo -n true 2>/dev/null; then
    fail "Installing a Linux service needs root, and sudo asks for a password over ssh. Allow passwordless sudo for $(id -un), or turn off \"Install as a service\" and later run: sudo ./svc.sh install $(id -un) && sudo ./svc.sh start"
  fi
fi

for name in $OR_NAMES; do
  if [ -f "$base/$name/.runner" ]; then
    fail "$base/$name already has a configured runner. Remove it first or pick another name."
  fi
done

mkdir -p "$cache" || fail "Couldn't create $cache"
if [ -s "$cache/$pkg" ]; then
  say "Using cached $pkg"
else
  say "Downloading actions-runner $OR_VERSION for $ros-$rarch…"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$cache/$pkg.part" "$url" || fail "Download failed: $url"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$cache/$pkg.part" "$url" || fail "Download failed: $url"
  else
    fail "Neither curl nor wget is installed."
  fi
  mv "$cache/$pkg.part" "$cache/$pkg" || fail "Couldn't save $pkg"
fi

for name in $OR_NAMES; do
  dir="$base/$name"
  say "Setting up $name in $dir"
  mkdir -p "$dir" || fail "Couldn't create $dir"
  tar xzf "$cache/$pkg" -C "$dir" || fail "Couldn't unpack $pkg into $dir"
  cd "$dir" || fail "Couldn't enter $dir"

  set -- --unattended --url "$OR_URL" --token "$OR_TOKEN" --name "$name" --work _work --replace
  [ -n "$OR_LABELS" ] && set -- "$@" --labels "$OR_LABELS"
  [ -n "$OR_GROUP" ] && set -- "$@" --runnergroup "$OR_GROUP"
  ./config.sh "$@" || fail "config.sh failed for $name. On Linux, missing libraries can be installed with: sudo $dir/bin/installdependencies.sh"

  if [ "$OR_SERVICE" = 1 ]; then
    say "Installing $name as a service…"
    if [ "$ros" = linux ]; then
      $SUDO ./svc.sh install "$(id -un)" || fail "svc.sh install failed for $name"
      $SUDO ./svc.sh start || fail "svc.sh start failed for $name"
    else
      ./svc.sh install || fail "svc.sh install failed for $name"
      ./svc.sh start || fail "svc.sh start failed for $name. macOS runs runner services in the user's login session: make sure $(id -un) is logged in (automatic login helps on headless Macs)."
    fi
  else
    say "Starting $name in the background (not a service, so it won't come back after a reboot)…"
    nohup ./run.sh </dev/null >/dev/null 2>&1 &
  fi
  say "✓ $name is installed"
done
