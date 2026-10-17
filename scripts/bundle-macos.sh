#!/bin/sh
# Builds Muxin.app (release build + Info.plist + icon) into dist/.
# The bundle runs the same binary: with no arguments it opens the window, and
# `Muxin.app/Contents/MacOS/muxin mcp` still works for agents. Its bundle id
# stays com.lumarylabs.muxen (from when it was called Muxen), so macOS keeps
# its Keychain items and permissions across the rename.
#
#   --universal    one app for Apple silicon and Intel Macs.
#   --release      --universal, signed for notarization: hardened runtime and a
#                  secure timestamp. Needs the Developer ID certificate.
#                  scripts/notarize-macos.sh then notarizes it and makes the DMG.
#   --skip-agents  use the agent builds already in dist/agents (CI builds them
#                  in a job of their own).
set -eu

cd "$(dirname "$0")/.."
release=
universal=
skip_agents=
for arg in "$@"; do
    case $arg in
        --release) release=1 universal=1 ;;
        --universal) universal=1 ;;
        --skip-agents) skip_agents=1 ;;
        *) echo "usage: $0 [--universal | --release] [--skip-agents]" >&2; exit 2 ;;
    esac
done
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
app=dist/Muxin.app

if [ -n "$universal" ]; then
    cargo build --release --locked --target aarch64-apple-darwin
    cargo build --release --locked --target x86_64-apple-darwin
    mkdir -p target/universal
    lipo -create -output target/universal/muxin \
        target/aarch64-apple-darwin/release/muxin target/x86_64-apple-darwin/release/muxin
    binary=target/universal/muxin
else
    cargo build --release
    binary=target/release/muxin
fi
# The agent builds the app installs on each machine it connects to.
if [ -z "$skip_agents" ]; then
    MUXEN_RELEASE=$release ./scripts/build-agents.sh >/dev/null
fi
ls dist/agents/muxen-agent-* >/dev/null 2>&1 || { echo "error: no agent builds in dist/agents" >&2; exit 1; }
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/agents"
cp "$binary" "$app/Contents/MacOS/muxin"
cp assets/brand/Muxin.icns "$app/Contents/Resources/Muxin.icns"
cp dist/agents/muxen-agent-* "$app/Contents/Resources/agents/"
# Its license, and the third-party ones that must ship with every copy.
cp LICENSE NOTICE THIRD_PARTY_NOTICES.md "$app/Contents/Resources/"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Muxin</string>
  <key>CFBundleDisplayName</key><string>Muxin</string>
  <key>CFBundleIdentifier</key><string>com.lumarylabs.muxen</string>
  <key>CFBundleExecutable</key><string>muxin</string>
  <key>CFBundleIconFile</key><string>Muxin</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSHumanReadableCopyright</key><string>© Lumary Labs LLC</string>
  <key>NSLocalNetworkUsageDescription</key><string>Muxin connects straight to your machines on your local network: faster than going through ssh or a VPN, and needed to show their desktops smoothly.</string>
</dict>
</plist>
PLIST

# Signed with the Developer ID when there is one (MUXEN_SIGN_IDENTITY, else the
# first in the keychain), so the app keeps one identity across builds: macOS
# keeps its Keychain items (cloud sign-ins) and permissions. Not --deep: the
# agents inside are already signed with their own identifier, which the
# permissions on the user's Macs (Screen Recording, Accessibility) belong to.
# By fingerprint: a keychain can hold several certificates of the same name,
# and codesign refuses an ambiguous one.
identity=${MUXEN_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null | sed -n 's/^ *[0-9][0-9]*) \([0-9A-F]\{40\}\) "Developer ID Application: .*/\1/p' | head -n1)}
if [ -n "$release" ]; then
    [ -n "$identity" ] || { echo "error: a release needs a Developer ID Application certificate in the keychain" >&2; exit 1; }
    codesign --force --options runtime --timestamp --identifier com.lumarylabs.muxen -s "$identity" "$app"
    codesign --verify --strict --verbose=2 "$app"
elif [ -n "$identity" ]; then
    codesign --force --timestamp=none --identifier com.lumarylabs.muxen -s "$identity" "$app" >/dev/null 2>&1 || echo "warning: signing with $identity failed" >&2
else
    codesign --force --sign - "$app" >/dev/null 2>&1 || echo "warning: codesign failed; the app still runs locally" >&2
fi
echo "Built $app ($version)"
