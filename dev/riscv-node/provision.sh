#!/bin/bash
# First-time setup inside the RISC-V node VM.
# Run once after first boot: make provision-node
#
# Installs: kernel modules (dummy_hcd, v4l2loopback, snd-aloop, usbip),
#           ffmpeg, Python deps, and copies the ozma node package.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "${SCRIPT_DIR}/../config.env"
REPO_ROOT="${SCRIPT_DIR}/../../.."

ssh_node() {
    ssh ${SSH_OPTS} -i "${SSH_KEY}" -p "${NODE_SSH_PORT}" \
        "${NODE_USER}@localhost" "$@"
}

scp_to_node() {
    scp ${SSH_OPTS} -i "${SSH_KEY}" -P "${NODE_SSH_PORT}" -r "$@"
}

echo "=== Waiting for SSH to become available... ==="
for i in $(seq 1 60); do
    if ssh_node true 2>/dev/null; then break; fi
    printf "."
    sleep 2
done
echo ""

echo "=== System update and packages ==="
ssh_node sudo apt-get update -qq
ssh_node sudo apt-get install -y -qq \
    linux-modules-extra-"$(ssh_node uname -r)" \
    v4l2loopback-dkms \
    usbip \
    ffmpeg \
    python3-venv \
    python3-dev \
    avahi-daemon \
    avahi-utils \
    v4l-utils \
    alsa-utils \
    2>&1 | grep -v "^$"

# linux-modules-extra may not exist on all kernels; try direct modprobe
ssh_node sudo modprobe dummy_hcd 2>/dev/null || \
    echo "  dummy_hcd: will be available after kernel module install"

echo "=== Python dependencies ==="
ssh_node uv pip install --quiet \
    asyncvnc \
    numpy \
    Pillow \
    fastapi \
    "uvicorn[standard]" \
    zeroconf \
    aiohttp

echo "=== Copying ozma node package ==="
scp_to_node "${REPO_ROOT}/node" "${NODE_USER}@localhost:/home/${NODE_USER}/ozma-node"
scp_to_node "${REPO_ROOT}/tinynode/gadget" "${NODE_USER}@localhost:/home/${NODE_USER}/ozma-node/gadget"

echo "=== Installing init service ==="
scp_to_node "${SCRIPT_DIR}/init.sh" "${NODE_USER}@localhost:/home/${NODE_USER}/ozma-init.sh"
ssh_node chmod +x "/home/${NODE_USER}/ozma-init.sh"

# Install as rc.local so it runs on every boot
ssh_node sudo bash -c 'cat > /etc/rc.local << "EOF"
#!/bin/bash
sudo -u ubuntu /home/ubuntu/ozma-init.sh >> /var/log/ozma-init.log 2>&1 &
exit 0
EOF
chmod +x /etc/rc.local'

# Enable rc.local service
ssh_node sudo systemctl enable rc-local 2>/dev/null || true

echo ""
echo "=== Provisioning complete ==="
echo "Run: make init-node   (start ozma on the VM now without rebooting)"
echo "  or reboot the VM and it starts automatically."
