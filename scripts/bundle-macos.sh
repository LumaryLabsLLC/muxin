#!/bin/sh
# Builds OpenRunner.app (release build + Info.plist + icon) into dist/.
# The bundle runs the same binary: with no arguments it opens the window, and
# `OpenRunner.app/Contents/MacOS/openrunner mcp` still works for agents.
set -eu

cd "$(dirname "$0")/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
app=dist/OpenRunner.app

cargo build --release
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/openrunner "$app/Contents/MacOS/openrunner"
cp assets/brand/OpenRunner.icns "$app/Contents/Resources/OpenRunner.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>OpenRunner</string>
  <key>CFBundleDisplayName</key><string>OpenRunner</string>
  <key>CFBundleIdentifier</key><string>dev.openrunner.OpenRunner</string>
  <key>CFBundleExecutable</key><string>openrunner</string>
  <key>CFBundleIconFile</key><string>OpenRunner</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Ad-hoc signature so Gatekeeper treats it as one app; replace with a Developer ID to distribute.
codesign --force --deep --sign - "$app" >/dev/null 2>&1 || echo "warning: codesign failed; the app still runs locally" >&2
echo "Built $app"
