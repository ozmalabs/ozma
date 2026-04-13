# ozma-agent — Windows Build Script
#
# Run from the repo root on a Windows machine:
#   powershell -ExecutionPolicy Bypass -File ozma-agent\installer\build-windows.ps1
#
# Prerequisites (auto-checked, installer links provided on failure):
#   - Rust toolchain  (rustup.rs)
#   - cargo-wix       (cargo install cargo-wix)
#   - WiX Toolset v3  (wixtoolset.org) — for MSI; skipped if absent
#
# Output:
#   dist\ozma-agent-<version>-windows-x86_64.zip   — standalone zip
#   target\wix\ozma-agent-<version>-x86_64.msi     — MSI installer (if WiX present)

$ErrorActionPreference = "Stop"

Write-Host "=== Ozma Agent Windows Build ===" -ForegroundColor Green

# ── Check Rust ────────────────────────────────────────────────────────────────

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host "ERROR: Rust not found. Install from https://rustup.rs" -ForegroundColor Red
    exit 1
}

$rustVer = rustc --version
Write-Host "Rust:   $rustVer"

# Ensure the MSVC target is installed
$target = "x86_64-pc-windows-msvc"
$installedTargets = rustup target list --installed 2>$null
if ($installedTargets -notmatch $target) {
    Write-Host "Adding Rust target $target..."
    rustup target add $target
}

# ── Build binary ──────────────────────────────────────────────────────────────

Write-Host ""
Write-Host "Building ozma-agent (release, $target)..."
cargo build --release --target $target -p ozma-agent

$exePath = "target\$target\release\ozma-agent.exe"
if (-not (Test-Path $exePath)) {
    Write-Host "ERROR: build produced no binary at $exePath" -ForegroundColor Red
    exit 1
}

# Read version from Cargo.toml
$version = (Select-String -Path "ozma-agent\Cargo.toml" -Pattern '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value
Write-Host "Version: $version"

# ── Standalone zip (no installer) ────────────────────────────────────────────

New-Item -ItemType Directory -Force -Path "dist" | Out-Null

$stageDir = "dist\ozma-agent-windows"
Remove-Item -Recurse -Force $stageDir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $stageDir | Out-Null

Copy-Item $exePath "$stageDir\ozma-agent.exe"

# Bundle README
@"
Ozma Agent
==========

Run: ozma-agent.exe --controller-url http://your-controller:7380

Options:
  --api-port         TCP port for the HTTP API   (default 7381)
  --metrics-port     TCP port for Prometheus      (default 9101)
  --wg-port          WireGuard UDP port           (default 51820)
  --controller-url   Controller URL               (default http://localhost:7380)

Environment variables mirror every --flag (OZMA_API_PORT, OZMA_CONTROLLER_URL, ...).

Install as a Windows service:
  ozma-agent.exe --install    (registers OzmaAgent service)
  ozma-agent.exe --uninstall  (removes OzmaAgent service)

Or run the MSI installer for a guided setup with Start Menu shortcuts.
"@ | Set-Content "$stageDir\README.txt"

$zipPath = "dist\ozma-agent-$version-windows-x86_64.zip"
Remove-Item $zipPath -ErrorAction SilentlyContinue
Compress-Archive -Path "$stageDir\*" -DestinationPath $zipPath
$zipSize = [math]::Round((Get-Item $zipPath).Length / 1MB, 1)
Write-Host "Standalone zip: $zipPath ($zipSize MB)" -ForegroundColor Green

# ── MSI installer via cargo-wix ───────────────────────────────────────────────

# Check cargo-wix
$cargoWix = cargo install --list 2>$null | Select-String "cargo-wix"
if (-not $cargoWix) {
    Write-Host ""
    Write-Host "Installing cargo-wix..." -ForegroundColor Cyan
    cargo install cargo-wix
}

# Check WiX Toolset (candle.exe / light.exe must be on PATH or in standard install dir)
$wixOnPath = Get-Command candle.exe -ErrorAction SilentlyContinue
$wixDefault = "C:\Program Files (x86)\WiX Toolset v3.11\bin\candle.exe"
$wixInstalled = $wixOnPath -or (Test-Path $wixDefault)

if (-not $wixInstalled) {
    Write-Host ""
    Write-Host "WiX Toolset v3 not found — skipping MSI build." -ForegroundColor Yellow
    Write-Host "To build the MSI installer:"
    Write-Host "  1. Download WiX Toolset v3.x from https://wixtoolset.org/releases/"
    Write-Host "  2. Re-run this script"
    Write-Host ""
    Write-Host "=== BUILD COMPLETE (zip only) ===" -ForegroundColor Green
    Write-Host "  $zipPath"
    exit 0
}

# Add WiX to PATH if only found at default location
if (-not $wixOnPath -and (Test-Path $wixDefault)) {
    $env:PATH = "C:\Program Files (x86)\WiX Toolset v3.11\bin;$env:PATH"
}

Write-Host ""
Write-Host "Building MSI installer via cargo-wix..." -ForegroundColor Cyan
cargo wix --package ozma-agent --target $target --nocapture

# cargo wix outputs to target/wix/
$msiPattern = "target\wix\ozma-agent-*.msi"
$msiFile = Get-ChildItem $msiPattern -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1

if ($msiFile) {
    $msiSize = [math]::Round($msiFile.Length / 1MB, 1)
    Write-Host ""
    Write-Host "=== BUILD COMPLETE ===" -ForegroundColor Green
    Write-Host "  Zip: $zipPath ($zipSize MB)"
    Write-Host "  MSI: $($msiFile.FullName) ($msiSize MB)"
} else {
    Write-Host "WARNING: cargo-wix ran but MSI not found at $msiPattern" -ForegroundColor Yellow
    Write-Host "Check cargo-wix output above for errors."
    exit 1
}
