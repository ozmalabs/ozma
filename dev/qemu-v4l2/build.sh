#!/bin/bash
# Build the ozma QEMU→v4l2 display bridge
#
# This bridges QEMU's display to a v4l2loopback device.
# QEMU outputs to a VNC unix socket (no TCP, no overhead).
# This process reads the framebuffer and writes to /dev/videoN.
#
# Usage:
#   bash dev/qemu-v4l2/build.sh
#   ./dev/qemu-v4l2/ozma-qemu-v4l2 --vnc=/tmp/qemu-vnc.sock --device=/dev/video10
#
# Prerequisites:
#   sudo modprobe v4l2loopback devices=1 video_nr=10 card_label=OzmaVM

set -e
DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Building ozma-qemu-v4l2..."
gcc -O2 -Wall -o "$DIR/ozma-qemu-v4l2" "$DIR/qemu-display-v4l2.c"
echo "Built: $DIR/ozma-qemu-v4l2"
echo ""
echo "Usage:"
echo "  # Start QEMU with VNC on a unix socket:"
echo "  qemu-system-x86_64 ... -vnc unix:/tmp/qemu-vnc.sock"
echo ""
echo "  # Bridge to v4l2loopback:"
echo "  $DIR/ozma-qemu-v4l2 --vnc=/tmp/qemu-vnc.sock --device=/dev/video10"
echo ""
echo "  # The VM display appears at /dev/video10"
echo "  # ffmpeg, OBS, or ozma can capture it like a real HDMI card"
