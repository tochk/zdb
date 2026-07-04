#!/usr/bin/env bash
# Build the `sqls` SQL language server (github.com/sqls-server/sqls) from source
# for the arches zdb ships. zdb bundles the matching binary next to `zdb[.exe]`
# and drives it as an LSP subprocess for SQL completion.
#
# We build from source (not the official release) because upstream ships an
# x86-64-only binary, and zdb targets Windows-on-ARM64. sqls is pure Go, so with
# CGO off it cross-compiles to every arch — except two bundled drivers pull in C
# (Oracle `godror`, `mattn/go-sqlite3`). zdb only speaks Postgres, so we drop
# those two blank imports before building.
#
# Usage: scripts/build-sqls.sh [output_dir]   (default: dist/sqls)
set -euo pipefail

VERSION="v0.2.47"
SRC="${SQLS_SRC:-$HOME/src/sqls}"
OUT="${1:-$(cd "$(dirname "$0")/.." && pwd)/dist/sqls}"

command -v go >/dev/null || { echo "error: Go toolchain not found on PATH"; exit 1; }

if [ ! -d "$SRC/.git" ]; then
    git clone --depth 1 --branch "$VERSION" https://github.com/sqls-server/sqls.git "$SRC"
fi
cd "$SRC"

# Drop the CGO-only drivers we don't use so a static, cross-compilable build is
# possible. Idempotent (sed no-ops if the lines are already gone).
sed -i '/_ "github.com\/godror\/godror"/d' internal/database/oracle.go
sed -i '/_ "github.com\/mattn\/go-sqlite3"/d' internal/database/sqlite3.go

mkdir -p "$OUT"
# os arch outname
targets=(
    "windows arm64 sqls-windows-arm64.exe"
    "windows amd64 sqls-windows-amd64.exe"
    "linux   arm64 sqls-linux-arm64"
    "linux   amd64 sqls-linux-amd64"
    "darwin  arm64 sqls-darwin-arm64"
    "darwin  amd64 sqls-darwin-amd64"
)
for t in "${targets[@]}"; do
    read -r os arch name <<<"$t"
    echo "building $os/$arch -> $name"
    CGO_ENABLED=0 GOOS="$os" GOARCH="$arch" \
        go build -ldflags="-s -w" -o "$OUT/$name" .
done
echo "done -> $OUT"
ls -la "$OUT"
