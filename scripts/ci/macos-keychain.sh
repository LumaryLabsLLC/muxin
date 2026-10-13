#!/bin/sh
# CI only: puts the Developer ID certificate (MACOS_CERT_P12, base64 of the .p12,
# and MACOS_CERT_PASSWORD) in a keychain of its own on the build machine, for
# codesign. And, when an App Store Connect API key is given (APPLE_API_KEY_P8,
# the .p8's contents), writes it where scripts/notarize-macos.sh finds it.
# The runner, and everything on it, is thrown away after the job.
set -eu

keychain=$RUNNER_TEMP/muxen-signing.keychain-db
password=$(openssl rand -hex 24)
printf '%s' "$MACOS_CERT_P12" | base64 --decode > "$RUNNER_TEMP/cert.p12"
security create-keychain -p "$password" "$keychain"
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$password" "$keychain"
security import "$RUNNER_TEMP/cert.p12" -k "$keychain" -P "$MACOS_CERT_PASSWORD" -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$password" "$keychain" >/dev/null
security list-keychains -d user -s "$keychain" $(security list-keychains -d user | tr -d '"')
rm -f "$RUNNER_TEMP/cert.p12"
security find-identity -v -p codesigning "$keychain" | grep -q "Developer ID Application" \
    || { echo "error: no Developer ID Application identity in MACOS_CERT_P12" >&2; exit 1; }

if [ -n "${APPLE_API_KEY_P8:-}" ]; then
    printf '%s\n' "$APPLE_API_KEY_P8" > "$RUNNER_TEMP/AuthKey.p8"
    echo "APPLE_API_KEY_PATH=$RUNNER_TEMP/AuthKey.p8" >> "$GITHUB_ENV"
fi
echo "▸ signing identity ready"
