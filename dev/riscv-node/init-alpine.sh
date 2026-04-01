#!/bin/sh
# Per-boot initialisation for the RISC-V Alpine node VM.
# Runs inside the VM as root (via /etc/local.d/ozma.start → rc.local).
#
# Steps:
#   1. Load kernel modules (dummy_hcd, libcomposite, usb_f_hid, usb_f_uac2, usbip)
#   2. Set up USB composite gadget (HID + UAC2) on dummy_hcd via ConfigFS
#   3. Export the gadget via USB/IP so the host can attach it to a target VM
#   4. Feed a test video/audio pattern into v4l2loopback / snd-aloop (optional)
#   5. Start node.py (registers with controller via --register-url)
set -eu

OZMA_DIR="/root/ozma-node"
GADGET_SCRIPT="${OZMA_DIR}/gadget/setup_gadget.sh"
NODE_NAME="${HOSTNAME:-ozma-riscv}"
CONTROLLER_HOST="${CONTROLLER_HOST:-10.0.2.2}"   # QEMU SLIRP default gateway = host
CONTROLLER_PORT="${CONTROLLER_PORT:-7380}"

log() { echo "[ozma-init] $*"; }

# ── 1. Kernel modules ─────────────────────────────────────────────────────────
log "Loading kernel modules..."

modprobe libcomposite    2>/dev/null || log "  libcomposite: unavailable"
modprobe usb_f_hid       2>/dev/null || true
modprobe usb_f_uac2      2>/dev/null || true
modprobe dummy_hcd       2>/dev/null || { log "ERROR: dummy_hcd not available"; exit 1; }
modprobe v4l2loopback video_nr=10 exclusive_caps=1 card_label="HDMI Capture" 2>/dev/null \
    || log "  v4l2loopback: unavailable (video disabled)"
modprobe snd-aloop       2>/dev/null || log "  snd-aloop: unavailable (audio disabled)"
modprobe usbip_core      2>/dev/null || true
modprobe usbip_host      2>/dev/null || log "  usbip_host: unavailable (USB/IP export disabled)"

sleep 1   # let udev settle

# ── 2. USB gadget setup ───────────────────────────────────────────────────────
log "Setting up USB composite gadget on dummy_hcd..."

if [ -f "${GADGET_SCRIPT}" ]; then
    sh "${GADGET_SCRIPT}" "OZMADEV001" 2>&1 | sed 's/^/  /'
else
    log "WARNING: gadget script not found: ${GADGET_SCRIPT}"
fi

# ── 3. USB/IP export ──────────────────────────────────────────────────────────
log "Exporting USB gadget via USB/IP..."

if command -v usbipd >/dev/null 2>&1; then
    pkill -x usbipd 2>/dev/null || true
    sleep 0.3
    usbipd &
    sleep 1

    # Find our composite gadget (Linux Foundation 1d6b:0104)
    BUSID=$(usbip list -l 2>/dev/null \
        | grep '1d6b:0104' \
        | grep -oE '[0-9]+-[0-9.]+' \
        | head -1 || true)

    if [ -n "${BUSID}" ]; then
        usbip bind -b "${BUSID}" && log "Gadget exported: busid=${BUSID}"
        echo "${BUSID}" > /tmp/ozma-gadget-busid
    else
        log "WARNING: gadget USB device not found — retrying in background"
        ( sleep 3
          B=$(usbip list -l 2>/dev/null | grep '1d6b:0104' | grep -oE '[0-9]+-[0-9.]+' | head -1 || true)
          if [ -n "$B" ]; then
              usbip bind -b "$B" && echo "$B" > /tmp/ozma-gadget-busid
          fi
        ) &
    fi
else
    log "WARNING: usbipd not found — USB/IP export unavailable"
fi

# ── 4. Test video + audio pattern ─────────────────────────────────────────────
if [ -e /dev/video10 ] && command -v ffmpeg >/dev/null 2>&1; then
    log "Starting test video pattern on /dev/video10..."
    pkill -f "v4l2.*video10" 2>/dev/null || true
    ffmpeg -loglevel error \
        -re -f lavfi -i "testsrc2=size=1920x1080:rate=30" \
        -f lavfi -i "sine=frequency=440:sample_rate=48000" \
        -map 0:v -f v4l2 /dev/video10 \
        -map 1:a -f alsa hw:Loopback,0 \
        >> /var/log/ozma-ffmpeg.log 2>&1 &
fi

# ── 5. Start node.py ──────────────────────────────────────────────────────────
log "Starting node.py..."
pkill -f "python.*node.py" 2>/dev/null || true
sleep 0.3

REGISTER_URL="http://${CONTROLLER_HOST}:${CONTROLLER_PORT}"
cd "${OZMA_DIR}"

python3 node.py \
    --name "${NODE_NAME}" \
    --register-url "${REGISTER_URL}" \
    --register-host "localhost" \
    >> /var/log/ozma-node.log 2>&1 &

echo $! > /tmp/ozma-node.pid
log "node.py started (pid=$(cat /tmp/ozma-node.pid))"
log "Controller: ${REGISTER_URL}"
log "Logs: /var/log/ozma-node.log"
