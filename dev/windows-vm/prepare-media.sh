#!/bin/bash
# Prepare offline media for air-gapped Windows Rust node (ozma-node --profile desktop) self-bootstrap.
#
# Prepares everything needed for the Windows VM to build, test, and install
# `ozma-node` from source (or use a pre-built binary if present) WITHOUT
# requiring network access in the VM. All files go into MEDIA_DIR, which the
# soft node / QEMU serves as a USB drive (E:\).
#
# What gets prepared:
#   - rust/rustup-init.exe          Rustup installer for Windows x86_64 (unless SKIP_DOWNLOADS=1)
#   - ozma/src/                     Rust workspace (Cargo.toml, Cargo.lock, ozma-* crates)
#   - ozma/src/vendor/              Vendored Cargo crates for --offline builds (unless SKIP_VENDOR=1)
#   - ozma/install-offline.ps1      Self-bootstrap PowerShell script: installs Rust if missing,
#                                   runs `cargo test -p ozma-input`, builds release `ozma-node.exe`,
#                                   registers & starts `OzmaNode` Windows service (`--profile desktop`).
#   - ozma/install-offline.bat      Batch wrapper for FirstLogonCommands / manual run
#   - ozma/controller.txt           Controller URL for the node to register with
#   - viostor/ ...                  Virtio drivers (if images/virtio-win.iso is present)
#
# Usage:
#   bash dev/windows-vm/prepare-media.sh
#   SKIP_DOWNLOADS=1 SKIP_VENDOR=1 MEDIA_DIR=/tmp/test-media bash dev/windows-vm/prepare-media.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
MEDIA_DIR="${MEDIA_DIR:-$REPO_DIR/images/ozma-media-win10}"
CONTROLLER_URL="${CONTROLLER_URL:-http://10.0.2.2:7380}"
SKIP_DOWNLOADS="${SKIP_DOWNLOADS:-0}"
SKIP_VENDOR="${SKIP_VENDOR:-0}"

# Prefer rustup's cargo if installed, else fall back to PATH
if [ -x "$HOME/.cargo/bin/cargo" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi

echo "=== Preparing offline media for Windows Rust node (ozma-node) ==="
echo "Repo dir:       $REPO_DIR"
echo "Media dir:      $MEDIA_DIR"
echo "Controller URL: $CONTROLLER_URL"
echo ""

mkdir -p "$MEDIA_DIR/ozma/src"
mkdir -p "$MEDIA_DIR/rust"

# ── Rustup installer for Windows ──────────────────────────────────────────────

RUSTUP_URL="https://win.rustup.rs/x86_64"
RUSTUP_FILE="$MEDIA_DIR/rust/rustup-init.exe"

if [ "$SKIP_DOWNLOADS" = "1" ]; then
    echo "Skipping downloads (SKIP_DOWNLOADS=1)"
elif [ ! -f "$RUSTUP_FILE" ]; then
    echo "Downloading rustup-init.exe (Windows x86_64)..."
    curl -fL -o "$RUSTUP_FILE" "$RUSTUP_URL"
else
    echo "rustup-init.exe: already downloaded"
fi

# ── Rust workspace source (all workspace crates for ozma-node) ────────────────

echo ""
echo "Copying Rust workspace source for ozma-node..."

SRC_DIR="$MEDIA_DIR/ozma/src"
rm -rf "$SRC_DIR"/ozma-* "$SRC_DIR"/softnode-qmp "$SRC_DIR"/proto "$SRC_DIR"/protocol 2>/dev/null || true

cp "$REPO_DIR/Cargo.toml" "$SRC_DIR/Cargo.toml"
cp "$REPO_DIR/Cargo.lock" "$SRC_DIR/Cargo.lock"
[ -f "$REPO_DIR/build.rs" ] && cp "$REPO_DIR/build.rs" "$SRC_DIR/build.rs"
[ -d "$REPO_DIR/proto" ] && cp -r "$REPO_DIR/proto" "$SRC_DIR/proto"
[ -d "$REPO_DIR/protocol" ] && cp -r "$REPO_DIR/protocol" "$SRC_DIR/protocol"

for crate_dir in "$REPO_DIR"/ozma-* "$REPO_DIR"/softnode-qmp; do
    [ -d "$crate_dir" ] || continue
    [ -f "$crate_dir/Cargo.toml" ] || continue
    crate_name="$(basename "$crate_dir")"
    cp -r "$crate_dir" "$SRC_DIR/$crate_name"
done

# Strip any local target/ directories inside copied crates (never touch vendor/)
rm -rf "$SRC_DIR/target" "$SRC_DIR"/ozma-*/target "$SRC_DIR"/softnode-qmp/target 2>/dev/null || true

# ── Pre-built Windows binary (optional fast-path fallback) ────────────────────

for prebuilt in \
    "$REPO_DIR/dist/ozma-node-windows/ozma-node.exe" \
    "$REPO_DIR/target/x86_64-pc-windows-msvc/release/ozma-node.exe" \
    "$REPO_DIR/target/x86_64-pc-windows-gnu/release/ozma-node.exe"; do
    if [ -f "$prebuilt" ]; then
        mkdir -p "$MEDIA_DIR/ozma/prebuilt"
        cp "$prebuilt" "$MEDIA_DIR/ozma/prebuilt/ozma-node.exe"
        echo "Included pre-built fallback binary: $prebuilt"
        break
    fi
done

# ── Vendor Cargo crates for air-gapped builds ─────────────────────────────────

if [ "$SKIP_VENDOR" = "1" ]; then
    echo "Skipping cargo vendor (SKIP_VENDOR=1)"
elif command -v cargo &>/dev/null; then
    echo ""
    echo "Vendoring Cargo dependencies for offline Windows build..."
    mkdir -p "$SRC_DIR/.cargo"
    (
        cd "$SRC_DIR"
        cargo vendor --locked vendor > .cargo/config.toml 2>/dev/null || \
            cargo vendor vendor > .cargo/config.toml
    )
    CRATE_COUNT=$(ls -d "$SRC_DIR/vendor"/*/ 2>/dev/null | wc -l)
    echo "Vendored $CRATE_COUNT crates into $SRC_DIR/vendor"
else
    echo "WARNING: cargo not found on host; skipping cargo vendor (VM will need network for crates.io)"
fi

# ── Offline self-bootstrap install script ─────────────────────────────────────

echo ""
echo "Writing Windows self-bootstrap script (install-offline.ps1)..."

cat > "$MEDIA_DIR/ozma/install-offline.ps1" << 'PSEOF'
# Ozma Rust Node (ozma-node --profile desktop) - Self-Bootstrap Build, Test & Install
#
# Runs inside the Windows VM from the OZMA USB drive (E:\).
# 1. Ensures Rust toolchain is installed (from E:\rust\rustup-init.exe if missing)
# 2. Copies E:\ozma\src -> C:\ozma-build
# 3. Runs `cargo test -p ozma-input` (verifies input sink + protocol natively on Windows)
# 4. Runs `cargo build --release -p ozma-node`
# 5. Copies ozma-node.exe to C:\Program Files\Ozma\Node\ and E:\ozma\ozma-node.exe
# 6. Registers and starts the OzmaNode Windows service (--profile desktop)

$ErrorActionPreference = "Stop"

$drive = (Get-Volume | Where-Object { $_.FileSystemLabel -eq "OZMA" } | Select-Object -First 1).DriveLetter
if (-not $drive) {
    foreach ($candidate in @("E", "D", "F", "G")) {
        if (Test-Path "${candidate}:\ozma\install-offline.ps1") {
            $drive = $candidate
            break
        }
    }
}
if (-not $drive) { $drive = "E" }
$root = "${drive}:\"

Write-Host "=== Ozma Rust Node Self-Bootstrap ===" -ForegroundColor Green
Write-Host "Media drive: $root"

$bundledToolchain = Join-Path $root "rust\toolchain"
if (Test-Path (Join-Path $bundledToolchain "bin\cargo.exe")) {
    $localRust = "C:\rust"
    if (-not (Test-Path (Join-Path $localRust "bin\cargo.exe"))) {
        Write-Host "`nCopying offline Rust toolchain to $localRust..." -ForegroundColor Cyan
        Copy-Item -Recurse -Force $bundledToolchain $localRust
    }
    $selfContained = Join-Path $localRust "lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained"
    $rustBin = Join-Path $localRust "bin"
    if (Test-Path (Join-Path $selfContained "dlltool.exe")) {
        Copy-Item -Force (Join-Path $selfContained "dlltool.exe") (Join-Path $rustBin "dlltool.exe")
    }
    $bundledMingw = Join-Path $root "rust\mingw64\w64devkit"
    if (Test-Path (Join-Path $bundledMingw "bin\gcc.exe")) {
        $localMingw = "C:\mingw64"
        if (-not (Test-Path (Join-Path $localMingw "bin\gcc.exe"))) {
            Write-Host "`nCopying offline MinGW-w64 (w64devkit) to $localMingw..." -ForegroundColor Cyan
            Copy-Item -Recurse -Force $bundledMingw $localMingw
        }
        Copy-Item -Force (Join-Path $localMingw "bin\as.exe") (Join-Path $rustBin "as.exe")
        $mingw32Make = Join-Path $localMingw "bin\mingw32-make.exe"
        if (-not (Test-Path $mingw32Make)) {
            Copy-Item -Force (Join-Path $localMingw "bin\make.exe") $mingw32Make
        }
        $env:CC = "$localMingw\bin\gcc.exe"
        $env:AR = "$localMingw\bin\ar.exe"
        $env:CXX = "$localMingw\bin\g++.exe"
        $env:CMAKE_GENERATOR = "MinGW Makefiles"
        $env:CMAKE_MAKE_PROGRAM = $mingw32Make
    }
    $bundledCmake = Join-Path $root "rust\cmake"
    if (Test-Path (Join-Path $bundledCmake "bin\cmake.exe")) {
        $localCmake = "C:\cmake"
        if (-not (Test-Path (Join-Path $localCmake "bin\cmake.exe"))) {
            Write-Host "`nCopying offline CMake to $localCmake..." -ForegroundColor Cyan
            Copy-Item -Recurse -Force $bundledCmake $localCmake
        }
        $env:PATH = "$localCmake\bin;$env:PATH"
        if (Test-Path (Join-Path $localMingw "bin\make.exe")) {
            Copy-Item -Force (Join-Path $localMingw "bin\make.exe") (Join-Path $localCmake "bin\mingw32-make.exe")
        }
    }
    $env:PATH = "$rustBin;C:\Windows\System32;$env:PATH"
} else {
    $cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
    if (Test-Path $cargoBin) {
        $env:PATH = "$cargoBin;$env:PATH"
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        $rustupExe = Join-Path $root "rust\rustup-init.exe"
        if (Test-Path $rustupExe) {
            Write-Host "`nInstalling Rust toolchain (x86_64-pc-windows-msvc)..." -ForegroundColor Cyan
            Start-Process -Wait -FilePath $rustupExe -ArgumentList "-y", "--default-toolchain", "stable-x86_64-pc-windows-msvc", "--profile", "minimal"
            $env:PATH = "$cargoBin;$env:PATH"
        } else {
            Write-Host "WARNING: cargo not found and $rustupExe missing." -ForegroundColor Yellow
        }
    }
}

$exeBuilt = $null

if (Get-Command cargo -ErrorAction SilentlyContinue) {
    Write-Host "Rust: $(rustc --version)" -ForegroundColor Green

    # Step 2: Copy workspace source to local disk for fast Cargo build
    $buildDir = "C:\ozma-build"
    Write-Host "`nCopying Rust workspace to $buildDir..."
    if ((Get-Location).Path -like "$buildDir*") {
        Set-Location C:\
    }
    if (Test-Path $buildDir) {
        Remove-Item -Recurse -Force $buildDir
    }
    Copy-Item -Recurse -Force (Join-Path $root "ozma\src") $buildDir

    Push-Location $buildDir
    try {
        $offlineFlag = @()
        if (Test-Path (Join-Path $buildDir "vendor")) {
            $offlineFlag = @("--offline")
            Write-Host "Using vendored crates (--offline)" -ForegroundColor Cyan
        }

        # Step 3: Run native Windows Rust tests
        Write-Host "`nRunning cargo test -p ozma-input..." -ForegroundColor Cyan
        & cargo test @offlineFlag -p ozma-input
        if ($LASTEXITCODE -ne 0) {
            throw "cargo test -p ozma-input failed with exit code $LASTEXITCODE"
        }
        Write-Host "Rust tests PASSED" -ForegroundColor Green

        # Step 4: Build ozma-node release binary
        Write-Host "`nBuilding ozma-node (release)..." -ForegroundColor Cyan
        & cargo build @offlineFlag --release -p ozma-node
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build -p ozma-node failed with exit code $LASTEXITCODE"
        }

        foreach ($candidate in @(
            (Join-Path $buildDir "target\release\ozma-node.exe"),
            (Join-Path $buildDir "target\x86_64-pc-windows-msvc\release\ozma-node.exe")
        )) {
            if (Test-Path $candidate) {
                $exeBuilt = $candidate
                break
            }
        }
    } finally {
        Pop-Location
    }
}

# Fallback to prebuilt binary if source build was skipped/unavailable
if (-not $exeBuilt) {
    $prebuilt = Join-Path $root "ozma\prebuilt\ozma-node.exe"
    if (Test-Path $prebuilt) {
        Write-Host "`nUsing prebuilt ozma-node.exe fallback..." -ForegroundColor Yellow
        $exeBuilt = $prebuilt
    } else {
        Write-Host "ERROR: No ozma-node.exe built and no prebuilt fallback found." -ForegroundColor Red
        exit 1
    }
}

# Step 5: Stage binary to Program Files, C:\, and back to USB drive
$installDir = "C:\Program Files\Ozma\Node"
New-Item -ItemType Directory -Force -Path $installDir | Out-Null
$installedExe = Join-Path $installDir "ozma-node.exe"
Copy-Item $exeBuilt $installedExe -Force
Copy-Item $exeBuilt "C:\ozma-node.exe" -Force
Copy-Item $exeBuilt (Join-Path $root "ozma\ozma-node.exe") -Force -ErrorAction SilentlyContinue

Write-Host "`nInstalled: $installedExe" -ForegroundColor Green

# Step 6: Configure and start OzmaNode Windows service (--profile desktop)
$controllerFile = Join-Path $root "ozma\controller.txt"
$controllerUrl = "http://localhost:7380"
if (Test-Path $controllerFile) {
    $controllerUrl = (Get-Content $controllerFile -Raw).Trim()
}

# Ensure VirtIO network driver and firewall rule are present
$netkvmInf = Join-Path $root "NetKVM\w10\amd64\netkvm.inf"
if (Test-Path $netkvmInf) {
    pnputil /add-driver $netkvmInf /install | Out-Null
}
netsh advfirewall firewall delete rule name="OzmaNode" 2>$null | Out-Null
netsh advfirewall firewall add rule name="OzmaNode" dir=in action=allow program="$installedExe" enable=yes | Out-Null

Write-Host "`nStarting OzmaNode (profile: desktop, controller: $controllerUrl)..." -ForegroundColor Cyan
taskkill /f /im ozma-node.exe 2>$null | Out-Null
Start-Sleep -Seconds 1
$nodeArgs = "--profile desktop --register-url $controllerUrl --register-host localhost"
schtasks /Create /F /SC ONSTART /RU SYSTEM /TN "OzmaNode" /TR "`"$installedExe`" $nodeArgs" | Out-Null
Start-Process -FilePath $installedExe -ArgumentList $nodeArgs -WindowStyle Minimized

# Wait briefly for /health
$healthy = $false
for ($i = 0; $i -lt 15; $i++) {
    try {
        $resp = Invoke-WebRequest -Uri "http://127.0.0.1:7382/health" -UseBasicParsing -TimeoutSec 2
        if ($resp.StatusCode -eq 200) {
            $healthy = $true
            break
        }
    } catch {}
    Start-Sleep -Seconds 1
}

if ($healthy) {
    Write-Host "Node /health: OK" -ForegroundColor Green
} else {
    Write-Host "WARNING: Node /health not yet responding (service may still be starting)" -ForegroundColor Yellow
}

Write-Host "`n=== Complete ===" -ForegroundColor Green
Write-Host "Rust ozma-node (--profile desktop) built, tested, and started."
Write-Host "Controller: $controllerUrl"
PSEOF

# Batch launcher
cat > "$MEDIA_DIR/ozma/install-offline.bat" << 'BATEOF'
@echo off
echo === Ozma Rust Node Self-Bootstrap ===
powershell -ExecutionPolicy Bypass -File "%~dp0install-offline.ps1"
if errorlevel 1 (
    echo Bootstrap failed with error %errorlevel%
    exit /b %errorlevel%
)
BATEOF

# Controller URL
echo "$CONTROLLER_URL" > "$MEDIA_DIR/ozma/controller.txt"

# ── Virtio drivers ────────────────────────────────────────────────────────────

VIRTIO_ISO="$REPO_DIR/images/virtio-win.iso"
if [ -d "$MEDIA_DIR/viostor" ]; then
    echo "Virtio drivers: already extracted"
elif [ -f "$VIRTIO_ISO" ]; then
    echo ""
    echo "Extracting virtio drivers..."
    VIRTIO_MNT=$(mktemp -d)
    sudo mount -o loop,ro "$VIRTIO_ISO" "$VIRTIO_MNT"
    for dir in viostor vioscsi NetKVM Balloon vioinput vioser qxldod; do
        [ -d "$VIRTIO_MNT/$dir" ] && cp -r "$VIRTIO_MNT/$dir" "$MEDIA_DIR/$dir" 2>/dev/null || true
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
echo "  rust/       rustup-init.exe (Windows x86_64)"
echo "  ozma/src/   Rust workspace (ozma-node + all workspace crates)"
echo "  ozma/       install-offline.ps1, install-offline.bat, controller.txt"
echo ""
echo "Next: bash dev/windows-vm/provision.sh create"
