#!/bin/bash
# First-time setup inside the Alpine Linux target VM.
# Run once after Alpine is installed: make provision-target
#
# Installs: usbip, evtest, alsa-utils, openssh.
# Sets up: SSH, usbip kernel modules on boot.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "${SCRIPT_DIR}/../config.env"

ssh_target() {
    ssh ${SSH_OPTS} -i "${SSH_KEY}" -p "${TARGET_SSH_PORT}" \
        "${TARGET_USER}@localhost" "$@"
}

echo "=== Waiting for SSH... ==="
for i in $(seq 1 30); do
    if ssh_target true 2>/dev/null; then break; fi
    printf "."; sleep 2
done
echo ""

echo "=== Installing packages ==="
ssh_target "apk add --no-cache usbip-utils usbip evtest alsa-utils linux-headers"

echo "=== Enabling USB/IP modules on boot ==="
ssh_target "echo 'vhci-hcd' >> /etc/modules"
ssh_target "modprobe vhci-hcd 2>/dev/null || true"

echo "=== Verifying usbip is available ==="
ssh_target "usbip version"

echo ""
echo "Target VM provisioned. Run: make attach-usb"
