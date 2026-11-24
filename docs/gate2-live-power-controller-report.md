# Gate 2 live power-action and controller/dashboard report

**Scope:** the Rust node's `/power/*` routes against a real disposable QEMU QMP
socket, plus registration, heartbeat, list/detail, power proxy, snapshot proxy,
HLS proxy, and dashboard rendering through the repaired Python controller.

**This is not a Gate 2 claim.** It covers power *actions* on an OS-less
disposable QEMU process and the controller/dashboard HTTP and browser paths for
one registered Rust node. It does not cover a real guest OS, guest-visible
input, WebRTC media, audio, D-Bus display capture, or hard power-off/destroy on
a real VM. See [Remaining Gate 2 blocker](#remaining-gate-2-blocker).

> No `sudo`, no libvirt domain, no `virsh` mutating verb, and no package install
> outside a `/tmp` venv were used. The only VM was a direct, headless
> `qemu-system-x86_64` TCG process with a temporary QMP Unix socket, no disk,
> and no network device. Every process, port, venv, and temporary file was
> removed at the end; see [Cleanup](#cleanup).

## 1. Run metadata

| Item | Value |
|---|---|
| Run | 2026-11-24 14:05–14:11 AEDT (2026-11-24T03:05–03:11Z) |
| Repo | `/home/matt/ozma/ozma` |
| HEAD at start | `cd8e8bd2408cdb0f81ba57139888efb71377c7a2` (`cd8e8bd`) |
| Node binary | `target/debug/ozma-node`, 280,782,536 bytes, mtime 2026-11-24 13:45:08 |
| Node version | `ozma-node 0.1.0` |
| QEMU | `qemu-system-x86_64` 11.1.3, **TCG** (`-machine pc,accel=tcg`) |
| ffmpeg | `n9.1` |
| Python | 3.14.10 |
| Controller | temporary copy of `controller/`, run from `/tmp` with an isolated venv |
| Playwright | 1.58.0 with cached Chromium, headless |
| Temp root | `/tmp/opencode/ozma-gate2-power-controller` (removed) |
| Ports | node HTTP 44915, node HID/UDP 60742, controller API 36651 |

The tested node binary predates source edits that appeared in the working tree
during the run. The tree was clean at start; during the run another process
modified `docs/gate2-live-qmp-report.md`, `docs/parity-matrix.md`,
`ozma-capture/src/{dbus_display.rs,provider.rs}`,
`ozma-node/src/{hls.rs,platform.rs}`, and `softnode/gst_webrtc_worker.py`.
This validation did not edit those files and did not rebuild. Results describe
the 13:45 binary and the controller copy tested, not a frozen commit.

## 2. Disposable QEMU and manual QMP check

```bash
ROOT=/tmp/opencode/ozma-gate2-power-controller
qemu-system-x86_64 \
  -name ozma-gate2-power-ctrl \
  -machine pc,accel=tcg \
  -m 256 \
  -nodefaults \
  -device VGA \
  -device qemu-xhci,id=xhci \
  -device usb-kbd,bus=xhci.0 \
  -display none \
  -qmp "unix:$ROOT/qemu/qmp.sock,server=on,wait=off" \
  -pidfile "$ROOT/qemu/qemu.pid" \
  -daemonize
```

Exit `0`, PID 3845053, empty stderr log. Manual QMP check (one client, then
closed):

```text
greeting: QEMU 11.1.3, capabilities ["oob"]
qmp_capabilities: {"return": {}}
query-status: {"return": {"status": "running", "running": true}}
screendump: {"return": {}}
PPM: b'P6\n720 400\n255\n...' size 864015 sha256 8c27567f…
```

## 3. Node command

```bash
ROOT=/tmp/opencode/ozma-gate2-power-controller
./target/debug/ozma-node \
  --name ozma-gate2-power-ctrl \
  --profile vm \
  --qmp-socket "$ROOT/qemu/qmp.sock" \
  --power-provider qmp \
  --input-provider recording \
  --http-port 44915 \
  --hid-udp-port 60742 \
  --cap hid,video,power \
  --machine-class server \
  --register-url http://127.0.0.1:36651 \
  --register-host 127.0.0.1 \
  --heartbeat-interval 5 \
  --hls --hls-dir "$ROOT/node/hls2" \
  --hls-fps 5 --hls-bitrate-kbps 1000 \
  --ffmpeg-bin /usr/bin/ffmpeg
```

`/api/v1/connection` reported:

```json
{
  "profile": "vm",
  "sink_kind": "fake-recording",
  "power": {"available": true, "status": "running"},
  "input": {"sink_kind": "fake-recording", "available": true, "injecting": false},
  "hls": "running",
  "hls_error": null
}
```

`injecting:false` is expected for the requested recording sink and is not input
parity. The QMP control client is shared by power, capture, snapshots, and HLS;
no second QMP connection was opened.

## 4. Power routes — PASS

The sequence was run directly against the node. Each state value is read back
through `/power/state`, which issues QMP `query-status` over the shared client.

| Step | Response | Observed state |
|---|---|---|
| `GET /power/state` (initial) | `200` `{"available":true,"status":"running"}` | running |
| `POST /power/stop` | `200` `{"action":"force-off","ok":true}` | paused |
| `GET /power/state` | `200` `{"available":true,"status":"paused"}` | paused |
| `POST /power/on` | `200` `{"action":"on","ok":true}` | running |
| `GET /power/state` | `200` `{"available":true,"status":"running"}` | running |
| `POST /power/reset` | `200` `{"action":"reset","ok":true}` | running |
| `GET /power/state` | `200` `{"available":true,"status":"running"}` | running |
| `POST /power/bogus` (negative) | `400` `{"error":"invalid power state or action: unknown power action \"bogus\"; expected one of: on, off, reset, force-off","ok":false}` | unchanged |

Findings:

* `POST /power/stop` is accepted as an alias for `force-off`, but on QMP it
  delegates to QEMU `stop`, which **pauses** the guest. It does not destroy it.
  The `{"action":"force-off"}` response is therefore semantically misleading;
  a caller must not treat it as a hard power-off.
* `POST /power/off` (ACPI powerdown) and destructive `force-off`/`destroy` were
  deliberately **not** called. On this OS-less disposable QEMU there is no guest
  to handle ACPI, and the task preferred stop/on/reset. Hard power-off remains
  unvalidated.
* No panic, QMP timeout, or failed-command log line appeared during the
  sequence.

## 5. Controller startup and registration

The repaired controller was run from a temporary copy so its relative data
directories stayed under `/tmp`, with an isolated venv built from
`controller/requirements.txt`:

```bash
python3 -m venv /tmp/opencode/ozma-gate2-power-controller/venv
/tmp/opencode/ozma-gate2-power-controller/venv/bin/python -m pip install \
  -r controller/requirements.txt

cd /tmp/opencode/ozma-gate2-power-controller/ctrl/controller
setsid env PYTHONDONTWRITEBYTECODE=1 OZMA_AUDIO=0 \
  OZMA_API_HOST=127.0.0.1 OZMA_API_PORT=36651 \
  /tmp/opencode/ozma-gate2-power-controller/venv/bin/python main.py \
  --host 127.0.0.1 --port 36651 \
  </dev/null >../controller2.log 2>&1 &
```

`GET /health` returned `200 {"status":"ok"}` after 6 s. Key startup lines:

```text
INFO ozma.moonlight.protocol: RTSP server listening on port 47996
INFO ozma.moonlight.server: Moonlight server started
INFO ozma.grid: Grid service started: Ozma Grid @ 192.168.40.19:7381
INFO:     Application startup complete.
INFO:     Uvicorn running on http://127.0.0.1:36651
```

The controller also bound its fixed auxiliary ports 47996 (RTSP) and 7381
(grid/MCP). Port 47984 was already owned by a host Sunshine/Moonlight listener
and was not used or touched.

Node registration and heartbeat:

```text
INFO ozma_node::registration: Registration accepted by controller
  node_id="ozma-gate2-power-ctrl._ozma._udp.local."
  url=http://127.0.0.1:36651/api/v1/nodes/register
INFO ozma_node::registration: Heartbeat OK
```

`GET /api/v1/nodes` contained exactly one node, and `last_seen` increased over
one heartbeat window:

```text
nodes t0 count 1  t1 count 1
last_seen t0 250089.963280369
last_seen t1 250095.03023342   increased=true
```

Detail fields:

```json
{
  "id": "ozma-gate2-power-ctrl._ozma._udp.local.",
  "host": "127.0.0.1",
  "port": 60742,
  "role": "compute",
  "hw": "x86_64",
  "fw_version": "0.1.0",
  "proto_version": 1,
  "capabilities": ["hid", "video", "power"],
  "machine_class": "server",
  "stream_url": "http://127.0.0.1:44915/stream/stream.m3u8",
  "api_port": 44915,
  "hid_active": true,
  "displays": []
}
```

## 6. Controller proxy results — PASS

All requests used the registered node id
`ozma-gate2-power-ctrl._ozma._udp.local.`.

| Check | Result |
|---|---|
| `GET /api/v1/nodes/{id}/power/state` | `200` `{"available":true,"status":"running"}` |
| `POST /api/v1/nodes/{id}/power/stop` | `200` `{"action":"force-off","ok":true}` |
| `GET /api/v1/nodes/{id}/power/state` after stop | `200` `{"available":true,"status":"paused"}` |
| `POST /api/v1/nodes/{id}/power/on` | `200` `{"action":"on","ok":true}` |
| `GET /api/v1/nodes/{id}/power/state` after on | `200` `{"available":true,"status":"running"}` |
| `GET /api/v1/streams/{id}/snapshot` | `200` `image/jpeg`, 19,898 bytes, 720×400 |
| `GET /api/v1/streams/{id}/hls/stream.m3u8` | `200` `application/vnd.apple.mpegurl`, 199 bytes |
| HLS segment via controller | `200` 17,108 bytes, MPEG-TS, h264 720×400 |
| Direct node `/stream/{segment}` | `200` 17,108 bytes (identical segment) |
| `GET /api/v1/streams` | `200` one `hls-remote` stream, `active:true` |

Snapshot verification: real JFIF JPEG, `FF D8 FF … FF D9`, SOF0 720×400,
63 distinct colors (mean 2.81, stddev 21.0), not an empty file or placeholder.

HLS segment verification:

```text
ffprobe: codec_name=h264  width=720  height=400
TS sync: 47 40 11 12 ... (0x47 at offset 0)
```

Honest HLS note: the first segment fetch through the controller returned
`503 {"detail":"HLS not available"}` because the playlist had been read before
`delete_segments` rotated out that exact segment. Re-fetching the playlist and
requesting its newest segment returned `200` immediately, and the
controller-proxied bytes matched the node's bytes. This is a stale-playlist
client race, not a broken proxy route; a dashboard/player should follow the
current playlist rather than cache an old segment name.

## 7. Dashboard and browser — PASS with cosmetic gaps

Endpoints:

| URL | Result |
|---|---|
| `GET /` | `200` `text/html`, 41,842 bytes — this is the simple switcher/index page |
| `GET /dashboard.html` | `200` `text/html`, 169,923 bytes — this is the actual dashboard |
| `GET /desk3d.html` | `200` `text/html`, 12,358 bytes |
| `GET /static/dashboard.html` | `404` |
| `GET /static/logo.svg` | `404` |
| `GET /static/favicon.ico` | `404` |

Headless Chromium loaded `http://127.0.0.1:36651/dashboard.html` with the full
UI mode selected (`localStorage['ozma-ui-mode']='full'`). Results:

* Page title `Ozma — Dashboard`, HTTP 200.
* No page errors, no console errors, no failed requests, and no HTTP ≥400
  responses on that page.
* Node count `1`; one Rust node card with data id
  `ozma-gate2-power-ctrl._ozma._udp.local.` and text including
  `host 127.0.0.1`, `hw x86_64`, `role compute`, `fw 0.1.0`,
  capabilities `HID VIDEO POWER`, and buttons `On Off Reset Force Cycle`,
  `Activate`, `Stream`, `Console`, `Terminal`, `USB`.
* Snapshot preview `img[data-snap-url]` completed with
  `naturalWidth=720`, `naturalHeight=400`; source was
  `/api/v1/streams/ozma-gate2-power-ctrl._ozma._udp.local./snapshot`.
* Clicking `Activate` changed the card to `ACTIVE`.
* Clicking the dashboard `⏻ On` power button produced inline status `on OK`.
* Screenshots were captured in the temporary evidence directory and removed
  with it.

Cosmetic/integration gaps observed:

1. `/static/logo.svg` and `/static/favicon.ico` are referenced by the HTML but
   404, because the only static mount is `app.mount("/", StaticFiles(...))`.
   The correct URLs are `/logo.svg` and `/favicon.ico`. The page still renders.
2. The default dashboard mode is `simple`, which has no snapshot preview and no
   power buttons. The full node card/preview requires the full UI mode.
3. The dashboard only renders power buttons when the node's advertised
   `capabilities` include `power`. The node does **not** add that capability
   automatically from `--power-provider qmp`; this run had to pass
   `--cap hid,video,power`. The documented example `--cap hid,video,audio`
   would show no power buttons even though `/power/state` and the power proxy
   work. That is a documentation/integration gap, not a route failure.

## 8. Pass / fail / skip summary

| Area | Result | Note |
|---|---|---|
| Disposable QEMU + QMP greeting/query/screendump | **PASS** | TCG, `-display none`, VGA |
| Node `/power/state` | **PASS** | real QMP `query-status` |
| Node `POST /power/stop` → pause | **PASS** | QEMU `stop`; state `paused` |
| Node `POST /power/on` → resume | **PASS** | QEMU `cont`; state `running` |
| Node `POST /power/reset` | **PASS** | QEMU `system_reset`; state `running` |
| Invalid power action | **PASS** (400) | typed error |
| Hard `force-off`/destroy | **SKIP** | not called; QMP `stop` is a pause |
| Controller registration + heartbeat | **PASS** | `last_seen` increased |
| Controller list/detail | **PASS** | one node, correct ports/caps |
| Controller power state proxy | **PASS** | 200 `running` |
| Controller power action proxy | **PASS** | stop→paused, on→running |
| Controller snapshot proxy | **PASS** | real 720×400 JPEG |
| Controller HLS playlist/segment proxy | **PASS** | h264 720×400, 17,108-byte segment |
| Dashboard node card + preview | **PASS** | Chromium, 720×400 preview |
| Dashboard activate + power button | **PASS** | `ACTIVE`, `on OK` |
| `/static/*` assets | **FAIL (cosmetic)** | logo/favicon 404; correct paths are root-relative |
| Controller graceful shutdown | **FAIL** | traceback + orphaned capture ffmpeg; see below |

## 9. Failures and remaining work

1. **Power semantics.** `/power/stop` is named `force-off` in its response but
   only pauses QEMU. A hard destroy is not implemented on the QMP backend. This
   must be documented or changed before claiming hard power-off parity.
2. **Controller shutdown traceback.** On SIGINT the repaired daemon logged:

   ```text
   File ".../controller/main.py", line 1001, in run
       from controller.gql.subscriptions import stop_event_router
   ModuleNotFoundError: No module named 'controller'
   ```

   The controller then exited and left its `/dev/video0` capture ffmpeg child
   running; that orphan (whose command line pointed at the temporary controller
   copy) was killed manually during cleanup. This is a shutdown/cleanup bug, not
   a blocked HTTP route, so no controller edit was made under this task's
   minimal-fix rule. It should be fixed before treating controller shutdown as
   clean.
3. **Dashboard static paths and power capability.** See §7 gaps 1–3.
4. **HLS stale-segment race.** A cached playlist can reference a segment already
   deleted by `delete_segments`, producing a controller 503. The route itself
   works when the current segment is requested.
5. **An initial controller launch was reaped.** The first nohup daemon died
   without a traceback before the browser run. The reported run was relaunched
   with `setsid`, stayed up for the full validation, and was the one used for
   all controller/dashboard evidence. The cause of the first exit was not
   established and is not counted as a controller result.

## Remaining Gate 2 blocker

The scoped power-action and controller/dashboard API paths now work against a
real disposable QMP source. Gate 2 still cannot be claimed because:

* there is **no guest OS**, so guest-visible behavior (ACPI handling, a key or
  pointer observed in a guest, resolution changes) is unproven;
* hard power-off/destroy was not exercised, and the QMP backend cannot destroy;
* WebRTC negotiation/media, audio, and D-Bus display capture were not exercised
  here and remain separately blocked;
* controller graceful shutdown/cleanup and the dashboard static/capability gaps
  above should be fixed before a final Gate 2 evidence run.

## Cleanup

| Item | PID / path | Final state |
|---|---|---|
| Disposable QEMU | 3845053 | terminated, socket removed with temp dir |
| Node run 1 (`--cap hid,video`) | 3845568 | exited before run 2 |
| Node run 2 (`--cap hid,video,power`) | 3854207 | SIGINT, `Ozma node shutting down`, exited |
| Node HLS ffmpeg | 3854248 | reaped by the node |
| Controller | 3850058 | SIGINT; shutdown traceback, then exited |
| Controller capture ffmpeg | 3850981 | orphaned by the shutdown bug; terminated during cleanup |
| Temp root + venv | `/tmp/opencode/ozma-gate2-power-controller` | removed |

Final checks:

```text
$ test -e /tmp/opencode/ozma-gate2-power-controller && echo EXISTS || echo ABSENT
ABSENT

$ ps -eo pid,args | grep -E 'ozma-node .*ozma-gate2-power-ctrl|qemu-system-x86_64 .*ozma-gate2-power-ctrl|ffmpeg.*ozma-gate2-power-ctrl|main.py --host 127.0.0.1 --port 36651'
(none)

$ ss -lntup | grep -E ':(44915|44917|60742|36651|47996|7381)\b'
(none)

$ git status --short
(pre-existing unrelated edits only; this report is the only file added by this validation)
```

No libvirt domain was started, stopped, defined, edited, or destroyed. No `sudo`
or global package install was used. No commit was made. The only file added to
the repository by this validation is this report.

Two unrelated disposable QEMU processes from other sessions
(`ozma-dbus-debug`, `ozma-gate2-webrtc`) were present during the final process
check; they do not match this run's name or sockets and were not touched.
