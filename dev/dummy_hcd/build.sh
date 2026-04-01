#!/bin/bash
# Build and load dummy_hcd kernel module
#
# dummy_hcd creates a virtual USB host + device controller pair inside
# the kernel. The soft node uses it to present a real USB composite
# device (HID + mass storage + audio) to a QEMU VM — identical to
# what a hardware node does over a physical USB cable.
#
# Usage:
#   bash dev/dummy_hcd/build.sh          # build + sign + load
#   bash dev/dummy_hcd/build.sh build    # build only
#   bash dev/dummy_hcd/build.sh load     # load pre-built module
#   bash dev/dummy_hcd/build.sh unload   # remove module

set -e
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

case "${1:-all}" in
    build|all)
        echo "Building dummy_hcd for $(uname -r)..."

        # Get source if not present
        if [ ! -f "$SCRIPT_DIR/dummy_hcd.c" ]; then
            SRC_TAR="/usr/src/linux-source-$(uname -r | cut -d- -f1).tar.bz2"
            if [ ! -f "$SRC_TAR" ]; then
                echo "Installing kernel source..."
                sudo apt install -y "linux-source-$(uname -r | cut -d- -f1)"
            fi
            tar xf "$SRC_TAR" --strip-components=4 \
                "linux-source-*/drivers/usb/gadget/udc/dummy_hcd.c" \
                -C "$SCRIPT_DIR/"
        fi

        # Build
        cd "$SCRIPT_DIR"
        make

        # Sign if Secure Boot is enabled
        if mokutil --sb-state 2>/dev/null | grep -q "enabled"; then
            MOK_KEY="/var/lib/shim-signed/mok/MOK.priv"
            MOK_CERT="/var/lib/shim-signed/mok/MOK.der"
            if [ -f "$MOK_KEY" ] && [ -f "$MOK_CERT" ]; then
                echo "Signing module for Secure Boot..."
                SIGN="/usr/src/linux-headers-$(uname -r)/scripts/sign-file"
                sudo "$SIGN" sha256 "$MOK_KEY" "$MOK_CERT" dummy_hcd.ko
            else
                echo "WARNING: Secure Boot enabled but no MOK key found"
                echo "Module may fail to load. See: mokutil --help"
            fi
        fi

        echo "Built: $SCRIPT_DIR/dummy_hcd.ko"
        ;;&

    load|all)
        echo "Loading dummy_hcd..."
        sudo modprobe libcomposite
        sudo insmod "$SCRIPT_DIR/dummy_hcd.ko" num=${NUM_PORTS:-1}
        echo "UDC available: $(ls /sys/class/udc/)"
        ;;

    unload)
        echo "Unloading dummy_hcd..."
        sudo rmmod dummy_hcd 2>/dev/null || true
        echo "Unloaded"
        ;;

    *)
        echo "Usage: $0 {build|load|unload|all}"
        ;;
esac
