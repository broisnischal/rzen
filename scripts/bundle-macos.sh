#!/usr/bin/env bash
# Builds an optimized rzen.app (needed for "Make default browser" and Dock integration).
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release
APP=target/release/rzen.app
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/rzen "$APP/Contents/MacOS/rzen"
cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
# macOS 26 renders the Icon Composer icon with Liquid Glass; older systems use the .icns.
ICON_NAME=""
if ACTOOL=$(xcrun --find actool 2>/dev/null); then
  ICONS=$(mktemp -d)
  "$ACTOOL" assets/AppIcon.icon --compile "$ICONS" --platform macosx --target-device mac \
    --minimum-deployment-target 13.0 --app-icon AppIcon \
    --output-partial-info-plist "$ICONS/partial.plist" >/dev/null
  cp "$ICONS/Assets.car" "$APP/Contents/Resources/Assets.car"
  rm -rf "$ICONS"
  ICON_NAME="<key>CFBundleIconName</key><string>AppIcon</string>"
fi
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>rzen</string>
  <key>CFBundleDisplayName</key><string>rzen</string>
  <key>CFBundleIdentifier</key><string>app.rzen.browser</string>
  <key>CFBundleExecutable</key><string>rzen</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  ${ICON_NAME}
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
  <key>NSCameraUsageDescription</key><string>Websites you allow can use the camera for video calls.</string>
  <key>NSMicrophoneUsageDescription</key><string>Websites you allow can use the microphone for calls and voice input.</string>
  <key>NSBluetoothAlwaysUsageDescription</key><string>Used to sign in with a passkey on your phone.</string>
  <key>NSLocationWhenInUseUsageDescription</key><string>Websites you allow can use your location.</string>
  <key>CFBundleURLTypes</key><array><dict>
    <key>CFBundleURLName</key><string>Web site URL</string>
    <key>CFBundleURLSchemes</key><array><string>http</string><string>https</string></array>
  </dict></array>
  <key>CFBundleDocumentTypes</key><array><dict>
    <key>CFBundleTypeName</key><string>HTML document</string>
    <key>CFBundleTypeRole</key><string>Viewer</string>
    <key>LSItemContentTypes</key><array><string>public.html</string><string>public.xhtml</string></array>
  </dict></array>
</dict></plist>
PLIST
# Passkeys need Apple's browser entitlement, which only works with a real signature and a
# provisioning profile that includes it:
#   RZEN_SIGN_IDENTITY="Developer ID Application: Name (TEAMID)" RZEN_TEAM_ID=TEAMID \
#   RZEN_PROVISION_PROFILE=path/to/rzen.provisionprofile ./scripts/bundle-macos.sh
if [[ -n "${RZEN_SIGN_IDENTITY:-}" && -n "${RZEN_PROVISION_PROFILE:-}" && -n "${RZEN_TEAM_ID:-}" ]]; then
  cp "$RZEN_PROVISION_PROFILE" "$APP/Contents/embedded.provisionprofile"
  ENT=$(mktemp)
  cat > "$ENT" <<ENTS
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.application-identifier</key><string>${RZEN_TEAM_ID}.app.rzen.browser</string>
  <key>com.apple.developer.team-identifier</key><string>${RZEN_TEAM_ID}</string>
  <key>com.apple.developer.web-browser.public-key-credential</key><true/>
  <key>com.apple.security.device.camera</key><true/>
  <key>com.apple.security.device.audio-input</key><true/>
  <key>com.apple.security.device.bluetooth</key><true/>
</dict></plist>
ENTS
  codesign --force --options runtime --entitlements "$ENT" --sign "$RZEN_SIGN_IDENTITY" "$APP"
  rm -f "$ENT"
  echo "Signed with passkey entitlement."
else
  # Ad-hoc signature: runs locally, but WebKit keeps passkeys disabled.
  codesign --force --sign - "$APP" >/dev/null 2>&1 || true
fi
echo "Built $APP — move it to /Applications, open it, then Settings → Make default."
