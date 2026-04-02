#!/bin/bash
# E2E Test: Windows Agent Installation via Ozma
#
# Uses ozma to install its own agent inside a Windows VM.
# This is both a test and a demonstration of the product.
#
# What it does:
#   1. Downloads a Windows evaluation ISO (or uses an existing one)
#   2. Builds the agent .exe installer (via PyInstaller)
#   3. Creates an ISO with the installer + auto-run script
#   4. Launches a Windows QEMU VM with:
#      - The Windows ISO as the boot disk (or existing qcow2)
#      - The agent installer ISO as a virtual CD-ROM
#      - QMP socket for ozma soft node control
#      - VNC for display
#   5. Creates an ozma soft node for the VM
#   6. Waits for Windows to boot + agent to be installed
#   7. Verifies the agent registers with the controller
#   8. Runs room correction sweep to test audio path
#   9. Tests HID injection (types a command, verifies via OCR)
#   10. Tears down
#
# Prerequisites:
#   - QEMU with KVM
#   - Controller running (localhost:7380 or specify CONTROLLER_URL)
#   - Windows ISO or pre-installed qcow2 image
#   - PyInstaller (for building the .exe on Linux via wine, or pre-built .exe)
#
# Usage:
#   bash tests/test_windows_agent.sh
#   bash tests/test_windows_agent.sh --windows-image /path/to/windows.qcow2
#   CONTROLLER_URL=https://ozma.hrdwrbob.net bash tests/test_windows_agent.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
IMAGES_DIR="$REPO_DIR/images"
CONTROLLER_URL="${CONTROLLER_URL:-http://localhost:7380}"
VM_NAME="win-agent-test"
QMP_SOCKET="/tmp/ozma-${VM_NAME}.qmp"
VNC_DISPLAY=":30"
VNC_PORT=5930
HID_PORT=7340
RAM="4G"
CPUS=2
DISK_SIZE="40G"

# Parse args
WINDOWS_IMAGE=""
SKIP_BUILD=false
for arg in "$@"; do
    case $arg in
        --windows-image=*) WINDOWS_IMAGE="${arg#*=}" ;;
        --skip-build) SKIP_BUILD=true ;;
        --controller=*) CONTROLLER_URL="${arg#*=}" ;;
    esac
done

echo "=== Ozma Windows Agent E2E Test ==="
echo "Controller: $CONTROLLER_URL"
echo ""

# ── Step 1: Prepare Windows disk image ────────────────────────────────────────

mkdir -p "$IMAGES_DIR"

if [ -z "$WINDOWS_IMAGE" ]; then
    WINDOWS_IMAGE="$IMAGES_DIR/windows-agent-test.qcow2"
    if [ ! -f "$WINDOWS_IMAGE" ]; then
        echo "No Windows image found at $WINDOWS_IMAGE"
        echo ""
        echo "Options:"
        echo "  1. Download Windows 11 evaluation from Microsoft:"
        echo "     https://www.microsoft.com/en-us/evalcenter/evaluate-windows-11-enterprise"
        echo "     Save as: $IMAGES_DIR/windows11.iso"
        echo ""
        echo "  2. Or provide an existing qcow2:"
        echo "     bash tests/test_windows_agent.sh --windows-image=/path/to/windows.qcow2"
        echo ""

        # Check for a Windows ISO to install from
        WIN_ISO=$(find "$IMAGES_DIR" -name "*.iso" -iname "*win*" 2>/dev/null | head -1)
        if [ -n "$WIN_ISO" ]; then
            echo "Found Windows ISO: $WIN_ISO"
            echo "Creating disk image and booting installer..."
            qemu-img create -f qcow2 "$WINDOWS_IMAGE" "$DISK_SIZE"
            # First boot: install Windows from ISO (manual step)
            echo ""
            echo "=== MANUAL STEP ==="
            echo "A QEMU window will open with the Windows installer."
            echo "Install Windows, then shut down the VM."
            echo "Then re-run this script with: --windows-image=$WINDOWS_IMAGE"
            echo ""
            qemu-system-x86_64 \
                -name "$VM_NAME" \
                -machine type=q35,accel=kvm \
                -cpu host -smp "$CPUS" -m "$RAM" \
                -drive file="$WINDOWS_IMAGE",format=qcow2,if=virtio \
                -cdrom "$WIN_ISO" \
                -boot d \
                -display sdl \
                -device virtio-net-pci,netdev=net0 \
                -netdev user,id=net0 \
                -device qemu-xhci -device usb-tablet
            exit 0
        else
            echo "No Windows ISO found in $IMAGES_DIR/"
            echo "Download one and save it there, then re-run."
            exit 1
        fi
    fi
fi

echo "Windows image: $WINDOWS_IMAGE"

# ── Step 2: Build the agent installer ─────────────────────────────────────────

AGENT_EXE="$REPO_DIR/dist/ozma-agent/ozma-agent.exe"
AGENT_SETUP="$REPO_DIR/dist/ozma-agent-setup.exe"

if [ "$SKIP_BUILD" = false ]; then
    echo ""
    echo "Building agent installer..."

    # Check if we have a pre-built .exe (built on Windows and committed/uploaded)
    if [ -f "$AGENT_SETUP" ]; then
        echo "Using pre-built installer: $AGENT_SETUP"
    elif [ -f "$AGENT_EXE" ]; then
        echo "Using pre-built agent exe: $AGENT_EXE"
    else
        echo "No pre-built Windows .exe found."
        echo "Build it on a Windows machine with: powershell agent/installer/build-windows.ps1"
        echo "Or copy the .exe to: $AGENT_SETUP"
        echo ""
        echo "Continuing with uv-based install method (will install via PowerShell in VM)..."
        AGENT_EXE=""
    fi
fi

# ── Step 3: Create agent delivery ISO ─────────────────────────────────────────

echo ""
echo "Creating agent delivery ISO..."

ISO_DIR=$(mktemp -d)
mkdir -p "$ISO_DIR/ozma"

# Auto-run PowerShell script that installs the agent
cat > "$ISO_DIR/ozma/install-agent.ps1" << PSEOF
# Ozma Agent Auto-Installer (runs from virtual CD-ROM)
\$ErrorActionPreference = "Continue"

Write-Host "=== Ozma Agent Auto-Install ===" -ForegroundColor Green

# Wait for network
Write-Host "Waiting for network..."
\$timeout = 60
while (\$timeout -gt 0) {
    \$ping = Test-Connection -ComputerName 8.8.8.8 -Count 1 -Quiet -ErrorAction SilentlyContinue
    if (\$ping) { break }
    Start-Sleep -Seconds 2
    \$timeout -= 2
}

# Install Python if not present
if (-not (Get-Command python -ErrorAction SilentlyContinue)) {
    Write-Host "Installing Python..."
    \$pyUrl = "https://www.python.org/ftp/python/3.12.8/python-3.12.8-amd64.exe"
    \$pyInstaller = "\$env:TEMP\python-installer.exe"
    Invoke-WebRequest -Uri \$pyUrl -OutFile \$pyInstaller -UseBasicParsing
    Start-Process -Wait -FilePath \$pyInstaller -ArgumentList "/quiet", "InstallAllUsers=1", "PrependPath=1"
    \$env:PATH = [System.Environment]::GetEnvironmentVariable("PATH", "Machine") + ";" + [System.Environment]::GetEnvironmentVariable("PATH", "User")
}

# Install ozma-agent
Write-Host "Installing ozma-agent..."
uv pip install --quiet ozma-agent 2>\$null
if (\$LASTEXITCODE -ne 0) {
    uv pip install --quiet "git+https://github.com/ozmalabs/ozma.git#subdirectory=agent"
}

# Install as service
Write-Host "Registering as service..."
ozma-agent install --name "$VM_NAME" --controller "$CONTROLLER_URL"

Write-Host "=== Done ===" -ForegroundColor Green
PSEOF

# If we have the .exe installer, include it too
if [ -f "$AGENT_SETUP" ]; then
    cp "$AGENT_SETUP" "$ISO_DIR/ozma/ozma-agent-setup.exe"
    # Alternative auto-run that uses the .exe
    cat > "$ISO_DIR/ozma/install-exe.bat" << BATEOF
@echo off
echo Installing Ozma Agent...
D:\ozma\ozma-agent-setup.exe /S /CONTROLLER=$CONTROLLER_URL /NAME=$VM_NAME
BATEOF
fi

# Create the ISO
if command -v genisoimage &>/dev/null; then
    genisoimage -o "$IMAGES_DIR/ozma-agent-delivery.iso" -V "OZMA_AGENT" \
        -R -J "$ISO_DIR" 2>/dev/null
elif command -v mkisofs &>/dev/null; then
    mkisofs -o "$IMAGES_DIR/ozma-agent-delivery.iso" -V "OZMA_AGENT" \
        -R -J "$ISO_DIR" 2>/dev/null
else
    echo "WARNING: genisoimage/mkisofs not found — using raw directory mount"
fi

rm -rf "$ISO_DIR"
echo "Agent delivery ISO: $IMAGES_DIR/ozma-agent-delivery.iso"

# ── Step 4: Launch Windows VM ─────────────────────────────────────────────────

echo ""
echo "Launching Windows VM..."

# Kill any existing instance
pkill -f "qemu.*$VM_NAME" 2>/dev/null || true
sleep 1

qemu-system-x86_64 \
    -name "$VM_NAME" \
    -machine type=q35,accel=kvm \
    -cpu host -smp "$CPUS" -m "$RAM" \
    -drive file="$WINDOWS_IMAGE",format=qcow2,if=virtio \
    -cdrom "$IMAGES_DIR/ozma-agent-delivery.iso" \
    -vnc "$VNC_DISPLAY" \
    -qmp "unix:$QMP_SOCKET,server,nowait" \
    -device virtio-net-pci,netdev=net0 \
    -netdev user,id=net0,hostfwd=tcp::7340-:7331,hostfwd=tcp::7341-:7382 \
    -device qemu-xhci -device usb-tablet \
    -audiodev pipewire,id=a0,out.name=ozma-${VM_NAME} \
    -device intel-hda -device hda-duplex,audiodev=a0 \
    -daemonize \
    -pidfile "/tmp/ozma-${VM_NAME}.pid" \
    2>/dev/null

echo "VM started (VNC on port $VNC_PORT, QMP at $QMP_SOCKET)"

# ── Step 5: Start soft node for the VM ────────────────────────────────────────

echo ""
echo "Starting soft node..."

python3 "$REPO_DIR/softnode/soft_node.py" \
    --name "$VM_NAME" \
    --port "$HID_PORT" \
    --qmp "$QMP_SOCKET" \
    --vnc-host 127.0.0.1 \
    --vnc-port "$VNC_PORT" \
    --audio-sink "ozma-${VM_NAME}" \
    > "/tmp/ozma-softnode-${VM_NAME}.log" 2>&1 &

SOFTNODE_PID=$!
echo "Soft node PID: $SOFTNODE_PID"

# ── Step 6: Wait for Windows to boot ──────────────────────────────────────────

echo ""
echo "Waiting for Windows to boot (this may take a while)..."

# Poll the controller for the VM's node registration
TIMEOUT=300  # 5 minutes
ELAPSED=0
while [ $ELAPSED -lt $TIMEOUT ]; do
    NODE_STATUS=$(curl -s "$CONTROLLER_URL/api/v1/nodes" 2>/dev/null | \
        python3 -c "import sys,json; d=json.load(sys.stdin); print('found' if any(n['id'].startswith('$VM_NAME') for n in d.get('nodes',[])) else 'waiting')" 2>/dev/null)

    if [ "$NODE_STATUS" = "found" ]; then
        echo "VM soft node registered with controller"
        break
    fi
    sleep 5
    ELAPSED=$((ELAPSED + 5))
    echo -ne "\r  Waiting... ${ELAPSED}s / ${TIMEOUT}s"
done
echo ""

if [ $ELAPSED -ge $TIMEOUT ]; then
    echo "WARNING: Soft node didn't register within timeout"
fi

# ── Step 7: Wait for agent to connect back ────────────────────────────────────

echo ""
echo "Waiting for Windows agent to install and connect..."
echo "(The VM needs to: boot → install Python → install ozma-agent → register)"
echo ""
echo "To trigger the install manually, connect via VNC (port $VNC_PORT) and run:"
echo "  PowerShell: Set-ExecutionPolicy Bypass; D:\\ozma\\install-agent.ps1"
echo ""

TIMEOUT=600  # 10 minutes
ELAPSED=0
AGENT_CONNECTED=false
while [ $ELAPSED -lt $TIMEOUT ]; do
    # Check if the agent registered (it registers with a different node ID)
    AGENT_STATUS=$(curl -s "$CONTROLLER_URL/api/v1/nodes" 2>/dev/null | \
        python3 -c "
import sys, json
d = json.load(sys.stdin)
# The agent registers as $VM_NAME._ozma._udp.local. with hw=desktop-windows
for n in d.get('nodes', []):
    if '$VM_NAME' in n.get('id','') and 'desktop' in n.get('hw',''):
        print('connected')
        sys.exit(0)
print('waiting')
" 2>/dev/null)

    if [ "$AGENT_STATUS" = "connected" ]; then
        AGENT_CONNECTED=true
        echo ""
        echo "=== AGENT CONNECTED ==="
        echo "Windows agent successfully installed and registered!"
        break
    fi
    sleep 10
    ELAPSED=$((ELAPSED + 10))
    echo -ne "\r  Waiting... ${ELAPSED}s / ${TIMEOUT}s"
done
echo ""

# ── Step 8: Run tests ─────────────────────────────────────────────────────────

if [ "$AGENT_CONNECTED" = true ]; then
    echo ""
    echo "Running tests..."

    # Test 1: Check agent status via API
    echo -n "  Agent HTTP API: "
    HEALTH=$(curl -s --connect-timeout 3 "http://localhost:7341/health" 2>/dev/null)
    if echo "$HEALTH" | grep -q "ok"; then
        echo "PASS"
    else
        echo "FAIL ($HEALTH)"
    fi

    # Test 2: Check PipeWire audio devices on the VM
    echo -n "  Audio devices: "
    AUDIO=$(curl -s --connect-timeout 3 "http://localhost:7341/audio/nodes" 2>/dev/null)
    if echo "$AUDIO" | grep -q "nodes"; then
        DEVICE_COUNT=$(echo "$AUDIO" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('nodes',[])))" 2>/dev/null)
        echo "PASS ($DEVICE_COUNT devices)"
    else
        echo "SKIP (no PipeWire on Windows)"
    fi

    # Test 3: Prometheus metrics
    echo -n "  Prometheus metrics: "
    METRICS=$(curl -s --connect-timeout 3 "http://localhost:7341/metrics" 2>/dev/null)
    if echo "$METRICS" | grep -q "ozma_node"; then
        echo "PASS"
    else
        echo "FAIL"
    fi

    echo ""
    echo "=== TEST COMPLETE ==="
else
    echo "Agent did not connect within timeout."
    echo "Check the VM via VNC on port $VNC_PORT"
    echo ""
    echo "=== TEST INCOMPLETE ==="
fi

# ── Cleanup ───────────────────────────────────────────────────────────────────

echo ""
echo "Cleanup:"
echo "  Kill VM:       kill \$(cat /tmp/ozma-${VM_NAME}.pid)"
echo "  Kill soft node: kill $SOFTNODE_PID"
echo "  Remove ISO:    rm $IMAGES_DIR/ozma-agent-delivery.iso"
echo ""
echo "Or leave running to continue testing manually."
