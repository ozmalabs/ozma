#!/usr/bin/env bash
# demo/run_demo.sh — Full Ozma demo: Controller + two nodes + QEMU VMs.
#
# What this starts:
#   1. Two QEMU VMs (vm1, vm2) via demo/start_vms.sh
#   2. Two Rust nodes (`ozma-node --profile vm`) pointing at those QMP sockets
#   3. The Ozma Controller daemon (still Python; the controller is the
#      remaining Python component)
#
# Layout:
#   Controller         → REST/WS on http://localhost:7380
#   Node vm1           → UDP 7332, HTTP 7382, QMP /tmp/ozma-vm1.qmp
#   Node vm2           → UDP 7333, HTTP 7383, QMP /tmp/ozma-vm2.qmp
#
# Usage:
#   bash demo/run_demo.sh           # Rust ozma-node (default)
#   bash demo/run_demo.sh --python  # Python soft nodes (one-release rollback)
#   OZMA_DEMO_BACKEND=python bash demo/run_demo.sh
#   bash demo/run_demo.sh --webrtc  # Rust nodes + GStreamer WebRTC helper
#   bash demo/run_demo.sh stop      # stop everything
#   bash demo/run_demo.sh status    # check process status
#   bash demo/run_demo.sh --help    # this message (no services started)
#
# The Rust path uses the same VM/QMP sockets, mDNS node names, and controller.
# Set OZMA_NODE_BIN to point at a specific ozma-node binary; otherwise the
# script looks in target/release, target/debug, then $PATH.
# WebRTC stays off by default; --webrtc passes the canonical helper at
# scripts/webrtc/gst_webrtc_worker.py and requires Python 3 + GStreamer.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

usage() {
    sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//'
}

# ---------------------------------------------------------------------------
# Node backend: Rust is the default. --python (or OZMA_DEMO_BACKEND=python)
# keeps the one-release Python rollback path. The variable is exported so the
# "test" recursion below keeps the selection.
DEMO_BACKEND="${OZMA_DEMO_BACKEND:-rust}"
case "${OZMA_DEMO_RUST:-0}" in
    1|true|yes|on) DEMO_BACKEND=rust ;;
esac
case "${OZMA_DEMO_PYTHON:-0}" in
    1|true|yes|on) DEMO_BACKEND=python ;;
esac
DEMO_WEBRTC="${OZMA_DEMO_WEBRTC:-0}"

ARGS=()
for arg in "$@"; do
    case "$arg" in
        --rust) DEMO_BACKEND=rust ;;
        --python) DEMO_BACKEND=python ;;
        --webrtc) DEMO_WEBRTC=1 ;;
        -h|--help) usage; exit 0 ;;
        *) ARGS+=("$arg") ;;
    esac
done
set -- "${ARGS[@]}"
case "$DEMO_BACKEND" in
    rust|python) ;;
    *)
        echo "ERROR: OZMA_DEMO_BACKEND must be 'rust' or 'python' (got '$DEMO_BACKEND')" >&2
        exit 1
        ;;
esac
case "$DEMO_WEBRTC" in
    0|1) ;;
    *)
        echo "ERROR: OZMA_DEMO_WEBRTC must be 0 or 1 (got '$DEMO_WEBRTC')" >&2
        exit 1
        ;;
esac
export OZMA_DEMO_BACKEND="$DEMO_BACKEND"

RUST_NODE_BIN="${OZMA_NODE_BIN:-}"
if [[ "$DEMO_BACKEND" == "rust" && -z "$RUST_NODE_BIN" ]]; then
    for candidate in "$REPO_ROOT/target/release/ozma-node" "$REPO_ROOT/target/debug/ozma-node"; do
        if [[ -x "$candidate" ]]; then
            RUST_NODE_BIN="$candidate"
            break
        fi
    done
    [[ -z "$RUST_NODE_BIN" ]] && RUST_NODE_BIN="$(command -v ozma-node 2>/dev/null || true)"
fi

WEBRTC_SCRIPT="${OZMA_WEBRTC_HELPER_SCRIPT:-$REPO_ROOT/scripts/webrtc/gst_webrtc_worker.py}"
if [[ "$DEMO_WEBRTC" == "1" ]]; then
    if [[ "$DEMO_BACKEND" != "rust" ]]; then
        echo "ERROR: --webrtc is only wired for the Rust backend; drop --python." >&2
        exit 1
    fi
    if [[ ! -f "$WEBRTC_SCRIPT" ]]; then
        echo "ERROR: WebRTC helper not found at $WEBRTC_SCRIPT" >&2
        echo "Set OZMA_WEBRTC_HELPER_SCRIPT or install it with scripts/install-rust.sh." >&2
        exit 1
    fi
fi
LOG_DIR="$SCRIPT_DIR/logs"
SCENARIOS_SRC="$SCRIPT_DIR/scenarios.json"
SCENARIOS_DEST="$REPO_ROOT/controller/scenarios.json"

PID_CTRL="/tmp/ozma-ctrl.pid"
PID_SN1="/tmp/ozma-sn1.pid"
PID_SN2="/tmp/ozma-sn2.pid"
LOG_CTRL="$LOG_DIR/controller.log"
LOG_SN1="$LOG_DIR/node-vm1.log"
LOG_SN2="$LOG_DIR/node-vm2.log"

# ---------------------------------------------------------------------------
_stop_processes() {
    echo "Stopping Ozma demo..."
    for pidfile in "$PID_CTRL" "$PID_SN1" "$PID_SN2"; do
        if [[ -f "$pidfile" ]]; then
            pid=$(cat "$pidfile")
            if kill -0 "$pid" 2>/dev/null; then
                kill "$pid" && echo "  killed PID $pid ($(basename "$pidfile" .pid))"
            fi
            rm -f "$pidfile"
        fi
    done
    bash "$SCRIPT_DIR/start_vms.sh" stop 2>/dev/null || true
    echo "Done."
}

# start_vm_node <name> <hid-port> <qmp> <vnc-port> <audio-sink> <log> <pidfile>
start_vm_node() {
    local name="$1" port="$2" qmp="$3" vnc_port="$4" sink="$5" log="$6" pidfile="$7"

    if [[ "$DEMO_BACKEND" == "rust" ]]; then
        if [[ -z "$RUST_NODE_BIN" || ! -x "$RUST_NODE_BIN" ]]; then
            echo "ERROR: --rust needs an ozma-node binary." >&2
            echo "Build it (cargo build -p ozma-node) or set OZMA_NODE_BIN=/path/to/ozma-node." >&2
            exit 1
        fi
        local http_port=$((port + 50))
        local hls_dir="$LOG_DIR/hls-$name"
        local audio_args=()
        local webrtc_args=()
        mkdir -p "$hls_dir"
        if command -v pactl >/dev/null 2>&1; then
            audio_args=(--audio-router pipewire --audio-capture-target "monitor:$sink" --audio-sink "$sink")
        fi
        if [[ "$DEMO_WEBRTC" == "1" ]]; then
            # QMP capture geometry is only known at runtime; fit the helper to
            # the real head instead of failing at the 640x480 default. The guest
            # can sit in a 720x400 text mode for ~3 s, so wait past it.
            webrtc_args=(--webrtc-helper python3 --webrtc-helper-script "$WEBRTC_SCRIPT" \
                --webrtc-fit-capture --webrtc-settle-ms 4000)
        fi
        "$RUST_NODE_BIN" \
            --name "$name" \
            --profile vm \
            --qmp-socket "$qmp" \
            --hid-udp-port "$port" \
            --http-port "$http_port" \
            --capture-provider qmp \
            --input-provider qmp \
            --hls --hls-dir "$hls_dir" --hls-fps 15 \
            "${audio_args[@]}" \
            "${webrtc_args[@]}" \
            >"$log" 2>&1 &
    else
        "$PYTHON" "$REPO_ROOT/softnode/soft_node.py" \
            --name "$name" --port "$port" --qmp "$qmp" \
            --vnc-host 127.0.0.1 --vnc-port "$vnc_port" \
            --audio-sink "$sink" \
            >"$log" 2>&1 &
    fi
    echo $! > "$pidfile"
    echo "  node $name  backend=$DEMO_BACKEND  PID=$(cat "$pidfile")  log: $log"
}

stop_all() {
    _stop_processes
    exit 0
}

status_all() {
    echo "Ozma demo status:"
    echo "  backend  $DEMO_BACKEND"
    for name_pid in "controller:$PID_CTRL" "node-vm1:$PID_SN1" "node-vm2:$PID_SN2"; do
        name="${name_pid%%:*}"
        pidfile="${name_pid##*:}"
        if [[ -f "$pidfile" ]]; then
            pid=$(cat "$pidfile")
            if kill -0 "$pid" 2>/dev/null; then
                echo "  $name  PID=$pid  [running]"
            else
                echo "  $name  PID=$pid  [dead — stale pidfile]"
            fi
        else
            echo "  $name  [not running]"
        fi
    done
    echo ""
    echo "QMP sockets:"
    for qmp in /tmp/ozma-vm1.qmp /tmp/ozma-vm2.qmp; do
        [[ -S "$qmp" ]] && echo "  $qmp  [ready]" || echo "  $qmp  [missing]"
    done
    echo ""
    echo "Controller API:  http://localhost:7380/api/v1/status"
    exit 0
}

run_tests() {
    echo ""
    echo "============================================================"
    echo "  Running Full Test Suite"
    echo "============================================================"
    echo ""
    local failed=0

    # Core switching + audio tests
    echo "--- Core: HID switching + audio routing ---"
    "$PYTHON" "$REPO_ROOT/tests/test_e2e_switching.py" || failed=1

    # Extended feature tests
    echo ""
    echo "--- Extended: API + features ---"
    "$PYTHON" "$REPO_ROOT/tests/test_e2e_features.py" || failed=1

    echo ""
    echo "============================================================"
    if [[ $failed -eq 0 ]]; then
        echo "  ALL TESTS PASSED"
    else
        echo "  SOME TESTS FAILED"
    fi
    echo "============================================================"
    return $failed
}

[[ "${1:-}" == "stop" ]]   && stop_all
[[ "${1:-}" == "status" ]] && status_all

# "test" mode: start everything, run tests, stop everything, exit with test result
if [[ "${1:-}" == "test" ]]; then
    shift
    # The parent process needs the interpreter for run_tests after the child
    # stack starts; resolve the same venv the child setup below uses.
    PYTHON="${PYTHON:-python3}"
    if [[ -f "$REPO_ROOT/.venv/bin/python" ]]; then
        PYTHON="$REPO_ROOT/.venv/bin/python"
    fi
    # Start the stack. `set -e` would otherwise skip cleanup when the stack or
    # the tests fail, leaving nodes, QEMU, and the controller running.
    result=0
    bash "$0" "$@" || result=$?
    if [[ $result -ne 0 ]]; then
        echo "ERROR: demo stack failed to start (exit $result); stopping what started." >&2
        _stop_processes
        exit $result
    fi
    sleep 3  # extra settle time for all services to stabilise
    run_tests || result=$?
    _stop_processes
    exit $result
fi

# ---------------------------------------------------------------------------
# Setup
mkdir -p "$LOG_DIR"

# Copy demo scenarios into the controller directory
cp "$SCENARIOS_SRC" "$SCENARIOS_DEST"
echo "Installed $SCENARIOS_DEST"

# Check Python environment — use venv if available
PYTHON="${PYTHON:-python3}"
if [[ -f "$REPO_ROOT/.venv/bin/python" ]]; then
    PYTHON="$REPO_ROOT/.venv/bin/python"
    echo "Using venv: $PYTHON"
fi
if ! "$PYTHON" -c "import fastapi, uvicorn, zeroconf" 2>/dev/null; then
    echo "ERROR: Missing Python dependencies. Run:"
    echo "  python3 -m venv .venv && source .venv/bin/activate"
    echo "  uv pip install fastapi uvicorn zeroconf asyncvnc numpy pillow aiohttp mido pydantic websockets"
    exit 1
fi

# ---------------------------------------------------------------------------
# 1. Start VMs (skip if --no-vms flag)
if [[ "${1:-}" != "--no-vms" ]]; then
    bash "$SCRIPT_DIR/start_vms.sh"
fi

# ---------------------------------------------------------------------------
# 2. Start nodes (Python soft nodes or Rust ozma-node, selected above)
echo ""
echo "Starting nodes (backend: $DEMO_BACKEND)..."

start_vm_node vm1 7332 /tmp/ozma-vm1.qmp 5901 ozma-vm1 "$LOG_SN1" "$PID_SN1"
start_vm_node vm2 7333 /tmp/ozma-vm2.qmp 5922 ozma-vm2 "$LOG_SN2" "$PID_SN2"

# ---------------------------------------------------------------------------
# 3. Start Controller (virtual-only mode: capture ozma-virtual-* devices only)
echo ""
echo "Starting Controller..."

"$PYTHON" "$REPO_ROOT/controller/main.py" \
    --virtual-only \
    >"$LOG_CTRL" 2>&1 &
echo $! > "$PID_CTRL"
echo "  controller     PID=$(cat "$PID_CTRL")  log: $LOG_CTRL"

# ---------------------------------------------------------------------------
# Wait for controller to come up
echo ""
echo "Waiting for Controller API..."
elapsed=0
until curl -sf http://localhost:7380/api/v1/status >/dev/null 2>&1; do
    sleep 0.5
    elapsed=$((elapsed + 1))
    if [[ $elapsed -ge 20 ]]; then
        echo "ERROR: Controller did not start within 10s. Check $LOG_CTRL"
        exit 1
    fi
done
echo "Controller ready."

# ---------------------------------------------------------------------------
echo ""
echo "============================================================"
echo "  Ozma Demo Running"
echo "============================================================"
echo "  Backend:      $DEMO_BACKEND (default: rust)"
echo "  WebRTC:       $([[ "$DEMO_WEBRTC" == "1" ]] && echo "helper enabled" || echo "off (typed 503)")"
echo "  Web UI:       http://localhost:7380"
echo "  API status:   http://localhost:7380/api/v1/status"
echo "  Scenarios:    http://localhost:7380/api/v1/scenarios"
echo ""
echo "  VM 1 (Blue):  VNC vnc://127.0.0.1:5901"
echo "  VM 2 (Green): VNC vnc://127.0.0.1:5922"
echo ""
echo "  Switch scenarios:"
echo "    curl -X POST http://localhost:7380/api/v1/scenarios/vm1/activate"
echo "    curl -X POST http://localhost:7380/api/v1/scenarios/vm2/activate"
echo ""
echo "  Auto-switch demo:"
echo "    python demo/switch.py"
echo ""
echo "  Stop everything:"
echo "    bash demo/run_demo.sh stop"
echo ""
echo "  One-release Python rollback:"
echo "    bash demo/run_demo.sh --python"
echo "============================================================"
