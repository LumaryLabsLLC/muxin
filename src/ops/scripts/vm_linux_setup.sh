# Runs as the container's command, as root, every time it starts: sshd with
# the user and their key, then sshd in the foreground. MUXEN_USER, MUXEN_KEY
# and MUXEN_NPM (agent CLIs to install, as npm packages, each after its own
# flags) come from the container's environment.
u=$MUXEN_USER
if [ ! -x /usr/sbin/sshd ]; then
  echo "Installing OpenSSH"
  if command -v apt-get >/dev/null 2>&1; then
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq && apt-get install -y -qq openssh-server sudo ca-certificates curl git procps >/dev/null
    # The GitHub CLI, for pushing from here; not worth failing over.
    apt-get install -y -qq gh >/dev/null 2>&1 || true
  elif command -v dnf >/dev/null 2>&1; then
    dnf install -y -q openssh-server sudo curl git procps-ng hostname shadow-utils >/dev/null
    dnf install -y -q gh >/dev/null 2>&1 || true
  elif command -v apk >/dev/null 2>&1; then
    apk add -q openssh sudo bash curl git shadow
    apk add -q github-cli >/dev/null 2>&1 || true
  elif command -v pacman >/dev/null 2>&1; then
    # -Syu, the whole system: Arch can't install from newer package lists
    # without it (-Sy alone stops at whichever library moved since the image
    # was built, and sshd never installs).
    pacman -Syu --noconfirm --needed openssh sudo curl git >/dev/null
    pacman -S --noconfirm --needed github-cli >/dev/null 2>&1 || true
  fi
fi
if ! id "$u" >/dev/null 2>&1; then
  shell=/bin/sh
  [ -x /bin/bash ] && shell=/bin/bash
  useradd -m -s "$shell" "$u" 2>/dev/null || adduser -D -s "$shell" "$u"
  # Key-only logins: an account without a password can't be locked out of them.
  usermod -p '*' "$u" 2>/dev/null || sed -i "s/^$u:!*:/$u:*:/" /etc/shadow
  mkdir -p /etc/sudoers.d
  echo "$u ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/muxen
  chmod 440 /etc/sudoers.d/muxen
fi
home=$(getent passwd "$u" 2>/dev/null | cut -d: -f6)
home=${home:-/home/$u}
mkdir -p "$home/.ssh"
printf '%s\n' "$MUXEN_KEY" > "$home/.ssh/authorized_keys"
chown -R "$u" "$home/.ssh"
chmod 700 "$home/.ssh"
chmod 600 "$home/.ssh/authorized_keys"
# Agent CLIs, once: Node.js from the system's packages, then npm.
if [ -n "${MUXEN_NPM:-}" ] && [ ! -f /var/lib/muxen-npm-done ]; then
  echo "Installing Node.js and the agent CLIs"
  if command -v apt-get >/dev/null 2>&1; then
    apt-get install -y -qq nodejs npm >/dev/null
  elif command -v dnf >/dev/null 2>&1; then
    dnf install -y -q nodejs npm >/dev/null
  elif command -v apk >/dev/null 2>&1; then
    apk add -q nodejs npm
  elif command -v pacman >/dev/null 2>&1; then
    pacman -S --noconfirm --needed nodejs npm >/dev/null
  fi
  # One npm install per CLI: in MUXEN_NPM a CLI's flags come before its
  # package and are its own. In one command for all, Pi's --ignore-scripts
  # would also stop the install scripts the others fetch their binaries with
  # (which npm 12 runs only when told to: their --allow-scripts).
  flags=
  failed=
  for word in $MUXEN_NPM; do
    case "$word" in
      -*) flags="$flags $word" ;;
      *)
        # shellcheck disable=SC2086
        npm install -g --no-fund --no-audit $flags "$word" >/dev/null 2>&1 || { echo "npm couldn't install $word"; failed=1; }
        flags=
        ;;
    esac
  done
  [ -n "$failed" ] || { mkdir -p /var/lib && touch /var/lib/muxen-npm-done; }
fi
ssh-keygen -A >/dev/null 2>&1
# The locale a Mac sends (en_US.UTF-8) isn't installed here; UTF-8 is.
sed -i 's/^AcceptEnv/#AcceptEnv/' /etc/ssh/sshd_config 2>/dev/null
grep -q '^LANG=' /etc/environment 2>/dev/null || echo 'LANG=C.UTF-8' >> /etc/environment
mkdir -p /run/sshd /var/run/sshd
# Paired for direct connections before it stopped: listen again (a
# container has no service manager to do it).
if [ -d "$home/.muxen/direct" ] && [ -x "$home/.muxen/bin/muxen-agent" ]; then
  su -s /bin/sh "$u" -c "cd ~ && nohup ~/.muxen/bin/muxen-agent listen >/dev/null 2>&1 &" 2>/dev/null
fi
echo "Ready"
exec /usr/sbin/sshd -D -e
