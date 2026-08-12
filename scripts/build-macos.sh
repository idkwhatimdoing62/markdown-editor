#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

VERSION="${1:-$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)}"
APP_NAME="Markdown Editor"
APP_BUNDLE="$ROOT_DIR/dist/$APP_NAME.app"
ZIP_PATH="$ROOT_DIR/dist/markdown-editor-v$VERSION-macos-universal.app.zip"

export MACOSX_DEPLOYMENT_TARGET=11.0

rustup target add x86_64-apple-darwin aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin

rm -rf "$APP_BUNDLE"
rm -f "$ZIP_PATH"
mkdir -p "$APP_BUNDLE/Contents/MacOS"

lipo -create \
  "$ROOT_DIR/target/x86_64-apple-darwin/release/markdown-editor" \
  "$ROOT_DIR/target/aarch64-apple-darwin/release/markdown-editor" \
  -output "$APP_BUNDLE/Contents/MacOS/markdown-editor"
chmod +x "$APP_BUNDLE/Contents/MacOS/markdown-editor"

sed "s/@VERSION@/$VERSION/g" \
  "$ROOT_DIR/packaging/macos/Info.plist" \
  > "$APP_BUNDLE/Contents/Info.plist"

codesign --force --deep --sign - "$APP_BUNDLE"
codesign --verify --deep --strict "$APP_BUNDLE"
lipo -info "$APP_BUNDLE/Contents/MacOS/markdown-editor"

ditto -c -k --sequesterRsrc --keepParent "$APP_BUNDLE" "$ZIP_PATH"
shasum -a 256 "$ZIP_PATH"
