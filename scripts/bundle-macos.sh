#!/usr/bin/env bash
# Builds a release binary and wraps it in a minimal macOS .app bundle.
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build --release

version=$(grep -m1 '^version' Cargo.toml | cut -d '"' -f2)
app=target/release/gpui-cut.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/gpui-cut "$app/Contents/MacOS/gpui-cut"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>gpui-cut</string>
  <key>CFBundleDisplayName</key><string>gpui-cut</string>
  <key>CFBundleIdentifier</key><string>dev.gpui-cut</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleExecutable</key><string>gpui-cut</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>Media</string>
      <key>CFBundleTypeRole</key><string>Editor</string>
      <key>LSItemContentTypes</key>
      <array>
        <string>public.movie</string>
        <string>public.audio</string>
      </array>
    </dict>
  </array>
</dict>
</plist>
PLIST

echo "Built $app"
