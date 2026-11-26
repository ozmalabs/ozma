#!/usr/bin/env bash
# Launch an emulated x86_64 macOS VM (QEMU/KVM + OpenCore) for ozma-node testing.
#
# Required environment/files (not committed to git):
#   OZMA_MACOS_VM_IMAGE   Path to macOS qcow2 disk (default: images/macos.qcow2)
#   OZMA_MACOS_OPENCORE   Path to OpenCore.qcow2 (default: images/OpenCore.qcow2)
#   OZMA_MACOS_OVMF_CODE  Path to OVMF_CODE.fd (default: images/OVMF_CODE.fd)
#   OZMA_MACOS_OVMF_VARS  Path to OVMF_VARS.fd (default: images/OVMF_VARS.fd)
#   OZMA_MACOS_OSK        Apple OSK string (set via env; required by AppleSMC)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

VM_IMAGE="${OZMA_MACOS_VM_IMAGE:-$REPO_DIR/images/macos.qcow2}"
OPENCORE_IMG="${OZMA_MACOS_OPENCORE:-$REPO_DIR/images/OpenCore.qcow2}"
OVMF_CODE="${OZMA_MACOS_OVMF_CODE:-$REPO_DIR/images/OVMF_CODE.fd}"
OVMF_VARS="${OZMA_MACOS_OVMF_VARS:-$REPO_DIR/images/OVMF_VARS.fd}"
MEDIA_ISO="${MEDIA_ISO:-$REPO_DIR/images/ozma-media-macos.img}"
OSK="${OZMA_MACOS_OSK:-}"

for f in "$VM_IMAGE" "$OPENCORE_IMG" "$OVMF_CODE" "$OVMF_VARS"; do
    if [ ! -f "$f" ]; then
        echo "Missing required VM file: $f" >&2
        exit 1
    fi
done
if [ -z "$OSK" ]; then
    echo "OZMA_MACOS_OSK must be set for isa-applesmc" >&2
    exit 1
fi

MEDIA_ARGS=()
if [ -f "$MEDIA_ISO" ]; then
    MEDIA_ARGS=(-drive "id=OzmaMedia,if=none,format=raw,readonly=on,file=$MEDIA_ISO" -device "ide-cd,bus=ide.2,drive=OzmaMedia")
fi

exec qemu-system-x86_64 \
    -name ozma-macos \
    -enable-kvm -m 6144 -smp 4 \
    -cpu Penryn,kvm=on,vendor=GenuineIntel,+invtsc,vmware-cpuid-freq=on,+ssse3,+sse4.2,+popcnt,+avx,+aes,+xsave,+xsaveopt,check \
    -machine q35 \
    -device "isa-applesmc,osk=$OSK" \
    -smbios type=2 \
    -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE" \
    -drive "if=pflash,format=raw,file=$OVMF_VARS" \
    -device ich9-ahci,id=sata \
    -drive "id=OpenCoreBoot,if=none,snapshot=on,format=qcow2,file=$OPENCORE_IMG" \
    -device ide-hd,bus=sata.2,drive=OpenCoreBoot \
    -drive "id=MacHDD,if=none,format=qcow2,file=$VM_IMAGE" \
    -device ide-hd,bus=sata.3,drive=MacHDD \
    "${MEDIA_ARGS[@]}" \
    -netdev "user,id=net0,hostfwd=tcp::7342-:7331,hostfwd=tcp::7343-:7382" \
    -device vmxnet3,netdev=net0 \
    -device qemu-xhci -device usb-kbd -device usb-tablet \
    -vga vmware \
    -vnc :32 \
    -qmp unix:/tmp/ozma-macos.qmp,server,nowait \
    -daemonize -pidfile /tmp/ozma-macos.pid
