#!/usr/bin/env bash
# Package a cross-compiled Windows build into a self-contained, distributable zip.
#
# Usage: scripts/package-windows.sh <rust-target> <arch-label>
#   e.g. scripts/package-windows.sh x86_64-pc-windows-gnu        x64
#        scripts/package-windows.sh aarch64-pc-windows-gnullvm   arm64
#
# The mingw/llvm-mingw builds statically link the C/C++ runtime and link only
# against system DLLs (DirectX, kernel32, ...), so no extra runtime files are
# bundled. Verifies that with objdump before packaging.
set -euo pipefail

TARGET="${1:?rust target triple required}"
ARCH="${2:?arch label required}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EXE="$ROOT/target/$TARGET/release/zdb.exe"
OUT="$ROOT/dist/zdb-windows-$ARCH"

[ -f "$EXE" ] || { echo "ERROR: $EXE not found (build it first)"; exit 1; }

# Detect any non-system DLL dependencies (third-party runtime DLLs that would
# need bundling). Prefer llvm-objdump: it reads PE imports for any arch (the
# per-target mingw objdump misreads a foreign-arch PE).
OBJDUMP="$HOME/.local/llvm-mingw/bin/llvm-objdump"
command -v "$OBJDUMP" >/dev/null || OBJDUMP="x86_64-w64-mingw32-objdump"
THIRD_PARTY=$("$OBJDUMP" -p "$EXE" 2>/dev/null \
  | grep -i 'DLL Name' | awk '{print tolower($NF)}' | sort -u \
  | grep -iE 'libgcc|libstdc|libwinpthread|libc\+\+|libunwind' || true)

rm -rf "$OUT"
mkdir -p "$OUT"
cp "$EXE" "$OUT/zdb.exe"

# Bundle gpui's runtime HLSL shaders next to the exe (our vendored gpui patch
# reads them from <exe_dir>/shaders/). Required for the app to start.
mkdir -p "$OUT/shaders"
cp "$ROOT"/vendor/gpui/src/platform/windows/*.hlsl "$OUT/shaders/"

# Bundle any required runtime DLLs from the matching llvm-mingw arch directory.
if [ -n "$THIRD_PARTY" ]; then
  case "$TARGET" in
    aarch64-*) MINGW_ARCH=aarch64-w64-mingw32 ;;
    x86_64-*)  MINGW_ARCH=x86_64-w64-mingw32 ;;
    *)         MINGW_ARCH="" ;;
  esac
  LLVM_BIN="$HOME/.local/llvm-mingw/$MINGW_ARCH/bin"
  for dll in $THIRD_PARTY; do
    if [ -f "$LLVM_BIN/$dll" ]; then
      cp "$LLVM_BIN/$dll" "$OUT/"
      echo "bundled runtime DLL: $dll"
    else
      echo "WARNING: needed $dll not found under $LLVM_BIN — bundle manually"
    fi
  done
fi

cat > "$OUT/README.txt" <<'EOF'
zdb — PostgreSQL client (Windows)

RUN
  Double-click zdb.exe. Requires Windows 10 (1903+) or Windows 11.
  No installer needed. Keep the whole folder together — zdb.exe needs the
  shaders\ folder (and any .dll here) next to it. Only built-in Windows DLLs
  (DirectX, etc.) are used otherwise.

CONNECT
  Click the globe icon (top-left) to open the connection manager. If you have
  no saved connections, the Add form opens directly. Fill in host / port /
  database / user / password / SSL mode and connect. Connections are saved
  (passwords go to the Windows Credential Manager, never to disk in plaintext);
  one connection is active at a time and you can switch between saved ones.

USE
  - Schema tree on the left: expand a schema, double-click a table to open its
    rows. The WHERE bar filters; click a column header to ORDER BY it.
  - Query editor up top: Ctrl+Enter runs. Ctrl+Shift+E opens the auto-saved
    scratch editor. Ctrl+Shift+P opens the command palette. Ctrl+` opens a
    terminal. Edit a cell by double-clicking; "Add row" inserts. Pending edits
    show the generated SQL before applying.
EOF

( cd "$ROOT/dist" && zip -r -q "zdb-windows-$ARCH.zip" "zdb-windows-$ARCH" )
echo "Packaged: $ROOT/dist/zdb-windows-$ARCH.zip"
ls -lh "$OUT/zdb.exe" "$ROOT/dist/zdb-windows-$ARCH.zip"
