#!/bin/bash
# Start the RISC-V QEMU VM (the emulated Milk-V Duo S / SG2000 node).
#
# The VM runs node.py with:
#   - dummy_hcd for USB gadget (HID keyboard + mouse via ConfigFS)
#   - v4l2loopback for fake HDMI video capture (optional)
#   - snd-aloop for fake HDMI audio (optional)
#   - usbipd to export the USB gadget to the target VM
#
# Network modes:
#   slirp (default): no root needed; mDNS won't work, use --register-url
#   tap:             real bridge; mDNS works; run 'make tap-up' first (needs root)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "${SCRIPT_DIR}/../config.env"

IMAGES_DIR="${SCRIPT_DIR}/../../images"
RISCV_IMAGE="${RISCV_IMAGE:-${IMAGES_DIR}/riscv-node.qcow2}"
PIDFILE="${IMAGES_DIR}/node-vm.pid"
LOGFILE="${IMAGES_DIR}/node-vm.log"

if [[ ! -f "${RISCV_IMAGE}" ]]; then
    echo "ERROR: RISC-V image not found: ${RISCV_IMAGE}"
    echo "  Run: make build-node-image"
    exit 1
fi

if [[ -f "${PIDFILE}" ]] && kill -0 "$(cat "${PIDFILE}")" 2>/dev/null; then
    echo "Node VM already running (pid $(cat "${PIDFILE}"))."
    exit 0
fi

# ── Firmware + kernel ────────────────────────────────────────────────────────
# Alpine images are built with a separate kernel + initrd (see build-riscv-image.sh).
# QEMU built-in OpenSBI handles M-mode; we provide S-mode kernel directly.
VMLINUZ="${IMAGES_DIR}/riscv-vmlinuz-virt"
INITRD="${IMAGES_DIR}/riscv-initramfs-virt"

if [[ -f "${VMLINUZ}" && -f "${INITRD}" ]]; then
    # Direct kernel boot (Alpine minirootfs build)
    KERNEL_ARGS="-bios default -kernel ${VMLINUZ} -initrd ${INITRD} -append 'root=LABEL=ozma-root rw console=ttyS0,115200 earlycon quiet'"
else
    # Fallback: boot from disk image with embedded bootloader (Ubuntu cloud images)
    KERNEL_ARGS="-bios default"
    UBOOT_PATHS=(
        "/usr/lib/u-boot/qemu-riscv64_smode/uboot.elf"
        "/usr/share/u-boot/qemu-riscv64_smode/uboot.elf"
    )
    for p in "${UBOOT_PATHS[@]}"; do
        if [[ -f "$p" ]]; then
            KERNEL_ARGS="-bios /usr/lib/riscv64-linux-gnu/opensbi/generic/fw_jump.bin -kernel $p"
            break
        fi
    done
fi

# ── Network ───────────────────────────────────────────────────────────────────
if [[ "${NETWORK_MODE}" == "tap" ]]; then
    if ! ip link show "${TAP_IFACE}" &>/dev/null; then
        echo "ERROR: TAP interface ${TAP_IFACE} not found."
        echo "  Run: sudo make tap-up"
        exit 1
    fi
    NETDEV="-netdev tap,id=eth0,ifname=${TAP_IFACE},script=no,downscript=no"
    NET_NOTE="TAP: vm=${TAP_VM_IP}, controller reachable via mDNS"
else
    # SLIRP: forward all required ports to host
    # USB/IP (3240) is forwarded so the host can attach the gadget via usbip attach -r 127.0.0.1
    NETDEV="-netdev user,id=eth0\
,hostfwd=tcp::${NODE_SSH_PORT}-:22\
,hostfwd=udp::${NODE_HID_PORT}-:${NODE_HID_PORT}\
,hostfwd=tcp::${NODE_STREAM_PORT}-:${NODE_STREAM_PORT}\
,hostfwd=tcp::${NODE_USBIP_PORT}-:3240"
    NET_NOTE="SLIRP: ssh=localhost:${NODE_SSH_PORT}, usbip=localhost:${NODE_USBIP_PORT}"
fi

# ── Launch ────────────────────────────────────────────────────────────────────
echo "Starting RISC-V node VM..."
echo "  Image : ${RISCV_IMAGE}"
echo "  Net   : ${NET_NOTE}"
echo "  Log   : ${LOGFILE}"

eval qemu-system-riscv64 \
    -machine virt \
    -cpu rv64 \
    -smp "${NODE_VCPUS}" \
    -m "${NODE_MEM}" \
    ${KERNEL_ARGS} \
    -drive "if=virtio,format=qcow2,file=${RISCV_IMAGE}" \
    -object rng-random,filename=/dev/urandom,id=rng0 \
    -device virtio-rng-device,rng=rng0 \
    -device virtio-net-device,netdev=eth0 \
    ${NETDEV} \
    -nographic \
    -serial "file:${LOGFILE}" \
    -monitor "unix:${IMAGES_DIR}/node-monitor.sock,server,nowait" \
    &

echo $! > "${PIDFILE}"
echo "Node VM started (pid $!)."
echo ""
echo "Wait ~30s for boot, then:"
echo "  make connect-vms    # connect USB gadget to vm1"
echo "  make shell-node     # SSH into the RISC-V node"
