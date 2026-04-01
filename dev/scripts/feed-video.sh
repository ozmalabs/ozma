#!/bin/bash
# Feed a test video + audio pattern into the v4l2loopback device in the
# RISC-V node VM, simulating HDMI input from the target machine.
#
# By default: ffmpeg testsrc2 (colour bars + moving elements) at 1080p/30.
# Pass a video file path as argument to feed a real video instead:
#   make feed-video VIDEO=/path/to/capture.mp4
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "${SCRIPT_DIR}/../config.env"

VIDEO_SRC="${VIDEO:-}"

ssh_node() {
    ssh ${SSH_OPTS} -i "${SSH_KEY}" -p "${NODE_SSH_PORT}" \
        "${NODE_USER}@localhost" "$@"
}

echo "=== Checking v4l2loopback device in node VM ==="
if ! ssh_node "test -e /dev/video10"; then
    echo "  /dev/video10 not found — loading v4l2loopback..."
    ssh_node "sudo modprobe v4l2loopback video_nr=10 exclusive_caps=1 card_label='HDMI Capture'"
fi

echo "=== Stopping any existing feed ==="
ssh_node "pkill -f 'ffmpeg.*video10' 2>/dev/null; sleep 0.3" || true

if [[ -n "${VIDEO_SRC}" ]]; then
    echo "=== Uploading video file to node VM ==="
    scp ${SSH_OPTS} -i "${SSH_KEY}" -P "${NODE_SSH_PORT}" \
        "${VIDEO_SRC}" "${NODE_USER}@localhost:/tmp/feed-video.mp4"

    echo "=== Starting video feed from file ==="
    ssh_node "nohup ffmpeg -loglevel error -re \
        -i /tmp/feed-video.mp4 \
        -loop 1 \
        -vf scale=1920:1080 \
        -f v4l2 /dev/video10 \
        >> /var/log/ozma-ffmpeg.log 2>&1 &"
    echo "  Feeding: ${VIDEO_SRC} → /dev/video10"
else
    echo "=== Starting test pattern feed ==="
    ssh_node "nohup ffmpeg -loglevel error -re \
        -f lavfi -i 'testsrc2=size=1920x1080:rate=30' \
        -f v4l2 /dev/video10 \
        >> /var/log/ozma-ffmpeg.log 2>&1 &"
    echo "  Feeding: testsrc2 (1080p/30) → /dev/video10"
    echo "  To use a real video: make feed-video VIDEO=/path/to/file.mp4"
fi

echo ""
echo "=== Verifying feed started ==="
sleep 1
ssh_node "v4l2-ctl -d /dev/video10 --info 2>/dev/null | grep -i 'width\|height\|format'" || true
