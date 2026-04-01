#!/bin/bash
# Connect the RISC-V node VM's USB gadget to a real target VM.
#
# Full chain (like real hardware):
#   RISC-V VM (dummy_hcd + ConfigFS HID gadget + node.py)
#     → USB/IP export (usbipd in VM, port 3240 → SLIRP-forwarded to host)
#     → host vhci_hcd (usbip attach -r 127.0.0.1)
#     → QEMU usb-host hotplug (via QMP device_add)
#     → vm1 USB EHCI controller
#
# The target VM (vm1) then sees a real USB HID keyboard+mouse.
# node.py in the RISC-V VM receives UDP HID from the controller and
# writes to /dev/hidg0 (real ConfigFS HID gadget on dummy_hcd).
#
# Usage:
#   bash dev/riscv-node/connect-to-vms.sh [--target vm1|vm2] [--detach]
#
# Prerequisites:
#   - make build-node-image && make node-vm
#   - sudo access for: modprobe vhci-hcd, usbip attach/detach

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "${SCRIPT_DIR}/../config.env"

IMAGES_DIR="${SCRIPT_DIR}/../../images"
SSH_KEY="${IMAGES_DIR}/dev_key"
SSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -i ${SSH_KEY} -p ${NODE_SSH_PORT}"

TARGET_VM="vm1"
DETACH=false

while [[ $# -gt 0 ]]; do
    case $1 in
        --target) TARGET_VM="$2"; shift 2 ;;
        --detach) DETACH=true; shift ;;
        *) echo "Unknown arg: $1"; exit 1 ;;
    esac
done

QMP_SOCK="/tmp/ozma-${TARGET_VM}.qmp"
QEMU_DEV_ID="ozma-riscv-hid"
STATE_FILE="/tmp/ozma-riscv-connect.state"

# ── Helpers ───────────────────────────────────────────────────────────────────
qmp() {
    python3 - "$@" << 'PYEOF'
import socket, json, time, sys

sock_path = sys.argv[1]
cmds = json.loads(sys.argv[2]) if len(sys.argv) > 2 else []

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock_path)
s.settimeout(5)

buf = b""
while True:
    try: chunk = s.recv(4096); buf += chunk
    except socket.timeout: break
    if b"\n" in chunk: break

s.send(b'{"execute":"qmp_capabilities"}\n')
time.sleep(0.3)
s.recv(4096)

results = []
for cmd in cmds:
    s.send(json.dumps(cmd).encode() + b"\n")
    time.sleep(0.3)
    try:
        resp = json.loads(s.recv(4096).decode().strip().split("\n")[0])
        results.append(resp)
    except Exception as e:
        results.append({"error": str(e)})

s.close()
print(json.dumps(results))
PYEOF
}

die() { echo "ERROR: $*" >&2; exit 1; }
info() { echo "  $*"; }

# ── Detach mode ───────────────────────────────────────────────────────────────
if [[ "${DETACH}" == "true" ]]; then
    echo "=== Detaching RISC-V USB gadget ==="
    if [[ -f "${STATE_FILE}" ]]; then
        . "${STATE_FILE}"
        if [[ -S "${QMP_SOCK}" ]]; then
            SOFT_NODE_PID=$(pgrep -f "soft_node.py.*${TARGET_VM}" | head -1 || true)
            SOFT_NODE_CMD=""
            if [[ -n "${SOFT_NODE_PID}" ]]; then
                SOFT_NODE_CMD=$(ps -p "${SOFT_NODE_PID}" -o args= 2>/dev/null || true)
                kill "${SOFT_NODE_PID}" 2>/dev/null || true
                sleep 0.5
            fi
            qmp "${QMP_SOCK}" "[{\"execute\":\"device_del\",\"arguments\":{\"id\":\"${QEMU_DEV_ID}\"}}]" >/dev/null 2>&1 || true
            if [[ -n "${SOFT_NODE_CMD}" ]]; then
                cd "$(dirname "${SCRIPT_DIR}")"
                eval "${SOFT_NODE_CMD}" &>/dev/null &
                disown
                cd - >/dev/null
            fi
            info "USB device removed from ${TARGET_VM}"
        fi
        if [[ -n "${VHCI_PORT:-}" ]]; then
            sudo usbip detach -p "${VHCI_PORT}" 2>/dev/null || true
            info "USB/IP detached (vhci port ${VHCI_PORT})"
        fi
        rm -f "${STATE_FILE}"
    fi
    echo "Done."
    exit 0
fi

# ── Check prerequisites ───────────────────────────────────────────────────────
echo "=== Connecting RISC-V node → ${TARGET_VM} ==="
echo ""

[[ -S "${QMP_SOCK}" ]] || die "QMP socket not found: ${QMP_SOCK}. Is ${TARGET_VM} running?"

if ! command -v usbip &>/dev/null; then
    echo "Installing usbip..."
    sudo pacman -S --noconfirm usbip || die "Could not install usbip"
fi

# ── Ensure RISC-V VM is running ───────────────────────────────────────────────
PIDFILE="${IMAGES_DIR}/node-vm.pid"

if [[ -f "${PIDFILE}" ]] && kill -0 "$(cat "${PIDFILE}")" 2>/dev/null; then
    info "RISC-V VM already running (pid $(cat "${PIDFILE}"))"
else
    echo "[1/5] Starting RISC-V node VM..."
    bash "${SCRIPT_DIR}/launch.sh"
    sleep 2
fi

# ── Wait for SSH ──────────────────────────────────────────────────────────────
echo "[2/5] Waiting for RISC-V VM to boot (SSH on port ${NODE_SSH_PORT})..."
SSH_READY=false
for i in $(seq 1 60); do
    if ${SSH} root@localhost echo ok >/dev/null 2>&1; then
        SSH_READY=true
        break
    fi
    printf "."
    sleep 2
done
echo ""
[[ "${SSH_READY}" == "true" ]] || die "RISC-V VM SSH not reachable after 120s"
info "SSH ready"

# ── Run init script in RISC-V VM ─────────────────────────────────────────────
echo "[3/5] Initialising RISC-V node (dummy_hcd + ConfigFS + usbipd + node.py)..."
${SSH} root@localhost 'sh /root/ozma-node/init.sh' 2>&1 | sed 's/^/    /'

# Wait for USB/IP gadget busid to be written
echo "  Waiting for USB/IP gadget export..."
BUSID=""
for i in $(seq 1 20); do
    BUSID=$(${SSH} root@localhost 'cat /tmp/ozma-gadget-busid 2>/dev/null || true' 2>/dev/null || true)
    [[ -n "${BUSID}" ]] && break
    sleep 2
done
[[ -n "${BUSID}" ]] || die "USB gadget busid not found after 40s. Check: make logs"
info "Gadget busid: ${BUSID}"

# ── Import gadget to host via USB/IP ─────────────────────────────────────────
echo "[4/5] Attaching gadget to host via USB/IP..."

sudo modprobe vhci-hcd 2>/dev/null || true
sleep 0.5

# Attach from host to the VM's usbipd (via SLIRP port-forward on localhost:3240)
sudo usbip attach -r 127.0.0.1 -b "${BUSID}"
sleep 1

VHCI_PORT=$(sudo usbip port 2>/dev/null \
    | grep -A3 "1d6b.*0104\|0104.*1d6b" \
    | grep "Port " | grep -oE '[0-9]+' | head -1 || true)

# Wait for USB device to appear on host
USB_DEV=""
for i in $(seq 1 15); do
    USB_DEV=$(lsusb 2>/dev/null | grep "1d6b:0104" | head -1 || true)
    [[ -n "${USB_DEV}" ]] && break
    sleep 1
done
[[ -n "${USB_DEV}" ]] || die "USB device (1d6b:0104) did not appear on host after 15s"
info "Device on host: ${USB_DEV}"

# ── Hotplug into target VM ────────────────────────────────────────────────────
echo "[5/5] Hotplugging USB device into ${TARGET_VM}..."

# QMP vendorid/productid must be decimal integers: 0x1d6b=7531, 0x0104=260
# soft_node.py holds the QMP socket — pause it briefly, hotplug, then restart
SOFT_NODE_PID=$(pgrep -f "soft_node.py.*${TARGET_VM}" | head -1 || true)
SOFT_NODE_CMD=""
if [[ -n "${SOFT_NODE_PID}" ]]; then
    SOFT_NODE_CMD=$(ps -p "${SOFT_NODE_PID}" -o args= 2>/dev/null || true)
    info "Pausing soft_node (pid ${SOFT_NODE_PID}) to access QMP..."
    kill "${SOFT_NODE_PID}" 2>/dev/null || true
    sleep 0.5
fi

RESULT=$(qmp "${QMP_SOCK}" "[
    {\"execute\":\"device_add\", \"arguments\":{
        \"driver\":\"usb-host\",
        \"vendorid\":7531,
        \"productid\":260,
        \"id\":\"${QEMU_DEV_ID}\"
    }}
]")

if [[ -n "${SOFT_NODE_CMD}" ]]; then
    cd "$(dirname "${SCRIPT_DIR}")"
    eval "${SOFT_NODE_CMD}" &>/dev/null &
    disown
    cd - >/dev/null
    info "soft_node restarted"
fi

if echo "${RESULT}" | grep -q '"error"'; then
    die "QMP device_add failed: ${RESULT}"
fi
info "USB device plugged into ${TARGET_VM}"

# ── Save state for teardown ───────────────────────────────────────────────────
cat > "${STATE_FILE}" << EOF
TARGET_VM="${TARGET_VM}"
BUSID="${BUSID}"
VHCI_PORT="${VHCI_PORT:-}"
EOF

echo ""
echo "=== Connected! ==="
echo ""
echo "  RISC-V node : dummy_hcd + ConfigFS gadget, node.py writing to /dev/hidg0"
echo "  USB gadget  : busid=${BUSID} → USB/IP → host vhci → ${TARGET_VM} USB EHCI"
echo "  ${TARGET_VM} sees   : USB HID keyboard + mouse (1d6b:0104)"
echo ""
echo "  To disconnect: make disconnect-vms"
