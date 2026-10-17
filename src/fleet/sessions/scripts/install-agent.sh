# d is the agent's bin folder; the upload arrives on stdin.
set -e
n="$d/muxen-agent.new.$$"
t="$d/muxen-agent"
p="$d/muxen-agent.prev"
expected='__EXPECTED_HASH__'
swapped=no
previous=no
manager=none
restarted=no
finish() {
    result=$?
    trap - EXIT HUP INT TERM
    if [ "$result" -ne 0 ] && [ "$swapped" = yes ]; then
        if [ "$previous" = yes ]; then
            if cp -p "$p" "$n" && mv -f "$n" "$t"; then
                echo 'Restored the previous agent.' >&2
            else
                echo 'Could not restore the previous agent.' >&2
                exit 1
            fi
        elif [ -e "$t" ]; then
            unlink "$t" || echo 'Could not remove the failed agent.' >&2
        fi
        if [ "$restarted" = yes ]; then
            case "$manager" in
                systemd) systemctl --user restart muxen-agent.service >&2 || echo 'Could not restart the previous agent.' >&2 ;;
                launchd) launchctl kickstart -k "gui/$(id -u)/com.lumarylabs.muxen-agent" >&2 || echo 'Could not restart the previous agent.' >&2 ;;
            esac
        fi
    fi
    [ ! -e "$n" ] || unlink "$n"
    [ ! -e "$p.new.$$" ] || unlink "$p.new.$$"
    exit "$result"
}
trap finish EXIT
trap 'exit 1' HUP INT TERM
cat > "$n"
chmod 755 "$n"
got=$("$n" hash) || { echo 'The uploaded agent could not run.' >&2; exit 1; }
[ "$got" = "$expected" ] || { echo 'The uploaded agent has the wrong hash.' >&2; exit 1; }
# Copy first: the live path remains in place until the atomic rename.
if [ -e "$t" ]; then
    cp -p "$t" "$p.new.$$"
    mv -f "$p.new.$$" "$p"
    previous=yes
fi
mv -f "$n" "$t"
swapped=yes
got=$("$t" hash) || { echo 'The new agent failed its health check.' >&2; exit 1; }
[ "$got" = "$expected" ] || { echo 'The new agent failed its health check.' >&2; exit 1; }
"$t" migrate >/dev/null 2>&1 || true
if systemctl --user is-enabled muxen-agent.service >/dev/null 2>&1 || systemctl --user is-active muxen-agent.service >/dev/null 2>&1; then
    manager=systemd
elif launchctl print "gui/$(id -u)/com.lumarylabs.muxen-agent" >/dev/null 2>&1; then
    manager=launchd
fi
if [ "$manager" != none ]; then
    restarted=yes
    pkill -f "$d/[m]uxen-agent listen" >/dev/null 2>&1 || true
    case "$manager" in
        systemd) systemctl --user restart muxen-agent.service ;;
        launchd) launchctl kickstart -k "gui/$(id -u)/com.lumarylabs.muxen-agent" ;;
    esac
fi
# The service has been restarted only after both hash checks passed.
swapped=no
[ ! -e "$d/openrunner-agent" ] || unlink "$d/openrunner-agent" || true
printf '%s\n' "$got"
