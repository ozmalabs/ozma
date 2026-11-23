#!/usr/bin/env sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Gate 2 orchestration wrapper.
#
# Runs scripts/gate2/qemu-preflight.sh and scripts/gate2/check-node.sh against
# an already-running VM/node/controller, and writes a timestamped report under
# OZMA_GATE2_REPORT_DIR (default: /tmp/ozma-gate2).
#
# This script never starts QEMU, the node, the controller, ffmpeg, or the
# WebRTC helper. It never uses sudo and performs no destructive VM operation.
#
# Required:
#   OZMA_NODE_URL       e.g. http://127.0.0.1:7382
#   OZMA_NODE_ID        e.g. ozma-vm-gate2._ozma._udp.local.
#
# Optional (see the two scripts for the full list):
#   OZMA_CONTROLLER_URL, OZMA_QEMU_DOMAIN, OZMA_QMP_SOCKET,
#   OZMA_VNC_HOST, OZMA_VNC_PORT, OZMA_FFMPEG_BIN, OZMA_HLS_DIR,
#   OZMA_WEBRTC_HELPER, OZMA_WEBRTC_HELPER_SCRIPT, OZMA_API_TOKEN,
#   OZMA_CONTROLLER_TOKEN, OZMA_GATE2_REPORT_DIR.
#
# Exit status:
#   0  both scripts passed
#   1  one or both scripts failed
#   2  usage/configuration error

set -u

PROG=$(basename "$0")
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPORT_DIR=${OZMA_GATE2_REPORT_DIR:-/tmp/ozma-gate2}

PREFLIGHT="$SCRIPT_DIR/qemu-preflight.sh"
NODE_CHECK="$SCRIPT_DIR/check-node.sh"

usage() {
    cat <<EOF
Usage: $PROG

Runs the read-only VM preflight and the node/controller checks, then writes a
report to: \$OZMA_GATE2_REPORT_DIR (default: $REPORT_DIR)

Required:
  OZMA_NODE_URL        Node base URL (http://127.0.0.1:7382)
  OZMA_NODE_ID         Full controller id (name._ozma._udp.local.)

Common optional variables:
  OZMA_CONTROLLER_URL  Controller base URL (http://127.0.0.1:7380)
  OZMA_QEMU_DOMAIN     libvirt domain name
  OZMA_QMP_SOCKET      QEMU QMP Unix socket
  OZMA_VNC_HOST/PORT   optional VNC endpoint
  OZMA_FFMPEG_BIN      ffmpeg executable
  OZMA_HLS_DIR         HLS output directory
  OZMA_WEBRTC_HELPER   WebRTC helper program (default: python3)
  OZMA_WEBRTC_HELPER_SCRIPT
                       WebRTC helper script
  OZMA_API_TOKEN       node bearer token, if configured
  OZMA_CONTROLLER_TOKEN
                       controller bearer token, if configured

The report is a log, not a Gate 2 claim. Attach it to the matching parity row
together with the commands' raw output and UI/guest screenshots.
EOF
}

case "${1:-}" in
    -h|--help)
        usage
        exit 0
        ;;
esac

if [ -z "${OZMA_NODE_URL:-}" ] || [ -z "${OZMA_NODE_ID:-}" ]; then
    printf 'ERROR: OZMA_NODE_URL and OZMA_NODE_ID are required.\n\n' >&2
    usage >&2
    exit 2
fi

if [ ! -x "$PREFLIGHT" ]; then
    printf 'ERROR: preflight script is missing or not executable: %s\n' "$PREFLIGHT" >&2
    exit 2
fi
if [ ! -x "$NODE_CHECK" ]; then
    printf 'ERROR: node check script is missing or not executable: %s\n' "$NODE_CHECK" >&2
    exit 2
fi

mkdir -p "$REPORT_DIR" || {
    printf 'ERROR: could not create report directory: %s\n' "$REPORT_DIR" >&2
    exit 2
}

STAMP=$(date -u '+%Y%m%dT%H%M%SZ')
REPORT="$REPORT_DIR/gate2-$STAMP.log"
PREFLIGHT_LOG="$REPORT_DIR/.gate2-preflight-$STAMP.log"
NODE_LOG="$REPORT_DIR/.gate2-node-$STAMP.log"

if ! : > "$REPORT"; then
    printf 'ERROR: could not write report: %s\n' "$REPORT" >&2
    exit 2
fi

{
    printf '# Ozma Gate 2 live validation report\n'
    printf 'timestamp_utc=%s\n' "$STAMP"
    printf 'node_url=%s\n' "$OZMA_NODE_URL"
    printf 'node_id=%s\n' "$OZMA_NODE_ID"
    printf 'controller_url=%s\n' "${OZMA_CONTROLLER_URL:-<unset>}"
    printf 'qemu_domain=%s\n' "${OZMA_QEMU_DOMAIN:-${OZMA_LIBVIRT_DOMAIN:-<unset>}}"
    printf 'qmp_socket=%s\n' "${OZMA_QMP_SOCKET:-<unset>}"
    printf 'vnc_host=%s\n' "${OZMA_VNC_HOST:-<unset>}"
    printf 'vnc_port=%s\n' "${OZMA_VNC_PORT:-<unset>}"
    printf 'ffmpeg_bin=%s\n' "${OZMA_FFMPEG_BIN:-ffmpeg}"
    printf 'hls_dir=%s\n' "${OZMA_HLS_DIR:-/tmp/ozma-gate2-hls}"
    printf 'webrtc_helper=%s\n' "${OZMA_WEBRTC_HELPER:-python3}"
    printf 'webrtc_helper_script=%s\n' "${OZMA_WEBRTC_HELPER_SCRIPT:-<default>}"
    printf 'node_token=%s\n' "$([ -n "${OZMA_API_TOKEN:-}" ] && printf configured || printf not-configured)"
    printf 'controller_token=%s\n' "$([ -n "${OZMA_CONTROLLER_TOKEN:-}" ] && printf configured || printf not-configured)"
    printf 'note=This report is evidence for a manual Gate 2 run, not a pass by itself.\n'
    printf '\n'
} >> "$REPORT"

run_stage() {
    stage_name=$1
    stage_log=$2
    shift 2
    printf '== %s ==\n' "$stage_name"
    if "$@" >"$stage_log" 2>&1; then
        stage_status=0
    else
        stage_status=$?
    fi
    tee -a "$REPORT" < "$stage_log"
    printf '\n'
    return "$stage_status"
}

PREFLIGHT_STATUS=0
NODE_STATUS=0

if run_stage "VM preflight" "$PREFLIGHT_LOG" "$PREFLIGHT"; then
    PREFLIGHT_STATUS=0
else
    PREFLIGHT_STATUS=$?
fi

if run_stage "Node and controller checks" "$NODE_LOG" "$NODE_CHECK"; then
    NODE_STATUS=0
else
    NODE_STATUS=$?
fi

{
    printf '== Summary ==\n'
    printf 'preflight_exit=%s\n' "$PREFLIGHT_STATUS"
    printf 'node_check_exit=%s\n' "$NODE_STATUS"
    if [ "$PREFLIGHT_STATUS" -eq 0 ] && [ "$NODE_STATUS" -eq 0 ]; then
        printf 'result=PASS (harness checks only; attach artifacts before claiming Gate 2)\n'
    else
        printf 'result=FAIL\n'
    fi
    printf 'report=%s\n' "$REPORT"
} | tee -a "$REPORT"

rm -f "$PREFLIGHT_LOG" "$NODE_LOG"

if [ "$PREFLIGHT_STATUS" -ne 0 ] || [ "$NODE_STATUS" -ne 0 ]; then
    exit 1
fi
exit 0
