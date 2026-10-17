#!/bin/sh
# Notarizes dist/Muxin.app (built by scripts/bundle-macos.sh --release), staples
# it, and packs it into dist/Muxin-<version>-macos.dmg, itself signed, notarized
# and stapled, so it opens without a warning even offline.
#
#   --no-notarize  only make the DMG, as it is (for a test build: macOS warns
#                  about an app that isn't notarized).
#
# Apple's notary service needs one of:
#   APPLE_API_KEY_PATH, APPLE_API_KEY_ID, APPLE_API_ISSUER_ID: an App Store
#       Connect API key (what CI uses), or
#   a notarytool keychain profile, MUXEN_NOTARY_PROFILE (default "muxen"),
#       saved once with `xcrun notarytool store-credentials muxen ...`.
# See RELEASING.md.
set -eu

cd "$(dirname "$0")/.."
notarize=1
case "${1:-}" in
    --no-notarize) notarize= ;;
    "") ;;
    *) echo "usage: $0 [--no-notarize]" >&2; exit 2 ;;
esac
app=dist/Muxin.app
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
dmg=dist/Muxin-$version-macos.dmg
[ -d "$app" ] || { echo "error: no $app: run scripts/bundle-macos.sh --release first" >&2; exit 1; }

if [ -n "${APPLE_API_KEY_PATH:-}" ]; then
    set -- --key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID"
else
    set -- --keychain-profile "${MUXEN_NOTARY_PROFILE:-muxen}"
fi

# Sends a file to the notary service and waits; shows Apple's log when it's refused.
notarize() { # <file> <credentials...>
    file=$1
    shift
    echo "▸ notarizing $(basename "$file")"
    out=$(xcrun notarytool submit "$file" "$@" --wait --output-format json) || true
    status=$(printf '%s' "$out" | sed -n 's/.*"status":"\([^"]*\)".*/\1/p')
    id=$(printf '%s' "$out" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    if [ "$status" != "Accepted" ]; then
        echo "error: notarization ${status:-failed}: $out" >&2
        [ -n "$id" ] && xcrun notarytool log "$id" "$@" >&2 || true
        exit 1
    fi
}

# Attaches the notary ticket. Apple's ticket server can drop the connection, or
# not have the ticket yet right after notarizing, so it tries a few times.
staple() { # <file>
    for try in 1 2 3 4 5; do
        xcrun stapler staple "$1" && return
        [ "$try" = 5 ] && break
        echo "▸ stapling failed; trying again in 20 seconds" >&2
        sleep 20
    done
    echo "error: couldn't staple $1" >&2
    exit 1
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The app first, so it's stapled (and opens offline) once it's out of the DMG.
if [ -n "$notarize" ]; then
    codesign --verify --strict "$app"
    ditto -c -k --keepParent "$app" "$work/Muxin.zip"
    notarize "$work/Muxin.zip" "$@"
    staple "$app"
fi

# Then the DMG: the app and a link to Applications to drag it to.
mkdir "$work/dmg"
ditto "$app" "$work/dmg/Muxin.app"
ln -s /Applications "$work/dmg/Applications"
rm -f "$dmg"
hdiutil create -quiet -volname "Muxin $version" -srcfolder "$work/dmg" -fs HFS+ -format UDZO "$dmg"
if [ -n "$notarize" ]; then
    # By fingerprint: a keychain can hold several certificates of the same name.
    identity=${MUXEN_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null | sed -n 's/^ *[0-9][0-9]*) \([0-9A-F]\{40\}\) "Developer ID Application: .*/\1/p' | head -n1)}
    codesign --force --timestamp -s "$identity" "$dmg"
    notarize "$dmg" "$@"
    staple "$dmg"
    spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg"
fi
echo "Built $dmg ($(du -h "$dmg" | cut -f1))"
