#!/bin/bash
# music-spec macOS .app bundler.
# Builds the release binary and wraps it with Info.plist + icns so the
# Dock/Launchpad shows the app icon (ADR-0001: Loopback requires macOS 13+,
# which is also the bundle's minimum system version).
set -euo pipefail

cd "$(dirname "$0")/.."

APP_NAME="music-spec"
BUNDLE_DIR="target/release/${APP_NAME}.app"
VERSION="$(cargo pkgid -p music-spec-app | sed 's/.*@//')"

echo "==> Building release binary"
cargo build --release -p music-spec-app

echo "==> Assembling ${BUNDLE_DIR}"
rm -rf "${BUNDLE_DIR}"
mkdir -p "${BUNDLE_DIR}/Contents/MacOS" "${BUNDLE_DIR}/Contents/Resources"

cp target/release/music-spec-app "${BUNDLE_DIR}/Contents/MacOS/${APP_NAME}"
cp assets/music-spec.icns "${BUNDLE_DIR}/Contents/Resources/AppIcon.icns"

cat > "${BUNDLE_DIR}/Contents/Info.plist" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>${APP_NAME}</string>
    <key>CFBundleDisplayName</key>
    <string>music-spec</string>
    <key>CFBundleExecutable</key>
    <string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key>
    <string>com.music-spec.desktop</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.music</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>用于显示麦克风/线路输入的实时频谱。</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

# Ad-hoc signature: unsigned ARM binaries refuse to launch locally.
codesign --force --sign - "${BUNDLE_DIR}"

plutil -lint "${BUNDLE_DIR}/Contents/Info.plist"
echo "==> Done: ${BUNDLE_DIR}"
echo "    open ${BUNDLE_DIR}"
