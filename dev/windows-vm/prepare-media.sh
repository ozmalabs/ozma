#!/bin/bash
# Prepare offline media for air-gapped Windows agent build
#
# Downloads everything needed to build the ozma agent on Windows
# WITHOUT any network access in the VM. All files go into the media
# directory, which the soft node serves as a USB drive.
#
# What gets downloaded:
#   - Python 3.12 Windows installer (.exe)
#   - All pip wheels for ozma-agent + pyinstaller + dependencies
#   - The ozma agent source code
#   - Offline install scripts
#
# Usage:
#   bash dev/windows-vm/prepare-media.sh
#   # Then: bash dev/windows-vm/provision.sh create

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
MEDIA_DIR="${MEDIA_DIR:-$REPO_DIR/images/ozma-media-win10}"
CONTROLLER_URL="${CONTROLLER_URL:-https://ozma.hrdwrbob.net}"

echo "=== Preparing offline media for Windows agent build ==="
echo "Media dir: $MEDIA_DIR"
echo ""

mkdir -p "$MEDIA_DIR/ozma"
mkdir -p "$MEDIA_DIR/python"
mkdir -p "$MEDIA_DIR/wheels"

# ── Python installer ──────────────────────────────────────────────────────────

PYTHON_URL="https://www.python.org/ftp/python/3.12.8/python-3.12.8-amd64.exe"
PYTHON_FILE="$MEDIA_DIR/python/python-3.12.8-amd64.exe"

if [ ! -f "$PYTHON_FILE" ]; then
    echo "Downloading Python 3.12.8 installer..."
    curl -L -o "$PYTHON_FILE" "$PYTHON_URL"
else
    echo "Python installer: already downloaded"
fi

# ── Pip wheels (all dependencies, Windows x86_64) ─────────────────────────────

echo ""
echo "Downloading pip wheels for Windows (this may take a minute)..."

# Create a temporary requirements file with everything the agent needs
cat > /tmp/ozma-agent-requirements.txt << 'EOF'
aiohttp>=3.8
zeroconf>=0.132
numpy>=1.24
pystray>=0.19
Pillow>=9.0
pyinstaller>=6.0
pynacl>=1.5
EOF

# Download wheels for Windows x86_64
pip download \
    --dest "$MEDIA_DIR/wheels" \
    --platform win_amd64 \
    --python-version 3.12 \
    --only-binary=:all: \
    -r /tmp/ozma-agent-requirements.txt \
    2>&1 | tail -5

# Some packages might not have binary wheels — download source as fallback
pip download \
    --dest "$MEDIA_DIR/wheels" \
    --no-binary=:none: \
    --platform win_amd64 \
    --python-version 3.12 \
    -r /tmp/ozma-agent-requirements.txt \
    2>/dev/null || true

# Also need pip, setuptools, wheel themselves
pip download \
    --dest "$MEDIA_DIR/wheels" \
    --platform win_amd64 \
    --python-version 3.12 \
    --only-binary=:all: \
    pip setuptools wheel \
    2>/dev/null || true

WHEEL_COUNT=$(ls "$MEDIA_DIR/wheels"/*.whl 2>/dev/null | wc -l)
echo "Downloaded $WHEEL_COUNT wheel files"

# ── Ozma agent source ─────────────────────────────────────────────────────────

echo ""
echo "Copying ozma agent source..."

# Copy agent code
cp -r "$REPO_DIR/agent/"*.py "$MEDIA_DIR/ozma/"
# Copy controller modules the agent needs
cp "$REPO_DIR/controller/room_correction.py" "$MEDIA_DIR/ozma/" 2>/dev/null || true

# Copy the PyInstaller spec
mkdir -p "$MEDIA_DIR/ozma/installer"
cp "$REPO_DIR/agent/installer/ozma-agent.spec" "$MEDIA_DIR/ozma/installer/" 2>/dev/null || true

# ── Offline install script ────────────────────────────────────────────────────

echo ""
echo "Writing offline install script..."

cat > "$MEDIA_DIR/ozma/install-offline.ps1" << 'PSEOF'
# Ozma Agent — Offline Install (no network required)
# Everything is on this USB drive.

$ErrorActionPreference = "Continue"
$drive = (Get-Volume | Where-Object { $_.FileSystemLabel -eq "OZMA" }).DriveLetter
if (-not $drive) { $drive = "E" }
$root = "${drive}:\"

Write-Host "=== Ozma Agent Offline Install ===" -ForegroundColor Green
Write-Host "Media drive: $root"

# Step 1: Install Python
Write-Host "`nInstalling Python 3.12 (offline)..."
$pyInstaller = Join-Path $root "python\python-3.12.8-amd64.exe"
if (Test-Path $pyInstaller) {
    Start-Process -Wait $pyInstaller -ArgumentList "/quiet","InstallAllUsers=1","PrependPath=1"
    $env:PATH = [Environment]::GetEnvironmentVariable("PATH","Machine") + ";" + [Environment]::GetEnvironmentVariable("PATH","User")
    Write-Host "Python: $(python --version)" -ForegroundColor Green
} else {
    Write-Host "ERROR: Python installer not found at $pyInstaller" -ForegroundColor Red
    exit 1
}

# Step 2: Install wheels (offline — no network)
Write-Host "`nInstalling pip packages (offline from wheels)..."
$wheelsDir = Join-Path $root "wheels"
python -m pip install --no-index --find-links $wheelsDir pip setuptools wheel 2>$null
python -m pip install --no-index --find-links $wheelsDir `
    aiohttp zeroconf numpy pystray Pillow pyinstaller pynacl 2>&1 | Select-String -NotMatch "already satisfied"

# Step 3: Install ozma-agent from source on the drive
Write-Host "`nInstalling ozma-agent from USB drive..."
$agentDir = Join-Path $root "ozma"
python -m pip install --no-index --find-links $wheelsDir $agentDir 2>$null
# If that fails, just copy modules to site-packages
if ($LASTEXITCODE -ne 0) {
    $sitePackages = python -c "import site; print(site.getsitepackages()[0])"
    Copy-Item "$agentDir\*.py" $sitePackages -Force
    Write-Host "Copied agent modules to $sitePackages"
}

# Step 4: Build the .exe with PyInstaller
Write-Host "`nBuilding ozma-agent.exe..."
$specFile = Join-Path $agentDir "installer\ozma-agent.spec"
if (Test-Path $specFile) {
    pyinstaller $specFile --noconfirm --clean 2>&1 | Select-String "Building|completed"
} else {
    # Simple one-file build
    pyinstaller --onefile --name ozma-agent --console (Join-Path $agentDir "cli.py") --noconfirm 2>&1 | Select-String "Building|completed"
}

# Step 5: Copy the .exe to the USB drive and C:\
$exe = "dist\ozma-agent\ozma-agent.exe"
if (-not (Test-Path $exe)) { $exe = "dist\ozma-agent.exe" }
if (Test-Path $exe) {
    Copy-Item $exe "C:\ozma-agent.exe" -Force
    Copy-Item $exe "$root\ozma\ozma-agent.exe" -Force
    Write-Host "`nBuilt: $exe" -ForegroundColor Green
    Write-Host "Copied to: C:\ozma-agent.exe"
    Write-Host "Copied to: ${root}ozma\ozma-agent.exe"
} else {
    Write-Host "WARNING: .exe not found after build" -ForegroundColor Yellow
}

# Step 6: Install as service
Write-Host "`nInstalling agent service..."
$controllerUrl = Get-Content "$root\ozma\controller.txt" -ErrorAction SilentlyContinue
if (-not $controllerUrl) { $controllerUrl = "http://localhost:7380" }
$controllerUrl = $controllerUrl.Trim()

if (Test-Path "C:\ozma-agent.exe") {
    & "C:\ozma-agent.exe" install --name $env:COMPUTERNAME --controller $controllerUrl
} else {
    ozma-agent install --name $env:COMPUTERNAME --controller $controllerUrl
}

Write-Host "`n=== Complete ===" -ForegroundColor Green
Write-Host "Agent installed. No network was required."
Write-Host "Controller: $controllerUrl"
PSEOF

# Batch launcher
cat > "$MEDIA_DIR/ozma/install-offline.bat" << 'BATEOF'
@echo off
echo === Ozma Agent Offline Install ===
powershell -ExecutionPolicy Bypass -File "%~dp0install-offline.ps1"
pause
BATEOF

# Controller URL
echo "$CONTROLLER_URL" > "$MEDIA_DIR/ozma/controller.txt"

# ── Virtio drivers ────────────────────────────────────────────────────────────

VIRTIO_ISO="$REPO_DIR/images/virtio-win.iso"
if [ -f "$VIRTIO_ISO" ]; then
    echo ""
    echo "Extracting virtio drivers..."
    VIRTIO_MNT=$(mktemp -d)
    sudo mount -o loop,ro "$VIRTIO_ISO" "$VIRTIO_MNT"
    for dir in viostor vioscsi NetKVM Balloon vioinput vioser qxldod; do
        [ -d "$VIRTIO_MNT/$dir" ] && cp -r "$VIRTIO_MNT/$dir" "$MEDIA_DIR/$dir" 2>/dev/null
    done
    sudo umount "$VIRTIO_MNT"
    rmdir "$VIRTIO_MNT"
fi

# ── Summary ───────────────────────────────────────────────────────────────────

echo ""
echo "=== Media prepared ==="
echo ""
TOTAL=$(du -sh "$MEDIA_DIR" | cut -f1)
echo "Total size: $TOTAL"
echo "Contents:"
echo "  python/     Python 3.12 installer"
echo "  wheels/     $WHEEL_COUNT pip wheels (offline)"
echo "  ozma/       Agent source + install scripts + controller URL"
echo "  viostor/    Virtio storage driver"
echo "  NetKVM/     Virtio network driver (not needed — air-gapped!)"
echo ""
echo "The VM needs NO network access."
echo "Everything installs from the USB drive."
echo ""
echo "Next: bash dev/windows-vm/provision.sh create"
