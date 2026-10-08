# Helpers shared by workspace scripts.

# A process and all of its descendants (portable: ps on macOS and Linux).
or_tree() {
  ps -A -o pid= -o ppid= 2>/dev/null | awk -v root="$1" '
    { parent[$1] = $2 }
    END {
      keep[root] = 1; print root; changed = 1
      while (changed) {
        changed = 0
        for (p in parent) if (!(p in keep) && (parent[p] in keep)) { keep[p] = 1; print p; changed = 1 }
      }
    }'
}

# or_kill_tree PID SIGNAL
or_kill_tree() {
  for or_p in $(or_tree "$1"); do kill -"$2" "$or_p" 2>/dev/null; done
}
