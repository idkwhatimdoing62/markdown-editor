#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

VERSION="${1:-$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)}"
APP_NAME="Markdown Editor"
APP_BUNDLE="$ROOT_DIR/dist/$APP_NAME.app"
ZIP_PATH="$ROOT_DIR/dist/markdown-editor-v$VERSION-macos-universal.app.zip"
ICON_SOURCE="$ROOT_DIR/assets/app-icon.png"
ICONSET_DIR="$ROOT_DIR/dist/AppIcon.iconset"

export MACOSX_DEPLOYMENT_TARGET=11.0

rustup target add x86_64-apple-darwin aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin

rm -rf "$APP_BUNDLE"
rm -f "$ZIP_PATH"
rm -rf "$ICONSET_DIR"
mkdir -p "$APP_BUNDLE/Contents/MacOS" "$APP_BUNDLE/Contents/Resources" "$ICONSET_DIR"

lipo -create \
  "$ROOT_DIR/target/x86_64-apple-darwin/release/markdown-editor" \
  "$ROOT_DIR/target/aarch64-apple-darwin/release/markdown-editor" \
  -output "$APP_BUNDLE/Contents/MacOS/markdown-editor"
chmod +x "$APP_BUNDLE/Contents/MacOS/markdown-editor"

sed "s/@VERSION@/$VERSION/g" \
  "$ROOT_DIR/packaging/macos/Info.plist" \
  > "$APP_BUNDLE/Contents/Info.plist"

for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ICON_SOURCE" \
    --out "$ICONSET_DIR/icon_${size}x${size}.png" >/dev/null
  double_size=$((size * 2))
  sips -z "$double_size" "$double_size" "$ICON_SOURCE" \
    --out "$ICONSET_DIR/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET_DIR" \
  -o "$APP_BUNDLE/Contents/Resources/AppIcon.icns"
rm -rf "$ICONSET_DIR"

codesign --force --deep --sign - "$APP_BUNDLE"
codesign --verify --deep --strict "$APP_BUNDLE"
lipo -info "$APP_BUNDLE/Contents/MacOS/markdown-editor"

ditto -c -k --sequesterRsrc --keepParent "$APP_BUNDLE" "$ZIP_PATH"
shasum -a 256 "$ZIP_PATH"
