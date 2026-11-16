#!/bin/sh
# Checks, on a real machine, what the desktop view does with a mouse button
# let go outside it. It has to be let go on the machine too: a Control-click
# (a Mac's right click) let go off the view once left the right button held
# there, and every click after it was a right click.
#
#   scripts/check-desktop-buttons.sh <ssh host>     # beelink, say
#
# What it does, so point it at a machine that's yours to test on:
# - Opens the built app (target/release/muxin, or MUXIN_BIN) on this Mac for
#   half a minute, on the built-in screen, with a setup of its own that
#   knows only that machine (over ssh, never paired). Nobody's mouse is
#   used: the events are put in the app's own queue (`--view mouse:...`,
#   src/ui/mouse_hook.rs).
# - Presses and drags at the middle of the machine's screen. Leave that
#   part of its desktop empty.
# - Puts this checkout's agent on the machine when it has another
#   (dist/agents, or MUXEN_AGENTS_DIR; scripts/build-agents.sh makes them).
# - Watches the machine's mouse buttons with a few lines of Python there
#   (python3 and libX11, an X11 desktop on DISPLAY=:0), and removes them.
#
# Needs a Mac here (the hook is a Mac's) and X11 there. Exits 0 when every
# button went down and up as it should, 1 when one didn't.
set -eu
cd "$(dirname "$0")/.."

host=${1:-}
[ -n "$host" ] || { echo "usage: scripts/check-desktop-buttons.sh <ssh host>" >&2; exit 2; }
[ "$(uname -s)" = Darwin ] || { echo "error: this check runs the Mac app" >&2; exit 2; }
bin=${MUXIN_BIN:-$PWD/target/release/muxin}
[ -x "$bin" ] || { echo "error: no app at $bin (cargo build --release)" >&2; exit 2; }
agents=${MUXEN_AGENTS_DIR:-$PWD/dist/agents}
ls "$agents"/muxen-agent-* >/dev/null 2>&1 || { echo "error: no agent builds in $agents (scripts/build-agents.sh)" >&2; exit 2; }

x11='DISPLAY=:0 XAUTHORITY=$HOME/.Xauthority'
watch=/tmp/muxin-watch-buttons
home=$(mktemp -d)
app=
cleanup() {
    [ -z "$app" ] || kill "$app" 2>/dev/null || true
    rm -rf "$home"
    ssh -o BatchMode=yes "$host" "rm -f $watch.py $watch.log" 2>/dev/null || true
}
trap cleanup EXIT

# A setup of its own: this machine only, over ssh.
printf '[[machine]]\nname = "Checked"\nhost = "%s"\n\n[settings]\nconnect_directly = false\n' "$host" > "$home/config.toml"
export MUXIN_HOME="$home" MUXEN_HOME="$home" MUXEN_AGENTS_DIR="$agents"
echo "▸ reaching $host"
"$bin" status --refresh --json >/dev/null

# The watcher: every change of the X pointer's buttons, and where it is. It
# only reads (XQueryPointer).
ssh -o BatchMode=yes "$host" "cat > $watch.py" <<'EOF'
import ctypes, ctypes.util, sys, time

seconds = float(sys.argv[1])
x11 = ctypes.CDLL(ctypes.util.find_library("X11"))
x11.XOpenDisplay.restype = ctypes.c_void_p
x11.XDefaultRootWindow.restype = ctypes.c_ulong
x11.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
x11.XQueryPointer.argtypes = [ctypes.c_void_p, ctypes.c_ulong] + [ctypes.c_void_p] * 7
display = x11.XOpenDisplay(None)
if not display:
    sys.exit("no display")
root = x11.XDefaultRootWindow(display)
r, c = ctypes.c_ulong(), ctypes.c_ulong()
rx, ry, wx, wy, mask = ctypes.c_int(), ctypes.c_int(), ctypes.c_int(), ctypes.c_int(), ctypes.c_uint()
names = {0x100: "left", 0x200: "middle", 0x400: "right"}
start, last = time.time(), None
while time.time() - start < seconds:
    x11.XQueryPointer(display, root, ctypes.byref(r), ctypes.byref(c), ctypes.byref(rx), ctypes.byref(ry), ctypes.byref(wx), ctypes.byref(wy), ctypes.byref(mask))
    held = mask.value & 0x700
    if held != last:
        down = "+".join(n for bit, n in names.items() if held & bit) or "none"
        print(f"{time.time() - start:.2f} held={down} at {rx.value},{ry.value}", flush=True)
        last = held
    time.sleep(0.004)
EOF

# A button already held there would make everything after it read wrong.
before=$(ssh -o BatchMode=yes "$host" "$x11 python3 $watch.py 0.2")
case "$before" in
    *held=none*) ;;
    *) echo "error: a mouse button is already held on $host: $before" >&2; exit 1 ;;
esac

ssh -o BatchMode=yes "$host" "$x11 nohup python3 $watch.py 34 > $watch.log 2>&1 &"

# The window is 1240 by 900 points, so 723,480 is on the picture, 723,20 on
# Muxin's own top bar, and -100,480 off the window.
steps="mouse:18000:move:left:723:480"
# A Control-click on the picture, let go off the window.
steps="$steps,mouse:18400:down:ctrl:723:480,mouse:18800:drag:ctrl:300:480,mouse:19100:up:ctrl:-100:480"
# A plain click.
steps="$steps,mouse:21000:down:left:723:480,mouse:21300:up:left:723:480"
# A drag, let go over the top bar.
steps="$steps,mouse:23000:down:left:723:480,mouse:23400:drag:left:723:300,mouse:23700:drag:left:723:20,mouse:24000:up:left:723:20"
# A click on the top bar, which isn't the machine's.
steps="$steps,mouse:26000:down:left:723:20,mouse:26300:up:left:723:20"

echo "▸ the app, for half a minute"
"$bin" open --view "display:builtin,size:1240x900,desktop:checked,$steps" >/dev/null 2>&1 &
app=$!
sleep 30
# (Waited for at once, so the shell doesn't say it was terminated.)
kill "$app" 2>/dev/null || true
wait "$app" 2>/dev/null || true
app=
# The watcher runs a little longer than the app.
sleep 5

log=$(ssh -o BatchMode=yes "$host" "cat $watch.log")
echo "$log" | sed 's/^/  /'
went=$(echo "$log" | sed -n 's/.*held=\([a-z+]*\).*/\1/p' | tr '\n' ' ')
want="none right none left none left none "
if [ "$went" != "$want" ]; then
    echo "✗ the buttons went: $went" >&2
    echo "  they should go:   $want" >&2
    exit 1
fi
# Where each was let go: at the picture's left edge, then at its top one.
edges=$(echo "$log" | sed -n 's/.*held=none at \([0-9]*\),\([0-9]*\)$/\1 \2/p')
left_edge=$(echo "$edges" | sed -n 2p | cut -d' ' -f1)
top_edge=$(echo "$edges" | sed -n 4p | cut -d' ' -f2)
if [ "$left_edge" != 0 ] || [ "$top_edge" != 0 ]; then
    echo "✗ let go off the picture, a button should be let go at its nearest edge (x 0, then y 0): x $left_edge, y $top_edge" >&2
    exit 1
fi
echo "✓ every button let go outside the view was let go on $host, and a click on Muxin's own bar wasn't sent"
