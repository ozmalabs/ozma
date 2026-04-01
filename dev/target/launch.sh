#!/bin/bash
# Start the target VM — the machine being KVM'd by the Ozma node.
#
# This is an Alpine Linux x86_64 VM. When USB/IP is attached (make attach-usb),
# the virtual Ozma USB device appears here as /dev/input/event* + /dev/snd/*.
# Verify with: evtest, aplay -l, dmesg | grep -i usb
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "${SCRIPT_DIR}/../config.env"

IMAGES_DIR="${SCRIPT_DIR}/../../images"
TARGET_IMAGE="${TARGET_IMAGE:-${IMAGES_DIR}/target.qcow2}"
ALPINE_ISO="${IMAGES_DIR}/alpine-virt-3.21.0-x86_64.iso"
PIDFILE="${IMAGES_DIR}/target-vm.pid"
LOGFILE="${IMAGES_DIR}/target-vm.log"

if [[ -f "${PIDFILE}" ]] && kill -0 "$(cat "${PIDFILE}")" 2>/dev/null; then
    echo "Target VM already running (pid $(cat "${PIDFILE}"))."
    exit 0
fi

# ── Detect KVM availability ───────────────────────────────────────────────────
KVM_ARGS=""
if [[ -w /dev/kvm ]]; then
    KVM_ARGS="-enable-kvm -cpu host"
    echo "KVM acceleration enabled."
else
    echo "Warning: /dev/kvm not accessible — running without acceleration (slow)."
fi

# ── Boot source: installed disk, or ISO for first-time setup ─────────────────
if [[ ! -f "${TARGET_IMAGE}" ]] || [[ "$(qemu-img info --output json "${TARGET_IMAGE}" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d["actual-size"])')" -lt 1048576 ]]; then
    if [[ ! -f "${ALPINE_ISO}" ]]; then
        echo "ERROR: Neither target disk nor Alpine ISO found."
        echo "  Run: make images"
        exit 1
    fi
    echo "Booting from Alpine ISO for first-time install..."
    BOOT_ARGS="-cdrom ${ALPINE_ISO} -boot d"
else
    echo "Booting from installed disk."
    BOOT_ARGS=""
fi

# ── Network ───────────────────────────────────────────────────────────────────
# The target talks to the RISC-V VM via the host (10.0.2.2).
# USB/IP: connects to host:NODE_USBIP_PORT which is forwarded to the RISC-V VM.
NETDEV="-netdev user,id=eth0,hostfwd=tcp::${TARGET_SSH_PORT}-:22,hostfwd=tcp::${TARGET_VNC_PORT}-:5900"

echo "Starting target VM..."
echo "  Disk : ${TARGET_IMAGE}"
echo "  Net  : ssh=localhost:${TARGET_SSH_PORT}, vnc=localhost:${TARGET_VNC_PORT}"

qemu-system-x86_64 \
    -machine q35 \
    ${KVM_ARGS} \
    -smp "${TARGET_VCPUS}" \
    -m "${TARGET_MEM}" \
    -drive "if=virtio,format=qcow2,file=${TARGET_IMAGE}" \
    ${BOOT_ARGS} \
    -device virtio-net-pci,netdev=eth0 \
    ${NETDEV} \
    -device intel-hda \
    -device hda-duplex \
    -device qemu-xhci,id=xhci \
    -nographic \
    -serial "file:${LOGFILE}" \
    -monitor "unix:${IMAGES_DIR}/target-monitor.sock,server,nowait" \
    &

echo $! > "${PIDFILE}"
echo "Target VM started (pid $!)."
echo ""
echo "First-time setup: make target-install  (runs Alpine setup-alpine)"
echo "Attach USB:       make attach-usb"
