#!/bin/bash
# Download and prepare QEMU disk images for the dev harness.
#
# RISC-V node image: Ubuntu 24.04 Server for RISC-V (works with qemu virt machine)
# Target image: Alpine Linux x86_64 (lightweight machine to be KVM'd)
#
# Images are stored in dev/images/ and are gitignored.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
IMAGES_DIR="${SCRIPT_DIR}/../../images"
mkdir -p "${IMAGES_DIR}"

# ── Ubuntu 24.04 RISC-V ───────────────────────────────────────────────────────
# Ubuntu provides pre-built cloud images for RISC-V that boot directly in QEMU.
# The unmatched image works with the generic 'virt' machine type.
RISCV_IMG="${IMAGES_DIR}/riscv-node.qcow2"

if [[ ! -f "${RISCV_IMG}" ]]; then
    echo "=== Downloading Ubuntu 24.04 RISC-V cloud image ==="
    # Check https://cdimage.ubuntu.com/releases/24.04/release/ for current filename.
    # Expected: ubuntu-24.04.X-preinstalled-server-riscv64+unmatched.img.xz
    UBUNTU_BASE="https://cdimage.ubuntu.com/releases/24.04/release"
    # Find the latest release file name dynamically
    FILENAME=$(curl -s "${UBUNTU_BASE}/" | grep -oP 'ubuntu-24\.04\.[0-9]+-preinstalled-server-riscv64\+unmatched\.img\.xz' | sort -V | tail -1)
    if [[ -z "${FILENAME}" ]]; then
        echo "ERROR: Could not find Ubuntu 24.04 RISC-V image at ${UBUNTU_BASE}/"
        echo "  Visit that URL and download a file matching:"
        echo "  ubuntu-24.04.*-preinstalled-server-riscv64+unmatched.img.xz"
        echo "  Then rename it to: ${RISCV_IMG}"
        exit 1
    fi
    TMPFILE="${IMAGES_DIR}/${FILENAME}"
    curl -L --progress-bar -o "${TMPFILE}" "${UBUNTU_BASE}/${FILENAME}"

    echo "Decompressing..."
    xz -d "${TMPFILE}"
    RAW="${TMPFILE%.xz}"

    echo "Converting to qcow2 (resizing to 20G for packages)..."
    qemu-img convert -f raw -O qcow2 "${RAW}" "${RISCV_IMG}"
    qemu-img resize "${RISCV_IMG}" 20G
    rm -f "${RAW}"
    echo "RISC-V image ready: ${RISCV_IMG}"
else
    echo "RISC-V image already present: ${RISCV_IMG}"
fi

# ── Alpine Linux x86_64 (target VM) ──────────────────────────────────────────
TARGET_IMG="${IMAGES_DIR}/target.qcow2"

if [[ ! -f "${TARGET_IMG}" ]]; then
    echo ""
    echo "=== Downloading Alpine Linux 3.21 x86_64 virtual image ==="
    ALPINE_BASE="https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/x86_64"
    ALPINE_FILE="alpine-virt-3.21.0-x86_64.iso"
    TMPISO="${IMAGES_DIR}/${ALPINE_FILE}"

    curl -L --progress-bar -o "${TMPISO}" "${ALPINE_BASE}/${ALPINE_FILE}"

    echo "Creating 4G target disk..."
    qemu-img create -f qcow2 "${TARGET_IMG}" 4G
    echo "Target ISO ready; disk will be set up on first boot."
    echo "ISO: ${TMPISO}"
    echo "Run: make target-install  (one-time Alpine setup)"
else
    echo "Target image already present: ${TARGET_IMG}"
fi

echo ""
echo "All images ready. Run: make provision-node"
