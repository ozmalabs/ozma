#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#
# Build a flashable SD card image for an ozma hardware node.
#
# Creates an Alpine Linux image with:
#   - ozma node daemon (node/node.py + dependencies)
#   - USB gadget setup (tinynode/gadget/setup_gadget.sh)
#   - Auto-start on boot (OpenRC service)
#   - mDNS announcement (avahi)
#   - Python 3.11+ with required packages
#
# Output: images/ozma-node-<arch>.img (raw disk image, dd to SD card)
#
# Usage:
#   bash dev/scripts/build-node-image.sh [--arch riscv64|aarch64|x86_64]
#
# Requirements:
#   - qemu-img, qemu-nbd (or losetup), mkfs.ext4, fdisk
#   - Root access (for loopback mount)
#   - Internet access (downloads Alpine minirootfs)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
IMAGE_DIR="$REPO_ROOT/images"
ARCH="${1:---arch}"
ARCH="${2:-aarch64}"  # default to ARM64 (most SBCs)

# Parse --arch flag
while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch) ARCH="$2"; shift 2 ;;
        *) shift ;;
    esac
done

ALPINE_VERSION="3.21"
ALPINE_MIRROR="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}"
ROOTFS_URL="${ALPINE_MIRROR}/releases/${ARCH}/alpine-minirootfs-${ALPINE_VERSION}.0-${ARCH}.tar.gz"
IMAGE_FILE="$IMAGE_DIR/ozma-node-${ARCH}.img"
IMAGE_SIZE="512M"
ROOTFS_TAR="$IMAGE_DIR/cache/alpine-minirootfs-${ARCH}.tar.gz"

echo "Building ozma node image"
echo "  Arch: $ARCH"
echo "  Alpine: $ALPINE_VERSION"
echo "  Output: $IMAGE_FILE"
echo ""

mkdir -p "$IMAGE_DIR/cache"

# Download Alpine minirootfs
if [[ ! -f "$ROOTFS_TAR" ]]; then
    echo "Downloading Alpine minirootfs..."
    curl -L --progress-bar -o "$ROOTFS_TAR" "$ROOTFS_URL"
fi

# Create disk image
echo "Creating disk image ($IMAGE_SIZE)..."
dd if=/dev/zero of="$IMAGE_FILE" bs=1M count=512 status=progress

# Partition: single ext4 partition
echo "Partitioning..."
echo -e "o\nn\np\n1\n\n\nw" | fdisk "$IMAGE_FILE" >/dev/null 2>&1

# Setup loopback
echo "Mounting..."
LOOP=$(sudo losetup --find --show --partscan "$IMAGE_FILE")
PART="${LOOP}p1"

# Wait for partition device
sleep 1
if [[ ! -b "$PART" ]]; then
    sudo partprobe "$LOOP"
    sleep 1
fi

sudo mkfs.ext4 -q -L ozma-node "$PART"

MOUNT_DIR=$(mktemp -d)
sudo mount "$PART" "$MOUNT_DIR"

# Extract Alpine rootfs
echo "Extracting Alpine rootfs..."
sudo tar xzf "$ROOTFS_TAR" -C "$MOUNT_DIR"

# Copy ozma node files
echo "Installing ozma node..."
sudo mkdir -p "$MOUNT_DIR/opt/ozma/node"
sudo mkdir -p "$MOUNT_DIR/opt/ozma/tinynode/gadget"
sudo cp -r "$REPO_ROOT/node/"*.py "$MOUNT_DIR/opt/ozma/node/"
sudo cp "$REPO_ROOT/node/requirements.txt" "$MOUNT_DIR/opt/ozma/node/"
sudo cp "$REPO_ROOT/tinynode/gadget/setup_gadget.sh" "$MOUNT_DIR/opt/ozma/tinynode/gadget/"
sudo chmod +x "$MOUNT_DIR/opt/ozma/tinynode/gadget/setup_gadget.sh"

# Create OpenRC init script
sudo tee "$MOUNT_DIR/etc/init.d/ozma-node" > /dev/null << 'INITEOF'
#!/sbin/openrc-run

name="ozma-node"
description="Ozma hardware node daemon"
command="/usr/bin/python3"
command_args="/opt/ozma/node/node.py --name $(hostname)"
pidfile="/run/ozma-node.pid"
command_background="yes"

depend() {
    need net
    after firewall
}

start_pre() {
    # Setup USB gadget
    /opt/ozma/tinynode/gadget/setup_gadget.sh "$(cat /sys/class/dmi/id/product_serial 2>/dev/null || hostname)" || true
}
INITEOF
sudo chmod +x "$MOUNT_DIR/etc/init.d/ozma-node"

# Create first-boot setup script
sudo tee "$MOUNT_DIR/etc/local.d/ozma-setup.start" > /dev/null << 'SETUPEOF'
#!/bin/sh
# First-boot setup for ozma node

# Enable community repo
sed -i 's/^#\(.*community\)/\1/' /etc/apk/repositories

# Install dependencies
apk update
apk add python3 py3-pip avahi avahi-tools

# Install Python deps
pip3 install --break-system-packages aiohttp zeroconf

# Enable services
rc-update add avahi-daemon default
rc-update add ozma-node default
service avahi-daemon start

# Remove this script (first-boot only)
rm -f /etc/local.d/ozma-setup.start

echo "Ozma node setup complete. Rebooting..."
reboot
SETUPEOF
sudo chmod +x "$MOUNT_DIR/etc/local.d/ozma-setup.start"

# Enable local service (runs first-boot script)
sudo mkdir -p "$MOUNT_DIR/etc/runlevels/default"
sudo ln -sf /etc/init.d/local "$MOUNT_DIR/etc/runlevels/default/local" 2>/dev/null || true

# Set hostname
echo "ozma-node" | sudo tee "$MOUNT_DIR/etc/hostname" > /dev/null

# Configure networking (DHCP on eth0)
sudo tee "$MOUNT_DIR/etc/network/interfaces" > /dev/null << 'NETEOF'
auto lo
iface lo inet loopback

auto eth0
iface eth0 inet dhcp
NETEOF

# Cleanup
echo "Finalising..."
sudo umount "$MOUNT_DIR"
sudo losetup -d "$LOOP"
rmdir "$MOUNT_DIR"

echo ""
echo "================================================"
echo "  Image built: $IMAGE_FILE"
echo "  Size: $(du -h "$IMAGE_FILE" | cut -f1)"
echo ""
echo "  Flash to SD card:"
echo "    sudo dd if=$IMAGE_FILE of=/dev/sdX bs=4M status=progress"
echo ""
echo "  First boot will install dependencies and configure"
echo "  the node automatically. Takes 2-3 minutes on first boot."
echo "================================================"
