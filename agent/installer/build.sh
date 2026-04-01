#!/bin/bash
# Ozma Agent — Build script
#
# Linux:   bash agent/installer/build.sh
#          → dist/ozma-agent (directory with executable)
#          → dist/ozma-agent.tar.gz (portable archive)
#
# Windows: Run build-windows.ps1 instead (needs native Python + NSIS)

set -e

cd "$(dirname "$0")/../.."
echo "=== Ozma Agent Build ==="

# Install build deps
pip install --quiet pyinstaller aiohttp zeroconf numpy 2>/dev/null || true

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
