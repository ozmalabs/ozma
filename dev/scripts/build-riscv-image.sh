#!/bin/bash
# Build a configured Alpine Linux RISC-V disk image for the ozma node VM.
#
# Creates images/riscv-node.qcow2 + extracts kernel/initrd from the installed
# linux-lts package (Alpine riscv64 has no virt ISO; only minirootfs is available).
# No interactive steps; runs in ~5 minutes (mostly download time).
#
# Requires root (for qemu-nbd).
# Run once: sudo bash dev/scripts/build-riscv-image.sh
#
# After this, boot with:
#   qemu-system-riscv64 \
#     -kernel images/riscv-vmlinuz-lts \
#     -initrd images/riscv-initramfs-lts \
#     -drive  file=images/riscv-node.qcow2,...
#     -append "root=LABEL=ozma-root rw console=ttyS0 quiet"
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
IMAGES_DIR="${REPO_ROOT}/images"
DEV_DIR="${REPO_ROOT}/dev"

ALPINE_VERSION="3.21.3"
ALPINE_BRANCH="v${ALPINE_VERSION%.*}"
BASE_URL="https://dl-cdn.alpinelinux.org/alpine/${ALPINE_BRANCH}/releases/riscv64"
MINIROOTFS_URL="${BASE_URL}/alpine-minirootfs-${ALPINE_VERSION}-riscv64.tar.gz"
MINIROOTFS_TAR="${IMAGES_DIR}/alpine-minirootfs-riscv64.tar.gz"

DISK_IMG="${IMAGES_DIR}/riscv-node.qcow2"
DISK_SIZE="4G"
NBD_DEV="/dev/nbd0"
MNT="/tmp/ozma-riscv-build"

SSH_KEY_PUB="${IMAGES_DIR}/dev_key.pub"

# ── Pre-flight ────────────────────────────────────────────────────────────────
if [[ $EUID -ne 0 ]]; then
    echo "ERROR: This script must be run as root (needs qemu-nbd)."
    echo "  sudo bash dev/scripts/build-riscv-image.sh"
    exit 1
fi

for cmd in qemu-nbd qemu-img qemu-riscv64-static curl; do
    command -v "$cmd" &>/dev/null || { echo "ERROR: $cmd not found"; exit 1; }
done

[[ -f "${SSH_KEY_PUB}" ]] || { echo "ERROR: SSH key not found: ${SSH_KEY_PUB}. Run: make ssh-key"; exit 1; }

if [[ -f "${DISK_IMG}" ]]; then
    echo "Image already exists: ${DISK_IMG}"
    echo "Delete it first to rebuild."
    exit 0
fi

mkdir -p "${IMAGES_DIR}"
echo "=== Building RISC-V Alpine node image ==="
echo "  Alpine : ${ALPINE_VERSION} riscv64"
echo "  Disk   : ${DISK_IMG}"
echo ""

# ── Step 1: Download minirootfs ───────────────────────────────────────────────
if [[ ! -f "${MINIROOTFS_TAR}" ]]; then
    echo "[1/5] Downloading Alpine minirootfs (~3 MB)..."
    curl -L --progress-bar -o "${MINIROOTFS_TAR}" "${MINIROOTFS_URL}"
else
    echo "[1/5] Alpine minirootfs cached."
fi

# ── Step 2: Create and partition disk image ───────────────────────────────────
echo "[2/5] Creating disk image..."
qemu-img create -f qcow2 "${DISK_IMG}" "${DISK_SIZE}"

echo "[3/5] Mounting and formatting..."
modprobe nbd max_part=8 2>/dev/null || true
qemu-nbd --disconnect "${NBD_DEV}" 2>/dev/null || true
sleep 0.5
qemu-nbd --connect="${NBD_DEV}" "${DISK_IMG}"
sleep 0.5

echo "label: dos
type=83" | sfdisk "${NBD_DEV}" --no-reread -q
partprobe "${NBD_DEV}" 2>/dev/null || true
sleep 1
mkfs.ext4 -q -L ozma-root "${NBD_DEV}p1"

mkdir -p "${MNT}"
mount "${NBD_DEV}p1" "${MNT}"

# ── Step 3: Extract minirootfs and configure ──────────────────────────────────
echo "[4/5] Extracting and configuring rootfs..."
tar -xzf "${MINIROOTFS_TAR}" -C "${MNT}"

# qemu static binary for binfmt_misc transparent RISC-V execution in chroot
cp /usr/bin/qemu-riscv64-static "${MNT}/usr/bin/"

# APK repositories
cat > "${MNT}/etc/apk/repositories" << EOF
https://dl-cdn.alpinelinux.org/alpine/${ALPINE_BRANCH}/main
https://dl-cdn.alpinelinux.org/alpine/${ALPINE_BRANCH}/community
EOF

# DNS (needed for apk downloads in chroot)
cp /etc/resolv.conf "${MNT}/etc/resolv.conf"

# Pseudo-filesystems
mount --bind /proc    "${MNT}/proc"
mount --bind /sys     "${MNT}/sys"
mount --bind /dev     "${MNT}/dev"
mount --bind /dev/pts "${MNT}/dev/pts"

cleanup() {
    umount "${MNT}/dev/pts" 2>/dev/null || true
    umount "${MNT}/dev"     2>/dev/null || true
    umount "${MNT}/sys"     2>/dev/null || true
    umount "${MNT}/proc"    2>/dev/null || true
    umount "${MNT}"         2>/dev/null || true
    qemu-nbd --disconnect "${NBD_DEV}" 2>/dev/null || true
    rm -rf "${MNT}"
    echo "Cleanup done."
}
trap cleanup EXIT

chroot "${MNT}" /bin/sh << 'CHROOT'
set -e
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

echo "  Updating apk index..."
apk update -q

echo "  Installing base system + kernel..."
apk add -q alpine-base openssh util-linux-misc e2fsprogs-extra ca-certificates
apk add -q linux-lts

echo "  Installing Python runtime..."
apk add -q python3 py3-aiohttp

# usbip for exporting the USB gadget via USB/IP
echo "  Installing USB/IP tools..."
apk add -q usbip-utils 2>/dev/null || echo "    usbip-utils not available, trying usbip..."
apk add -q usbip 2>/dev/null || echo "    usbip not in repos — will use kernel built-ins if available"

# Optional: ffmpeg for test video pattern
echo "  Installing ffmpeg (optional)..."
apk add -q ffmpeg 2>/dev/null || echo "    ffmpeg not available"

# zeroconf for mDNS announcement from node.py
echo "  Installing zeroconf via uv..."
pip3 install --quiet --break-system-packages uv 2>/dev/null || true
uv pip install --system --quiet --break-system-packages zeroconf 2>/dev/null \
    || uv pip install --system --quiet zeroconf

# SSH config — key-only access (authorized_keys installed outside chroot)
sed -i 's/#PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config
sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config
rc-update add sshd default
ssh-keygen -A -q

# Networking
cat > /etc/network/interfaces << 'NET'
auto lo
iface lo inet loopback
auto eth0
iface eth0 inet dhcp
NET
rc-update add networking default

# Hostname
echo "ozma-riscv-node" > /etc/hostname

# Serial console for QEMU
echo "ttyS0::respawn:/sbin/getty -L ttyS0 115200 vt100" >> /etc/inittab

CHROOT

# ── Step 4: Extract kernel and initrd from installed location ─────────────────
echo "[5/5] Extracting kernel and initrd from installed image..."

VMLINUZ_SRC=$(find "${MNT}/boot" -name "vmlinuz*" 2>/dev/null | head -1)
INITRD_SRC=$(find "${MNT}/boot" -name "initramfs*" -o -name "initrd*" 2>/dev/null | head -1)

[[ -n "${VMLINUZ_SRC}" ]] || { echo "ERROR: vmlinuz not found in ${MNT}/boot"; ls "${MNT}/boot" 2>/dev/null || true; exit 1; }
[[ -n "${INITRD_SRC}" ]]  || { echo "ERROR: initrd not found in ${MNT}/boot";  ls "${MNT}/boot" 2>/dev/null || true; exit 1; }

# Alpine riscv64 vmlinuz is gzip-compressed; QEMU -kernel needs uncompressed Image
if file "${VMLINUZ_SRC}" | grep -q gzip; then
    zcat "${VMLINUZ_SRC}" > "${IMAGES_DIR}/riscv-vmlinuz-lts"
else
    cp "${VMLINUZ_SRC}" "${IMAGES_DIR}/riscv-vmlinuz-lts"
fi
cp "${INITRD_SRC}" "${IMAGES_DIR}/riscv-initramfs-lts"

# Also create symlinks with the -virt suffix that launch.sh looks for
ln -sf "${IMAGES_DIR}/riscv-vmlinuz-lts"    "${IMAGES_DIR}/riscv-vmlinuz-virt"
ln -sf "${IMAGES_DIR}/riscv-initramfs-lts"  "${IMAGES_DIR}/riscv-initramfs-virt"

echo "  Kernel : ${IMAGES_DIR}/riscv-vmlinuz-lts  ($(du -sh "${IMAGES_DIR}/riscv-vmlinuz-lts" | cut -f1))"
echo "  Initrd : ${IMAGES_DIR}/riscv-initramfs-lts ($(du -sh "${IMAGES_DIR}/riscv-initramfs-lts" | cut -f1))"

# ── Configure SSH key, node code, init script ─────────────────────────────────
mkdir -p "${MNT}/root/.ssh"
chmod 700 "${MNT}/root/.ssh"
cp "${SSH_KEY_PUB}" "${MNT}/root/.ssh/authorized_keys"
chmod 600 "${MNT}/root/.ssh/authorized_keys"

# Copy ozma node code
OZMA_NODE_DIR="${MNT}/root/ozma-node"
mkdir -p "${OZMA_NODE_DIR}"
cp -r "${REPO_ROOT}/node/"*         "${OZMA_NODE_DIR}/"
mkdir -p "${OZMA_NODE_DIR}/gadget"
cp -r "${REPO_ROOT}/tinynode/gadget/"* "${OZMA_NODE_DIR}/gadget/"
cp "${DEV_DIR}/riscv-node/init-alpine.sh" "${OZMA_NODE_DIR}/init.sh"
chmod +x "${OZMA_NODE_DIR}/init.sh" "${OZMA_NODE_DIR}/gadget/setup_gadget.sh"

# rc.local startup
cat > "${MNT}/etc/local.d/ozma.start" << 'EOF'
#!/bin/sh
exec >> /var/log/ozma-init.log 2>&1
echo "=== ozma init $(date) ==="
bash /root/ozma-node/init.sh
EOF
chmod +x "${MNT}/etc/local.d/ozma.start"
chroot "${MNT}" rc-update add local default 2>/dev/null || true

# fstab
printf 'LABEL=ozma-root  /  ext4  defaults,noatime  0 1\n' > "${MNT}/etc/fstab"

# Fix ownership so non-root user can use the images
REAL_USER="${SUDO_USER:-${USER}}"
chown "${REAL_USER}" "${DISK_IMG}" "${IMAGES_DIR}/riscv-vmlinuz-lts" "${IMAGES_DIR}/riscv-initramfs-lts" 2>/dev/null || true

echo ""
echo "=== Image built successfully ==="
echo ""
echo "  Disk   : ${DISK_IMG}"
echo "  Kernel : ${IMAGES_DIR}/riscv-vmlinuz-lts"
echo "  Initrd : ${IMAGES_DIR}/riscv-initramfs-lts"
echo ""
echo "Next steps:"
echo "  cd dev && make node-vm          # start the RISC-V VM"
echo "  make connect-vms                # connect USB gadget to vm1"
