# What every component step and up_to_date test can use (recipe::apply puts
# it before them).
#
# pkg_install <package>...: Homebrew when it's there and this isn't root (it
# refuses root), else the system's apt, dnf, apk or pacman. When a package
# isn't found, the package lists are refreshed once and it's tried again: a
# machine's lists can be old, or empty in a fresh container.
export DEBIAN_FRONTEND=noninteractive
pkg_install() {
  if [ "$(id -u)" != 0 ] && command -v brew >/dev/null 2>&1; then
    brew install "$@"
  elif command -v apt-get >/dev/null 2>&1; then
    apt-get -o DPkg::Lock::Timeout=600 install -y "$@" ||
      { apt-get -o DPkg::Lock::Timeout=600 update && apt-get -o DPkg::Lock::Timeout=600 install -y "$@"; }
  elif command -v dnf >/dev/null 2>&1; then
    dnf install -y "$@"
  elif command -v apk >/dev/null 2>&1; then
    apk add "$@" || { apk update && apk add "$@"; }
  elif command -v pacman >/dev/null 2>&1; then
    pacman -S --noconfirm --needed "$@" || pacman -Sy --noconfirm --needed "$@"
  else
    echo "error: There's no package manager Muxin knows here (apt, dnf, apk, pacman, or Homebrew)." >&2
    return 1
  fi
}

# npm_prefix <command>: the npm prefix of the copy of <command> on PATH when
# it's an npm global one (<prefix>/lib/node_modules/...), else npm's own. A
# new Node.js can move npm's: Ubuntu's npm uses /usr/local, NodeSource's /usr.
# Updating there updates the copy that runs, not a second one behind it.
npm_prefix() {
  npm_found=$(realpath "$(command -v "$1")" 2>/dev/null) || true
  case "$npm_found" in
    */lib/node_modules/*) echo "${npm_found%%/lib/node_modules/*}" ;;
    *) npm prefix -g ;;
  esac
}
