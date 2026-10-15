#!/usr/bin/env bash
# Packs a downloadable rzen under dist/: a universal (Apple silicon + Intel) rzen.app in a
# drag-to-Applications disk image, a zip of the app, and their SHA-256 checksums.
# Signing with a Developer ID works the same as for bundle-macos.sh (RZEN_SIGN_* variables).
set -euo pipefail
cd "$(dirname "$0")/.."
RZEN_UNIVERSAL=1 ./scripts/bundle-macos.sh
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
NAME="rzen-$VERSION-macos-universal"
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
ditto target/release/rzen.app "$STAGE/rzen.app"
ln -s /Applications "$STAGE/Applications"
mkdir -p dist
rm -f "dist/$NAME.dmg" "dist/$NAME.zip" "dist/$NAME.sha256"
hdiutil create -volname rzen -srcfolder "$STAGE" -fs HFS+ -format UDZO "dist/$NAME.dmg" >/dev/null
ditto -c -k --keepParent target/release/rzen.app "dist/$NAME.zip"
(cd dist && shasum -a 256 "$NAME.dmg" "$NAME.zip" | tee "$NAME.sha256")
