#!/usr/bin/env bash
# demo/start_vms.sh — Start two QEMU VMs for the Ozma soft-node demo.
#
# Each VM gets:
#   - A QMP Unix socket  (/tmp/ozma-vm1.qmp, /tmp/ozma-vm2.qmp)
#   - A VNC display      (127.0.0.1:5901, 127.0.0.1:5902)
#   - A USB tablet for absolute mouse coordinates
#   - Serial output to a log file (demo/logs/vm{1,2}.log)
#   - 256 MB RAM, 1 vCPU — minimal footprint
#
# The VMs boot from the Alpine Linux "virtual" ISO (downloaded once into
# demo/cache/). No persistent disk is required; the ISO boots to a live shell.
#
# Usage:
#   bash demo/start_vms.sh          # start both VMs in background
#   bash demo/start_vms.sh stop     # kill them (reads PIDs from /tmp)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE_DIR="$SCRIPT_DIR/cache"
LOG_DIR="$SCRIPT_DIR/logs"
ALPINE_VERSION="3.21.3"
ALPINE_ARCH="x86_64"
ALPINE_ISO="alpine-virt-${ALPINE_VERSION}-${ALPINE_ARCH}.iso"
ALPINE_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION%.*}/releases/${ALPINE_ARCH}/${ALPINE_ISO}"

QMP1="/tmp/ozma-vm1.qmp"
QMP2="/tmp/ozma-vm2.qmp"
PID1="/tmp/ozma-vm1.pid"
PID2="/tmp/ozma-vm2.pid"

# PipeWire null sink names (AudioRouter uses these for per-VM audio routing)
SINK_VM1="ozma-vm1"
SINK_VM2="ozma-vm2"

# ---------------------------------------------------------------------------
stop_vms() {
    echo "Stopping VMs..."
    for pidfile in "$PID1" "$PID2"; do
        if [[ -f "$pidfile" ]]; then
            pid=$(cat "$pidfile")
            if kill -0 "$pid" 2>/dev/null; then
                kill "$pid" && echo "  killed PID $pid"
            fi
            rm -f "$pidfile"
        fi
    done
    rm -f "$QMP1" "$QMP2"
    echo "Done."
    exit 0
}

[[ "${1:-}" == "stop" ]] && stop_vms

# ---------------------------------------------------------------------------
# PipeWire/PulseAudio null sinks for per-VM audio routing
# Each sink acts as a virtual output device for the QEMU VM.
# The sink monitor source is then routed to the operator output by AudioRouter.
setup_audio_sinks() {
    if ! command -v pactl &>/dev/null; then
        echo "Warning: pactl not found — audio routing will not be available."
        return
    fi
    for sink in "$SINK_VM1" "$SINK_VM2"; do
        # Idempotent: if the sink already exists, pactl prints its index anyway
        if pactl list sinks short 2>/dev/null | grep -q "^[0-9]*[[:space:]]*${sink}[[:space:]]"; then
            echo "  Audio sink '$sink' already exists."
        else
            pactl load-module module-null-sink \
                sink_name="$sink" \
                "sink_properties=device.description=Ozma-${sink}" \
                &>/dev/null && echo "  Created audio sink: $sink" \
                            || echo "  Warning: failed to create audio sink '$sink'"
        fi
    done
}

# ---------------------------------------------------------------------------
# Pre-flight checks
if ! command -v qemu-system-x86_64 &>/dev/null; then
    echo "ERROR: qemu-system-x86_64 not found. Install QEMU:"
    echo "  Arch:   sudo pacman -S qemu-system-x86"
    echo "  Debian: sudo apt install qemu-system-x86"
    exit 1
fi

mkdir -p "$CACHE_DIR" "$LOG_DIR"

# Download Alpine ISO if not cached
ISO_PATH="$CACHE_DIR/$ALPINE_ISO"
if [[ ! -f "$ISO_PATH" ]]; then
    echo "Downloading Alpine Linux ${ALPINE_VERSION} ISO (~50 MB)..."
    if command -v curl &>/dev/null; then
        curl -L --progress-bar -o "$ISO_PATH" "$ALPINE_URL"
    else
        wget -O "$ISO_PATH" "$ALPINE_URL"
    fi
    echo "Cached at $ISO_PATH"
fi

# Kill any leftover instances from a previous run
for pidfile in "$PID1" "$PID2"; do
    if [[ -f "$pidfile" ]]; then
        pid=$(cat "$pidfile")
        kill -0 "$pid" 2>/dev/null && kill "$pid" 2>/dev/null || true
        rm -f "$pidfile"
    fi
done
rm -f "$QMP1" "$QMP2"

# ---------------------------------------------------------------------------
# Launch VM helper
# $1 = instance name (vm1 / vm2)
# $2 = QMP socket path
# $3 = VNC port offset (1 → :5901, 2 → :5902)
# $4 = pid file
# $5 = PipeWire audio sink name (optional)
start_vm() {
    local name="$1" qmp="$2" vnc_offset="$3" pidfile="$4" audio_sink="${5:-}"
    local logfile="$LOG_DIR/${name}.log"
    local vnc_addr="127.0.0.1:${vnc_offset}"   # VNC listens on :590N

    # Audio device args: route VM audio to the dedicated PW null sink
    local audio_args=()
    if [[ -n "$audio_sink" ]] && command -v pactl &>/dev/null; then
        audio_args=(
            -audiodev "pa,id=a0,out.name=${audio_sink},in.name=${audio_sink}-mic"
            -device intel-hda
            -device "hda-duplex,audiodev=a0"
        )
    fi

    echo "Starting $name (QMP: $qmp, VNC: vnc://$vnc_addr${audio_sink:+, audio: $audio_sink})..."

    # Use KVM if available, fall back to TCG (slower but works everywhere)
    local accel="kvm"
    if ! test -w /dev/kvm 2>/dev/null; then
        accel="tcg"
        echo "  Warning: KVM not available, using TCG (slower)"
    fi

    qemu-system-x86_64 \
        -name "$name" \
        -m 256M \
        -smp 1 \
        -accel "$accel" \
        -cdrom "$ISO_PATH" \
        -boot d \
        -serial "file:$logfile" \
        -monitor none \
        -qmp "unix:${qmp},server,nowait" \
        -device usb-ehci \
        -device usb-tablet \
        -vga std \
        -vnc "$vnc_addr" \
        -no-reboot \
        "${audio_args[@]}" \
        &>/dev/null &

    echo $! > "$pidfile"
    echo "  PID $(cat "$pidfile") — serial log: $logfile"
}

# ---------------------------------------------------------------------------
# V4L2 loopback for virtual display capture
setup_video_capture() {
    if ! modinfo v4l2loopback &>/dev/null; then
        echo "  v4l2loopback not available — virtual capture disabled."
        return
    fi
    # Check if our devices already exist
    if grep -rq "Ozma Virtual" /sys/class/video4linux/*/name 2>/dev/null; then
        echo "  Virtual capture devices already exist."
        return
    fi
    echo "  Creating v4l2loopback devices (may need sudo password)..."
    sudo modprobe -r v4l2loopback 2>/dev/null || true
    sudo modprobe v4l2loopback \
        video_nr=10,11 \
        card_label="Ozma_Virtual_vm1","Ozma_Virtual_vm2" \
        exclusive_caps=1,1 \
        2>/dev/null && {
            echo "  /dev/video10 → vm1 virtual capture"
            echo "  /dev/video11 → vm2 virtual capture"
        } || echo "  Warning: v4l2loopback setup failed (sudo required). Virtual capture disabled."
}

echo "Setting up virtual capture devices..."
setup_video_capture
echo ""
echo "Setting up audio sinks..."
setup_audio_sinks
echo ""

start_vm "vm1" "$QMP1" 1 "$PID1" "$SINK_VM1"
start_vm "vm2" "$QMP2" 22 "$PID2" "$SINK_VM2"  # :22 → 5922 (5902 conflicts with Xvnc)

echo ""
echo "VMs started. Waiting for QMP sockets to appear..."

# Wait up to 15 seconds for both QMP sockets
for qmp in "$QMP1" "$QMP2"; do
    elapsed=0
    while [[ ! -S "$qmp" && $elapsed -lt 15 ]]; do
        sleep 0.5
        elapsed=$((elapsed + 1))
    done
    if [[ -S "$qmp" ]]; then
        echo "  $qmp  [ready]"
    else
        echo "  $qmp  [WARNING: not yet available — QEMU may still be starting]"
    fi
done

echo ""
echo "VMs are running:"
echo "  vm1  QMP=$QMP1   VNC=vnc://127.0.0.1:5901"
echo "  vm2  QMP=$QMP2   VNC=vnc://127.0.0.1:5922"
echo ""
echo "To stop: bash demo/start_vms.sh stop"
