#!/bin/bash
# Cross-compile USB gadget + USB/IP modules for Alpine Linux riscv64 6.12.74-0-lts.
# Installs the resulting .ko files into the running RISC-V VM via SSH.
#
# Run on the host (x86_64) — requires riscv64-linux-gnu-gcc.
# Usage: bash dev/kernel-build/build-gadget-modules.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
KVER="6.12.74"
KSRC="${SCRIPT_DIR}/linux-${KVER}"
KTAR="${SCRIPT_DIR}/linux-${KVER}.tar.xz"
KCONFIG="${SCRIPT_DIR}/alpine-riscv64-lts.config"
CROSS="riscv64-linux-gnu-"
ARCH="riscv"
JOBS=$(nproc)

SSH_KEY="${SCRIPT_DIR}/../../images/dev_key"
SSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -i ${SSH_KEY} -p 2222"
SCP="scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -i ${SSH_KEY} -P 2222"

echo "=== Building USB gadget modules for riscv64 Linux ${KVER} ==="

# ── Step 1: Extract kernel source ──────────────────────────────────────────────
if [[ ! -d "${KSRC}" ]]; then
    [[ -f "${KTAR}" ]] || { echo "ERROR: kernel tarball not found: ${KTAR}"; exit 1; }
    echo "[1/5] Extracting kernel source..."
    tar -xf "${KTAR}" -C "${SCRIPT_DIR}"
    echo "  Extracted to ${KSRC}"
else
    echo "[1/5] Kernel source already extracted."
fi

# ── Step 2: Apply Alpine config + enable gadget modules ───────────────────────
echo "[2/5] Configuring kernel..."
cp "${KCONFIG}" "${KSRC}/.config"

# Enable USB gadget subsystem and needed functions
# CONFIG_USB_GADGET depends on CONFIG_USB_SUPPORT
cat >> "${KSRC}/.config" << 'EOF'

# USB gadget modules needed for ozma node
CONFIG_USB_SUPPORT=y
CONFIG_USB_GADGET=m
CONFIG_USB_DUMMY_HCD=m
CONFIG_USB_LIBCOMPOSITE=m
CONFIG_USB_CONFIGFS=m
CONFIG_USB_CONFIGFS_F_HID=y
CONFIG_USB_F_HID=m
CONFIG_USB_F_UAC2=m
CONFIG_USB_CONFIGFS_F_UAC2=y

# USB/IP
CONFIG_USBIP_CORE=m
CONFIG_USBIP_HOST=m
CONFIG_USBIP_VHCI_HCD=m

# HID gadget legacy interface
CONFIG_USB_G_HID=m
EOF

# Resolve config dependencies silently
make -C "${KSRC}" ARCH="${ARCH}" CROSS_COMPILE="${CROSS}" \
    olddefconfig \
    >/dev/null 2>&1
echo "  Config ready"

# Verify key options are set
for opt in CONFIG_USB_DUMMY_HCD CONFIG_USB_LIBCOMPOSITE CONFIG_USB_F_HID CONFIG_USBIP_CORE; do
    val=$(grep "^${opt}=" "${KSRC}/.config" 2>/dev/null || echo "NOT SET")
    echo "  ${opt}=${val#*=}"
done

# ── Step 3: Prepare build system ───────────────────────────────────────────────
echo "[3/5] Preparing kernel build system..."
make -C "${KSRC}" ARCH="${ARCH}" CROSS_COMPILE="${CROSS}" \
    prepare modules_prepare \
    -j"${JOBS}" 2>&1 | tail -3
echo "  Build system ready"

# ── Step 4: Build the specific modules ────────────────────────────────────────
echo "[4/5] Compiling modules (${JOBS} jobs)..."
echo "  This takes ~5 minutes..."

# Build USB gadget subsystem
make -C "${KSRC}" ARCH="${ARCH}" CROSS_COMPILE="${CROSS}" \
    M=drivers/usb/gadget \
    modules -j"${JOBS}" 2>&1 | tail -5

# Build USB/IP
make -C "${KSRC}" ARCH="${ARCH}" CROSS_COMPILE="${CROSS}" \
    M=drivers/usb/usbip \
    modules -j"${JOBS}" 2>&1 | tail -5

echo "  Modules built:"
find "${KSRC}/drivers/usb/gadget" "${KSRC}/drivers/usb/usbip" \
    -name "*.ko" 2>/dev/null | sort | while read -r ko; do
    echo "    $(basename "$ko")"
done

# ── Step 5: Install into the VM ───────────────────────────────────────────────
echo "[5/5] Installing modules into RISC-V VM..."

MODVER=$(${SSH} root@localhost 'uname -r' 2>/dev/null)
echo "  VM kernel: ${MODVER}"

# Create module directories on VM
${SSH} root@localhost "
    mkdir -p /lib/modules/${MODVER}/kernel/drivers/usb/gadget/function
    mkdir -p /lib/modules/${MODVER}/kernel/drivers/usb/gadget/udc
    mkdir -p /lib/modules/${MODVER}/kernel/drivers/usb/usbip
"

# Copy modules
find "${KSRC}/drivers/usb/gadget" -name "*.ko" | while read -r ko; do
    base=$(basename "$ko")
    subdir=$(dirname "$ko" | sed "s|${KSRC}/drivers/usb/gadget||")
    dest="/lib/modules/${MODVER}/kernel/drivers/usb/gadget${subdir}/"
    ${SSH} root@localhost "mkdir -p '${dest}'"
    ${SCP} "$ko" "root@localhost:${dest}${base}"
done

find "${KSRC}/drivers/usb/usbip" -name "*.ko" | while read -r ko; do
    base=$(basename "$ko")
    ${SCP} "$ko" "root@localhost:/lib/modules/${MODVER}/kernel/drivers/usb/usbip/${base}"
done

# Compress modules (Alpine uses .ko.gz)
${SSH} root@localhost "
    find /lib/modules/${MODVER}/kernel/drivers/usb/gadget \
         /lib/modules/${MODVER}/kernel/drivers/usb/usbip \
         -name '*.ko' -exec gzip -f '{}' \;
    depmod ${MODVER}
    echo 'depmod done'
"

echo ""
echo "=== Done! Modules installed. ==="
echo ""
echo "Test in VM:"
echo "  modprobe dummy_hcd"
echo "  modprobe libcomposite"
echo "  modprobe usb_f_hid"
echo ""
echo "To persist across image rebuilds, rebuild the image:"
echo "  sudo bash dev/scripts/build-riscv-image.sh"
