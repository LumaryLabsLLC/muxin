# What every component step and up_to_date test can use (recipe::apply puts
# it before them).
#
# pkg_manager: the package manager pkg_install uses here, as one word: brew
# when it's there and this isn't root (it refuses root), else the system's
# apt, dnf, apk or pacman; nothing when there's none. For steps whose
# packages have other names on other systems.
pkg_manager() {
  if [ "$(id -u)" != 0 ] && command -v brew >/dev/null 2>&1; then
    echo brew
  elif command -v apt-get >/dev/null 2>&1; then
    echo apt
  elif command -v dnf >/dev/null 2>&1; then
    echo dnf
  elif command -v apk >/dev/null 2>&1; then
    echo apk
  elif command -v pacman >/dev/null 2>&1; then
    echo pacman
  fi
}

# pkg_install <package>...: installs them with pkg_manager's. When a package
# isn't found, the package lists are refreshed once and it's tried again: a
# machine's lists can be old, or empty in a fresh container.
#
# Arch is the exception: installing from newer lists without upgrading
# everything can break what's installed. So only a system whose lists were
# never fetched (a fresh container) is brought up to date; anywhere else
# that's left to its owner, with a line saying so.
export DEBIAN_FRONTEND=noninteractive
pkg_install() {
  case "$(pkg_manager)" in
    brew) brew install "$@" ;;
    apt)
      apt-get -o DPkg::Lock::Timeout=600 install -y "$@" ||
        { apt-get -o DPkg::Lock::Timeout=600 update && apt-get -o DPkg::Lock::Timeout=600 install -y "$@"; }
      ;;
    dnf) dnf install -y "$@" ;;
    apk) apk add "$@" || { apk update && apk add "$@"; } ;;
    pacman)
      if [ ! -e /var/lib/pacman/sync/core.db ]; then
        pacman -Syu --noconfirm --needed "$@"
      elif ! pacman -S --noconfirm --needed "$@"; then
        echo "error: pacman didn't install it. If its package lists are old, run \`sudo pacman -Syu\` there, then apply again." >&2
        return 1
      fi
      ;;
    *)
      echo "error: There's no package manager Muxin knows here (apt, dnf, apk, pacman, or Homebrew)." >&2
      return 1
      ;;
  esac
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
