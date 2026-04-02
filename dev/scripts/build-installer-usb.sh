#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#
# Build a USB installer for the Ozma Controller.
#
# The USB boots into a minimal installer that:
#   1. Detects the target machine's internal storage (eMMC, NVMe, SSD)
#   2. Confirms with the user ("Install to /dev/nvme0n1? ALL DATA WILL BE ERASED")
#   3. Partitions the internal disk with A/B layout
#   4. Installs Alpine + ozma controller + all dependencies
#   5. Installs bootloader (syslinux/GRUB) on the internal disk
#   6. Prompts to remove USB and reboot
#
# The USB itself is a tiny live environment (~200MB) — just enough to run
# the installer script. The target disk gets the full A/B partitioned system.
#
# Output: images/ozma-installer-x86_64.img (~256MB, dd to any USB)
#
# Usage:
#   bash dev/scripts/build-installer-usb.sh              # interactive installer (default)
#   bash dev/scripts/build-installer-usb.sh --auto       # auto installer (no prompts)
#   sudo dd if=images/ozma-installer-x86_64.img of=/dev/sdX bs=4M status=progress
#
# Interactive: boots to disk selection menu, asks for confirmation
# Auto (--auto): picks largest internal disk, installs without prompting.
#   Plug USB into N100, boot, walk away. Done.
#
# At boot time, select "Interactive Install" or "Auto Install" from the
# syslinux menu. Or pass ozma.auto on the kernel command line.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
IMAGE_DIR="$REPO_ROOT/images"
CACHE_DIR="$IMAGE_DIR/cache"

ALPINE_VERSION="3.21"
ALPINE_ARCH="x86_64"
ALPINE_MINIROOTFS_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/releases/${ALPINE_ARCH}/alpine-minirootfs-${ALPINE_VERSION}.0-${ALPINE_ARCH}.tar.gz"
ALPINE_MINIROOTFS="$CACHE_DIR/alpine-minirootfs-${ALPINE_ARCH}.tar.gz"

INSTALLER_IMG="$IMAGE_DIR/ozma-installer-${ALPINE_ARCH}.img"
INSTALLER_SIZE_MB=512  # small — just the live installer environment
OZMA_VERSION="$(cd "$REPO_ROOT" && git describe --tags 2>/dev/null || echo '0.1.0-dev')"
DEFAULT_BOOT="installer"  # "installer" (interactive) or "auto"

# --auto flag makes auto-install the default boot entry
for arg in "$@"; do
    [[ "$arg" == "--auto" ]] && DEFAULT_BOOT="auto"
done

echo "╔════════════════════════════════════════════════════╗"
echo "║  Ozma Controller — USB Installer Builder           ║"
echo "╠════════════════════════════════════════════════════╣"
echo "║  Version: $OZMA_VERSION"
echo "║  Output:  $INSTALLER_IMG"
echo "╚════════════════════════════════════════════════════╝"
echo ""

mkdir -p "$CACHE_DIR" "$IMAGE_DIR"

# ── Download Alpine ─────────────────────────────────────────────────────────

if [[ ! -f "$ALPINE_MINIROOTFS" ]]; then
    echo "[1/5] Downloading Alpine minirootfs..."
    curl -L --progress-bar -o "$ALPINE_MINIROOTFS" "$ALPINE_MINIROOTFS_URL"
else
    echo "[1/5] Alpine minirootfs cached."
fi

# ── Create installer image ──────────────────────────────────────────────────

echo "[2/5] Creating ${INSTALLER_SIZE_MB}MB installer image..."
dd if=/dev/zero of="$INSTALLER_IMG" bs=1M count="$INSTALLER_SIZE_MB" status=none

parted -s "$INSTALLER_IMG" mklabel msdos
parted -s "$INSTALLER_IMG" mkpart primary ext4 1MiB 100%
parted -s "$INSTALLER_IMG" set 1 boot on

LOOP=$(sudo losetup --find --show --partscan "$INSTALLER_IMG")
sleep 1
sudo partprobe "$LOOP" 2>/dev/null || true
sleep 1

INST_PART="${LOOP}p1"
sudo mkfs.ext4 -q -L OZMA-INST "$INST_PART"

MOUNT=$(mktemp -d)
sudo mount "$INST_PART" "$MOUNT"

# ── Build installer environment ─────────────────────────────────────────────

echo "[3/5] Building installer environment..."
sudo tar xzf "$ALPINE_MINIROOTFS" -C "$MOUNT"

# APK repos
echo "https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main" | sudo tee "$MOUNT/etc/apk/repositories" >/dev/null
echo "https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/community" | sudo tee -a "$MOUNT/etc/apk/repositories" >/dev/null
sudo cp /etc/resolv.conf "$MOUNT/etc/resolv.conf"

# Install minimal packages for the installer itself
sudo chroot "$MOUNT" apk update --quiet 2>/dev/null
sudo chroot "$MOUNT" apk add --quiet --no-progress \
    linux-lts linux-firmware-none mkinitfs syslinux \
    parted e2fsprogs dosfstools util-linux bash dialog \
    2>/dev/null || true

# ── Copy ozma source + deps to installer ────────────────────────────────────

echo "[4/5] Bundling ozma source and dependencies..."
sudo mkdir -p "$MOUNT/opt/ozma-install"
sudo cp -r "$REPO_ROOT/controller" "$MOUNT/opt/ozma-install/"
sudo cp -r "$REPO_ROOT/softnode" "$MOUNT/opt/ozma-install/"
sudo cp "$REPO_ROOT/controller/requirements.txt" "$MOUNT/opt/ozma-install/"

# Pre-download pip packages so the installer works offline
sudo chroot "$MOUNT" sh -c "
    apk add --quiet python3 py3-pip 2>/dev/null
    pip3 download --quiet --dest /opt/ozma-install/pip-cache \
        fastapi uvicorn zeroconf aiohttp asyncvnc numpy pillow mido pydantic websockets pynacl \
        2>/dev/null || true
" 2>/dev/null

# ── Embed the actual installer script ───────────────────────────────────────

echo "[5/5] Embedding installer script..."

sudo tee "$MOUNT/opt/ozma-install/install-to-disk.sh" > /dev/null << 'INSTALLER_SCRIPT'
#!/bin/bash
# Ozma Controller — Install to internal disk
# This script runs on the target machine from the USB installer.
#
# Modes:
#   Interactive (default): shows disk selection, asks for confirmation
#   Auto (--auto or /proc/cmdline contains ozma.auto): picks largest
#     internal disk and installs without prompting. Plug USB into N100,
#     boot, walk away, done.

set -euo pipefail

OZMA_SRC="/opt/ozma-install"
BOOT_SIZE=32      # MB
MIN_DISK_SIZE=4000 # MB — minimum disk size we'll install to

# Check for auto/live mode
AUTO_MODE=false
LIVE_MODE=false
if [[ "${1:-}" == "--auto" ]] || grep -q "ozma.auto" /proc/cmdline 2>/dev/null; then
    AUTO_MODE=true
fi
if [[ "${1:-}" == "--live" ]] || grep -q "ozma.live" /proc/cmdline 2>/dev/null; then
    LIVE_MODE=true
fi

# Live mode: skip installation, run controller directly from USB
if $LIVE_MODE; then
    echo -e "${BOLD}╔══════════════════════════════════════════════════╗${NC}"
    echo -e "${BOLD}║  Ozma Controller — Live Mode (USB)               ║${NC}"
    echo -e "${BOLD}║                                                  ║${NC}"
    echo -e "${BOLD}║  Running directly from USB. No installation.     ║${NC}"
    echo -e "${BOLD}║  Data is stored on USB (persistent if writable). ║${NC}"
    echo -e "${BOLD}╚══════════════════════════════════════════════════╝${NC}"
    echo ""
    # Create data directory on USB
    mkdir -p /data/config /data/recordings /data/plugins 2>/dev/null || true
    # Symlink config
    ln -sf /data/config/scenarios.json /opt/ozma-install/controller/scenarios.json 2>/dev/null || true
    ln -sf /data/config/mesh_registry.json /opt/ozma-install/controller/mesh_registry.json 2>/dev/null || true
    # Start the controller directly
    echo -e "${GREEN}Starting Ozma Controller...${NC}"
    echo -e "  Dashboard: http://$(hostname -I 2>/dev/null | awk '{print $1}' || echo '<this-ip>'):7380"
    echo ""
    cd /opt/ozma-install/controller
    exec python3 main.py
    # Does not return
fi

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

echo ""
echo -e "${BOLD}╔══════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}║  Ozma Controller Installer                       ║${NC}"
echo -e "${BOLD}║                                                  ║${NC}"
echo -e "${BOLD}║  Easy things automatic.                          ║${NC}"
echo -e "${BOLD}║  Hard things easy.                               ║${NC}"
echo -e "${BOLD}║  Amazing things possible.                        ║${NC}"
if $AUTO_MODE; then
echo -e "${BOLD}║                                                  ║${NC}"
echo -e "${BOLD}║  ${GREEN}AUTO MODE — no prompts, installing now${NC}${BOLD}          ║${NC}"
fi
echo -e "${BOLD}╚══════════════════════════════════════════════════╝${NC}"
echo ""

# ── Check for existing installation ─────────────────────────────────────────

if $AUTO_MODE; then
    # If ozma is already installed on an internal disk, just boot it
    EXISTING=$(blkid -L ozma-root-a 2>/dev/null || true)
    if [[ -n "$EXISTING" ]]; then
        EXISTING_DISK=$(echo "$EXISTING" | sed 's/[0-9]*$//' | sed 's/p[0-9]*$//')
        echo -e "${GREEN}Existing ozma installation found on $EXISTING_DISK${NC}"
        echo -e "${GREEN}Booting existing installation...${NC}"
        # Switch root to the existing installation
        TMPROOT=$(mktemp -d)
        mount "$EXISTING" "$TMPROOT"
        if [[ -f "$TMPROOT/opt/ozma/controller/main.py" ]]; then
            # Mount data partition too
            DATA_DEV=$(blkid -L ozma-data 2>/dev/null || true)
            [[ -n "$DATA_DEV" ]] && mount "$DATA_DEV" "$TMPROOT/data" 2>/dev/null || true
            # Pivot into the installed system using switch_root or kexec
            # For simplicity: just chainload via kexec if available
            BOOT_DEV=$(blkid -L OZMA-BOOT 2>/dev/null || true)
            if [[ -n "$BOOT_DEV" ]] && command -v kexec &>/dev/null; then
                BOOT_MNT=$(mktemp -d)
                mount "$BOOT_DEV" "$BOOT_MNT"
                # Load the installed kernel and boot into it
                SLOT=$(cat "$TMPROOT/data/config/ozma-boot.json" 2>/dev/null | \
                    python3 -c "import sys,json; print(json.load(sys.stdin).get('active_slot','a'))" 2>/dev/null || echo "a")
                kexec -l "$BOOT_MNT/vmlinuz-${SLOT}" \
                    --initrd="$BOOT_MNT/initramfs-${SLOT}" \
                    --command-line="root=LABEL=ozma-root-${SLOT} modules=ext4 quiet" 2>/dev/null && \
                    kexec -e
                umount "$BOOT_MNT" 2>/dev/null
                rmdir "$BOOT_MNT"
            fi
            # kexec not available or failed — just reboot and let BIOS pick internal disk
            umount "$TMPROOT/data" 2>/dev/null || true
            umount "$TMPROOT"
            echo "Rebooting into installed system (set BIOS to boot from internal disk)..."
            sleep 2
            reboot
        fi
        umount "$TMPROOT" 2>/dev/null
        rmdir "$TMPROOT"
    fi
fi

# ── Find target disk ────────────────────────────────────────────────────────

echo -e "${BLUE}Detecting internal storage...${NC}"
echo ""

# List all block devices except the USB we booted from
BOOT_DEV=$(findmnt -n -o SOURCE / | sed 's/[0-9]*$//' | sed 's/p[0-9]*$//')
CANDIDATES=()

for disk in /dev/sd? /dev/nvme?n? /dev/mmcblk? /dev/vd?; do
    [[ -b "$disk" ]] || continue
    # Skip the USB installer disk
    [[ "$disk" == "$BOOT_DEV"* ]] && continue
    SIZE_MB=$(( $(blockdev --getsize64 "$disk") / 1048576 ))
    [[ $SIZE_MB -lt $MIN_DISK_SIZE ]] && continue
    MODEL=$(cat "/sys/block/$(basename "$disk")/device/model" 2>/dev/null || echo "unknown")
    MODEL=$(echo "$MODEL" | xargs)  # trim whitespace
    echo "  $disk — ${SIZE_MB}MB — $MODEL"
    CANDIDATES+=("$disk|${SIZE_MB}|${MODEL}")
done

if [[ ${#CANDIDATES[@]} -eq 0 ]]; then
    echo -e "${RED}No suitable internal storage found (need >${MIN_DISK_SIZE}MB).${NC}"
    if $AUTO_MODE; then
        echo "Auto mode: waiting 30s for disks to appear..."
        sleep 30
        # Re-scan once
        exec "$0" "$@"
    fi
    exit 1
fi

# Select disk
if $AUTO_MODE; then
    # Auto mode: pick the largest internal disk
    BEST_SIZE=0
    for c in "${CANDIDATES[@]}"; do
        s=$(echo "$c" | cut -d'|' -f2)
        if [[ $s -gt $BEST_SIZE ]]; then
            BEST_SIZE=$s
            TARGET_DISK=$(echo "$c" | cut -d'|' -f1)
            TARGET_SIZE=$s
            TARGET_MODEL=$(echo "$c" | cut -d'|' -f3)
        fi
    done
    echo -e "${GREEN}Auto-selected: $TARGET_DISK (${TARGET_SIZE}MB, $TARGET_MODEL)${NC}"
elif [[ ${#CANDIDATES[@]} -eq 1 ]]; then
    TARGET_DISK=$(echo "${CANDIDATES[0]}" | cut -d'|' -f1)
    TARGET_SIZE=$(echo "${CANDIDATES[0]}" | cut -d'|' -f2)
    TARGET_MODEL=$(echo "${CANDIDATES[0]}" | cut -d'|' -f3)
else
    echo ""
    echo "Multiple disks found. Select target:"
    for i in "${!CANDIDATES[@]}"; do
        d=$(echo "${CANDIDATES[$i]}" | cut -d'|' -f1)
        s=$(echo "${CANDIDATES[$i]}" | cut -d'|' -f2)
        m=$(echo "${CANDIDATES[$i]}" | cut -d'|' -f3)
        echo "  $((i+1))) $d — ${s}MB — $m"
    done
    read -p "Select [1]: " choice
    choice=${choice:-1}
    idx=$((choice - 1))
    TARGET_DISK=$(echo "${CANDIDATES[$idx]}" | cut -d'|' -f1)
    TARGET_SIZE=$(echo "${CANDIDATES[$idx]}" | cut -d'|' -f2)
    TARGET_MODEL=$(echo "${CANDIDATES[$idx]}" | cut -d'|' -f3)
fi

echo ""
if $AUTO_MODE; then
    echo -e "${GREEN}Auto mode: installing to $TARGET_DISK (${TARGET_SIZE}MB)${NC}"
    sleep 3  # brief pause so the message is visible on screen
else
    echo -e "${RED}${BOLD}WARNING: ALL DATA ON $TARGET_DISK WILL BE ERASED!${NC}"
    echo -e "  Disk: $TARGET_DISK (${TARGET_SIZE}MB, $TARGET_MODEL)"
    echo ""
    read -p "Type 'yes' to continue: " confirm
    if [[ "$confirm" != "yes" ]]; then
        echo "Aborted."
        exit 0
    fi
fi

# ── Calculate partition sizes ───────────────────────────────────────────────

if [[ $TARGET_SIZE -ge 64000 ]]; then
    ROOT_SIZE=8192
elif [[ $TARGET_SIZE -ge 32000 ]]; then
    ROOT_SIZE=4096
elif [[ $TARGET_SIZE -ge 16000 ]]; then
    ROOT_SIZE=2048
elif [[ $TARGET_SIZE -ge 8000 ]]; then
    ROOT_SIZE=1536
elif [[ $TARGET_SIZE -ge 4000 ]]; then
    ROOT_SIZE=1024
else
    ROOT_SIZE=512
fi

DATA_SIZE=$((TARGET_SIZE - BOOT_SIZE - ROOT_SIZE * 2))
echo ""
echo -e "${BLUE}Partition layout:${NC}"
echo "  Boot:    ${BOOT_SIZE}MB (FAT32)"
echo "  Root A:  ${ROOT_SIZE}MB (ext4)"
echo "  Root B:  ${ROOT_SIZE}MB (ext4)"
echo "  Data:    ~${DATA_SIZE}MB (ext4, persistent)"
echo ""

# ── Partition ───────────────────────────────────────────────────────────────

echo -e "${BLUE}[1/6] Partitioning $TARGET_DISK...${NC}"

BOOT_START=1
BOOT_END=$((BOOT_START + BOOT_SIZE))
ROOTA_START=$BOOT_END
ROOTA_END=$((ROOTA_START + ROOT_SIZE))
ROOTB_START=$ROOTA_END
ROOTB_END=$((ROOTB_START + ROOT_SIZE))
DATA_START=$ROOTB_END

parted -s "$TARGET_DISK" mklabel msdos
parted -s "$TARGET_DISK" mkpart primary fat32 ${BOOT_START}MiB ${BOOT_END}MiB
parted -s "$TARGET_DISK" set 1 boot on
parted -s "$TARGET_DISK" mkpart primary ext4 ${ROOTA_START}MiB ${ROOTA_END}MiB
parted -s "$TARGET_DISK" mkpart primary ext4 ${ROOTB_START}MiB ${ROOTB_END}MiB
parted -s "$TARGET_DISK" mkpart primary ext4 ${DATA_START}MiB 100%

# Determine partition naming (/dev/sda1 vs /dev/nvme0n1p1)
if [[ "$TARGET_DISK" == *nvme* || "$TARGET_DISK" == *mmcblk* ]]; then
    P="${TARGET_DISK}p"
else
    P="$TARGET_DISK"
fi
BOOT_PART="${P}1"
ROOTA_PART="${P}2"
ROOTB_PART="${P}3"
DATA_PART="${P}4"

sleep 1
partprobe "$TARGET_DISK" 2>/dev/null || true
sleep 1

# ── Format ──────────────────────────────────────────────────────────────────

echo -e "${BLUE}[2/6] Formatting...${NC}"
mkfs.vfat -n OZMA-BOOT "$BOOT_PART" >/dev/null
mkfs.ext4 -q -L ozma-root-a "$ROOTA_PART"
mkfs.ext4 -q -L ozma-root-b "$ROOTB_PART"
mkfs.ext4 -q -L ozma-data "$DATA_PART"

# ── Mount ───────────────────────────────────────────────────────────────────

ROOT_MNT=$(mktemp -d)
mount "$ROOTA_PART" "$ROOT_MNT"
mkdir -p "$ROOT_MNT/boot" "$ROOT_MNT/data"
BOOT_MNT=$(mktemp -d)
mount "$BOOT_PART" "$BOOT_MNT"
DATA_MNT=$(mktemp -d)
mount "$DATA_PART" "$DATA_MNT"

# ── Install system ──────────────────────────────────────────────────────────

echo -e "${BLUE}[3/6] Installing Alpine Linux + ozma...${NC}"

# Bootstrap Alpine to root A
echo "https://dl-cdn.alpinelinux.org/alpine/v3.21/main" > /tmp/apk-repos
echo "https://dl-cdn.alpinelinux.org/alpine/v3.21/community" >> /tmp/apk-repos

apk -X "https://dl-cdn.alpinelinux.org/alpine/v3.21/main" \
    -U --allow-untrusted --root "$ROOT_MNT" --initdb \
    add alpine-base linux-lts linux-firmware-none mkinitfs syslinux \
    python3 \
    ffmpeg avahi avahi-tools \
    pipewire pipewire-pulse wireplumber \
    v4l-utils openssh openrc e2fsprogs \
    2>/dev/null || echo "  (using bundled packages)"

# Copy ozma controller
echo -e "${BLUE}[4/6] Installing ozma controller...${NC}"
mkdir -p "$ROOT_MNT/opt/ozma"
cp -r "$OZMA_SRC/controller" "$ROOT_MNT/opt/ozma/"
cp -r "$OZMA_SRC/softnode" "$ROOT_MNT/opt/ozma/"

# Install Python deps from cache or network
if [[ -d "$OZMA_SRC/pip-cache" ]]; then
    chroot "$ROOT_MNT" uv pip install --system --quiet --break-system-packages \
        --no-index --find-links=/opt/ozma-install/pip-cache \
        fastapi uvicorn zeroconf aiohttp asyncvnc numpy pillow mido pydantic websockets pynacl \
        2>/dev/null || true
    # Copy pip cache to installed system for offline use
    cp -r "$OZMA_SRC/pip-cache" "$ROOT_MNT/opt/ozma/pip-cache"
else
    chroot "$ROOT_MNT" uv pip install --system --quiet --break-system-packages \
        fastapi uvicorn zeroconf aiohttp asyncvnc numpy pillow mido pydantic websockets pynacl \
        2>/dev/null || true
fi

# ── Configure ───────────────────────────────────────────────────────────────

echo -e "${BLUE}[5/6] Configuring system...${NC}"

# fstab
cat > "$ROOT_MNT/etc/fstab" << 'FSTAB'
LABEL=ozma-boot    /boot   vfat   defaults,ro    0 0
LABEL=ozma-data    /data   ext4   defaults       0 2
FSTAB

# Hostname
echo "ozma-controller" > "$ROOT_MNT/etc/hostname"

# Networking
cat > "$ROOT_MNT/etc/network/interfaces" << 'NET'
auto lo
iface lo inet loopback

auto eth0
iface eth0 inet dhcp
NET

# Data partition structure
mkdir -p "$DATA_MNT/config" "$DATA_MNT/recordings" "$DATA_MNT/audit" "$DATA_MNT/plugins"

# Symlinks
chroot "$ROOT_MNT" sh -c "
    ln -sf /data/config/scenarios.json /opt/ozma/controller/scenarios.json
    ln -sf /data/config/mesh_registry.json /opt/ozma/controller/mesh_registry.json
    ln -sf /data/config/connect_cache.json /opt/ozma/controller/connect_cache.json
    ln -sf /data/plugins /opt/ozma/controller/plugins
" 2>/dev/null || true

# Boot metadata
cat > "$DATA_MNT/config/ozma-boot.json" << BOOTJSON
{
  "active_slot": "a",
  "slot_a": {
    "version": "$(cat $OZMA_SRC/controller/main.py 2>/dev/null | grep -o 'version.*' | head -1 || echo '0.1.0')",
    "installed_at": "$(date -Iseconds)",
    "boot_count_since_update": 0,
    "healthy": true,
    "pending_validation": false
  },
  "slot_b": {"version": "", "healthy": false, "pending_validation": false},
  "max_boot_attempts": 3,
  "update_channel": "stable"
}
BOOTJSON

# OpenRC service
cat > "$ROOT_MNT/etc/init.d/ozma-controller" << 'INIT'
#!/sbin/openrc-run
name="ozma-controller"
description="Ozma Controller"
command="/usr/bin/python3"
command_args="/opt/ozma/controller/main.py"
command_background="yes"
pidfile="/run/ozma-controller.pid"
directory="/opt/ozma/controller"
depend() { need net; after firewall avahi-daemon; }
start_post() { /usr/bin/python3 /opt/ozma/controller/update_manager.py --health-check & }
INIT
chmod +x "$ROOT_MNT/etc/init.d/ozma-controller"

# Enable services
chroot "$ROOT_MNT" sh -c "
    rc-update add devfs sysinit; rc-update add dmesg sysinit
    rc-update add hwclock boot; rc-update add modules boot
    rc-update add networking boot; rc-update add hostname boot
    rc-update add avahi-daemon default; rc-update add sshd default
    rc-update add ozma-controller default
" 2>/dev/null

# MOTD
cat > "$ROOT_MNT/etc/motd" << 'MOTD'

  ╔══════════════════════════════════════════════════╗
  ║  Ozma Controller                                 ║
  ║                                                  ║
  ║  Easy things automatic.                          ║
  ║  Hard things easy.                               ║
  ║  Amazing things possible.                        ║
  ║                                                  ║
  ║  Dashboard: http://<this-ip>:7380                ║
  ║                                                  ║
  ║  Add machines:                                   ║
  ║    uv pip install ozma-softnode                    ║
  ║    ozma-softnode --name my-desktop               ║
  ╚══════════════════════════════════════════════════╝

MOTD

# ── Bootloader ──────────────────────────────────────────────────────────────

echo -e "${BLUE}[6/6] Installing bootloader...${NC}"

mkdir -p "$BOOT_MNT/syslinux"

VMLINUZ=$(ls "$ROOT_MNT/boot/vmlinuz-"*lts 2>/dev/null | head -1)
INITRAMFS=$(ls "$ROOT_MNT/boot/initramfs-"*lts 2>/dev/null | head -1)
[[ -n "$VMLINUZ" ]] && cp "$VMLINUZ" "$BOOT_MNT/vmlinuz-a" && cp "$VMLINUZ" "$BOOT_MNT/vmlinuz-b"
[[ -n "$INITRAMFS" ]] && cp "$INITRAMFS" "$BOOT_MNT/initramfs-a" && cp "$INITRAMFS" "$BOOT_MNT/initramfs-b"

cat > "$BOOT_MNT/syslinux/syslinux.cfg" << 'SYSLINUX'
DEFAULT ozma-a
TIMEOUT 30
PROMPT 0

LABEL ozma-a
    LINUX /vmlinuz-a
    INITRD /initramfs-a
    APPEND root=LABEL=ozma-root-a modules=ext4 quiet

LABEL ozma-b
    LINUX /vmlinuz-b
    INITRD /initramfs-b
    APPEND root=LABEL=ozma-root-b modules=ext4 quiet
SYSLINUX

# Install syslinux to boot partition
syslinux --install "$BOOT_PART" 2>/dev/null || true
for mbr in /usr/share/syslinux/mbr.bin /usr/lib/syslinux/mbr/mbr.bin /usr/lib/syslinux/bios/mbr.bin; do
    [[ -f "$mbr" ]] && dd if="$mbr" of="$TARGET_DISK" bs=440 count=1 conv=notrunc 2>/dev/null && break
done

# ── Cleanup ─────────────────────────────────────────────────────────────────

umount "$BOOT_MNT" "$DATA_MNT" "$ROOT_MNT"
rmdir "$BOOT_MNT" "$DATA_MNT" "$ROOT_MNT"

echo ""
echo -e "${GREEN}${BOLD}╔══════════════════════════════════════════════════╗${NC}"
echo -e "${GREEN}${BOLD}║  Installation complete!                          ║${NC}"
echo -e "${GREEN}${BOLD}║                                                  ║${NC}"
echo -e "${GREEN}${BOLD}║  Dashboard: http://<this-ip>:7380                ║${NC}"
echo -e "${GREEN}${BOLD}╚══════════════════════════════════════════════════╝${NC}"
echo ""
if $AUTO_MODE; then
    echo "Auto mode: rebooting in 5 seconds (remove USB now)..."
    sleep 5
    reboot
else
    echo "Remove the USB drive, then press Enter to reboot."
    read -p "Press Enter to reboot..."
    reboot
fi
INSTALLER_SCRIPT

sudo chmod +x "$MOUNT/opt/ozma-install/install-to-disk.sh"

# Auto-run the installer on boot
sudo tee "$MOUNT/etc/inittab" > /dev/null << 'INITTAB'
::sysinit:/sbin/openrc sysinit
::sysinit:/sbin/openrc boot
::wait:/sbin/openrc default
# Auto-launch the ozma installer
tty1::respawn:/opt/ozma-install/install-to-disk.sh
tty2::respawn:/sbin/getty 38400 tty2
INITTAB

# ── Bootloader for the USB itself ───────────────────────────────────────────

VMLINUZ=$(ls "$MOUNT/boot/vmlinuz-"*lts 2>/dev/null | head -1)
INITRAMFS=$(ls "$MOUNT/boot/initramfs-"*lts 2>/dev/null | head -1)

sudo mkdir -p "$MOUNT/boot/syslinux"
if [[ -n "$VMLINUZ" ]]; then
    sudo cp "$VMLINUZ" "$MOUNT/boot/vmlinuz"
fi
if [[ -n "$INITRAMFS" ]]; then
    sudo cp "$INITRAMFS" "$MOUNT/boot/initramfs"
fi

sudo tee "$MOUNT/boot/syslinux/syslinux.cfg" > /dev/null << SYSEOF
DEFAULT $DEFAULT_BOOT
TIMEOUT 30
PROMPT 0

LABEL installer
    MENU LABEL Ozma Controller — Interactive Install
    LINUX /boot/vmlinuz
    INITRD /boot/initramfs
    APPEND root=LABEL=OZMA-INST modules=ext4 quiet

LABEL auto
    MENU LABEL Ozma Controller — Auto Install (largest disk, no prompts)
    LINUX /boot/vmlinuz
    INITRD /boot/initramfs
    APPEND root=LABEL=OZMA-INST modules=ext4 quiet ozma.auto

LABEL live
    MENU LABEL Ozma Controller — Run from USB (no install)
    LINUX /boot/vmlinuz
    INITRD /boot/initramfs
    APPEND root=LABEL=OZMA-INST modules=ext4 quiet ozma.live
SYSEOF

if command -v syslinux &>/dev/null; then
    sudo syslinux --install "$INST_PART" 2>/dev/null || true
fi
for mbr in /usr/lib/syslinux/mbr/mbr.bin /usr/share/syslinux/mbr.bin /usr/lib/syslinux/bios/mbr.bin; do
    if [[ -f "$mbr" ]]; then
        sudo dd if="$mbr" of="$LOOP" bs=440 count=1 conv=notrunc 2>/dev/null
        break
    fi
done

# ── Cleanup ─────────────────────────────────────────────────────────────────

sudo umount "$MOUNT"
sudo losetup -d "$LOOP"
rmdir "$MOUNT"

echo ""
echo "╔══════════════════════════════════════════════════════╗"
echo "║  Installer USB image built!                          ║"
echo "╠══════════════════════════════════════════════════════╣"
echo "║  Image: $INSTALLER_IMG"
echo "║  Size:  $(du -h "$INSTALLER_IMG" | cut -f1)"
echo "║                                                      ║"
echo "║  Flash:                                              ║"
echo "║    sudo dd if=$INSTALLER_IMG \\"
echo "║       of=/dev/sdX bs=4M status=progress              ║"
echo "║                                                      ║"
echo "║  Then boot target machine from USB.                  ║"
echo "║  The installer runs automatically.                   ║"
echo "╚══════════════════════════════════════════════════════╝"
