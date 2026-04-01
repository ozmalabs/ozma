#!/usr/bin/env bash
# demo/run_demo.sh — Full Ozma demo: Controller + two Soft Nodes + QEMU VMs.
#
# What this starts:
#   1. Two QEMU VMs (vm1, vm2) via demo/start_vms.sh
#   2. Two Soft Nodes pointing at those QMP sockets
#   3. The Ozma Controller daemon
#
# Layout:
#   Controller         → REST/WS on http://localhost:7380
#   Soft Node vm1      → UDP 7332, QMP /tmp/ozma-vm1.qmp
#   Soft Node vm2      → UDP 7333, QMP /tmp/ozma-vm2.qmp
#
# Usage:
#   bash demo/run_demo.sh           # start everything
#   bash demo/run_demo.sh stop      # stop everything
#   bash demo/run_demo.sh status    # check process status

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOG_DIR="$SCRIPT_DIR/logs"
SCENARIOS_SRC="$SCRIPT_DIR/scenarios.json"
SCENARIOS_DEST="$REPO_ROOT/controller/scenarios.json"

PID_CTRL="/tmp/ozma-ctrl.pid"
PID_SN1="/tmp/ozma-sn1.pid"
PID_SN2="/tmp/ozma-sn2.pid"
LOG_CTRL="$LOG_DIR/controller.log"
LOG_SN1="$LOG_DIR/softnode-vm1.log"
LOG_SN2="$LOG_DIR/softnode-vm2.log"

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

stop_all() {
    _stop_processes
    exit 0
}

status_all() {
    echo "Ozma demo status:"
    for name_pid in "controller:$PID_CTRL" "softnode-vm1:$PID_SN1" "softnode-vm2:$PID_SN2"; do
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
    # Start the stack
    bash "$0" "$@"
    sleep 3  # extra settle time for all services to stabilise
    run_tests
    result=$?
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
    echo "  pip install fastapi uvicorn zeroconf asyncvnc numpy pillow aiohttp mido pydantic websockets"
    exit 1
fi

# ---------------------------------------------------------------------------
# 1. Start VMs (skip if --no-vms flag)
if [[ "${1:-}" != "--no-vms" ]]; then
    bash "$SCRIPT_DIR/start_vms.sh"
fi

# ---------------------------------------------------------------------------
# 2. Start Soft Nodes
echo ""
echo "Starting Soft Nodes..."

"$PYTHON" "$REPO_ROOT/softnode/soft_node.py" \
    --name vm1 --port 7332 --qmp /tmp/ozma-vm1.qmp \
    --vnc-host 127.0.0.1 --vnc-port 5901 \
    --audio-sink ozma-vm1 \
    >"$LOG_SN1" 2>&1 &
echo $! > "$PID_SN1"
echo "  soft-node vm1  PID=$(cat "$PID_SN1")  log: $LOG_SN1"

"$PYTHON" "$REPO_ROOT/softnode/soft_node.py" \
    --name vm2 --port 7333 --qmp /tmp/ozma-vm2.qmp \
    --vnc-host 127.0.0.1 --vnc-port 5922 \
    --audio-sink ozma-vm2 \
    >"$LOG_SN2" 2>&1 &
echo $! > "$PID_SN2"
echo "  soft-node vm2  PID=$(cat "$PID_SN2")  log: $LOG_SN2"

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
echo "============================================================"
