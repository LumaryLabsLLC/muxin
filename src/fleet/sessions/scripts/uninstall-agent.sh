# This runs over ssh, without starting or updating muxen-agent.
removed() { printf 'removed\t%s\n' "$1"; }
failed() { printf 'failed\t%s\n' "$1"; }
remove_path() {
    if [ -e "$1" ] || [ -L "$1" ]; then
        if problem=$(find "$1" -depth -delete 2>&1); then
            removed "$1"
        else
            failed "$1: $problem"
        fi
    fi
}
stop_launchd() {
    for domain in "gui/$uid" "user/$uid"; do
        if launchctl print "$domain/$1" >/dev/null 2>&1; then
            if problem=$(launchctl bootout "$domain/$1" 2>&1); then
                removed "Stopped $1"
            else
                failed "Could not stop $1: $problem"
            fi
        fi
    done
}
read_crontab() {
    cron_errors=$(mktemp "${TMPDIR:-/tmp}/muxin-crontab.XXXXXXXX") || {
        failed 'Could not check crontab: could not create a temporary file.'
        return 1
    }
    current=$(LC_ALL=C crontab -l 2>"$cron_errors")
    cron_status=$?
    if [ "$cron_status" -ne 0 ]; then
        problem=$(cat "$cron_errors")
        case "$cron_status:$problem" in
            '1:no crontab for '*|'1:crontab: no crontab for '*) ;;
            *) failed "Could not read crontab: ${problem:-exit $cron_status}" ;;
        esac
    fi
    unlink "$cron_errors" || failed 'Could not remove the temporary crontab error file.'
    return "$cron_status"
}
uid=$(id -u)
case '__OS__' in
    darwin)
        for label in com.lumarylabs.muxen-agent com.lumarylabs.openrunner-agent; do
            stop_launchd "$label"
            remove_path "$HOME/Library/LaunchAgents/$label.plist"
        done
        for plist in "$HOME/Library/LaunchAgents"/com.lumarylabs.muxen.schedule.*.plist; do
            [ -e "$plist" ] || continue
            label=${plist##*/}
            stop_launchd "${label%.plist}"
            remove_path "$plist"
        done
        ;;
    linux)
        reload=no
        for unit in muxen-agent.service openrunner-agent.service; do
            path="$HOME/.config/systemd/user/$unit"
            if [ -e "$path" ] || systemctl --user cat "$unit" >/dev/null 2>&1; then
                if problem=$(systemctl --user disable --now "$unit" 2>&1); then
                    removed "Stopped and disabled $unit"
                else
                    failed "Could not stop $unit: $problem"
                fi
                remove_path "$path"
                remove_path "$HOME/.config/systemd/user/default.target.wants/$unit"
                reload=yes
            fi
        done
        if [ "$reload" = yes ]; then
            problem=$(systemctl --user daemon-reload 2>&1) || failed "Could not reload systemd: $problem"
        fi
        # Linux schedules use a marked crontab block, not systemd timers.
        if command -v crontab >/dev/null 2>&1; then
            if read_crontab; then
                if printf '%s\n' "$current" | grep -Fqx '# muxen schedules: managed by Muxen (edits between these lines are replaced)'; then
                    if printf '%s\n' "$current" | grep -Fqx '# end of muxen schedules'; then
                        cleaned=$(printf '%s\n' "$current" | awk '
                            $0 == "# muxen schedules: managed by Muxen (edits between these lines are replaced)" { inside=1; next }
                            $0 == "# end of muxen schedules" { inside=0; next }
                            !inside { print }
                        ')
                        if problem=$(printf '%s\n' "$cleaned" | crontab - 2>&1); then
                            removed 'Muxen schedules from crontab'
                        else
                            failed "Could not remove Muxen schedules from crontab: $problem"
                        fi
                    else
                        failed 'Could not remove an unfinished Muxen block from crontab.'
                    fi
                fi
            fi
        elif [ -d "$HOME/.muxen/schedules" ]; then
            failed 'Could not check crontab: crontab is missing.'
        fi
        ;;
esac
# Services are gone first, so they cannot restart a process we stop here.
pattern="$HOME/.muxen/bin/[m]uxen-agent( |$)"
if command -v pgrep >/dev/null 2>&1 && command -v pkill >/dev/null 2>&1; then
    if pgrep -u "$uid" -f "$pattern" >/dev/null 2>&1; then
        pkill -u "$uid" -f "$pattern" 2>/dev/null || true
        count=0
        while pgrep -u "$uid" -f "$pattern" >/dev/null 2>&1 && [ "$count" -lt 20 ]; do
            sleep 0.1
            count=$((count + 1))
        done
        if pgrep -u "$uid" -f "$pattern" >/dev/null 2>&1; then
            pkill -KILL -u "$uid" -f "$pattern" 2>/dev/null || true
        fi
        if pgrep -u "$uid" -f "$pattern" >/dev/null 2>&1; then
            failed 'Could not stop every agent process.'
        else
            removed 'Stopped the agent processes'
        fi
    fi
else
    failed 'Could not check agent processes: pgrep or pkill is missing.'
fi
remove_path "$HOME/.muxen"
printf 'complete\n'
