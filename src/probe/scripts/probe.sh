# OpenRunner machine probe (macOS / Linux). Read-only.
# Prints key=value lines; runner N is reported as rN.<key>=<value>.
# OR_DIRS (newline-separated glob patterns) is prepended by OpenRunner.

LC_ALL=C
export LC_ALL
PATH="$PATH:/usr/sbin:/sbin:/usr/bin:/bin:/usr/local/bin"

kv() { printf '%s=%s\n' "$1" "$2"; }

os=$(uname -s 2>/dev/null | tr 'ABCDEFGHIJKLMNOPQRSTUVWXYZ' 'abcdefghijklmnopqrstuvwxyz')
now=$(date +%s)
kv probe 1
kv os "$os"
kv arch "$(uname -m 2>/dev/null)"
kv hostname "$(hostname 2>/dev/null)"

case "$os" in
darwin)
  kv os_version "macOS $(sw_vers -productVersion 2>/dev/null)"
  kv cores "$(sysctl -n hw.ncpu 2>/dev/null)"
  kv load "$(sysctl -n vm.loadavg 2>/dev/null | tr -d '{}')"
  boot=$(sysctl -n kern.boottime 2>/dev/null | sed -n 's/^{ sec = \([0-9]*\),.*/\1/p')
  [ -n "$boot" ] && kv uptime_s "$((now - boot))"
  memsize=$(sysctl -n hw.memsize 2>/dev/null)
  kv mem_total "$memsize"
  # "Memory used" as Activity Monitor shows it: app memory + wired + compressed.
  vm_stat 2>/dev/null | awk -v total="$memsize" '
    function n(s) { sub(/\.$/, "", s); return s + 0 }
    /page size of/                 { ps = $8 }
    /^Anonymous pages/             { anon = n($3) }
    /^Pages purgeable/             { purg = n($3) }
    /^Pages active/                { active = n($3) }
    /^Pages wired down/            { wired = n($4) }
    /^Pages occupied by compressor/ { comp = n($5) }
    END {
      if (!ps) exit
      used = (anon > 0) ? (anon - purg + wired + comp) * ps : (active + wired + comp) * ps
      if (total > 0 && used > total) used = total
      printf "mem_used=%.0f\n", used
    }'
  # Second sample of top is the CPU usage over the last second.
  top -l 2 -n 0 -s 1 2>/dev/null | awk '/CPU usage/ { idle = $7 } END { sub(/%/, "", idle); if (idle != "") printf "cpu_pct=%.1f\n", 100 - idle }'
  disk=/
  [ -d /System/Volumes/Data ] && disk=/System/Volumes/Data
  ;;
linux)
  if [ -r /etc/os-release ]; then
    kv os_version "$(. /etc/os-release 2>/dev/null; printf '%s' "${PRETTY_NAME:-Linux}")"
  else
    kv os_version "Linux $(uname -r 2>/dev/null)"
  fi
  kv cores "$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null)"
  kv load "$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null)"
  kv uptime_s "$(cut -d. -f1 /proc/uptime 2>/dev/null)"
  awk '/^MemTotal:/ { t = $2 } /^MemAvailable:/ { a = $2 }
       END { if (t) printf "mem_total=%.0f\nmem_used=%.0f\n", t * 1024, (t - a) * 1024 }' /proc/meminfo 2>/dev/null
  s1=$(head -n1 /proc/stat 2>/dev/null)
  sleep 1
  s2=$(head -n1 /proc/stat 2>/dev/null)
  printf '%s\n%s\n' "$s1" "$s2" | awk '
    { t = 0; for (i = 2; i <= NF; i++) t += $i; idle = $5 + $6 }
    NR == 1 { t1 = t; i1 = idle }
    NR == 2 { dt = t - t1; if (dt > 0) printf "cpu_pct=%.1f\n", (1 - (idle - i1) / dt) * 100 }'
  disk=/
  ;;
*)
  kv os_version "$(uname -sr 2>/dev/null)"
  disk=/
  ;;
esac

df -kP "$disk" 2>/dev/null | awk 'NR == 2 { printf "disk_total=%.0f\ndisk_free=%.0f\ndisk_path=%s\n", $2 * 1024, $4 * 1024, $6 }'

# --- Runners -----------------------------------------------------------------
# A runner is a folder with a .runner config. Find them from running
# Runner.Listener / Runner.Worker processes and from the usual install places.

procs=$(ps -A -o pid= -o command= 2>/dev/null | grep -E '/bin/Runner\.(Listener|Worker)( |$)' | grep -v grep)

candidates() {
  printf '%s\n' "$procs" | awk '{
    for (i = 2; i <= NF; i++) if ($i ~ /^\/.*\/bin\/Runner\.(Listener|Worker)$/) { sub(/\/bin\/Runner\.[A-Za-z]*$/, "", $i); print $i; break }
  }'
  old_ifs=$IFS
  IFS='
'
  for pattern in $OR_DIRS \
    "$HOME"/actions-runner/* "$HOME"/actions-runner* "$HOME"/runners/* "$HOME"/actions-runners/* "$HOME"/actions/* \
    /opt/actions-runner* /opt/runners/* /opt/actions-runners/* /srv/actions-runner* /srv/runners/* \
    /var/lib/actions-runner* /Users/*/actions-runner* /home/*/actions-runner*; do
    case "$pattern" in "~"|"~/"*) pattern="$HOME${pattern#\~}" ;; esac
    for d in $pattern; do
      [ -f "$d/.runner" ] && printf '%s\n' "${d%/}"
    done
  done
  IFS=$old_ifs
}

pid_of() { printf '%s\n' "$procs" | awk -v bin="$1" '{ for (i = 2; i <= NF; i++) if ($i == bin) { print $1; exit } }'; }

candidates | sort -u | {
  i=0
  while IFS= read -r d; do
    [ -n "$d" ] && [ -f "$d/.runner" ] || continue
    p="r$i"
    kv "$p.dir" "$d"
    kv "$p.config" "$(tr -d '\r\n' < "$d/.runner" 2>/dev/null)"
    kv "$p.listener" "$(pid_of "$d/bin/Runner.Listener")"
    kv "$p.worker" "$(pid_of "$d/bin/Runner.Worker")"
    if [ -x "$d/bin/Runner.Listener" ]; then
      # Some builds print log lines too; the version is the bare x.y.z line.
      kv "$p.version" "$("$d/bin/Runner.Listener" --version 2>/dev/null | grep -E '^v?[0-9]+\.[0-9]+\.[0-9]+' | tail -n1)"
    fi
    if [ -f "$d/.service" ]; then
      svc=$(head -n1 "$d/.service" 2>/dev/null)
      kv "$p.service" "$svc"
      state=unknown
      case "$os" in
        darwin)
          svc_name=$(basename "$svc" .plist)
          if launchctl list "$svc_name" >/dev/null 2>&1; then state=active; else state=inactive; fi ;;
        linux)
          if command -v systemctl >/dev/null 2>&1; then
            if systemctl is-active --quiet "$svc" 2>/dev/null; then state=active; else state=inactive; fi
          fi ;;
      esac
      kv "$p.service_state" "$state"
    fi
    i=$((i + 1))
  done
}

# --- sudo (Linux) --------------------------------------------------------------
# Whether a systemd service can be installed without a password prompt.
if [ "$os" = linux ]; then
  if [ "$(id -u)" = 0 ]; then
    kv sudo root
  elif command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
    kv sudo passwordless
  else
    kv sudo password
  fi
fi

# --- Docker -------------------------------------------------------------------
# Runner containers: the ones OpenRunner made (label openrunner.runner) and any
# other container running GitHub's actions-runner image.
if command -v docker >/dev/null 2>&1; then
  dv=$(docker version --format '{{.Server.Version}}' 2>/dev/null)
  if [ -n "$dv" ]; then
    kv docker "$dv"
    docker ps -a --no-trunc --format '{{.Names}}|{{.State}}|{{.Image}}|{{.Label "openrunner.runner"}}|{{.Label "openrunner.scope"}}' 2>/dev/null | {
      j=0
      while IFS='|' read -r cname cstate cimage rname rscope; do
        case "$cimage" in
          *actions-runner*) ;;
          *) [ -n "$rname" ] || continue ;;
        esac
        p="c$j"
        kv "$p.container" "$cname"
        kv "$p.state" "$cstate"
        kv "$p.image" "$cimage"
        kv "$p.name" "$rname"
        kv "$p.scope" "$rscope"
        kv "$p.restarts" "$(docker inspect -f '{{.RestartCount}}' "$cname" 2>/dev/null)"
        if [ "$cstate" = running ]; then
          top=$(docker top "$cname" -o pid,args 2>/dev/null)
          kv "$p.listener" "$(printf '%s\n' "$top" | awk '/Runner\.Listener/ { print $1; exit }')"
          kv "$p.worker" "$(printf '%s\n' "$top" | awk '/Runner\.Worker/ { print $1; exit }')"
          kv "$p.config" "$(docker exec "$cname" sh -c 'cat /home/runner/.runner 2>/dev/null' 2>/dev/null | tr -d '\r\n')"
          kv "$p.version" "$(docker exec "$cname" sh -c '/home/runner/bin/Runner.Listener --version 2>/dev/null' 2>/dev/null | grep -E '^[0-9]+\.[0-9]+\.[0-9]+$' | tail -n1)"
        fi
        j=$((j + 1))
      done
    }
  else
    kv docker_error "$(docker version 2>&1 | grep -i -m1 -E 'permission denied|cannot connect|error' | cut -c1-200)"
  fi
fi

kv elapsed_s "$(( $(date +%s) - now ))"
