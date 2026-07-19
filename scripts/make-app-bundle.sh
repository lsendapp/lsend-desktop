#!/usr/bin/env bash
# Build a minimal macOS .app bundle for lsend-desktop.
#
# Why: macOS Launch Services refuses to fully register a bare Mach-O executable
# (`error -50 "This process is not the executable of a bundle."`), and emits a
# system "Choose Application / Where is use_default?" prompt at first launch.
# Wrapping the binary in a real .app bundle lets lsregister complete cleanly
# and gets the activation policy + LSUIElement right out of the gate.
#
# Usage:
#   scripts/make-app-bundle.sh              # build release bundle
#   scripts/make-app-bundle.sh release      # explicit profile
#   scripts/make-app-bundle.sh universal  # universal (arm64+intel) release bundle
#
# Output: dist/lsend-desktop.app
#
# Tested on macOS 14+ (uses NSStatusItem-backed accessory activation policy).

set -euo pipefail

cd "$(dirname "$0")/.."

REQUESTED_PROFILE="${1:-release}"
PROFILE="$REQUESTED_PROFILE"

# Validate profile before doing any work.
case "$PROFILE" in
  release|debug|run|universal) ;;
  *) echo "usage: $0 [release|debug|run|universal]"; exit 64 ;;
esac

# `cargo build` only knows about release/debug; `run` means "build the bundle
# and launch via `open`", so normalise to a real profile before computing
# BIN_PATH.
if [ "$PROFILE" = "run" ]; then
  PROFILE="release"
fi

# Build the requested profile every time so the bundle always contains the
# current source rather than whichever binary happened to exist already.
#
# `universal` builds aarch64-apple-darwin and x86_64-apple-darwin separately
# then merges them with `lipo` into a fat Mach-O at the same path a plain
# `release` build would use, so the rest of the script (BIN_PATH lookup,
# bundle copy, icon, plist) works unchanged. The host toolchain must already
# have both targets installed (`rustup target add aarch64-apple-darwin
# x86_64-apple-darwin`).
if [ "$PROFILE" = "universal" ]; then
  cargo build --release --target aarch64-apple-darwin --bin lsend-desktop
  cargo build --release --target x86_64-apple-darwin --bin lsend-desktop
  if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    UNIVERSAL_OUT="$CARGO_TARGET_DIR/release/lsend-desktop"
    ARM_BIN="$CARGO_TARGET_DIR/aarch64-apple-darwin/release/lsend-desktop"
    X86_BIN="$CARGO_TARGET_DIR/x86_64-apple-darwin/release/lsend-desktop"
  else
    UNIVERSAL_OUT="../target/release/lsend-desktop"
    ARM_BIN="../target/aarch64-apple-darwin/release/lsend-desktop"
    X86_BIN="../target/x86_64-apple-darwin/release/lsend-desktop"
  fi
  mkdir -p "$(dirname "$UNIVERSAL_OUT")"
  lipo -create -output "$UNIVERSAL_OUT" "$ARM_BIN" "$X86_BIN"
  lipo -info "$UNIVERSAL_OUT"
  PROFILE="release"
elif [ "$PROFILE" = "release" ]; then
  cargo build --release --bin lsend-desktop
else
  cargo build --bin lsend-desktop
fi

# Locate the binary. We honour CARGO_TARGET_DIR (lets CI funnel artifacts
# elsewhere) and fall back to the workspace target dir at the repo root,
# which is where `cargo build --release -p lsend-desktop` puts it under a
# Cargo workspace layout.
if [ -n "${CARGO_TARGET_DIR:-}" ]; then
  BIN_PATH="$CARGO_TARGET_DIR/$PROFILE/lsend-desktop"
else
  BIN_PATH="../target/$PROFILE/lsend-desktop"
fi

BUNDLE_DIR="dist/lsend-desktop.app"
BUNDLE_BIN_DIR="$BUNDLE_DIR/Contents/MacOS"
BUNDLE_RES_DIR="$BUNDLE_DIR/Contents/Resources"
APP_ICON_PNG="assets/app-icon.png"
APP_ICON_ICNS="$BUNDLE_RES_DIR/AppIcon.icns"

# Re-create the bundle on every run so we don't ship a half-updated app.
rm -rf "$BUNDLE_DIR"
mkdir -p "$BUNDLE_BIN_DIR" "$BUNDLE_RES_DIR"

# Copy the executable so the bundle remains self-contained when moved, when
# the Cargo target directory is cleaned, or when the app is distributed.
install -m 755 "$BIN_PATH" "$BUNDLE_BIN_DIR/lsend-desktop"

if [ ! -f "$APP_ICON_PNG" ]; then
  echo "missing $APP_ICON_PNG (1024x1024 PNG required for the app bundle icon)" >&2
  exit 1
fi

ICONSET_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/lsend-icon.XXXXXX")"
ICONSET="$ICONSET_ROOT/AppIcon.iconset"
mkdir "$ICONSET"
cleanup_iconset() { rm -rf "$ICONSET_ROOT"; }
trap cleanup_iconset EXIT

make_icon() {
  local size="$1"
  local name="$2"
  sips -z "$size" "$size" "$APP_ICON_PNG" --out "$ICONSET/$name" >/dev/null
}

make_icon 16 icon_16x16.png
make_icon 32 icon_16x16@2x.png
make_icon 32 icon_32x32.png
make_icon 64 icon_32x32@2x.png
make_icon 128 icon_128x128.png
make_icon 256 icon_128x128@2x.png
make_icon 256 icon_256x256.png
make_icon 512 icon_256x256@2x.png
make_icon 512 icon_512x512.png
make_icon 1024 icon_512x512@2x.png
iconutil -c icns "$ICONSET" -o "$APP_ICON_ICNS"

# Minimal Info.plist. The `LSUIElement` flag is the macOS equivalent of our
# tao `ActivationPolicy::Accessory`: hide dock icon, menu-bar only.
#
# Version strings honour the `LSEND_VERSION` env var (set by CI from the
# release tag); default `0.1.0` if unset so local builds keep working.
cat > "$BUNDLE_DIR/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>            <string>LSend</string>
  <key>CFBundleDisplayName</key>     <string>LSend</string>
  <key>CFBundleExecutable</key>      <string>lsend-desktop</string>
  <key>CFBundleIconFile</key>       <string>AppIcon</string>
  <key>CFBundleIdentifier</key>      <string>app.lsend.desktop</string>
  <key>CFBundleVersion</key>         <string>${LSEND_VERSION:-0.1.0}</string>
  <key>CFBundleShortVersionString</key><string>${LSEND_VERSION:-0.1.0}</string>
  <key>CFBundlePackageType</key>     <string>APPL</string>
  <key>CFBundleSignature</key>       <string>????</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSMinimumSystemVersion</key>  <string>11.0</string>
  <key>LSUIElement</key>             <true/>
  <key>NSHighResolutionCapable</key> <true/>
  <key>NSPrincipalClass</key>        <string>NSApplication</string>
  <key>NSSupportsAutomaticGraphicsSwitching</key> <true/>
</dict>
</plist>
PLIST

# PkgInfo is the legacy companion to Info.plist; macOS still reads it.
printf 'APPL????' > "$BUNDLE_DIR/Contents/PkgInfo"

# Re-touch the bundle so Launch Services re-scans it.
touch "$BUNDLE_DIR"

# Register the bundle with Launch Services so the system recognises it on
# first run and won't prompt the user to "Choose Application". The `-f` flag
# forces a re-register even if a stale entry exists from a previous build.
LSREG="/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister"
if [ -x "$LSREG" ]; then
  "$LSREG" -f "$BUNDLE_DIR" >/dev/null 2>&1 || true
fi

echo "Built: $BUNDLE_DIR"

if [ "$REQUESTED_PROFILE" = "run" ]; then
  exec open "$BUNDLE_DIR"
fi
