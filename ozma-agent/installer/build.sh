#!/bin/bash
# ozma-agent — Linux + Windows (cross) build script
#
# Linux native:
#   bash ozma-agent/installer/build.sh
#   → dist/ozma-agent-linux-x86_64.tar.gz
#
# Cross-compile for Windows (requires mingw-w64 toolchain):
#   bash ozma-agent/installer/build.sh --windows
#   → dist/ozma-agent-windows-x86_64.zip
#
# Windows installer (MSI) — run on a Windows host instead:
#   .\ozma-agent\installer\build-windows.ps1

set -e

# ── Setup ─────────────────────────────────────────────────────────────────────

cd "$(dirname "$0")/../.."   # repo root
echo "=== Ozma Agent Build ==="

WINDOWS=0
for arg in "$@"; do
    [ "$arg" = "--windows" ] && WINDOWS=1
done

# ── Rust toolchain check ──────────────────────────────────────────────────────

if ! command -v cargo &>/dev/null; then
    echo "ERROR: cargo not found. Install Rust from https://rustup.rs" >&2
    exit 1
fi
RUST_VERSION=$(rustc --version)
echo "Rust:   $RUST_VERSION"
echo "Target: $(uname -m)-$(uname -s)"

# ── Linux build ───────────────────────────────────────────────────────────────

echo
echo "Building ozma-agent (release)..."
cargo build --release -p ozma-agent

BINARY="target/release/ozma-agent"
if [ ! -f "$BINARY" ]; then
    echo "ERROR: build produced no binary at $BINARY" >&2
    exit 1
fi

VERSION=$(cargo metadata --format-version 1 --no-deps \
    | python3 -c "import sys,json; print(next(p['version'] for p in json.load(sys.stdin)['packages'] if p['name']=='ozma-agent'))" 2>/dev/null \
    || grep '^version' ozma-agent/Cargo.toml | head -1 | awk -F'"' '{print $2}')

mkdir -p dist

# Portable tar.gz (mirrors agent/installer/build.sh output layout)
STAGE="dist/ozma-agent"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp "$BINARY" "$STAGE/ozma-agent"

# Bundle a minimal README
cat > "$STAGE/README.txt" <<'EOF'
Ozma Agent
==========

Run: ./ozma-agent --controller-url http://your-controller:7380

Options:
  --api-port         TCP port for the HTTP API   (default 7381)
  --metrics-port     TCP port for Prometheus      (default 9101)
  --wg-port          WireGuard UDP port           (default 51820)
  --controller-url   Controller URL               (default http://localhost:7380)

Environment variables mirror every --flag (OZMA_API_PORT, OZMA_CONTROLLER_URL, …).

Install as systemd service: see https://ozma.dev/docs/agent
EOF

ARCHIVE="dist/ozma-agent-${VERSION}-linux-x86_64.tar.gz"
tar -czf "$ARCHIVE" -C dist ozma-agent/
echo
echo "Linux build complete:"
echo "  Binary:  $BINARY"
echo "  Archive: $ARCHIVE  ($(du -sh "$ARCHIVE" | cut -f1))"

# ── Windows cross-compile ─────────────────────────────────────────────────────

if [ "$WINDOWS" = "1" ]; then
    echo
    echo "Cross-compiling for Windows (x86_64-pc-windows-gnu)..."

    TARGET="x86_64-pc-windows-gnu"

    # Check cross-compilation toolchain
    if ! rustup target list --installed | grep -q "$TARGET"; then
        echo "Adding Rust target $TARGET..."
        rustup target add "$TARGET"
    fi

    # Check mingw linker
    if ! command -v x86_64-w64-mingw32-gcc &>/dev/null; then
        echo "ERROR: mingw-w64 not found. Install with:"
        echo "  Ubuntu/Debian: sudo apt install gcc-mingw-w64-x86-64"
        exit 1
    fi

    cargo build --release --target "$TARGET" -p ozma-agent

    WIN_BINARY="target/${TARGET}/release/ozma-agent.exe"
    if [ ! -f "$WIN_BINARY" ]; then
        echo "ERROR: Windows binary not produced at $WIN_BINARY" >&2
        exit 1
    fi

    WIN_STAGE="dist/ozma-agent-windows"
    rm -rf "$WIN_STAGE"
    mkdir -p "$WIN_STAGE"
    cp "$WIN_BINARY" "$WIN_STAGE/ozma-agent.exe"
    cp "$STAGE/README.txt" "$WIN_STAGE/README.txt"

    WIN_ARCHIVE="dist/ozma-agent-${VERSION}-windows-x86_64.zip"
    (cd dist && zip -r "../$WIN_ARCHIVE" ozma-agent-windows/)
    echo
    echo "Windows cross-build complete:"
    echo "  Binary:  $WIN_BINARY"
    echo "  Archive: $WIN_ARCHIVE  ($(du -sh "$WIN_ARCHIVE" | cut -f1))"
    echo
    echo "NOTE: For the MSI installer run build-windows.ps1 on a Windows host."
fi
