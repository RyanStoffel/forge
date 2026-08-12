#!/usr/bin/env bash
set -euo pipefail

: "${FORGE_BINARY:?FORGE_BINARY is required}"
: "${FORGE_VERSION:?FORGE_VERSION is required}"
: "${FORGE_REVISION:?FORGE_REVISION is required}"
: "${OUTPUT_DIR:?OUTPUT_DIR is required}"

bundle="${OUTPUT_DIR}/Forge.app"
contents="${bundle}/Contents"

rm -rf "${bundle}"
mkdir -p "${contents}/MacOS" "${contents}/Resources"
cp "${FORGE_BINARY}" "${contents}/MacOS/forge"
chmod 755 "${contents}/MacOS/forge"

cat >"${contents}/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>Forge</string>
  <key>CFBundleExecutable</key>
  <string>forge</string>
  <key>CFBundleIdentifier</key>
  <string>dev.ryanstoffel.forge</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>Forge</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>${FORGE_VERSION}</string>
  <key>CFBundleVersion</key>
  <string>${FORGE_REVISION}</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSHumanReadableCopyright</key>
  <string>Copyright © 2026 Ryan Stoffel</string>
  <key>NSPrincipalClass</key>
  <string>NSApplication</string>
</dict>
</plist>
PLIST

plutil -lint "${contents}/Info.plist" >/dev/null
codesign --force --deep --sign - "${bundle}"
