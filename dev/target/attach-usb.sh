#!/bin/bash
# Attach the virtual Ozma USB device (from the RISC-V VM) to the target VM
# using USB/IP over the host as relay.
#
# Flow:
#   RISC-V VM (:3240) → [host:NODE_USBIP_PORT] ← target VM (10.0.2.2)
#
# After attach, inside the target VM:
#   dmesg | tail -20           -- see USB enumeration
#   lsusb                      -- shows "OzmaLabs Ozma TinyNode"
#   ls /dev/input/event*       -- HID keyboard + mouse
#   aplay -l                   -- UAC2 audio card
#   evtest /dev/input/eventN   -- live keypress test
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "${SCRIPT_DIR}/../config.env"

IMAGES_DIR="${SCRIPT_DIR}/../../images"

ssh_node() {
    ssh ${SSH_OPTS} -i "${SSH_KEY}" -p "${NODE_SSH_PORT}" \
        "${NODE_USER}@localhost" "$@"
}

ssh_target() {
    ssh ${SSH_OPTS} -i "${SSH_KEY}" -p "${TARGET_SSH_PORT}" \
        "${TARGET_USER}@localhost" "$@"
}

# ── Get busid from node VM ────────────────────────────────────────────────────
echo "=== Finding gadget busid on node VM ==="

BUSID=$(ssh_node "cat /tmp/ozma-gadget-busid 2>/dev/null || true")

if [[ -z "${BUSID}" ]]; then
    echo "  Querying USB/IP server on node VM..."
    BUSID=$(ssh_node "usbip list -r localhost 2>/dev/null | grep '1d6b:0104' | grep -oP '[0-9]+-[0-9.]+'" || true)
fi

if [[ -z "${BUSID}" ]]; then
    echo "ERROR: Could not determine gadget busid."
    echo "  1. Check node VM is running: make status"
    echo "  2. Check gadget setup: make shell-node  then: usbip list -l"
    echo "  3. Check usbipd is running: make shell-node  then: ps aux | grep usbipd"
    exit 1
fi

echo "  Found busid: ${BUSID}"

# ── Detach any existing attachment in target VM ───────────────────────────────
echo "=== Detaching any existing USB/IP device in target VM ==="
ssh_target "modprobe vhci-hcd 2>/dev/null; usbip detach -p 0 2>/dev/null || true; usbip detach -p 1 2>/dev/null || true" || true

# ── Attach from target VM ─────────────────────────────────────────────────────
# 10.0.2.2 is QEMU's SLIRP gateway = the host machine.
# The host has port NODE_USBIP_PORT forwarded from the RISC-V node VM.
echo "=== Attaching USB device to target VM ==="
ssh_target "usbip attach -r 10.0.2.2 -b ${BUSID}"

echo ""
echo "=== USB/IP attached. Verifying in target VM ==="
sleep 2

ssh_target "dmesg | tail -15 | grep -i 'usb\|hid\|input\|audio'" || true
echo ""
ssh_target "lsusb 2>/dev/null | grep -i ozma" || echo "  (lsusb not available in Alpine)"
echo ""
INPUT_DEVS=$(ssh_target "ls /dev/input/event* 2>/dev/null" || echo "  none yet")
echo "  Input devices: ${INPUT_DEVS}"
AUDIO_DEVS=$(ssh_target "aplay -l 2>/dev/null | grep -i uac\|gadget\|ozma" || echo "  none yet")
echo "  Audio devices: ${AUDIO_DEVS}"

echo ""
echo "=== Verification commands (run in target VM: make shell-target) ==="
echo "  evtest /dev/input/eventN    -- test keyboard input"
echo "  arecord -D hw:UAC2Gadget,0 -f S16_LE -r 48000 test.wav  -- test audio"
