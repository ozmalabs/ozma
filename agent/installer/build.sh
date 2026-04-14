#!/bin/bash
# Ozma Agent — Build script
#
# Linux:   bash agent/installer/build.sh
#          → dist/ozma-agent (directory with executable)
#          → dist/ozma-agent.tar.gz (portable archive)
#
# Also builds ozma-agent-ui if cargo is available:
#   bash agent/installer/build.sh --ui
#          → dist/ozma-agent-ui (if Rust toolchain present)
#
# Windows: Run build-windows.ps1 instead (needs native Python + NSIS)

set -e

cd "$(dirname "$0")/../.."
echo "=== Ozma Agent Build ==="

BUILD_UI=0
for arg in "$@"; do
    [ "$arg" = "--ui" ] && BUILD_UI=1
done

# Install build deps
uv pip install --quiet pyinstaller aiohttp zeroconf numpy 2>/dev/null || true

# Build
echo "Building with PyInstaller..."
pyinstaller agent/installer/ozma-agent.spec --noconfirm --clean

if [ -f "dist/ozma-agent/ozma-agent" ]; then
    echo "Build complete: dist/ozma-agent/"

    # Create portable tarball
    cd dist
    tar czf ozma-agent-linux-x86_64.tar.gz ozma-agent/
    echo "Archive: dist/ozma-agent-linux-x86_64.tar.gz ($(du -sh ozma-agent-linux-x86_64.tar.gz | cut -f1))"
    cd ..
else
    echo "Build failed"
    exit 1
fi

# Build ozma-agent-ui (Rust) if requested
if [ "$BUILD_UI" = "1" ]; then
    echo
    echo "=== Building ozma-agent-ui ==="
    
    if ! command -v cargo &>/dev/null; then
        echo "WARNING: cargo not found. Skipping ozma-agent-ui build." >&2
        echo "  Install Rust from https://rustup.rs to enable UI builds."
    elif [ ! -d "ozma-agent-ui" ]; then
        echo "WARNING: ozma-agent-ui directory not found. Skipping." >&2
    else
        if cargo build --release -p ozma-agent-ui 2>/dev/null; then
            UI_BINARY="target/release/ozma-agent-ui"
            if [ -f "$UI_BINARY" ]; then
                echo "  Binary: $UI_BINARY"
                
                # Stage UI binary
                UI_STAGE="dist/ozma-agent-ui"
                rm -rf "$UI_STAGE"
                mkdir -p "$UI_STAGE"
                cp "$UI_BINARY" "$UI_STAGE/ozma-agent-ui"
                
                # Copy desktop file if exists
                if [ -f "ozma-agent-ui/installer/ozma-agent-ui.desktop" ]; then
                    mkdir -p "$UI_STAGE/applications"
                    cp "ozma-agent-ui/installer/ozma-agent-ui.desktop" "$UI_STAGE/applications/"
                fi
                
                echo "  Staged: $UI_STAGE"
                
                # Check for cargo-deb
                if command -v cargo-deb &>/dev/null; then
                    echo "Building .deb package..."
                    cargo deb -p ozma-agent-ui
                    DEB_FILE=$(ls target/debian/ozma-agent-ui_*.deb 2>/dev/null | head -1)
                    if [ -n "$DEB_FILE" ]; then
                        echo "  Deb: $DEB_FILE"
                        cp "$DEB_FILE" dist/
                    fi
                else
                    echo "NOTE: Install cargo-deb for .deb packaging:"
                    echo "  cargo install cargo-deb"
                fi
            else
                echo "WARNING: ozma-agent-ui binary not found at $UI_BINARY" >&2
            fi
        else
            echo "WARNING: ozma-agent-ui build failed" >&2
        fi
    fi
fi
