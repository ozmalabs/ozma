#!/bin/bash
# Proxmox VE plugin E2E test environment setup
#
# Builds a PVE VM with auto-installer, installs the ozma plugin,
# creates a test VM inside it, and runs the full E2E test suite.
#
# Usage:
#   bash tests/proxmox/setup_pve_test.sh          # full setup + test
#   bash tests/proxmox/setup_pve_test.sh test      # run tests only (VM exists)
#   bash tests/proxmox/setup_pve_test.sh teardown   # destroy PVE VM
#
# Prerequisites:
#   - QEMU/KVM with nested virt enabled (kvm_amd.nested=1 or kvm_intel.nested=1)
#   - Proxmox VE 9.1 ISO at /var/lib/libvirt/images/proxmox-ve_9.1-1.iso
#   - sshpass, xorriso, qemu-system-x86_64
#   - ~8GB RAM free, 50GB disk

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

PVE_ISO="/var/lib/libvirt/images/proxmox-ve_9.1-1.iso"
PVE_AUTO_ISO="/var/lib/libvirt/images/proxmox-ve_9.1-auto.iso"
PVE_DISK="/var/lib/libvirt/images/pve-test.qcow2"
PVE_PASSWORD="ozmatest123"
PVE_SSH_PORT=2250
PVE_WEB_PORT=8006
PVE_VNC_DISPLAY=50

log() { echo "[$(date +%H:%M:%S)] $*"; }

ssh_pve() {
    sshpass -p "$PVE_PASSWORD" ssh \
        -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR \
        -p "$PVE_SSH_PORT" \
        root@localhost "$@"
}

scp_pve() {
    sshpass -p "$PVE_PASSWORD" scp \
        -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR \
        -P "$PVE_SSH_PORT" \
        "$@"
}

# ── Step 1: Build auto-installer ISO ────────────────────────────────

build_iso() {
    log "Building auto-installer ISO..."

    if [ ! -f "$PVE_ISO" ]; then
        log "ERROR: PVE ISO not found at $PVE_ISO"
        log "Download from: https://download.proxmox.com/iso/proxmox-ve_9.1-1.iso"
        exit 1
    fi

    # Extract ISO
    local workdir="/tmp/pve-iso-work"
    rm -rf "$workdir"
    mkdir -p "$workdir"
    xorriso -osirrox on -indev "$PVE_ISO" -extract / "$workdir" 2>/dev/null

    # Add auto-installer config
    cat > "$workdir/auto-installer-mode.toml" << 'TOML'
[mode]
iso = {}
TOML

    # Copy answer file
    cp "$SCRIPT_DIR/answer.toml" "$workdir/answer.toml"

    # Set grub to auto-select installer with short timeout
    if grep -q "auto-installer-mode.toml" "$workdir/boot/grub/grub.cfg"; then
        sudo sed -i '/set timeout-style=menu/i\    set default=0' "$workdir/boot/grub/grub.cfg"
        sudo sed -i 's/set timeout=10/set timeout=3/' "$workdir/boot/grub/grub.cfg"
    fi

    # Rebuild ISO
    sudo xorriso -as mkisofs \
        -o "$PVE_AUTO_ISO" \
        -r -V "PVE" -J -joliet-long \
        -b boot/grub/i386-pc/eltorito.img \
        -c boot/grub/boot.cat \
        -no-emul-boot -boot-load-size 4 -boot-info-table \
        --grub2-boot-info \
        --grub2-mbr /usr/lib/grub/i386-pc/boot_hybrid.img \
        -eltorito-alt-boot -e efi.img -no-emul-boot \
        -isohybrid-gpt-basdat \
        "$workdir" 2>&1 | tail -2

    log "Auto-installer ISO built: $PVE_AUTO_ISO"
}

# ── Step 2: Install PVE ─────────────────────────────────────────────

install_pve() {
    log "Installing Proxmox VE..."

    # Kill existing VM
    sudo pkill -f "qemu.*pve-test" 2>/dev/null || true
    sleep 2

    # Create fresh disk
    sudo qemu-img create -f qcow2 "$PVE_DISK" 50G

    # Boot with auto-installer ISO
    sudo qemu-system-x86_64 -name pve-test \
        -m 8G -cpu host -enable-kvm -smp 4 -machine q35 \
        -drive "file=$PVE_DISK,format=qcow2,if=virtio" \
        -cdrom "$PVE_AUTO_ISO" \
        -boot d -vga std \
        -net nic,model=virtio \
        -net "user,hostfwd=tcp::${PVE_SSH_PORT}-:22,hostfwd=tcp::${PVE_WEB_PORT}-:8006" \
        -vnc ":${PVE_VNC_DISPLAY}" \
        -serial "file:/tmp/pve-serial.log" \
        -daemonize

    # Wait for installation to complete (monitor serial log)
    log "Waiting for PVE installation (this takes ~5-10 minutes)..."
    local max_wait=600
    local elapsed=0
    while [ $elapsed -lt $max_wait ]; do
        sleep 15
        elapsed=$((elapsed + 15))
        if grep -q "rebooting" /tmp/pve-serial.log 2>/dev/null; then
            log "Installation complete, VM rebooting..."
            break
        fi
        local progress
        progress=$(grep -o 'progress  [0-9.]*' /tmp/pve-serial.log 2>/dev/null | tail -1 || echo "waiting")
        log "  $progress ($elapsed/${max_wait}s)"
    done

    if [ $elapsed -ge $max_wait ]; then
        log "ERROR: Installation timed out"
        exit 1
    fi

    # Wait for reboot, then restart without CDROM
    sleep 5
    sudo pkill -f "qemu.*pve-test" 2>/dev/null || true
    sleep 3

    sudo qemu-system-x86_64 -name pve-test \
        -m 8G -cpu host -enable-kvm -smp 4 -machine q35 \
        -drive "file=$PVE_DISK,format=qcow2,if=virtio" \
        -boot c -vga std \
        -net nic,model=virtio \
        -net "user,hostfwd=tcp::${PVE_SSH_PORT}-:22,hostfwd=tcp::${PVE_WEB_PORT}-:8006" \
        -vnc ":${PVE_VNC_DISPLAY}" \
        -daemonize

    # Wait for PVE to boot
    log "Waiting for PVE to boot..."
    for i in $(seq 1 30); do
        if ssh_pve "pveversion" 2>/dev/null; then
            log "PVE is up!"
            return 0
        fi
        sleep 5
    done

    log "ERROR: PVE failed to boot"
    exit 1
}

# ── Step 3: Install ozma plugin ──────────────────────────────────────

install_plugin() {
    log "Installing ozma plugin..."

    # Package plugin + softnode
    tar czf /tmp/ozma-plugin.tar.gz \
        -C "$REPO_DIR" \
        proxmox-plugin/ \
        softnode/*.py \
        agent/ozma_desktop_agent.py agent/ozma_agent.py 2>/dev/null || true

    scp_pve /tmp/ozma-plugin.tar.gz root@localhost:/tmp/

    ssh_pve bash -s << 'INSTALL'
set -e
cd /tmp && tar xzf ozma-plugin.tar.gz

# Install Perl module
mkdir -p /usr/share/perl5/PVE/QemuServer
cp proxmox-plugin/perl/OzmaQemu.pm /usr/share/perl5/PVE/QemuServer/Ozma.pm

# Install Python services
mkdir -p /usr/lib/ozma-proxmox/softnode
cp proxmox-plugin/python/*.py /usr/lib/ozma-proxmox/
cp softnode/*.py /usr/lib/ozma-proxmox/softnode/

# Systemd template
cat > /lib/systemd/system/ozma-display@.service << 'SVC'
[Unit]
Description=Ozma Display Service for VM %i
After=network.target
[Service]
Type=simple
ExecStart=/usr/bin/python3 /usr/lib/ozma-proxmox/display-service.py %i
Restart=on-failure
RestartSec=5
[Install]
WantedBy=multi-user.target
SVC
systemctl daemon-reload
mkdir -p /var/run/ozma /var/lib/ozma /etc/ozma

# Fix apt repos (no-subscription)
for f in /etc/apt/sources.list.d/pve-enterprise.list /etc/apt/sources.list.d/ceph.list; do
    [ -f "$f" ] && echo "" > "$f"
done
echo 'deb http://download.proxmox.com/debian/pve trixie pve-no-subscription' > /etc/apt/sources.list.d/pve-no-sub.list
apt-get update -qq 2>&1 | tail -1
apt-get install -y -qq python3-aiohttp python3-pil 2>&1 | tail -1

echo "Plugin installed"
INSTALL

    log "Plugin installed"
}

# ── Step 4: Create test VM ───────────────────────────────────────────

create_test_vm() {
    log "Creating test VM inside PVE..."

    ssh_pve bash -s << 'CREATE'
set -e

# Enable images on local storage
pvesm set local --content iso,images,vztmpl,backup,snippets,rootdir

# Download Alpine ISO (small and fast)
wget -q -O /var/lib/vz/template/iso/alpine.iso \
    "https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/x86_64/alpine-virt-3.21.3-x86_64.iso"

# Create VM 100
qm create 100 --name doom-test --memory 1024 --cores 2 --ostype l26 \
    --ide2 local:iso/alpine.iso,media=cdrom \
    --virtio0 local:4,format=qcow2 \
    --net0 virtio,bridge=vmbr0 \
    --vga virtio \
    --serial0 socket \
    --boot order='ide2;virtio0'

# Add ozma D-Bus display + dedicated QMP socket
cat >> /etc/pve/qemu-server/100.conf << 'ARGS'
args: -display dbus,p2p=yes -chardev socket,id=ozma-mon,path=/var/run/ozma/vm100-ctrl.qmp,server=on,wait=off -mon chardev=ozma-mon,mode=control
ARGS

mkdir -p /var/run/ozma
qm start 100
echo "VM 100 created and started"
CREATE

    log "Test VM created"
}

# ── Step 5: Run tests ───────────────────────────────────────────────

run_tests() {
    log "Running E2E test suite..."
    cd "$REPO_DIR"

    if [ -f .venv/bin/python ]; then
        .venv/bin/python -m pytest tests/proxmox/test_proxmox_e2e.py -v --tb=short
    else
        python3 -m pytest tests/proxmox/test_proxmox_e2e.py -v --tb=short
    fi

    log "All tests passed!"
}

# ── Teardown ─────────────────────────────────────────────────────────

teardown() {
    log "Tearing down PVE test VM..."
    sudo pkill -f "qemu.*pve-test" 2>/dev/null || true
    sudo rm -f "$PVE_DISK" "$PVE_AUTO_ISO"
    rm -f /tmp/pve-serial.log /tmp/ozma-plugin.tar.gz
    log "Cleaned up"
}

# ── Main ─────────────────────────────────────────────────────────────

case "${1:-full}" in
    full)
        build_iso
        install_pve
        install_plugin
        create_test_vm
        run_tests
        log "=== All done! ==="
        log "PVE Web UI: https://localhost:${PVE_WEB_PORT}"
        log "PVE SSH: sshpass -p $PVE_PASSWORD ssh -p $PVE_SSH_PORT root@localhost"
        ;;
    test)
        run_tests
        ;;
    teardown)
        teardown
        ;;
    *)
        echo "Usage: $0 [full|test|teardown]"
        exit 1
        ;;
esac
