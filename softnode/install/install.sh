#!/bin/bash
# Install ozma-virtual-node system configuration.
#
# Sets up udev rules, AppArmor policies, and libvirt QEMU config
# so that virtual evdev devices can be used with QEMU input-linux.
#
# Usage: sudo bash softnode/install/install.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Installing ozma-virtual-node system config..."

# 1. udev rules — set group=kvm on ozma evdev devices
cp "$SCRIPT_DIR/99-ozma-evdev.rules" /etc/udev/rules.d/
udevadm control --reload-rules
echo "  udev rules installed"

# 2. AppArmor drop-in — allow QEMU to read /dev/input/event*
mkdir -p /etc/apparmor.d/abstractions/libvirt-qemu.d
cp "$SCRIPT_DIR/ozma-apparmor-libvirt-qemu" /etc/apparmor.d/abstractions/libvirt-qemu.d/ozma
if systemctl is-active --quiet apparmor; then
    systemctl reload apparmor
    echo "  AppArmor profile installed and reloaded"
else
    echo "  AppArmor profile installed (apparmor not running)"
fi

# 3. libvirt cgroup_device_acl — allow QEMU to access /dev/input/event*
QEMU_CONF="/etc/libvirt/qemu.conf"
if [ -f "$QEMU_CONF" ] && ! grep -q '^cgroup_device_acl' "$QEMU_CONF"; then
    cat >> "$QEMU_CONF" << 'CONF'

# Ozma: allow QEMU to access input devices for evdev input-linux
cgroup_device_acl = [
    "/dev/null", "/dev/full", "/dev/zero",
    "/dev/random", "/dev/urandom",
    "/dev/ptmx", "/dev/kvm",
    "/dev/input/event0", "/dev/input/event1", "/dev/input/event2",
    "/dev/input/event3", "/dev/input/event4", "/dev/input/event5",
    "/dev/input/event6", "/dev/input/event7", "/dev/input/event8",
    "/dev/input/event9", "/dev/input/event10", "/dev/input/event11",
    "/dev/input/event12", "/dev/input/event13", "/dev/input/event14",
    "/dev/input/event15", "/dev/input/event16", "/dev/input/event17",
    "/dev/input/event18", "/dev/input/event19", "/dev/input/event20",
    "/dev/input/event21", "/dev/input/event22", "/dev/input/event23",
    "/dev/input/event24", "/dev/input/event25", "/dev/input/event26",
    "/dev/input/event27", "/dev/input/event28", "/dev/input/event29",
    "/dev/input/event30", "/dev/input/event31"
]
CONF
    echo "  cgroup_device_acl added to qemu.conf"
    echo "  NOTE: restart libvirtd for changes to take effect:"
    echo "    sudo systemctl restart libvirtd"
else
    echo "  cgroup_device_acl already configured (or qemu.conf not found)"
fi

echo ""
echo "Done. VMs must be restarted for cgroup changes to take effect."
echo "Run 'ozma-virtual-node' to auto-manage all VMs."
