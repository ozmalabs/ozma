# Ozma Agent — Windows Build Script
#
# Run from the repo root on a Windows machine:
#   powershell -ExecutionPolicy Bypass -File agent\installer\build-windows.ps1
#
# Prerequisites:
#   - Python 3.11+ (python.org installer, add to PATH)
#   - NSIS 3.x (nsis.sourceforge.io, add to PATH)
#   - NSSM (nssm.cc — optional, bundled for service management)
#
# Output:
#   dist\ozma-agent-setup.exe  — the installer

$ErrorActionPreference = "Stop"

Write-Host "=== Ozma Agent Windows Build ===" -ForegroundColor Green

# Check prerequisites
if (-not (Get-Command python -ErrorAction SilentlyContinue)) {
    Write-Host "ERROR: Python not found. Install from python.org" -ForegroundColor Red
    exit 1
}

$pyVer = python --version 2>&1
Write-Host "Python: $pyVer"

# Install build dependencies
Write-Host "`nInstalling build dependencies..."
uv pip install --quiet pyinstaller aiohttp zeroconf numpy

# Optional: dxcam for DXGI capture (may fail on non-Windows or CI)
uv pip install --quiet dxcam 2>$null

# Build with PyInstaller
Write-Host "`nBuilding with PyInstaller..."
python -m PyInstaller agent\installer\ozma-agent.spec --noconfirm --clean

if (-not (Test-Path "dist\ozma-agent\ozma-agent.exe")) {
    Write-Host "ERROR: PyInstaller build failed" -ForegroundColor Red
    exit 1
}

Write-Host "PyInstaller build complete: dist\ozma-agent\" -ForegroundColor Green

# Download NSSM if not present (for Windows service management)
$nssmPath = "dist\ozma-agent\nssm.exe"
if (-not (Test-Path $nssmPath)) {
    Write-Host "`nDownloading NSSM (service manager)..."
    $nssmUrl = "https://nssm.cc/release/nssm-2.24.zip"
    $nssmZip = "$env:TEMP\nssm.zip"
    try {
        Invoke-WebRequest -Uri $nssmUrl -OutFile $nssmZip -UseBasicParsing
        Expand-Archive -Path $nssmZip -DestinationPath "$env:TEMP\nssm" -Force
        Copy-Item "$env:TEMP\nssm\nssm-2.24\win64\nssm.exe" $nssmPath
        Remove-Item $nssmZip -Force
        Remove-Item "$env:TEMP\nssm" -Recurse -Force
        Write-Host "NSSM bundled" -ForegroundColor Green
    } catch {
        Write-Host "NSSM download failed (will use Task Scheduler fallback)" -ForegroundColor Yellow
    }
}

# Build NSIS installer
if (Get-Command makensis -ErrorAction SilentlyContinue) {
    Write-Host "`nBuilding NSIS installer..."
    makensis agent\installer\ozma-agent.nsi
    if (Test-Path "dist\ozma-agent-setup.exe") {
        $size = (Get-Item "dist\ozma-agent-setup.exe").Length / 1MB
        Write-Host "`n=== BUILD COMPLETE ===" -ForegroundColor Green
        Write-Host "Installer: dist\ozma-agent-setup.exe ($([math]::Round($size, 1)) MB)"
    } else {
        Write-Host "NSIS build failed" -ForegroundColor Red
        exit 1
    }
} else {
    Write-Host "`nNSIS not found — skipping installer. Standalone exe at dist\ozma-agent\" -ForegroundColor Yellow
    Write-Host "Install NSIS from https://nsis.sourceforge.io to build the installer"
}
