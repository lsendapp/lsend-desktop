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
#   scripts/make-app-bundle.sh debug        # debug profile
#   scripts/make-app-bundle.sh run          # build + `open` the bundle
#
# Output: dist/lsend-desktop.app
#
# Tested on macOS 14+ (uses NSStatusItem-backed accessory activation policy).

set -euo pipefail

cd "$(dirname "$0")/.."

PROFILE="${1:-release}"

# Validate profile before doing any work.
case "$PROFILE" in
  release|debug|run) ;;
  *) echo "usage: $0 [release|debug|run]"; exit 64 ;;
esac

# `cargo build` only knows about release/debug; `run` means "build the bundle
# and launch via `open`", so normalise to a real profile before computing
# BIN_PATH.
if [ "$PROFILE" = "run" ]; then
  PROFILE="release"
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

if [ ! -x "$BIN_PATH" ]; then
  echo "binary not found at $BIN_PATH; running cargo build"
  cargo build --"$PROFILE" --bin lsend-desktop
fi

BUNDLE_DIR="dist/lsend-desktop.app"
BUNDLE_BIN_DIR="$BUNDLE_DIR/Contents/MacOS"
BUNDLE_RES_DIR="$BUNDLE_DIR/Contents/Resources"

# Re-create the bundle on every run so we don't ship a half-updated app.
rm -rf "$BUNDLE_DIR"
mkdir -p "$BUNDLE_BIN_DIR" "$BUNDLE_RES_DIR"

# Use an absolute symlink so `open` (which can chdir before exec) finds the
# binary regardless of where the bundle lives on disk.
ln -s "$(cd "$(dirname "$BIN_PATH")" && pwd)/$(basename "$BIN_PATH")" "$BUNDLE_BIN_DIR/lsend-desktop"

# Minimal Info.plist. The `LSUIElement` flag is the macOS equivalent of our
# tao `ActivationPolicy::Accessory`: hide dock icon, menu-bar only.
cat > "$BUNDLE_DIR/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>            <string>lsend</string>
  <key>CFBundleDisplayName</key>     <string>lsend</string>
  <key>CFBundleExecutable</key>      <string>lsend-desktop</string>
  <key>CFBundleIdentifier</key>      <string>app.lsend.desktop</string>
  <key>CFBundleVersion</key>         <string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
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

# `run` was normalised to `release` above; remember whether the caller asked
# for run so we know to launch at the end.
WANT_RUN="${1:-}"
if [ "$WANT_RUN" = "run" ]; then
  exec open "$BUNDLE_DIR"
fi
