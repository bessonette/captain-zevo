#!/bin/sh
# Builds "Captain Zevo.app" for macOS, with the app icon.
#
#   scripts/bundle-macos.sh             build into dist/
#   scripts/bundle-macos.sh --install   also copy it to /Applications
#
# The app is ad-hoc signed: it runs on the Mac that built it. Distributing it
# to other people needs an Apple Developer ID signature and notarisation.
set -eu
cd "$(dirname "$0")/.."

if [ "$(uname -s)" != "Darwin" ]; then
    echo "This script builds a macOS app and must run on macOS." >&2
    exit 1
fi

TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/zevo-target}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
APP="dist/Captain Zevo.app"

cargo build --release -p zevo-app --target-dir "$TARGET_DIR"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$TARGET_DIR/release/zevo-app" "$APP/Contents/MacOS/zevo-app"
cp assets/icons/icon.icns "$APP/Contents/Resources/icon.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Captain Zevo</string>
    <key>CFBundleDisplayName</key><string>Captain Zevo</string>
    <key>CFBundleIdentifier</key><string>dev.captain-zevo.app</string>
    <key>CFBundleExecutable</key><string>zevo-app</string>
    <key>CFBundleIconFile</key><string>icon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

codesign --force --deep --sign - "$APP"
echo "Built $APP"

if [ "${1:-}" = "--install" ]; then
    rm -rf "/Applications/Captain Zevo.app"
    cp -R "$APP" /Applications/
    echo "Installed to /Applications/Captain Zevo.app"
fi
