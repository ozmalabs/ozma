#!/usr/bin/env bash
# dev/receiver/launch.sh — Start the receiver (operator console) VM.
#
# The receiver VM is NOT a target machine — it is the operator's console.
# It connects to the controller API to watch which scenario is active, receives
# the active target's audio stream, and displays the controller UI.
#
# The receiver does NOT have a soft node or USB gadget. It is a plain
# Alpine x86_64 VM with audio output and a web browser.
#
# Ports:
#   SSH:  localhost:2224  → receiver:22
#   VNC:  localhost:5903  → receiver VGA (for dev — real use has a physical monitor)
#
# The receiver talks to the controller on the host at 10.0.2.2:7380 (SLIRP gateway).
#
# Usage:
#   bash dev/receiver/launch.sh          # start in background
#   bash dev/receiver/launch.sh stop     # stop

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
IMAGES_DIR="${REPO_ROOT}/images"
CACHE_DIR="${REPO_ROOT}/demo/cache"
LOG_DIR="${REPO_ROOT}/demo/logs"

ALPINE_VERSION="3.21.3"
ALPINE_ARCH="x86_64"
ALPINE_ISO="${CACHE_DIR}/alpine-virt-${ALPINE_VERSION}-${ALPINE_ARCH}.iso"

QMP_SOCK="/tmp/ozma-receiver.qmp"
PID_FILE="/tmp/ozma-receiver.pid"
LOG_FILE="${LOG_DIR}/receiver.log"

RECEIVER_SSH_PORT=2224
RECEIVER_VNC_DISPLAY=3      # VNC listens on :5903

# ---------------------------------------------------------------------------
stop() {
    echo "Stopping receiver VM..."
    if [[ -f "${PID_FILE}" ]]; then
        pid=$(cat "${PID_FILE}")
        if kill -0 "${pid}" 2>/dev/null; then
            kill "${pid}" && echo "  killed PID ${pid}"
        fi
        rm -f "${PID_FILE}"
    fi
    rm -f "${QMP_SOCK}"
    exit 0
}

[[ "${1:-}" == "stop" ]] && stop

# ---------------------------------------------------------------------------
# Pre-flight
if [[ -f "${PID_FILE}" ]] && kill -0 "$(cat "${PID_FILE}")" 2>/dev/null; then
    echo "Receiver VM already running (pid $(cat "${PID_FILE}"))."
    exit 0
fi

if [[ ! -f "${ALPINE_ISO}" ]]; then
    echo "ERROR: Alpine ISO not found at ${ALPINE_ISO}"
    echo "  Run: bash demo/start_vms.sh  (it downloads the ISO)"
    exit 1
fi

mkdir -p "${LOG_DIR}"
rm -f "${QMP_SOCK}" "${PID_FILE}"

# ---------------------------------------------------------------------------
# Detect KVM
KVM_ARGS=()
if [[ -w /dev/kvm ]]; then
    KVM_ARGS+=(-accel kvm)
    echo "KVM acceleration enabled."
else
    echo "Warning: KVM not available — receiver VM will be slow."
fi

echo "Starting receiver VM..."
echo "  SSH:  ssh -p ${RECEIVER_SSH_PORT} root@localhost"
echo "  VNC:  vnc://127.0.0.1:$(( 5900 + RECEIVER_VNC_DISPLAY ))"
echo "  Controller visible inside VM at: http://10.0.2.2:7380"
echo ""

qemu-system-x86_64 \
    -name receiver \
    -m 512M \
    -smp 2 \
    "${KVM_ARGS[@]}" \
    -cdrom "${ALPINE_ISO}" \
    -boot d \
    -serial "file:${LOG_FILE}" \
    -monitor none \
    -qmp "unix:${QMP_SOCK},server,nowait" \
    -device intel-hda \
    -device hda-duplex \
    -device virtio-net-pci,netdev=eth0 \
    -netdev "user,id=eth0,hostfwd=tcp::${RECEIVER_SSH_PORT}-:22" \
    -vga std \
    -vnc "127.0.0.1:${RECEIVER_VNC_DISPLAY}" \
    -no-reboot \
    &>/tmp/qemu-receiver-stderr.log &

echo $! > "${PID_FILE}"
echo "Receiver VM started (pid $!)."
echo ""

# Wait for QMP socket
echo "Waiting for QMP socket..."
elapsed=0
while [[ ! -S "${QMP_SOCK}" && ${elapsed} -lt 15 ]]; do
    sleep 0.5
    elapsed=$(( elapsed + 1 ))
done

if [[ -S "${QMP_SOCK}" ]]; then
    echo "  ${QMP_SOCK}  [ready]"
else
    echo "  WARNING: QMP socket not yet available"
fi

echo ""
echo "Receiver VM is running."
echo "  To stop: bash dev/receiver/launch.sh stop"
echo "  Serial log: ${LOG_FILE}"
