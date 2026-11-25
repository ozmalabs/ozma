#!/usr/bin/env sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Read-only Gate 2 preflight for a QEMU/libvirt VM and its helper tools.
#
# This script does not start, stop, suspend, define, or edit a VM. It only
# inspects the libvirt domain, QMP sockets, ffmpeg capabilities, the WebRTC
# helper, and (optionally) the controller.
#
# Required unless OZMA_GATE2_SKIP_LIBVIRT=1:
#   OZMA_QEMU_DOMAIN            libvirt domain name (falls back to OZMA_LIBVIRT_DOMAIN)
#
# Required unless OZMA_GATE2_SKIP_QMP=1:
#   OZMA_QMP_SOCKET             QEMU control QMP Unix socket
#
# Optional:
#   OZMA_QMP_INPUT_SOCKET       dedicated QMP input socket
#   OZMA_LIBVIRT_URI            libvirt connection URI (default: virsh default)
#   OZMA_VNC_HOST/OZMA_VNC_PORT optional VNC endpoint (both or neither)
#   OZMA_FFMPEG_BIN             ffmpeg executable (default: ffmpeg)
#   OZMA_HLS_DIR                HLS output directory (default: /tmp/ozma-gate2-hls)
#   OZMA_WEBRTC_HELPER          helper program (default: python3)
#   OZMA_WEBRTC_HELPER_SCRIPT   helper script (default: scripts/webrtc/gst_webrtc_worker.py,
#                               falling back to the legacy softnode path)
#   OZMA_CONTROLLER_URL         controller base URL, for an optional /health check
#   OZMA_GATE2_SKIP_LIBVIRT=1   allow a direct-QEMU setup with no libvirt domain
#   OZMA_GATE2_SKIP_QMP=1       skip the QMP checks (not a Gate 2 pass)
#   OZMA_GATE2_SKIP_WEBRTC=1    skip the WebRTC helper checks
#
# Exit status:
#   0  all required checks passed (warnings/skips are reported)
#   1  at least one required check failed
#   2  usage/configuration error

set -u

PROG=$(basename "$0")

DOMAIN=${OZMA_QEMU_DOMAIN:-${OZMA_LIBVIRT_DOMAIN:-}}
LIBVIRT_URI=${OZMA_LIBVIRT_URI:-}
QMP_SOCKET=${OZMA_QMP_SOCKET:-}
QMP_INPUT_SOCKET=${OZMA_QMP_INPUT_SOCKET:-}
VNC_HOST=${OZMA_VNC_HOST:-}
VNC_PORT=${OZMA_VNC_PORT:-}
FFMPEG_BIN=${OZMA_FFMPEG_BIN:-ffmpeg}
HLS_DIR=${OZMA_HLS_DIR:-/tmp/ozma-gate2-hls}
WEBRTC_HELPER=${OZMA_WEBRTC_HELPER:-python3}
WEBRTC_SCRIPT=${OZMA_WEBRTC_HELPER_SCRIPT:-}
CONTROLLER_URL=${OZMA_CONTROLLER_URL:-}
CONTROLLER_TOKEN=${OZMA_CONTROLLER_TOKEN:-}
SKIP_LIBVIRT=${OZMA_GATE2_SKIP_LIBVIRT:-0}
SKIP_QMP=${OZMA_GATE2_SKIP_QMP:-0}
SKIP_WEBRTC=${OZMA_GATE2_SKIP_WEBRTC:-0}

PASSES=0
FAILURES=0
WARNINGS=0
SKIPS=0

usage() {
    cat <<EOF
Usage: $PROG

Read-only preflight for the already-running VM used by Gate 2.

Environment:
  OZMA_QEMU_DOMAIN          libvirt domain name (or OZMA_LIBVIRT_DOMAIN)
  OZMA_QMP_SOCKET           QEMU control QMP Unix socket
  OZMA_QMP_INPUT_SOCKET     optional dedicated QMP input socket
  OZMA_LIBVIRT_URI          optional libvirt URI
  OZMA_VNC_HOST/VNC_PORT    optional VNC endpoint (both or neither)
  OZMA_FFMPEG_BIN           ffmpeg executable (default: ffmpeg)
  OZMA_HLS_DIR              HLS output directory (default: /tmp/ozma-gate2-hls)
  OZMA_WEBRTC_HELPER        helper program (default: python3)
  OZMA_WEBRTC_HELPER_SCRIPT helper script
  OZMA_CONTROLLER_URL       optional controller base URL
  OZMA_CONTROLLER_TOKEN     optional controller bearer token

The script never starts or modifies a VM. Use the remediation text to fix
prerequisites manually, then rerun it.
EOF
}

case "${1:-}" in
    -h|--help)
        usage
        exit 0
        ;;
esac

pass() {
    PASSES=$((PASSES + 1))
    printf 'PASS  %s\n' "$*"
}

fail() {
    FAILURES=$((FAILURES + 1))
    printf 'FAIL  %s\n' "$*"
}

warn() {
    WARNINGS=$((WARNINGS + 1))
    printf 'WARN  %s\n' "$*"
}

skip() {
    SKIPS=$((SKIPS + 1))
    printf 'SKIP  %s\n' "$*"
}

have() {
    command -v "$1" >/dev/null 2>&1
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)

printf 'Gate 2 VM preflight (read-only)\n'
printf 'Repository: %s\n' "$repo_root"
printf 'No VM, QMP socket, container, or controller is started or modified.\n\n'

# ---------------------------------------------------------------------------
# libvirt domain
# ---------------------------------------------------------------------------

if [ "$SKIP_LIBVIRT" = "1" ]; then
    skip "libvirt domain check disabled (OZMA_GATE2_SKIP_LIBVIRT=1)"
else
    if ! have virsh; then
        fail "virsh is not installed; install libvirt-clients or set OZMA_GATE2_SKIP_LIBVIRT=1 for a direct-QEMU VM"
    elif [ -z "$DOMAIN" ]; then
        fail "OZMA_QEMU_DOMAIN (or OZMA_LIBVIRT_DOMAIN) is not set; list domains with: virsh list --all"
    else
        if [ -n "$LIBVIRT_URI" ]; then
            virsh_output=$(virsh --connect "$LIBVIRT_URI" domstate "$DOMAIN" 2>&1)
            virsh_status=$?
        else
            virsh_output=$(virsh domstate "$DOMAIN" 2>&1)
            virsh_status=$?
        fi
        if [ "$virsh_status" -ne 0 ]; then
            fail "virsh could not read domain '$DOMAIN': $virsh_output (check the domain name and libvirt socket permissions)"
        else
            case "$virsh_output" in
                *running*|*paused*)
                    pass "libvirt domain '$DOMAIN' is $virsh_output"
                    ;;
                *)
                    fail "libvirt domain '$DOMAIN' is '$virsh_output'; start or resume it manually (this harness never does)"
                    ;;
            esac
        fi
    fi
fi

# ---------------------------------------------------------------------------
# QMP sockets
# ---------------------------------------------------------------------------

check_qmp_socket() {
    qmp_label=$1
    qmp_path=$2
    if [ -z "$qmp_path" ]; then
        fail "$qmp_label path is not set"
        return
    fi
    if [ ! -e "$qmp_path" ]; then
        fail "$qmp_label '$qmp_path' does not exist; check the QEMU -qmp argument and that the VM is running"
        return
    fi
    if [ ! -S "$qmp_path" ]; then
        fail "$qmp_label '$qmp_path' is not a Unix socket"
        return
    fi
    if ! have python3; then
        warn "$qmp_label '$qmp_path' exists, but python3 is unavailable so connectability was not tested"
        return
    fi
    if python3 - "$qmp_path" <<'PY'
import json
import socket
import sys

path = sys.argv[1]
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.settimeout(3.0)
try:
    sock.connect(path)
    data = b""
    while b"\n" not in data and len(data) < 65536:
        chunk = sock.recv(4096)
        if not chunk:
            break
        data += chunk
finally:
    sock.close()

if b"\n" not in data:
    print("no newline-delimited QMP greeting", file=sys.stderr)
    sys.exit(1)
try:
    greeting = json.loads(data.split(b"\n", 1)[0].decode("utf-8"))
except Exception as error:
    print("greeting is not JSON: %s" % error, file=sys.stderr)
    sys.exit(1)
if "QMP" not in greeting:
    print("greeting is not a QMP object: %r" % (greeting,), file=sys.stderr)
    sys.exit(1)
PY
    then
        pass "$qmp_label '$qmp_path' is connectable and sent a QMP greeting"
    else
        fail "$qmp_label '$qmp_path' exists but did not answer with a QMP greeting (permissions or a non-QMP socket)"
    fi
}

if [ "$SKIP_QMP" = "1" ]; then
    skip "QMP socket checks disabled (OZMA_GATE2_SKIP_QMP=1); this is not a Gate 2 pass"
else
    check_qmp_socket "QMP control socket" "$QMP_SOCKET"
    if [ -n "$QMP_INPUT_SOCKET" ]; then
        check_qmp_socket "QMP input socket" "$QMP_INPUT_SOCKET"
    elif [ -n "$QMP_SOCKET" ]; then
        skip "no dedicated QMP input socket; the node uses the control socket for input"
    fi
fi

# ---------------------------------------------------------------------------
# ffmpeg / HLS / VNC
# ---------------------------------------------------------------------------

VNC_REQUESTED=0
if [ -n "$VNC_HOST" ] || [ -n "$VNC_PORT" ]; then
    VNC_REQUESTED=1
    if [ -z "$VNC_HOST" ] || [ -z "$VNC_PORT" ]; then
        fail "OZMA_VNC_HOST and OZMA_VNC_PORT must be provided together"
    fi
fi

if ! have "$FFMPEG_BIN" && [ ! -x "$FFMPEG_BIN" ]; then
    fail "ffmpeg '$FFMPEG_BIN' is not executable; install ffmpeg or set OZMA_FFMPEG_BIN"
else
    if "$FFMPEG_BIN" -hide_banner -version >/dev/null 2>&1; then
        pass "ffmpeg '$FFMPEG_BIN' runs"
    else
        fail "ffmpeg '$FFMPEG_BIN' could not run"
    fi

    demuxers=$("$FFMPEG_BIN" -hide_banner -demuxers 2>/dev/null || true)
    if printf '%s\n' "$demuxers" | grep -Eq '^[[:space:]]*D[[:space:]]+vnc([[:space:]]|$)'; then
        pass "ffmpeg has the VNC demuxer"
    elif [ "$VNC_REQUESTED" = "1" ]; then
        fail "ffmpeg has no VNC demuxer; install an ffmpeg built with libvncclient/native VNC, or use the QMP capture path"
    else
        warn "ffmpeg has no VNC demuxer; VNC capture is unavailable (QMP capture can still work)"
    fi

    encoders=$("$FFMPEG_BIN" -hide_banner -encoders 2>/dev/null || true)
    if printf '%s\n' "$encoders" | grep -Eq '(^|[[:space:]])libx264([[:space:]]|$)'; then
        pass "ffmpeg has the libx264 encoder required by HLS"
    else
        fail "ffmpeg has no libx264 encoder; HLS cannot start"
    fi

    muxers=$("$FFMPEG_BIN" -hide_banner -muxers 2>/dev/null || true)
    if printf '%s\n' "$muxers" | grep -Eq '(^|[[:space:]])hls([[:space:]]|$)'; then
        pass "ffmpeg has the HLS muxer"
    else
        fail "ffmpeg has no HLS muxer; HLS cannot start"
    fi
fi

if [ -e "$HLS_DIR" ]; then
    if [ -d "$HLS_DIR" ] && [ -w "$HLS_DIR" ]; then
        pass "HLS output directory '$HLS_DIR' exists and is writable"
    else
        fail "HLS output path '$HLS_DIR' exists but is not a writable directory"
    fi
else
    hls_parent=$(dirname "$HLS_DIR")
    if [ -d "$hls_parent" ] && [ -w "$hls_parent" ]; then
        pass "HLS output parent '$hls_parent' is writable; the node will create '$HLS_DIR'"
    else
        fail "HLS output parent '$hls_parent' is missing or not writable; create it manually with the correct ownership"
    fi
fi

if [ "$VNC_REQUESTED" = "1" ] && [ -n "$VNC_HOST" ] && [ -n "$VNC_PORT" ]; then
    case "$VNC_PORT" in
        ''|*[!0-9]*)
            fail "OZMA_VNC_PORT must be numeric, got '$VNC_PORT'"
            ;;
        *)
            if [ "$VNC_PORT" -lt 1 ] || [ "$VNC_PORT" -gt 65535 ]; then
                fail "OZMA_VNC_PORT must be in 1..65535, got '$VNC_PORT'"
            elif have python3; then
                if python3 - "$VNC_HOST" "$VNC_PORT" <<'PY'
import socket
import sys

host = sys.argv[1]
port = int(sys.argv[2])
try:
    connection = socket.create_connection((host, port), timeout=3.0)
except OSError as error:
    print("TCP connect failed: %s" % error, file=sys.stderr)
    sys.exit(1)
else:
    connection.close()
PY
                then
                    pass "VNC endpoint $VNC_HOST:$VNC_PORT accepts TCP connections"
                else
                    fail "VNC endpoint $VNC_HOST:$VNC_PORT is not reachable; check the QEMU -vnc argument"
                fi
            else
                warn "python3 is unavailable; VNC endpoint $VNC_HOST:$VNC_PORT was not connect-tested"
            fi
            ;;
    esac
fi

# ---------------------------------------------------------------------------
# WebRTC helper
# ---------------------------------------------------------------------------

if [ "$SKIP_WEBRTC" = "1" ]; then
    skip "WebRTC helper checks disabled (OZMA_GATE2_SKIP_WEBRTC=1); this is not a Gate 2 pass"
else
    if [ -z "$WEBRTC_SCRIPT" ]; then
        # Canonical location first; the softnode path is a one-release fallback
        # for older checkouts that still have the legacy tree.
        if [ -f "$repo_root/scripts/webrtc/gst_webrtc_worker.py" ]; then
            WEBRTC_SCRIPT="$repo_root/scripts/webrtc/gst_webrtc_worker.py"
        else
            WEBRTC_SCRIPT="$repo_root/softnode/gst_webrtc_worker.py"
        fi
    fi

    case "$WEBRTC_HELPER" in
        */*)
            if [ -x "$WEBRTC_HELPER" ]; then
                pass "WebRTC helper program '$WEBRTC_HELPER' is executable"
            else
                fail "WebRTC helper program '$WEBRTC_HELPER' is missing or not executable"
            fi
            ;;
        *)
            if have "$WEBRTC_HELPER"; then
                pass "WebRTC helper program '$WEBRTC_HELPER' is on PATH"
            else
                fail "WebRTC helper program '$WEBRTC_HELPER' is not on PATH"
            fi
            ;;
    esac

    if [ ! -f "$WEBRTC_SCRIPT" ]; then
        fail "WebRTC helper script '$WEBRTC_SCRIPT' does not exist; set OZMA_WEBRTC_HELPER_SCRIPT"
    else
        pass "WebRTC helper script '$WEBRTC_SCRIPT' exists"
        case "$(basename "$WEBRTC_HELPER")" in
            python*)
                if "$WEBRTC_HELPER" - "$WEBRTC_SCRIPT" <<'PY'
import ast
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    ast.parse(handle.read(), filename=sys.argv[1])
PY
                then
                    pass "WebRTC helper script parses as Python"
                else
                    fail "WebRTC helper script is not valid Python: $WEBRTC_SCRIPT"
                fi

                if "$WEBRTC_HELPER" - <<'PY' >/dev/null 2>&1
import gi
gi.require_version("Gst", "1.0")
gi.require_version("GstWebRTC", "1.0")
gi.require_version("GstSdp", "1.0")
from gi.repository import Gst, GstWebRTC, GstSdp, GLib  # noqa: F401
PY
                then
                    pass "Python has GStreamer Gst/GstWebRTC/GstSdp bindings"
                else
                    fail "Python cannot import the GStreamer bindings; install python3-gi and gstreamer1.0-plugins-bad (webrtcbin)"
                fi
                ;;
            *)
                warn "WebRTC helper '$WEBRTC_HELPER' is not Python; skipped the Python/GStreamer import check"
                ;;
        esac
    fi

    if have gst-inspect-1.0; then
        for element in webrtcbin rtph264pay; do
            if gst-inspect-1.0 "$element" >/dev/null 2>&1; then
                pass "GStreamer element '$element' is available"
            else
                fail "GStreamer element '$element' is missing; install gstreamer1.0-plugins-bad"
            fi
        done
        if gst-inspect-1.0 x264enc >/dev/null 2>&1 || gst-inspect-1.0 nvh264enc >/dev/null 2>&1; then
            pass "GStreamer has an H.264 encoder (x264enc or nvh264enc)"
        else
            fail "GStreamer has neither x264enc nor nvh264enc; install gstreamer1.0-plugins-ugly or the NVIDIA plugins"
        fi
    else
        warn "gst-inspect-1.0 is unavailable; GStreamer elements were not inspected"
    fi
fi

# ---------------------------------------------------------------------------
# Optional controller
# ---------------------------------------------------------------------------

if [ -n "$CONTROLLER_URL" ] && have curl; then
    controller_base=$(printf '%s' "$CONTROLLER_URL" | sed 's:/*$::')
    if [ -n "$CONTROLLER_TOKEN" ]; then
        controller_code=$(curl --globoff -sS --max-time 5 --connect-timeout 3 \
            -o /tmp/ozma-gate2-controller-health.$$ -w '%{http_code}' \
            -H "Authorization: Bearer $CONTROLLER_TOKEN" \
            "$controller_base/health" 2>/dev/null) || true
    else
        controller_code=$(curl --globoff -sS --max-time 5 --connect-timeout 3 \
            -o /tmp/ozma-gate2-controller-health.$$ -w '%{http_code}' \
            "$controller_base/health" 2>/dev/null) || true
    fi
    if [ "$controller_code" = "200" ] && grep -q '"status"[[:space:]]*:[[:space:]]*"ok"' "/tmp/ozma-gate2-controller-health.$$" 2>/dev/null; then
        pass "controller '$controller_base' is reachable and healthy"
    else
        fail "controller '$controller_base' did not return a healthy /health response (${controller_code:-000})"
    fi
    rm -f "/tmp/ozma-gate2-controller-health.$$"
elif [ -n "$CONTROLLER_URL" ]; then
    warn "OZMA_CONTROLLER_URL is set but curl is unavailable; skipped the controller check"
else
    skip "controller check disabled (OZMA_CONTROLLER_URL is not set)"
fi

printf '\nSummary: %d passed, %d failed, %d warnings, %d skipped\n' \
    "$PASSES" "$FAILURES" "$WARNINGS" "$SKIPS"
if [ "$FAILURES" -ne 0 ]; then
    exit 1
fi
exit 0
