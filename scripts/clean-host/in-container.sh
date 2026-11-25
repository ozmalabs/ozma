#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#
# Runs inside the clean-host container (or on a host when run.sh --host is
# used). Builds the Rust node binaries, runs a bounded or full test selection,
# verifies both binaries answer --help, and starts ozma-node with synthetic
# providers for a health-check smoke.
#
# Environment:
#   OZMA_CLEAN_HOST_FULL=1     run cargo test --workspace instead of the subset
#   OZMA_CLEAN_HOST_SMOKE=0    skip the binary start/health smoke
#   CARGO_TARGET_DIR           defaults to target/clean-host in the repo
#
# Exit status: 0 all selected checks passed; 1 one or more failed.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target/clean-host}"
export CARGO_TARGET_DIR="$TARGET_DIR"

FULL="${OZMA_CLEAN_HOST_FULL:-0}"
SMOKE="${OZMA_CLEAN_HOST_SMOKE:-1}"

PASSES=0
FAILURES=0
SKIPS=0

pass() { PASSES=$((PASSES + 1)); printf 'PASS  %s\n' "$*"; }
fail() { FAILURES=$((FAILURES + 1)); printf 'FAIL  %s\n' "$*" >&2; }
skip() { SKIPS=$((SKIPS + 1)); printf 'SKIP  %s\n' "$*"; }
section() { printf '\n== %s ==\n' "$*"; }

run_step() {
    description=$1
    shift
    section "$description"
    if "$@"; then
        pass "$description"
    else
        fail "$description"
    fi
}

# ---------------------------------------------------------------------------
section "Environment"
printf 'repo:       %s\n' "$REPO_ROOT"
printf 'target:     %s\n' "$TARGET_DIR"
printf 'full tests: %s\n' "$FULL"
printf 'smoke:      %s\n' "$SMOKE"
printf 'os:         %s\n' "$(uname -srm)"
command -v cargo >/dev/null 2>&1 && printf 'cargo:      %s\n' "$(cargo --version)"
command -v rustc >/dev/null 2>&1 && printf 'rustc:      %s\n' "$(rustc --version)"

# ---------------------------------------------------------------------------
if "$SCRIPT_DIR/check-deps.sh" --container; then
    pass "native build dependencies"
else
    fail "native build dependencies"
    echo "Cannot continue without the required build dependencies." >&2
    exit 1
fi

# ---------------------------------------------------------------------------
run_step "cargo build -p ozma-node -p ozma-softnode -p ozma-multiseat --bins" \
    cargo build --bins -p ozma-node -p ozma-softnode -p ozma-multiseat

NODE_BIN="$TARGET_DIR/debug/ozma-node"
VNM_BIN="$TARGET_DIR/debug/ozma-virtual-node"
MULTISEAT_BIN="$TARGET_DIR/debug/ozma-multiseat"
MEETING_BIN="$TARGET_DIR/debug/ozma-meeting-detect"

if [ ! -x "$NODE_BIN" ]; then
    fail "expected binary $NODE_BIN"
fi
if [ ! -x "$VNM_BIN" ]; then
    fail "expected binary $VNM_BIN"
fi
if [ ! -x "$MULTISEAT_BIN" ]; then
    fail "expected binary $MULTISEAT_BIN"
fi
if [ ! -x "$MEETING_BIN" ]; then
    fail "expected binary $MEETING_BIN"
fi

if [ "$FAILURES" -ne 0 ]; then
    section "Summary"
    printf '%d passed, %d failed, %d skipped\n' "$PASSES" "$FAILURES" "$SKIPS"
    exit 1
fi

# ---------------------------------------------------------------------------
section "Binary --help smoke"
for pair in "$NODE_BIN:--profile" "$VNM_BIN:--node-bin" "$MULTISEAT_BIN:--seats-file"; do
    bin="${pair%%:*}"
    expected="${pair#*:}"
    if ! output=$("$bin" --help 2>&1); then
        fail "$(basename "$bin") --help exited non-zero"
        printf '%s\n' "$output" | tail -n 20 >&2
        continue
    fi
    if printf '%s' "$output" | grep -Fq -- "$expected"; then
        pass "$(basename "$bin") --help (contains $expected)"
    else
        fail "$(basename "$bin") --help does not contain $expected"
    fi
done

if "$NODE_BIN" --version >/dev/null 2>&1; then
    pass "ozma-node --version ($("$NODE_BIN" --version))"
else
    fail "ozma-node --version"
fi
# ozma-virtual-node currently declares no clap version; --help is the
# supported startup smoke for that binary.

# ---------------------------------------------------------------------------
section "Shell and unit-file syntax"
for script in \
    scripts/clean-host/run.sh \
    scripts/clean-host/in-container.sh \
    scripts/clean-host/check-deps.sh \
    scripts/install-rust.sh \
    scripts/install-system.sh \
    scripts/verify-system-install.sh \
    scripts/systemd/verify-units.sh \
    demo/run_demo.sh; do
    if bash -n "$script" 2>/dev/null; then
        pass "bash -n $script"
    else
        fail "bash -n $script"
    fi
done
if sh -n scripts/clean-host/check-deps.sh 2>/dev/null; then
    pass "sh -n scripts/clean-host/check-deps.sh"
else
    fail "sh -n scripts/clean-host/check-deps.sh"
fi

if command -v systemd-analyze >/dev/null 2>&1; then
    if scripts/systemd/verify-units.sh; then
        pass "systemd-analyze verify"
    else
        fail "systemd-analyze verify"
    fi
else
    skip "systemd-analyze is not installed in the test image"
fi

if scripts/install-rust.sh --dry-run --from-target "$TARGET_DIR/debug" --prefix "$TARGET_DIR/install-dry-run" >"$TARGET_DIR/install-dry-run.log" 2>&1; then
    if [ -e "$TARGET_DIR/install-dry-run" ]; then
        fail "install-rust.sh --dry-run wrote files"
    elif ! grep -q 'DRY   write .*ozma-multiseat\.service' "$TARGET_DIR/install-dry-run.log" ||
         ! grep -q 'DRY   install .*ozma-meeting-detect' "$TARGET_DIR/install-dry-run.log" ||
         ! grep -q 'ozma-multiseat\.env\.example' "$TARGET_DIR/install-dry-run.log"; then
        fail "install-rust.sh --dry-run omitted the multiseat or meeting-detect assets"
    else
        pass "install-rust.sh --dry-run wrote nothing and covered multiseat + meeting-detect"
    fi
else
    fail "install-rust.sh --dry-run"
fi

if scripts/install-rust.sh --dry-run --no-multiseat --no-meeting-detect \
        --from-target "$TARGET_DIR/debug" --prefix "$TARGET_DIR/install-dry-run-optout" \
        >"$TARGET_DIR/install-dry-run-optout.log" 2>&1; then
    if [ -e "$TARGET_DIR/install-dry-run-optout" ]; then
        fail "install-rust.sh --dry-run opt-out wrote files"
    elif grep -q 'DRY   write .*ozma-multiseat\.service' "$TARGET_DIR/install-dry-run-optout.log" ||
         grep -q 'DRY   install .*ozma-meeting-detect' "$TARGET_DIR/install-dry-run-optout.log"; then
        fail "install-rust.sh opt-out still planned the new assets"
    else
        pass "install-rust.sh --dry-run --no-multiseat --no-meeting-detect wrote nothing and skipped them"
    fi
else
    fail "install-rust.sh --dry-run opt-out"
fi

# Rootless temp-prefix check for the system user/udev/AppArmor/libvirt surface.
if system_verify_output=$(scripts/verify-system-install.sh 2>&1); then
    pass "system install surface (rootless temp-prefix)"
else
    fail "system install surface (rootless temp-prefix)"
    printf '%s\n' "$system_verify_output" >&2
fi

# ---------------------------------------------------------------------------
if [ "$FULL" = "1" ]; then
    run_step "cargo test --workspace" cargo test --workspace
else
    # Bounded but meaningful subset: node lib/API/compat tests plus the
    # supervisor's provisioning and subprocess tests. No hardware required.
    run_step "cargo test -p ozma-node --lib" cargo test -p ozma-node --lib
    run_step "cargo test -p ozma-softnode" cargo test -p ozma-softnode
fi

# ---------------------------------------------------------------------------
if [ "$SMOKE" = "1" ]; then
    section "ozma-node startup smoke with synthetic providers"
    HTTP_PORT=$((20000 + (($$ * 4) % 20000)))
    HID_PORT=$((HTTP_PORT + 1))
    SMOKE_LOG="$TARGET_DIR/clean-host-smoke.log"
    mkdir -p "$TARGET_DIR"

    "$NODE_BIN" \
        --profile desktop \
        --name clean-host-smoke \
        --http-port "$HTTP_PORT" \
        --hid-udp-port "$HID_PORT" \
        --capture-provider fake \
        --fake-width 320 \
        --fake-height 240 \
        --fake-fps 5 \
        --input-provider fake-recording \
        >"$SMOKE_LOG" 2>&1 &
    SMOKE_PID=$!

    cleanup_smoke() {
        if kill -0 "$SMOKE_PID" 2>/dev/null; then
            kill "$SMOKE_PID" 2>/dev/null || true
            wait "$SMOKE_PID" 2>/dev/null || true
        fi
    }
    trap cleanup_smoke EXIT

    HEALTH_URL="http://127.0.0.1:$HTTP_PORT/health"
    READY=0
    for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25; do
        if curl -fsS --max-time 1 "$HEALTH_URL" >/dev/null 2>&1; then
            READY=1
            break
        fi
        if ! kill -0 "$SMOKE_PID" 2>/dev/null; then
            break
        fi
        sleep 0.2
    done

    if [ "$READY" = "1" ] && curl -fsS --max-time 2 "$HEALTH_URL" | grep -Fq '"ok":true'; then
        pass "ozma-node desktop profile answered /health on port $HTTP_PORT"
    else
        fail "ozma-node desktop profile did not answer /health"
        echo "--- $SMOKE_LOG (tail) ---" >&2
        tail -n 40 "$SMOKE_LOG" >&2 || true
    fi

    cleanup_smoke
    trap - EXIT
fi

# ---------------------------------------------------------------------------
section "Summary"
printf '%d passed, %d failed, %d skipped\n' "$PASSES" "$FAILURES" "$SKIPS"
if [ "$FAILURES" -ne 0 ]; then
    echo "RESULT: FAIL" >&2
    exit 1
fi
echo "RESULT: PASS"
exit 0
