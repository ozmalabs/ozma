# Gate 2 live validation scripts

These scripts check an **already-running** VM setup:

- a QEMU/libvirt VM with a QMP socket (and optionally VNC),
- `ozma-node --profile vm` built from this tree,
- optionally the unmodified Python controller,
- optional HLS and the external GStreamer/Python WebRTC helper.

They do not create, start, stop, suspend, define, or edit a VM. They do not
start the node, controller, ffmpeg, or helper. They never use `sudo` and never
perform a destructive VM operation. They are not a substitute for
[`docs/gate2-validation.md`](../../docs/gate2-validation.md); a PASS report is
evidence, not a Gate 2 claim.

## Scripts

| Script | Purpose |
|---|---|
| `qemu-preflight.sh` | Read-only check of `virsh`/the domain state, QMP socket existence and greeting, ffmpeg VNC demuxer/HLS support, HLS output directory, WebRTC helper script and GStreamer elements, and optional controller health. |
| `check-node.sh` | HTTP checks against the running node: `/health`, `/api/v1/connection`, `/power/state`, `/stream/stream.m3u8` when HLS is running, and `/webrtc/offer` only when a helper is configured. With `OZMA_CONTROLLER_URL`, also checks controller health, registration, the node/power proxy, and the HLS proxy. |
| `webrtc-client.py` | Real GStreamer `webrtcbin` peer for a running node: receive-only H.264 offer (optional Opus), ICE/DTLS, decoded-media counters, optional `/webrtc/bitrate`. Uses system Python/GStreamer and installs nothing. |
| `run-gate2.sh` | Runs both scripts and writes a timestamped report under `${OZMA_GATE2_REPORT_DIR:-/tmp/ozma-gate2}`. Exits non-zero if either stage fails. |

## Minimal usage

```bash
export OZMA_NODE_URL=http://127.0.0.1:7382
export OZMA_NODE_ID=ozma-vm-gate2._ozma._udp.local.
export OZMA_CONTROLLER_URL=http://127.0.0.1:7380

# Optional, for the preflight:
export OZMA_QEMU_DOMAIN=ozma-gate2
export OZMA_QMP_SOCKET=/run/ozma/ozma-gate2.qmp
export OZMA_VNC_HOST=127.0.0.1       # optional
export OZMA_VNC_PORT=5901            # optional
export OZMA_HLS_DIR=/tmp/ozma-gate2-hls
export OZMA_WEBRTC_HELPER=/usr/bin/python3
export OZMA_WEBRTC_HELPER_SCRIPT="$PWD/softnode/gst_webrtc_worker.py"

scripts/gate2/run-gate2.sh
```

Run the pieces individually when debugging:

```bash
scripts/gate2/qemu-preflight.sh
scripts/gate2/check-node.sh
```

## Environment

### Required for `check-node.sh`

| Variable | Meaning |
|---|---|
| `OZMA_NODE_URL` | Node base URL, e.g. `http://127.0.0.1:7382`. |
| `OZMA_NODE_ID` | Full controller id, e.g. `ozma-vm-gate2._ozma._udp.local.` (the node name plus `._ozma._udp.local.`). |

### Optional

| Variable | Default | Meaning |
|---|---|---|
| `OZMA_CONTROLLER_URL` | unset | Controller base URL. Enables controller registration/proxy checks. |
| `OZMA_API_TOKEN` | unset | Bearer token for node control endpoints. Do **not** set this when validating the unmodified controller's proxy path: the controller proxy does not send a node token. |
| `OZMA_CONTROLLER_TOKEN` | unset | Bearer token for the controller, if controller auth is enabled. |
| `OZMA_GATE2_EXPECT_PROFILE` | `vm` | Expected `/api/v1/connection.profile`. |
| `OZMA_GATE2_HLS_ADVANCE_WAIT` | `10` | Seconds to poll for a playlist change; `0` disables the advance comparison. |
| `OZMA_GATE2_HTTP_TIMEOUT` | `20` | Per-request curl timeout in seconds (the WebRTC helper offer budget is 10 s). |
| `OZMA_GATE2_HTTP_CONNECT_TIMEOUT` | `5` | Per-request curl connect timeout in seconds. |
| `OZMA_GATE2_SKIP_WEBRTC_OFFER` | `0` | Set to `1` to skip the `/webrtc/offer` POST. |
| `OZMA_GATE2_WEBRTC_SDP_FILE` | unset | Real browser offer JSON (`{"sdp":"...","type":"offer"}`). Without it, `check-node.sh` sends a minimal SDP and reports a warning if the helper rejects it. |
| `OZMA_GATE2_REQUIRE_WEBRTC_OFFER` | `0` | Set to `1` to turn a rejected minimal offer into a failure. |
| `OZMA_GATE2_CONTROLLER_INPUT` | `0` | Set to `1` to POST a harmless key **release** through the controller input proxy. |
| `OZMA_QEMU_DOMAIN` / `OZMA_LIBVIRT_DOMAIN` | unset | libvirt domain for preflight. |
| `OZMA_LIBVIRT_URI` | unset | Optional `virsh --connect` URI. |
| `OZMA_QMP_SOCKET` | unset | QMP control socket checked by preflight. |
| `OZMA_QMP_INPUT_SOCKET` | unset | Optional dedicated QMP input socket. |
| `OZMA_VNC_HOST` / `OZMA_VNC_PORT` | unset | Optional VNC endpoint. Provide both or neither. |
| `OZMA_FFMPEG_BIN` | `ffmpeg` | ffmpeg executable. |
| `OZMA_HLS_DIR` | `/tmp/ozma-gate2-hls` | HLS output directory; preflight checks its parent is writable. |
| `OZMA_WEBRTC_HELPER` | `python3` | Helper program. |
| `OZMA_WEBRTC_HELPER_SCRIPT` | `softnode/gst_webrtc_worker.py` | Helper script. |
| `OZMA_GATE2_REPORT_DIR` | `/tmp/ozma-gate2` | `run-gate2.sh` report directory. |
| `OZMA_GATE2_SKIP_LIBVIRT` | `0` | `1` allows a direct-QEMU setup with no libvirt domain. |
| `OZMA_GATE2_SKIP_QMP` | `0` | `1` skips QMP checks; this is not a Gate 2 pass. |
| `OZMA_GATE2_SKIP_WEBRTC` | `0` | `1` skips helper checks; this is not a Gate 2 pass. |

## Output and exit codes

Each script prints `PASS`, `FAIL`, `WARN`, or `SKIP` per check and a summary.
`WARN`/`SKIP` are honest gaps, not passes. Both scripts exit `1` if any check
failed and `0` otherwise; usage errors exit `2`.

`run-gate2.sh` writes `gate2-<UTC timestamp>.log` in the report directory with
the environment (tokens redacted to `configured`/`not-configured`), both stages'
output, and their exit codes. Attach that log and the raw command output to the
matching parity-matrix row.

## What remains manual

The scripts cannot prove:

- guest-visible input (a key press and pointer move/click in the guest),
- a browser receiving WebRTC media (`webrtc-client.py` proves a non-browser
  GStreamer peer, not a browser/controller run),
- the controller dashboard rendering and proxying its UI paths,
- controller pytest/demo/clean-host regression.

The checklist in [`docs/gate2-validation.md`](../../docs/gate2-validation.md)
records the manual steps and the artifacts required for each Gate 2 row.
