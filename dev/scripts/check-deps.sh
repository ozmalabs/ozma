#!/bin/bash
# Verify host machine has everything needed to run the dev harness.
set -euo pipefail

PASS=0
FAIL=0

check() {
    local name="$1"; shift
    if "$@" &>/dev/null; then
        printf "  [ok]  %s\n" "$name"
        ((PASS++)) || true
    else
        printf "  [!!]  %s\n" "$name"
        ((FAIL++)) || true
    fi
}

echo "=== Host dependencies ==="
check "qemu-system-riscv64"   which qemu-system-riscv64
check "qemu-system-x86_64"    which qemu-system-x86_64
check "qemu-img"              which qemu-img
check "ssh"                   which ssh
check "ffmpeg"                which ffmpeg
check "curl"                  which curl
check "xz"                    which xz

echo ""
echo "=== QEMU RISC-V machine support ==="
check "virt machine"  bash -c 'qemu-system-riscv64 -machine help 2>&1 | grep -q virt'

echo ""
echo "=== Optional (TAP networking for mDNS) ==="
if check "ip command"   which ip; then
    check "tun module"  bash -c 'lsmod | grep -q tun || modinfo tun &>/dev/null'
fi

echo ""
if [[ $FAIL -gt 0 ]]; then
    echo "Install missing tools:"
    echo "  # Debian/Ubuntu:"
    echo "  apt install qemu-system-riscv qemu-system-x86 qemu-utils ffmpeg curl xz-utils"
    echo ""
    echo "  # Arch:"
    echo "  pacman -S qemu-system-riscv qemu-system-x86 ffmpeg curl xz"
    exit 1
else
    echo "All required dependencies present."
fi
