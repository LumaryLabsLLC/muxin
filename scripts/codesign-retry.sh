#!/bin/sh
# Runs codesign with the arguments given, and runs it again when Apple's
# timestamp server doesn't answer ("A timestamp was expected but was not
# found"). That server can be down for a few minutes, as it was for the 0.17.0
# release. Any other failure stops at once, and so does the fifth miss, with
# codesign's own exit code. For signatures with --timestamp; one with
# --timestamp=none asks no server and needs no second try.
#
#   scripts/codesign-retry.sh --force --options runtime --timestamp -s <identity> <path>
#
# MUXEN_CODESIGN_PAUSE sets the first pause in seconds (default 15); it doubles
# after each miss: 15, 30, 60, 120.
set -eu

errors=$(mktemp)
trap 'rm -f "$errors"' EXIT
pause=${MUXEN_CODESIGN_PAUSE:-15}
for try in 1 2 3 4 5; do
    status=0
    codesign "$@" 2>"$errors" || status=$?
    cat "$errors" >&2
    [ "$status" = 0 ] && exit 0
    grep -q "A timestamp was expected but was not found" "$errors" || exit "$status"
    [ "$try" = 5 ] && break
    echo "▸ Apple's timestamp server didn't answer; signing again in $pause seconds (try $((try + 1)) of 5)" >&2
    sleep "$pause"
    pause=$((pause * 2))
done
echo "error: no timestamp from Apple after 5 tries; check http://timestamp.apple.com/ts01 and run the job again" >&2
exit "$status"
