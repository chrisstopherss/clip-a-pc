#!/bin/bash
# Builds clip-a-pc.app for Apple Silicon and Intel, signs it ad hoc, and zips it with a SHA-256 file.
# Output: out/clip-a-pc-mac.zip and out/clip-a-pc-mac.zip.sha256
set -euo pipefail
cd "$(dirname "$0")/.."

export MACOSX_DEPLOYMENT_TARGET=15.0
VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')

rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

rm -rf out
APP=out/clip-a-pc.app
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create -output "$APP/Contents/MacOS/clip-a-pc" \
  target/aarch64-apple-darwin/release/clip-a-pc \
  target/x86_64-apple-darwin/release/clip-a-pc

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>clip-a-pc</string>
  <key>CFBundleIdentifier</key><string>io.github.chrisstopherss.clip-a-pc</string>
  <key>CFBundleName</key><string>clip-a-pc</string>
  <key>CFBundleDisplayName</key><string>clip-a-pc</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key><string>clip-a-pc records your microphone into the clips and recordings you save.</string>
</dict>
</plist>
PLIST

codesign --force --deep --sign - "$APP"
ditto -c -k --keepParent "$APP" out/clip-a-pc-mac.zip
(cd out && shasum -a 256 clip-a-pc-mac.zip | sed 's/  .*/  clip-a-pc-mac.zip/' > clip-a-pc-mac.zip.sha256)
echo "built clip-a-pc ${VERSION} for macOS"
