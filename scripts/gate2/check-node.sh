#!/usr/bin/env sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Gate 2 live check for an already-running ozma-node VM node.
#
# This script performs HTTP checks only. It does not start QEMU, the node, the
# controller, ffmpeg, or the WebRTC helper, and it does not modify the VM.
#
# Required:
#   OZMA_NODE_URL       e.g. http://127.0.0.1:7382
#   OZMA_NODE_ID        e.g. ozma-vm-gate2._ozma._udp.local.
#
# Optional:
#   OZMA_CONTROLLER_URL          e.g. http://127.0.0.1:7380
#   OZMA_API_TOKEN               bearer token for the node, if configured
#   OZMA_CONTROLLER_TOKEN        bearer token for the controller, if configured
#   OZMA_GATE2_EXPECT_PROFILE    default: vm
#   OZMA_GATE2_HLS_ADVANCE_WAIT  seconds between playlist fetches; 0 disables
#                                the advance check (default: 4)
#   OZMA_GATE2_SKIP_WEBRTC_OFFER set to 1 to skip the signaling POST
#   OZMA_GATE2_WEBRTC_SDP_FILE   real browser offer JSON ({"sdp":...,"type":"offer"})
#   OZMA_GATE2_REQUIRE_WEBRTC_OFFER
#                                set to 1 to fail when a minimal-offer probe
#                                is rejected instead of only warning
#   OZMA_GATE2_CONTROLLER_INPUT  set to 1 to also POST a key *release* through
#                                the controller input proxy (no key is pressed)
#
# Exit status:
#   0  every executed check passed (warnings/skips are reported)
#   1  at least one check failed
#   2  usage/configuration error

set -u

PROG=$(basename "$0")

NODE_URL=${OZMA_NODE_URL:-}
NODE_ID=${OZMA_NODE_ID:-}
CONTROLLER_URL=${OZMA_CONTROLLER_URL:-}
NODE_TOKEN=${OZMA_API_TOKEN:-}
CONTROLLER_TOKEN=${OZMA_CONTROLLER_TOKEN:-}
EXPECT_PROFILE=${OZMA_GATE2_EXPECT_PROFILE:-vm}
HLS_ADVANCE_WAIT=${OZMA_GATE2_HLS_ADVANCE_WAIT:-10}
SKIP_WEBRTC_OFFER=${OZMA_GATE2_SKIP_WEBRTC_OFFER:-0}
WEBRTC_SDP_FILE=${OZMA_GATE2_WEBRTC_SDP_FILE:-}
REQUIRE_WEBRTC_OFFER=${OZMA_GATE2_REQUIRE_WEBRTC_OFFER:-0}
CONTROLLER_INPUT=${OZMA_GATE2_CONTROLLER_INPUT:-0}

HTTP_TIMEOUT=${OZMA_GATE2_HTTP_TIMEOUT:-20}
HTTP_CONNECT_TIMEOUT=${OZMA_GATE2_HTTP_CONNECT_TIMEOUT:-5}

PASSES=0
FAILURES=0
WARNINGS=0
SKIPS=0

usage() {
    cat <<EOF
Usage: $PROG

Checks an already-running ozma-node and optionally the unmodified controller.

Required environment:
  OZMA_NODE_URL            Node base URL (http://127.0.0.1:7382)
  OZMA_NODE_ID             Full controller id (name._ozma._udp.local.)

Optional environment:
  OZMA_CONTROLLER_URL      Controller base URL (http://127.0.0.1:7380)
  OZMA_API_TOKEN           Bearer token for the node
  OZMA_CONTROLLER_TOKEN    Bearer token for the controller
  OZMA_GATE2_EXPECT_PROFILE
                           Expected profile (default: vm)
  OZMA_GATE2_HLS_ADVANCE_WAIT
                           Seconds to poll for a playlist change (default: 10;
                           0 disables the advance comparison)
  OZMA_GATE2_HTTP_TIMEOUT  Per-request curl timeout in seconds (default: 20)
  OZMA_GATE2_HTTP_CONNECT_TIMEOUT
                           Per-request connect timeout in seconds (default: 5)
  OZMA_GATE2_SKIP_WEBRTC_OFFER
                           Set to 1 to skip the offer POST
  OZMA_GATE2_WEBRTC_SDP_FILE
                           Real browser offer JSON for /webrtc/offer
  OZMA_GATE2_REQUIRE_WEBRTC_OFFER
                           Set to 1 to fail instead of warn when a minimal
                           offer is rejected
  OZMA_GATE2_CONTROLLER_INPUT
                           Set to 1 to POST a harmless key release through
                           the controller input proxy

No tokens are written to stdout. This script never starts or stops a VM.
EOF
}

case "${1:-}" in
    -h|--help)
        usage
        exit 0
        ;;
esac

if [ -z "$NODE_URL" ] || [ -z "$NODE_ID" ]; then
    printf 'ERROR: OZMA_NODE_URL and OZMA_NODE_ID are required.\n\n' >&2
    usage >&2
    exit 2
fi

if ! command -v curl >/dev/null 2>&1; then
    printf 'ERROR: curl is required.\n' >&2
    exit 2
fi

case "$HLS_ADVANCE_WAIT" in
    ''|*[!0-9]*)
        printf 'ERROR: OZMA_GATE2_HLS_ADVANCE_WAIT must be a non-negative integer.\n' >&2
        exit 2
        ;;
esac

NODE_BASE=$(printf '%s' "$NODE_URL" | sed 's:/*$::')
if [ -n "$CONTROLLER_URL" ]; then
    CONTROLLER_BASE=$(printf '%s' "$CONTROLLER_URL" | sed 's:/*$::')
else
    CONTROLLER_BASE=""
fi

TMPDIR_GATE2=$(mktemp -d "${TMPDIR:-/tmp}/ozma-gate2-check.XXXXXX") || {
    printf 'ERROR: could not create a temporary directory.\n' >&2
    exit 2
}
cleanup() {
    rm -rf "$TMPDIR_GATE2"
}
trap cleanup 0 1 2 15

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

# curl request helpers. The body is left in HTTP_BODY and the status in
# HTTP_CODE. A transport failure is reported as 000.
HTTP_BODY=""
HTTP_CODE=""
HTTP_ERR=""

http_get() {
    http_url=$1
    http_auth=${2:-}
    HTTP_BODY="$TMPDIR_GATE2/body"
    HTTP_CODE=000
    HTTP_ERR=""
    if [ -n "$http_auth" ]; then
        HTTP_CODE=$(curl --globoff -sS --max-time "$HTTP_TIMEOUT" \
            --connect-timeout "$HTTP_CONNECT_TIMEOUT" \
            -o "$HTTP_BODY" -w '%{http_code}' \
            -H "Authorization: Bearer $http_auth" \
            "$http_url" 2>"$TMPDIR_GATE2/curl.err") || true
    else
        HTTP_CODE=$(curl --globoff -sS --max-time "$HTTP_TIMEOUT" \
            --connect-timeout "$HTTP_CONNECT_TIMEOUT" \
            -o "$HTTP_BODY" -w '%{http_code}' \
            "$http_url" 2>"$TMPDIR_GATE2/curl.err") || true
    fi
    [ -n "$HTTP_CODE" ] || HTTP_CODE=000
    if [ "$HTTP_CODE" = "000" ]; then
        HTTP_ERR=$(head -n 1 "$TMPDIR_GATE2/curl.err" 2>/dev/null || true)
    fi
    [ -f "$HTTP_BODY" ] || : > "$HTTP_BODY"
}

http_post_json() {
    http_url=$1
    http_payload=$2
    http_auth=${3:-}
    HTTP_BODY="$TMPDIR_GATE2/body"
    HTTP_CODE=000
    HTTP_ERR=""
    if [ -n "$http_auth" ]; then
        HTTP_CODE=$(curl --globoff -sS --max-time "$HTTP_TIMEOUT" \
            --connect-timeout "$HTTP_CONNECT_TIMEOUT" \
            -o "$HTTP_BODY" -w '%{http_code}' \
            -X POST \
            -H 'Content-Type: application/json' \
            -H "Authorization: Bearer $http_auth" \
            --data-binary "$http_payload" \
            "$http_url" 2>"$TMPDIR_GATE2/curl.err") || true
    else
        HTTP_CODE=$(curl --globoff -sS --max-time "$HTTP_TIMEOUT" \
            --connect-timeout "$HTTP_CONNECT_TIMEOUT" \
            -o "$HTTP_BODY" -w '%{http_code}' \
            -X POST \
            -H 'Content-Type: application/json' \
            --data-binary "$http_payload" \
            "$http_url" 2>"$TMPDIR_GATE2/curl.err") || true
    fi
    [ -n "$HTTP_CODE" ] || HTTP_CODE=000
    if [ "$HTTP_CODE" = "000" ]; then
        HTTP_ERR=$(head -n 1 "$TMPDIR_GATE2/curl.err" 2>/dev/null || true)
    fi
    [ -f "$HTTP_BODY" ] || : > "$HTTP_BODY"
}

json_string() {
    # json_string <file> <key>
    sed -n 's/.*"'"$2"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$1" \
        | head -n 1
}

json_bool() {
    # json_bool <file> <key>
    sed -n 's/.*"'"$2"'"[[:space:]]*:[[:space:]]*\(true\|false\).*/\1/p' "$1" \
        | head -n 1
}

body_preview() {
    head -c 400 "$1" 2>/dev/null || true
}

is_extm3u() {
    head -n 1 "$1" 2>/dev/null | grep -q '^#EXTM3U'
}

# ---------------------------------------------------------------------------
# Node checks
# ---------------------------------------------------------------------------

printf 'Gate 2 node checks: %s (id %s)\n' "$NODE_BASE" "$NODE_ID"
if [ -n "$NODE_TOKEN" ]; then
    printf 'Node bearer token: configured (value not printed)\n'
else
    printf 'Node bearer token: not configured\n'
fi

# 1. /health
http_get "$NODE_BASE/health" "$NODE_TOKEN"
if [ "$HTTP_CODE" != "200" ]; then
    fail "/health returned ${HTTP_CODE}${HTTP_ERR:+ ($HTTP_ERR)}"
else
    if [ "$(json_bool "$HTTP_BODY" ok)" = "true" ]; then
        pass "/health returned 200 and ok=true"
    else
        fail "/health returned 200 but ok is not true: $(body_preview "$HTTP_BODY")"
    fi
fi

# 2. /api/v1/connection
HLS_STATE=""
WEBRTC_STATE=""
CONNECTION_BODY="$TMPDIR_GATE2/connection.json"
: > "$CONNECTION_BODY"
http_get "$NODE_BASE/api/v1/connection" "$NODE_TOKEN"
if [ "$HTTP_CODE" != "200" ]; then
    fail "/api/v1/connection returned ${HTTP_CODE}${HTTP_ERR:+ ($HTTP_ERR)}"
else
    cp "$HTTP_BODY" "$CONNECTION_BODY"
    connection_id=$(json_string "$HTTP_BODY" node_id)
    connection_profile=$(json_string "$HTTP_BODY" profile)
    HLS_STATE=$(json_string "$HTTP_BODY" hls)
    WEBRTC_STATE=$(json_string "$HTTP_BODY" webrtc)
    if [ "$(json_bool "$HTTP_BODY" ok)" != "true" ]; then
        fail "/api/v1/connection ok is not true: $(body_preview "$HTTP_BODY")"
    elif [ "$connection_id" != "$NODE_ID" ]; then
        fail "/api/v1/connection node_id '$connection_id' does not match OZMA_NODE_ID '$NODE_ID'"
    elif [ "$connection_profile" != "$EXPECT_PROFILE" ]; then
        fail "/api/v1/connection profile '$connection_profile' does not match expected '$EXPECT_PROFILE'"
    else
        pass "/api/v1/connection returned 200 for node_id '$connection_id' (profile '$connection_profile')"
    fi
    if [ -z "$HLS_STATE" ] || [ -z "$WEBRTC_STATE" ]; then
        warn "/api/v1/connection is missing hls/webrtc state; expected a current ozma-node build"
    fi
fi

# 3. /power/state
http_get "$NODE_BASE/power/state" "$NODE_TOKEN"
if [ "$HTTP_CODE" != "200" ]; then
    fail "/power/state returned ${HTTP_CODE}${HTTP_ERR:+ ($HTTP_ERR)}: $(body_preview "$HTTP_BODY")"
else
    if [ "$(json_bool "$HTTP_BODY" available)" = "true" ]; then
        power_status=$(json_string "$HTTP_BODY" status)
        pass "/power/state reports available=true (status '$power_status')"
    else
        fail "/power/state reports available=false; configure --qmp-socket or --libvirt-domain: $(body_preview "$HTTP_BODY")"
    fi
fi

# 4. HLS playlist, when the node reports it running.
case "$HLS_STATE" in
    running)
        http_get "$NODE_BASE/stream/stream.m3u8" ""
        if [ "$HTTP_CODE" != "200" ]; then
            fail "/stream/stream.m3u8 returned ${HTTP_CODE}${HTTP_ERR:+ ($HTTP_ERR)}"
        elif ! is_extm3u "$HTTP_BODY"; then
            fail "/stream/stream.m3u8 is not an HLS playlist: $(body_preview "$HTTP_BODY")"
        else
            pass "/stream/stream.m3u8 returned 200 and starts with #EXTM3U (no bearer required)"
            cp "$HTTP_BODY" "$TMPDIR_GATE2/playlist.first"
            if [ "$HLS_ADVANCE_WAIT" -gt 0 ]; then
                elapsed=0
                advanced=0
                while [ "$elapsed" -lt "$HLS_ADVANCE_WAIT" ]; do
                    sleep 1
                    elapsed=$((elapsed + 1))
                    http_get "$NODE_BASE/stream/stream.m3u8" ""
                    if [ "$HTTP_CODE" != "200" ]; then
                        fail "playlist fetch ${elapsed}s after the first returned ${HTTP_CODE}"
                        advanced=-1
                        break
                    elif ! cmp -s "$TMPDIR_GATE2/playlist.first" "$HTTP_BODY"; then
                        pass "HLS playlist advanced within ${elapsed}s"
                        advanced=1
                        break
                    fi
                done
                if [ "$advanced" = "0" ]; then
                    warn "HLS playlist did not change in ${HLS_ADVANCE_WAIT}s; capture may be stalled or the QMP segment interval is longer"
                fi
            else
                skip "HLS playlist advance check disabled (OZMA_GATE2_HLS_ADVANCE_WAIT=0)"
            fi
        fi
        ;;
    failed)
        hls_error=$(json_string "$CONNECTION_BODY" hls_error)
        fail "HLS is configured but failed: ${hls_error:-unknown error}"
        ;;
    not-configured)
        skip "HLS is not configured (start the node with --hls)"
        ;;
    "")
        skip "HLS state is unavailable because /api/v1/connection could not be read"
        ;;
    *)
        fail "unexpected HLS state '$HLS_STATE'"
        ;;
esac

# 5. WebRTC: only post when a helper is configured.
if [ "$SKIP_WEBRTC_OFFER" = "1" ]; then
    skip "WebRTC offer POST disabled (OZMA_GATE2_SKIP_WEBRTC_OFFER=1)"
else
    case "$WEBRTC_STATE" in
        running)
            if [ -n "$WEBRTC_SDP_FILE" ]; then
                if [ ! -f "$WEBRTC_SDP_FILE" ]; then
                    fail "OZMA_GATE2_WEBRTC_SDP_FILE does not exist: $WEBRTC_SDP_FILE"
                elif ! command -v python3 >/dev/null 2>&1; then
                    fail "python3 is required to JSON-encode OZMA_GATE2_WEBRTC_SDP_FILE"
                else
                    offer_payload=$(python3 - "$WEBRTC_SDP_FILE" <<'PY'
import json
import sys
with open(sys.argv[1], "r", encoding="utf-8") as handle:
    document = json.load(handle)
if "sdp" not in document:
    raise SystemExit("offer JSON has no sdp field")
document.setdefault("type", "offer")
print(json.dumps(document))
PY
) || offer_payload=""
                    if [ -z "$offer_payload" ]; then
                        fail "could not read a valid offer from $WEBRTC_SDP_FILE"
                    else
                        http_post_json "$NODE_BASE/webrtc/offer" "$offer_payload" "$NODE_TOKEN"
                        answer_type=$(json_string "$HTTP_BODY" type)
                        answer_sdp=$(json_string "$HTTP_BODY" sdp)
                        if [ "$HTTP_CODE" != "200" ]; then
                            fail "/webrtc/offer returned ${HTTP_CODE}: $(body_preview "$HTTP_BODY")"
                        elif [ "$answer_type" != "answer" ] || [ -z "$answer_sdp" ]; then
                            fail "/webrtc/offer did not return a non-empty answer: $(body_preview "$HTTP_BODY")"
                        else
                            pass "/webrtc/offer returned a non-empty answer for the supplied browser offer"
                        fi
                    fi
                fi
            else
                minimal_offer='{"sdp":"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\nc=IN IP4 0.0.0.0\r\na=rtpmap:96 H264/90000\r\na=sendonly\r\n","type":"offer"}'
                http_post_json "$NODE_BASE/webrtc/offer" "$minimal_offer" "$NODE_TOKEN"
                minimal_answer_sdp=$(json_string "$HTTP_BODY" sdp)
                if [ "$HTTP_CODE" = "200" ] \
                    && grep -q '"type"[[:space:]]*:[[:space:]]*"answer"' "$HTTP_BODY" \
                    && [ -n "$minimal_answer_sdp" ]; then
                    pass "/webrtc/offer accepted a minimal offer and returned an answer"
                elif [ "$HTTP_CODE" = "000" ]; then
                    fail "/webrtc/offer request failed: ${HTTP_ERR:-transport error}"
                else
                    if [ "$REQUIRE_WEBRTC_OFFER" = "1" ]; then
                        fail "/webrtc/offer rejected the minimal offer (${HTTP_CODE}): $(body_preview "$HTTP_BODY")"
                    else
                        warn "/webrtc/offer route is reachable but rejected the minimal offer (${HTTP_CODE}); set OZMA_GATE2_WEBRTC_SDP_FILE from a real browser offer for a full negotiation check"
                    fi
                fi
            fi
            ;;
        failed)
            webrtc_error=$(json_string "$CONNECTION_BODY" webrtc_error)
            fail "WebRTC helper is configured but failed: ${webrtc_error:-unknown error}"
            ;;
        not-configured)
            skip "WebRTC is not configured (start the node with --webrtc-helper)"
            ;;
        "")
            skip "WebRTC state is unavailable because /api/v1/connection could not be read"
            ;;
        *)
            fail "unexpected WebRTC state '$WEBRTC_STATE'"
            ;;
    esac
fi

# ---------------------------------------------------------------------------
# Optional controller checks
# ---------------------------------------------------------------------------

if [ -z "$CONTROLLER_BASE" ]; then
    skip "controller checks disabled (OZMA_CONTROLLER_URL is not set)"
else
    printf '\nGate 2 controller checks: %s\n' "$CONTROLLER_BASE"
    if [ -n "$CONTROLLER_TOKEN" ]; then
        printf 'Controller bearer token: configured (value not printed)\n'
    else
        printf 'Controller bearer token: not configured\n'
    fi

    http_get "$CONTROLLER_BASE/health" "$CONTROLLER_TOKEN"
    if [ "$HTTP_CODE" = "200" ] && grep -q '"status"[[:space:]]*:[[:space:]]*"ok"' "$HTTP_BODY"; then
        pass "controller /health reports status=ok"
    else
        fail "controller /health returned ${HTTP_CODE}: $(body_preview "$HTTP_BODY")"
    fi

    http_get "$CONTROLLER_BASE/api/v1/nodes" "$CONTROLLER_TOKEN"
    if [ "$HTTP_CODE" != "200" ]; then
        fail "controller /api/v1/nodes returned ${HTTP_CODE}: $(body_preview "$HTTP_BODY")"
    elif grep -Fq "\"$NODE_ID\"" "$HTTP_BODY"; then
        pass "controller lists node '$NODE_ID'"
    else
        fail "controller does not list '$NODE_ID'; check node registration logs"
    fi

    http_get "$CONTROLLER_BASE/api/v1/nodes/$NODE_ID" "$CONTROLLER_TOKEN"
    if [ "$HTTP_CODE" = "200" ]; then
        pass "controller node detail for '$NODE_ID' returned 200"
    else
        fail "controller node detail returned ${HTTP_CODE}: $(body_preview "$HTTP_BODY")"
    fi

    http_get "$CONTROLLER_BASE/api/v1/nodes/$NODE_ID/power/state" "$CONTROLLER_TOKEN"
    if [ "$HTTP_CODE" = "200" ] && [ "$(json_bool "$HTTP_BODY" available)" = "true" ]; then
        pass "controller power proxy reports available=true"
    else
        fail "controller power proxy returned ${HTTP_CODE}: $(body_preview "$HTTP_BODY")"
    fi

    if [ "$HLS_STATE" = "running" ]; then
        http_get "$CONTROLLER_BASE/api/v1/streams/$NODE_ID/hls/stream.m3u8" "$CONTROLLER_TOKEN"
        if [ "$HTTP_CODE" = "200" ] && is_extm3u "$HTTP_BODY"; then
            pass "controller HLS proxy returned the node playlist"
        else
            fail "controller HLS proxy returned ${HTTP_CODE}: $(body_preview "$HTTP_BODY") (the controller proxies to 127.0.0.1:<api_port>; the node must be local to the controller)"
        fi
    else
        skip "controller HLS proxy not checked because node HLS is '$HLS_STATE'"
    fi

    if [ "$CONTROLLER_INPUT" = "1" ]; then
        # A key release only. It does not press a key or change guest state.
        http_post_json "$CONTROLLER_BASE/api/v1/nodes/$NODE_ID/proxy/input/key" \
            '{"keycode":30,"down":false}' "$CONTROLLER_TOKEN"
        if [ "$HTTP_CODE" = "200" ] && [ "$(json_bool "$HTTP_BODY" ok)" = "true" ]; then
            pass "controller input proxy accepted a key-release route check"
        else
            fail "controller input proxy returned ${HTTP_CODE}: $(body_preview "$HTTP_BODY") (the proxy does not send a node bearer token)"
        fi
    else
        skip "controller input proxy not checked (set OZMA_GATE2_CONTROLLER_INPUT=1 for a release-only route check)"
    fi
fi

printf '\nSummary: %d passed, %d failed, %d warnings, %d skipped\n' \
    "$PASSES" "$FAILURES" "$WARNINGS" "$SKIPS"
if [ "$FAILURES" -ne 0 ]; then
    exit 1
fi
exit 0
