# Creates a VM for Muxin on this machine: Windows in Docker with KVM
# (dockur/windows), or Linux in a container that runs sshd.
# Prepended: OR_OS OR_NAME OR_IMAGE OR_VERSION OR_CPUS OR_MEM OR_DISK OR_USER
# OR_PASS OR_KEY OR_HOSTNAME OR_SETUP OR_NPM. Prints ssh_port=N, udp_port=N
# (and web_port=N).
set -u
PATH="$PATH:/usr/local/bin:/opt/homebrew/bin:/usr/sbin:/sbin"
fail() { echo "error: $*" >&2; exit 1; }

command -v docker >/dev/null 2>&1 || fail "Docker isn't installed on $(hostname)."
docker version >/dev/null 2>&1 || fail "Docker is installed, but $(id -un) can't use it (add it to the docker group)."
docker inspect "$OR_NAME" >/dev/null 2>&1 && fail "A container called $OR_NAME is already there."

# Ports in use: listeners, and every container's bindings (stopped ones too).
bound() {
  ids=$(docker ps -aq 2>/dev/null)
  # shellcheck disable=SC2086
  [ -n "$ids" ] && docker inspect --format '{{range $p, $b := .HostConfig.PortBindings}}{{range $b}}{{.HostPort}} {{end}}{{end}}' $ids 2>/dev/null | tr ' ' '\n'
}
in_use() {
  { ss -Hltn 2>/dev/null | awk '{print $4}'; netstat -an 2>/dev/null | awk '/LISTEN/ {print $4}'; } | grep -Eq "[:.]$1\$" && return 0
  bound | grep -qx "$1"
}
free_port() { p=$1; while in_use "$p"; do p=$((p + 1)); done; echo "$p"; }
ssh_port=$(free_port 2201)
# Muxin's direct connections (QUIC) to the VM's agent: a UDP port on every
# address here, forwarded to the agent's 47821 inside (it only talks to
# apps it's paired with).
udp_in_use() {
  { ss -Hlun 2>/dev/null | awk '{print $4}'; netstat -an -p udp 2>/dev/null | awk '{print $4}'; } | grep -Eq "[:.]$1\$" && return 0
  bound | grep -qx "$1"
}
udp_port=47830
while udp_in_use "$udp_port"; do udp_port=$((udp_port + 1)); done

case "$OR_OS" in
windows)
  [ "$(uname -s)" = Linux ] || fail "Windows VMs need a Linux machine with KVM; $(hostname) runs $(uname -s)."
  [ "$(uname -m)" = x86_64 ] || fail "Windows VMs need an x86-64 machine; $(hostname) is $(uname -m)."
  [ -e /dev/kvm ] || fail "$(hostname) has no /dev/kvm: turn on virtualization (VT-x or AMD-V) in its firmware settings."
  dir="$HOME/.muxen/vms/$OR_NAME"
  mkdir -p "$dir/storage" "$dir/oem" || fail "Couldn't make $dir."
  free=$(df -Pk "$dir" | awk 'NR == 2 { print int($4 / 1048576) }')
  [ "${free:-0}" -ge 30 ] || fail "$(hostname) has ${free:-0} GB free; Windows needs about 30 GB."
  web_port=$(free_port 8007)

  # Windows runs install.bat once, at the end of its setup: OpenSSH with
  # this computer's key, and PowerShell as its shell.
  printf '%s\n' "$OR_KEY" > "$dir/oem/muxen.pub"
  printf '@echo off\r\npowershell -NoProfile -ExecutionPolicy Bypass -File C:\\OEM\\muxen.ps1 > C:\\OEM\\muxen.log 2>&1\r\n' > "$dir/oem/install.bat"
  cat > "$dir/oem/muxen.ps1" <<'PS1'
$ProgressPreference = 'SilentlyContinue'
Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0
$dir = Join-Path $env:ProgramData 'ssh'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$keys = Join-Path $dir 'administrators_authorized_keys'
Copy-Item 'C:\OEM\muxen.pub' $keys -Force
icacls $keys /inheritance:r /grant '*S-1-5-32-544:F' /grant '*S-1-5-18:F' | Out-Null
New-ItemProperty -Path HKLM:\SOFTWARE\OpenSSH -Name DefaultShell -Value "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -PropertyType String -Force | Out-Null
Set-Service -Name sshd -StartupType Automatic
Start-Service sshd
New-NetFirewallRule -Name muxen-sshd -DisplayName 'OpenSSH (Muxin)' -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 -ErrorAction SilentlyContinue | Out-Null
'ready' | Out-File C:\OEM\muxen.done
PS1
  # The account's name and password in a file only this user reads, not on a command line.
  umask 077
  printf 'USERNAME=%s\nPASSWORD=%s\n' "$OR_USER" "$OR_PASS" > "$dir/env"
  echo "▸ Starting $OR_NAME (Docker fetches the dockurr/windows image the first time)"
  docker run -d --name "$OR_NAME" --label muxen.vm="$OR_NAME" \
    -e VERSION="$OR_VERSION" -e RAM_SIZE="$OR_MEM" -e CPU_CORES="$OR_CPUS" -e DISK_SIZE="$OR_DISK" \
    --env-file "$dir/env" \
    -p "127.0.0.1:$ssh_port:22" -p "127.0.0.1:$web_port:8006" -p "$udp_port:47821/udp" \
    --device=/dev/kvm --device=/dev/net/tun --cap-add NET_ADMIN \
    -v "$dir/storage:/storage" -v "$dir/oem:/oem" \
    --stop-timeout 120 dockurr/windows >/dev/null
  status=$?
  rm -f "$dir/env"
  [ $status -eq 0 ] || fail "Docker couldn't start it."
  echo "ssh_port=$ssh_port"
  echo "web_port=$web_port"
  echo "udp_port=$udp_port"
  ;;
linux)
  echo "▸ Starting $OR_NAME from $OR_IMAGE"
  # shellcheck disable=SC2086
  docker run -d --name "$OR_NAME" --label muxen.vm="$OR_NAME" --hostname "$OR_HOSTNAME" \
    -p "127.0.0.1:$ssh_port:22" -p "$udp_port:47821/udp" -e MUXEN_USER="$OR_USER" -e MUXEN_KEY="$OR_KEY" -e MUXEN_NPM="$OR_NPM" \
    ${OR_CPUS:+--cpus "$OR_CPUS"} ${OR_MEM:+--memory "$OR_MEM"} \
    "$OR_IMAGE" sh -c "$OR_SETUP" >/dev/null || fail "Docker couldn't start it."
  echo "ssh_port=$ssh_port"
  echo "udp_port=$udp_port"
  ;;
*) fail "Muxin can't make a $OR_OS VM." ;;
esac
