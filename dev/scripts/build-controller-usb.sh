#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#
# Build a LIVE USB image for the Ozma Controller.
#
# Runs directly from USB — no installation to internal disk required.
# For DIYers, machines with no internal storage, weird platforms,
# testing, or temporary setups.
#
# The USB has A/B partitions so it can update itself:
#   Part 1: Boot    (32MB,  FAT32, syslinux + kernels)
#   Part 2: Root A  (scaled, ext4, active root filesystem)
#   Part 3: Root B  (scaled, ext4, inactive, receives updates)
#   Part 4: Data    (rest,  ext4,  persistent user data)
#
# For installing to internal disk instead, use build-installer-usb.sh.
#   - Health check after reboot — rollback if fails 3 times
#
# Output: images/ozma-controller-x86_64.img
#
# Usage:
#   bash dev/scripts/build-controller-usb.sh
#   sudo dd if=images/ozma-controller-x86_64.img of=/dev/sdX bs=4M status=progress

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
IMAGE_DIR="$REPO_ROOT/images"
CACHE_DIR="$IMAGE_DIR/cache"

ALPINE_VERSION="3.21"
ALPINE_ARCH="x86_64"
ALPINE_MINIROOTFS_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/releases/${ALPINE_ARCH}/alpine-minirootfs-${ALPINE_VERSION}.0-${ALPINE_ARCH}.tar.gz"
ALPINE_MINIROOTFS="$CACHE_DIR/alpine-minirootfs-${ALPINE_ARCH}.tar.gz"

IMAGE_FILE="$IMAGE_DIR/ozma-controller-${ALPINE_ARCH}.img"
IMAGE_SIZE_MB="${OZMA_IMAGE_SIZE:-0}"  # 0 = auto or default
DEVICE=""                              # target device for auto-sizing
OZMA_VERSION="$(cd "$REPO_ROOT" && git describe --tags 2>/dev/null || echo '0.1.0-dev')"

# Parse command-line args
for arg in "$@"; do
    case "$arg" in
        --device=*) DEVICE="${arg#*=}" ;;
        --size=*) IMAGE_SIZE_MB="${arg#*=}" ;;
    esac
done

# Auto-detect size from target device
if [[ -n "$DEVICE" ]] && command -v blockdev &>/dev/null && [[ -b "$DEVICE" ]]; then
    DEVICE_SIZE_MB=$(( $(blockdev --getsize64 "$DEVICE") / 1048576 ))
    if [[ $DEVICE_SIZE_MB -gt 0 ]]; then
        IMAGE_SIZE_MB=$DEVICE_SIZE_MB
    fi
fi

# Default 4GB if not specified
[[ $IMAGE_SIZE_MB -eq 0 ]] && IMAGE_SIZE_MB=4096

# Scale root partition size based on total available space
# Bigger disk = more room for ML models, media assets, widget packs
BOOT_SIZE=32
if [[ $IMAGE_SIZE_MB -ge 64000 ]]; then
    ROOT_SIZE=8192   # 8GB each on 64GB+ (NVMe, large eMMC)
elif [[ $IMAGE_SIZE_MB -ge 32000 ]]; then
    ROOT_SIZE=4096   # 4GB each on 32-64GB — Whisper models, full media
elif [[ $IMAGE_SIZE_MB -ge 16000 ]]; then
    ROOT_SIZE=2048   # 2GB each on 16-32GB
elif [[ $IMAGE_SIZE_MB -ge 8000 ]]; then
    ROOT_SIZE=1536   # 1.5GB each on 8-16GB
elif [[ $IMAGE_SIZE_MB -ge 4000 ]]; then
    ROOT_SIZE=1024   # 1GB each on 4-8GB
else
    ROOT_SIZE=512    # 512MB each on <4GB (minimum viable)
fi

DATA_SIZE=$((IMAGE_SIZE_MB - BOOT_SIZE - ROOT_SIZE * 2))

echo "╔════════════════════════════════════════════════════╗"
echo "║  Ozma Controller USB Image Builder (A/B)           ║"
echo "╠════════════════════════════════════════════════════╣"
echo "║  Version:    $OZMA_VERSION"
echo "║  Disk:       ${IMAGE_SIZE_MB}MB"
echo "║  Boot:       ${BOOT_SIZE}MB (FAT32, syslinux)"
echo "║  Root A/B:   ${ROOT_SIZE}MB each (ext4, squashfs)"
echo "║  Data:       ~${DATA_SIZE}MB (ext4, persistent)"
echo "║  Output:     $IMAGE_FILE"
echo "╚════════════════════════════════════════════════════╝"
echo ""

mkdir -p "$CACHE_DIR" "$IMAGE_DIR"

# ── Download Alpine minirootfs ──────────────────────────────────────────────

if [[ ! -f "$ALPINE_MINIROOTFS" ]]; then
    echo "[1/8] Downloading Alpine Linux minirootfs..."
    curl -L --progress-bar -o "$ALPINE_MINIROOTFS" "$ALPINE_MINIROOTFS_URL"
else
    echo "[1/8] Alpine minirootfs cached."
fi

# ── Create disk image ───────────────────────────────────────────────────────

echo "[2/8] Creating ${IMAGE_SIZE_MB}MB disk image..."
dd if=/dev/zero of="$IMAGE_FILE" bs=1M count="$IMAGE_SIZE_MB" status=none

# ── Partition ───────────────────────────────────────────────────────────────

echo "[3/8] Partitioning (boot + root A + root B + data)..."
# Calculate offsets
BOOT_START=1     # 1 MiB aligned
BOOT_END=$((BOOT_START + BOOT_SIZE))
ROOTA_START=$BOOT_END
ROOTA_END=$((ROOTA_START + ROOT_SIZE))
ROOTB_START=$ROOTA_END
ROOTB_END=$((ROOTB_START + ROOT_SIZE))
DATA_START=$ROOTB_END

parted -s "$IMAGE_FILE" mklabel msdos
parted -s "$IMAGE_FILE" mkpart primary fat32 ${BOOT_START}MiB ${BOOT_END}MiB
parted -s "$IMAGE_FILE" set 1 boot on
parted -s "$IMAGE_FILE" mkpart primary ext4 ${ROOTA_START}MiB ${ROOTA_END}MiB
parted -s "$IMAGE_FILE" mkpart primary ext4 ${ROOTB_START}MiB ${ROOTB_END}MiB
parted -s "$IMAGE_FILE" mkpart primary ext4 ${DATA_START}MiB 100%

# Setup loopback
LOOP=$(sudo losetup --find --show --partscan "$IMAGE_FILE")
sleep 1
sudo partprobe "$LOOP" 2>/dev/null || true
sleep 1

BOOT_PART="${LOOP}p1"
ROOTA_PART="${LOOP}p2"
ROOTB_PART="${LOOP}p3"
DATA_PART="${LOOP}p4"

# Format
sudo mkfs.vfat -n OZMA-BOOT "$BOOT_PART" >/dev/null
sudo mkfs.ext4 -q -L ozma-root-a "$ROOTA_PART"
sudo mkfs.ext4 -q -L ozma-root-b "$ROOTB_PART"
sudo mkfs.ext4 -q -L ozma-data "$DATA_PART"

echo "  Boot:   $BOOT_PART (${BOOT_SIZE}MB FAT32)"
echo "  Root A: $ROOTA_PART (${ROOT_SIZE}MB ext4)"
echo "  Root B: $ROOTB_PART (${ROOT_SIZE}MB ext4)"
echo "  Data:   $DATA_PART (remaining, ext4)"

# ── Mount ───────────────────────────────────────────────────────────────────

MOUNT_ROOT=$(mktemp -d)
sudo mount "$ROOTA_PART" "$MOUNT_ROOT"
sudo mkdir -p "$MOUNT_ROOT/boot" "$MOUNT_ROOT/data"
MOUNT_BOOT=$(mktemp -d)
sudo mount "$BOOT_PART" "$MOUNT_BOOT"
MOUNT_DATA=$(mktemp -d)
sudo mount "$DATA_PART" "$MOUNT_DATA"

# ── Install Alpine base ────────────────────────────────────────────────────

echo "[4/8] Installing Alpine Linux base system..."
sudo tar xzf "$ALPINE_MINIROOTFS" -C "$MOUNT_ROOT"

# Configure APK repositories
echo "https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main" | sudo tee "$MOUNT_ROOT/etc/apk/repositories" >/dev/null
echo "https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/community" | sudo tee -a "$MOUNT_ROOT/etc/apk/repositories" >/dev/null

# Resolve DNS in chroot
sudo cp /etc/resolv.conf "$MOUNT_ROOT/etc/resolv.conf"

# Install packages
sudo chroot "$MOUNT_ROOT" apk update --quiet 2>/dev/null
sudo chroot "$MOUNT_ROOT" apk add --quiet --no-progress \
    linux-lts linux-firmware-none mkinitfs syslinux \
    python3 py3-pip \
    ffmpeg avahi avahi-tools \
    pipewire pipewire-pulse wireplumber \
    v4l-utils openssh openrc e2fsprogs \
    2>/dev/null || echo "  (some packages may not be available — continuing)"

# ── Install ozma controller ────────────────────────────────────────────────

echo "[5/8] Installing ozma controller..."
sudo mkdir -p "$MOUNT_ROOT/opt/ozma"
sudo cp -r "$REPO_ROOT/controller" "$MOUNT_ROOT/opt/ozma/"
sudo cp -r "$REPO_ROOT/softnode" "$MOUNT_ROOT/opt/ozma/"

# Install Python dependencies
sudo chroot "$MOUNT_ROOT" pip3 install --quiet --break-system-packages \
    fastapi uvicorn zeroconf aiohttp asyncvnc numpy pillow mido pydantic websockets pynacl \
    2>/dev/null || true

# ── Configure data partition ────────────────────────────────────────────────

echo "[6/8] Configuring persistent data partition..."

# Create data directory structure
sudo mkdir -p "$MOUNT_DATA/config" "$MOUNT_DATA/recordings" "$MOUNT_DATA/audit" "$MOUNT_DATA/plugins"

# Default scenarios
if [[ -f "$REPO_ROOT/demo/scenarios.json" ]]; then
    sudo cp "$REPO_ROOT/demo/scenarios.json" "$MOUNT_DATA/config/scenarios.json"
fi

# fstab — mount data partition at /data
sudo tee "$MOUNT_ROOT/etc/fstab" > /dev/null << 'FSTAB'
LABEL=ozma-boot   /boot   vfat   defaults,ro    0 0
LABEL=ozma-data    /data   ext4   defaults       0 2
FSTAB

# Symlink config from /data into the controller directory
sudo mkdir -p "$MOUNT_ROOT/opt/ozma/controller"
sudo chroot "$MOUNT_ROOT" sh -c "
    ln -sf /data/config/scenarios.json /opt/ozma/controller/scenarios.json 2>/dev/null || true
    ln -sf /data/config/mesh_registry.json /opt/ozma/controller/mesh_registry.json 2>/dev/null || true
    ln -sf /data/config/connect_cache.json /opt/ozma/controller/connect_cache.json 2>/dev/null || true
    ln -sf /data/plugins /opt/ozma/controller/plugins 2>/dev/null || true
"

# ── Boot metadata (A/B tracking) ───────────────────────────────────────────

sudo tee "$MOUNT_DATA/config/ozma-boot.json" > /dev/null << BOOTJSON
{
  "active_slot": "a",
  "slot_a": {
    "version": "$OZMA_VERSION",
    "installed_at": "$(date -Iseconds)",
    "boot_count_since_update": 0,
    "healthy": true,
    "pending_validation": false
  },
  "slot_b": {
    "version": "",
    "installed_at": "",
    "boot_count_since_update": 0,
    "healthy": false,
    "pending_validation": false
  },
  "max_boot_attempts": 3,
  "update_channel": "stable"
}
BOOTJSON

# ── System configuration ───────────────────────────────────────────────────

echo "[7/8] Configuring system..."

# Hostname
echo "ozma-controller" | sudo tee "$MOUNT_ROOT/etc/hostname" >/dev/null

# Networking (DHCP)
sudo tee "$MOUNT_ROOT/etc/network/interfaces" > /dev/null << 'NETEOF'
auto lo
iface lo inet loopback

auto eth0
iface eth0 inet dhcp
NETEOF

# OpenRC init script for ozma
sudo tee "$MOUNT_ROOT/etc/init.d/ozma-controller" > /dev/null << 'INITEOF'
#!/sbin/openrc-run

name="ozma-controller"
description="Ozma Controller"
command="/usr/bin/python3"
command_args="/opt/ozma/controller/main.py"
command_background="yes"
pidfile="/run/ozma-controller.pid"
directory="/opt/ozma/controller"

depend() {
    need net
    after firewall avahi-daemon
}

start_post() {
    # A/B health check: validate the new slot after boot
    /opt/ozma/controller/update_manager.py --health-check &
}
INITEOF
sudo chmod +x "$MOUNT_ROOT/etc/init.d/ozma-controller"

# Enable services
sudo chroot "$MOUNT_ROOT" sh -c "
    rc-update add devfs sysinit 2>/dev/null || true
    rc-update add dmesg sysinit 2>/dev/null || true
    rc-update add hwclock boot 2>/dev/null || true
    rc-update add modules boot 2>/dev/null || true
    rc-update add networking boot 2>/dev/null || true
    rc-update add hostname boot 2>/dev/null || true
    rc-update add avahi-daemon default 2>/dev/null || true
    rc-update add sshd default 2>/dev/null || true
    rc-update add ozma-controller default 2>/dev/null || true
" 2>/dev/null

# MOTD
sudo tee "$MOUNT_ROOT/etc/motd" > /dev/null << MOTDEOF

  ╔══════════════════════════════════════════════════╗
  ║  Ozma Controller v$OZMA_VERSION
  ║                                                  ║
  ║  Easy things automatic.                          ║
  ║  Hard things easy.                               ║
  ║  Amazing things possible.                        ║
  ║                                                  ║
  ║  Dashboard: http://<this-ip>:7380                ║
  ║                                                  ║
  ║  Add machines:                                   ║
  ║    pip install ozma-softnode                      ║
  ║    ozma-softnode --name my-desktop               ║
  ║                                                  ║
  ║  A/B partitions: automatic rollback on failure   ║
  ╚══════════════════════════════════════════════════╝

MOTDEOF

# ── Bootloader (syslinux) ──────────────────────────────────────────────────

echo "[8/8] Installing bootloader..."
sudo mkdir -p "$MOUNT_BOOT/syslinux"

# Copy kernel + initramfs from installed Alpine
VMLINUZ=$(ls "$MOUNT_ROOT/boot/vmlinuz-"*lts 2>/dev/null | head -1)
INITRAMFS=$(ls "$MOUNT_ROOT/boot/initramfs-"*lts 2>/dev/null | head -1)

if [[ -n "$VMLINUZ" ]]; then
    sudo cp "$VMLINUZ" "$MOUNT_BOOT/vmlinuz-a"
    sudo cp "$VMLINUZ" "$MOUNT_BOOT/vmlinuz-b"  # same kernel initially
fi
if [[ -n "$INITRAMFS" ]]; then
    sudo cp "$INITRAMFS" "$MOUNT_BOOT/initramfs-a"
    sudo cp "$INITRAMFS" "$MOUNT_BOOT/initramfs-b"
fi

# Syslinux config — boots the active slot
sudo tee "$MOUNT_BOOT/syslinux/syslinux.cfg" > /dev/null << 'SYSEOF'
DEFAULT ozma-a
TIMEOUT 30
PROMPT 0

LABEL ozma-a
    MENU LABEL Ozma Controller (Slot A)
    LINUX /vmlinuz-a
    INITRD /initramfs-a
    APPEND root=LABEL=ozma-root-a modules=ext4 quiet

LABEL ozma-b
    MENU LABEL Ozma Controller (Slot B)
    LINUX /vmlinuz-b
    INITRD /initramfs-b
    APPEND root=LABEL=ozma-root-b modules=ext4 quiet

LABEL ozma-rollback
    MENU LABEL Ozma Controller (Rollback)
    LINUX /vmlinuz-a
    INITRD /initramfs-a
    APPEND root=LABEL=ozma-root-a modules=ext4 quiet ozma.rollback=1
SYSEOF

# Install syslinux bootloader
if command -v syslinux &>/dev/null; then
    sudo syslinux --install "$BOOT_PART" 2>/dev/null || true
fi
# Write MBR
for mbr_path in /usr/lib/syslinux/mbr/mbr.bin /usr/share/syslinux/mbr.bin /usr/lib/syslinux/bios/mbr.bin; do
    if [[ -f "$mbr_path" ]]; then
        sudo dd if="$mbr_path" of="$LOOP" bs=440 count=1 conv=notrunc 2>/dev/null
        break
    fi
done

# ── Cleanup ─────────────────────────────────────────────────────────────────

sudo umount "$MOUNT_BOOT"
sudo umount "$MOUNT_DATA"
sudo umount "$MOUNT_ROOT"
sudo losetup -d "$LOOP"
rmdir "$MOUNT_ROOT" "$MOUNT_BOOT" "$MOUNT_DATA"

echo ""
echo "╔══════════════════════════════════════════════════════╗"
echo "║  USB image built!                                    ║"
echo "╠══════════════════════════════════════════════════════╣"
echo "║  Image:     $IMAGE_FILE"
echo "║  Size:      $(du -h "$IMAGE_FILE" | cut -f1)"
echo "║  Version:   $OZMA_VERSION"
echo "║  Partitions:"
echo "║    Boot    ${BOOT_SIZE}MB  (FAT32, syslinux + kernels)"
echo "║    Root A  ${ROOT_SIZE}MB  (ext4, active rootfs)"
echo "║    Root B  ${ROOT_SIZE}MB  (ext4, inactive, for updates)"
echo "║    Data    ~$((IMAGE_SIZE_MB - BOOT_SIZE - ROOT_SIZE - ROOT_SIZE))MB  (ext4, persistent)"
echo "║"
echo "║  Flash:"
echo "║    sudo dd if=$IMAGE_FILE \\"
echo "║       of=/dev/sdX bs=4M status=progress"
echo "╚══════════════════════════════════════════════════════╝"
