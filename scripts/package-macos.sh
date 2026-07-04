#!/usr/bin/env bash
# Assemble zdb.app for macOS. MUST run ON a Mac — cross-compiling gpui to macOS
# from Linux is not possible (Apple frameworks + Metal shader toolchain ship only
# with the macOS SDK / Xcode and are not redistributable). Builds natively for
# the host arch, then bundles the matching `sqls` binary next to the executable.
#
# Usage: scripts/package-macos.sh              (host arch)
#        scripts/package-macos.sh x86_64       (force Intel)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${1:-$(uname -m)}"
case "$ARCH" in
    arm64|aarch64) RUST_TARGET=aarch64-apple-darwin; SQLS=sqls-darwin-arm64 ;;
    x86_64)        RUST_TARGET=x86_64-apple-darwin;  SQLS=sqls-darwin-amd64 ;;
    *) echo "unknown arch: $ARCH"; exit 1 ;;
esac

[ "$(uname -s)" = "Darwin" ] || { echo "ERROR: run this on macOS (native build only)"; exit 1; }

echo "building zdb for $RUST_TARGET…"
cargo build -p zdb-app --release --target "$RUST_TARGET"

APP="$ROOT/dist/zdb.app"
MACOS="$APP/Contents/MacOS"
rm -rf "$APP"
mkdir -p "$MACOS" "$APP/Contents/Resources"

cp "$ROOT/target/$RUST_TARGET/release/zdb" "$MACOS/zdb"

# Bundle sqls next to the executable (zdb finds it there). Build it first if
# missing (Go cross-builds fine on any host).
[ -f "$ROOT/dist/sqls/$SQLS" ] || bash "$ROOT/scripts/build-sqls.sh"
if [ -f "$ROOT/dist/sqls/$SQLS" ]; then
    cp "$ROOT/dist/sqls/$SQLS" "$MACOS/sqls"
    chmod +x "$MACOS/sqls"
    echo "bundled sqls: $SQLS"
fi

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>zdb</string>
  <key>CFBundleDisplayName</key><string>zdb</string>
  <key>CFBundleIdentifier</key><string>dev.zdb.app</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>zdb</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

echo "built $APP"
