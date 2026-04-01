#!/bin/bash
# Set up TAP networking for the RISC-V VM.
# Enables mDNS multicast (UDP 224.0.0.251) to reach the host controller.
# Requires root / CAP_NET_ADMIN.
#
# Usage:
#   sudo ./setup-tap.sh up    -- create and configure tap interface
#   sudo ./setup-tap.sh down  -- remove tap interface
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "${SCRIPT_DIR}/../config.env"

case "${1:-up}" in
up)
    if ip link show "${TAP_IFACE}" &>/dev/null; then
        echo "TAP interface ${TAP_IFACE} already exists."
        exit 0
    fi
    ip tuntap add dev "${TAP_IFACE}" mode tap user "$(logname 2>/dev/null || echo "$SUDO_USER")"
    ip link set "${TAP_IFACE}" up
    ip addr add "${TAP_HOST_IP}/24" dev "${TAP_IFACE}"

    # Enable IP forwarding and NAT so the VM can reach the internet
    sysctl -q net.ipv4.ip_forward=1
    IFACE=$(ip route get 8.8.8.8 | awk '/dev/{print $5; exit}')
    iptables -t nat -A POSTROUTING -s "${TAP_VM_IP}/24" -o "${IFACE}" -j MASQUERADE
    iptables -A FORWARD -i "${TAP_IFACE}" -j ACCEPT
    iptables -A FORWARD -o "${TAP_IFACE}" -j ACCEPT

    echo "TAP interface ${TAP_IFACE} up: host=${TAP_HOST_IP}, vm=${TAP_VM_IP}"
    echo "mDNS multicast will flow between VM and host controller."
    ;;
down)
    iptables -t nat -D POSTROUTING -s "${TAP_VM_IP}/24" -j MASQUERADE 2>/dev/null || true
    iptables -D FORWARD -i "${TAP_IFACE}" -j ACCEPT 2>/dev/null || true
    iptables -D FORWARD -o "${TAP_IFACE}" -j ACCEPT 2>/dev/null || true
    ip link del "${TAP_IFACE}" 2>/dev/null || true
    echo "TAP interface ${TAP_IFACE} removed."
    ;;
*)
    echo "Usage: $0 {up|down}"
    exit 1
    ;;
esac
