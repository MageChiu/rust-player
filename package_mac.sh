#!/bin/bash
set -e

echo "==> Building rust-player in release mode..."
export FFMPEG_DIR=$(brew --prefix ffmpeg@6)
export PKG_CONFIG_PATH="$FFMPEG_DIR/lib/pkgconfig:$PKG_CONFIG_PATH"
export BINDGEN_EXTRA_CLANG_ARGS="-isysroot $(xcrun --show-sdk-path)"
cargo build -p rust-player --release

echo "==> Creating macOS .app bundle..."
APP_NAME="RustPlayer"
APP_DIR="dist/${APP_NAME}.app"
CONTENTS_DIR="${APP_DIR}/Contents"
MACOS_DIR="${CONTENTS_DIR}/MacOS"
FRAMEWORKS_DIR="${CONTENTS_DIR}/Frameworks"
RESOURCES_DIR="${CONTENTS_DIR}/Resources"

rm -rf "dist"
mkdir -p "${MACOS_DIR}"
mkdir -p "${FRAMEWORKS_DIR}"
mkdir -p "${RESOURCES_DIR}"

echo "==> Creating Info.plist..."
cat << PLIST > "${CONTENTS_DIR}/Info.plist"
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>rust-player</string>
    <key>CFBundleIdentifier</key>
    <string>com.rust-player.app</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>${APP_NAME}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSMinimumSystemVersion</key>
    <string>10.13</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

echo "==> Copying executable..."
cp target/release/rust-player "${MACOS_DIR}/"

echo "==> Bundling dynamic libraries (FFmpeg, etc.)..."
# Use dylibbundler to copy dependencies into Frameworks/ and fix rpaths
dylibbundler -x "${MACOS_DIR}/rust-player" -b -d "${FRAMEWORKS_DIR}" -p "@executable_path/../Frameworks" -cd -of -s /opt/homebrew/lib -s /usr/local/lib

echo "==> Done! The app is now fully standalone at ${APP_DIR}."
echo "You can double click dist/RustPlayer.app in Finder. It will run natively and look crisp."
