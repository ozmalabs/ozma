#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Loopback smoke test for the direct-surface FreeRDP bridge.
#
# Requires: cc, pkg-config, FreeRDP shadow, Xvfb, xfreerdp3, ImageMagick
# (import/compare), python3.  Uses synthetic OZFR frames; no node is needed.

set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
CACHE=${TMPDIR:-/tmp}
WORK=$(mktemp -d "$CACHE/ozma-rdp-direct-smoke.XXXXXX")
WIDTH=${WIDTH:-320}
HEIGHT=${HEIGHT:-240}
BRIDGE="$WORK/rdp-direct"
FIFO="$WORK/frames.ozfr"
CREDS="$WORK/creds"
BRIDGE_LOG="$WORK/bridge.log"
STATUS=0

cleanup() {
    [ -n "${VIEWER:-}" ] && kill "$VIEWER" 2>/dev/null || true
    [ -n "${WRITER:-}" ] && kill "$WRITER" 2>/dev/null || true
    [ -n "${BRIDGE_PID:-}" ] && kill "$BRIDGE_PID" 2>/dev/null || true
    [ -n "${XVFB:-}" ] && kill "$XVFB" 2>/dev/null || true
    wait 2>/dev/null || true
    if [ "$STATUS" -eq 0 ]; then
        rm -rf "$WORK"
    else
        echo "smoke-test artifacts kept in $WORK" >&2
    fi
}
trap cleanup EXIT INT TERM

fail() {
    echo "FAIL: $*" >&2
    STATUS=1
    exit 1
}

require() {
    command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

require cc
require pkg-config
require Xvfb
require xfreerdp3
require python3

echo "== build =="
# shellcheck disable=SC2046
cc -O2 -Wall -Wextra -Wno-unused-parameter -Wno-deprecated-declarations \
    "$HERE/rdp-direct.c" \
    $(pkg-config --cflags --libs freerdp-shadow3) \
    -o "$BRIDGE"

printf 'ozma:correct-horse\n' > "$CREDS"
chmod 600 "$CREDS"
mkfifo "$FIFO"

echo "== synthetic OZFR source =="
python3 - "$FIFO" "$WIDTH" "$HEIGHT" <<'PY' &
import struct, sys, time
path, w, h = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
colors = [(220, 30, 30), (30, 200, 60), (40, 70, 220)]
with open(path, 'wb', buffering=0) as stream:
    for index in range(240):
        r, g, b = colors[(index // 4) % len(colors)]
        stream.write(b'OZFR' + struct.pack('>III', w, h, w * 4) +
                     bytes([r, g, b, 255]) * (w * h))
        time.sleep(0.25)
PY
WRITER=$!

echo "== bridge =="
"$BRIDGE" --port 3394 --auth-file "$CREDS" --max-viewers 1 \
    --config-dir "$WORK/config" < "$FIFO" > "$WORK/bridge.out" 2> "$BRIDGE_LOG" &
BRIDGE_PID=$!
for _ in $(seq 1 60); do
    grep -q "direct surface" "$BRIDGE_LOG" 2>/dev/null && break
    sleep 0.1
done
grep -q "direct surface" "$BRIDGE_LOG" || fail "bridge did not start"

DISPLAY_NUMBER=120
while [ -e "/tmp/.X${DISPLAY_NUMBER}-lock" ]; do
    DISPLAY_NUMBER=$((DISPLAY_NUMBER + 1))
done
Xvfb ":$DISPLAY_NUMBER" -screen 0 "${WIDTH}x${HEIGHT}x24" -nolisten tcp \
    > "$WORK/xvfb.log" 2>&1 &
XVFB=$!
sleep 0.5
export DISPLAY=":$DISPLAY_NUMBER"

viewer() {
    password=$1
    timeout 8 xfreerdp3 /v:127.0.0.1:3394 /u:ozma /p:"$password" \
        /sec:tls /cert:ignore "/size:${WIDTH}x${HEIGHT}" -gfx -rfx /bpp:32 \
        /log-level:ERROR
}

echo "== wrong token is rejected =="
set +e
viewer wrong-token > "$WORK/wrong.out" 2> "$WORK/wrong.err"
set -e
grep -q "viewer rejected.*invalid username or viewer token" "$BRIDGE_LOG" ||
    fail "wrong token was not rejected with a clear diagnostic"

echo "== stock viewer connects and paints =="
viewer correct-horse > "$WORK/viewer.out" 2> "$WORK/viewer.err" &
VIEWER=$!
for _ in $(seq 1 50); do
    grep -q "viewer connected" "$BRIDGE_LOG" 2>/dev/null && break
    sleep 0.1
done
grep -q "viewer connected" "$BRIDGE_LOG" || fail "stock viewer did not connect"

if command -v import >/dev/null 2>&1 && command -v compare >/dev/null 2>&1; then
    sleep 1
    import -window root "$WORK/shot1.png" 2>/dev/null || fail "cannot capture screenshot 1"
    sleep 2
    import -window root "$WORK/shot2.png" 2>/dev/null || fail "cannot capture screenshot 2"
    pixels=$(compare -metric AE "$WORK/shot1.png" "$WORK/shot2.png" null: 2>&1 || true)
    case "$pixels" in
        ''|0|0.0) fail "screenshots are identical; no frame update reached the viewer" ;;
        *) echo "screenshot delta: $pixels pixel(s)" ;;
    esac
else
    echo "WARN: ImageMagick import/compare unavailable; skipping screenshot assertion" >&2
fi

echo "== concurrent viewer limit =="
set +e
viewer correct-horse > "$WORK/second.out" 2> "$WORK/second.err"
set -e
grep -q "viewer limit reached" "$BRIDGE_LOG" ||
    fail "second viewer was not rejected by the viewer limit"

echo "PASS: direct surface, frame updates, auth rejection, and viewer limit"
STATUS=0
exit 0
