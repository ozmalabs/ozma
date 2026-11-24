# Gate 2 environment report

Read-only audit of the host that would run the Gate 2 live validation from
[`gate2-validation.md`](gate2-validation.md), plus the safe regression runs that
can execute without a VM, controller, or node. **Gate 2 has not been run and is
not claimed.**

> **No VM, controller, or node state was changed by this audit.** No VM was
> started, stopped, defined, edited, or destroyed; no `virsh` mutating verb was
> used; no `sudo`; no libvirt/QEMU configuration or XML was modified; no PipeWire
> or PulseAudio sink/source was created or removed; the controller, `ozma-node`,
> ffmpeg, and the WebRTC helper were never started. The only writes are this
> report, `/tmp/opencode/*` logs, a `/tmp/opencode/gate2-report/` harness log, and
> a gitignored `softnode/__pycache__/*.pyc` from the requested `py_compile` check.

## 1. Run metadata

| Item | Value |
|---|---|
| Audit start | 2026-11-23T23:11:10Z (2026-11-24 10:11:10 AEDT) |
| Audit end | 2026-11-24 10:18 AEDT (approx.) |
| Host | `SFFPC`, CachyOS (Arch-like, rolling), kernel `7.3.5-2-cachyos` x86_64 |
| CPU / RAM | 12 threads, 30 GiB |
| Repo | `/home/matt/ozma/ozma`, `HEAD=65bca10` (`feat(gate2): apply HLS codec overrides and provision VM evdev/audio`) |
| Working tree | **dirty and moving**: another process was editing it throughout the audit. At 10:11 AEDT `git status --porcelain` was empty; by 10:16 it showed modifications to `ozma-node/src/{hls,main,webrtc,controller_compat}.rs`, `ozma-node/tests/parity.rs`, `ozma-softnode/src/*`, `softnode/gst_webrtc_worker.py`, and `docs/parity-matrix.md`, plus untracked `softnode/gst_webrtc_worker_test.py`. All source facts below describe the tree as read at ~10:11–10:18 AEDT and were re-checked at 10:16 AEDT. |

The dirty tree means any captured artifact is **not** tied to a single commit.
Re-run the checklist below on a frozen tree before recording Gate 2 evidence.

## 2. Tool versions

| Tool | Version / state |
|---|---|
| `virsh` | 12.9.0, default URI `qemu:///session` |
| QEMU | `qemu-system-x86_64` 11.1.3 |
| libvirt daemon | `libvirtd` active, `virtqemud` inactive |
| `ffmpeg` | n9.1 (GCC 16), with `--enable-libx264 --enable-libx265 --enable-libsvtav1 --enable-libaom --enable-libvpl --enable-nvenc` |
| Python | 3.14.10 (`/usr/bin/python3`) |
| PyGObject / GStreamer | `gi` OK; Gst/GstWebRTC/GstSdp bindings OK; `gst-inspect-1.0` 1.28.9 |
| PulseAudio/PipeWire | `pipewire` 1.6.11 + `wireplumber` + `pipewire-pulse` running; `pactl info` → "PulseAudio (on PipeWire 1.6.11)", server protocol 35 |
| pytest | 9.0.2 |
| cargo / rustc | 1.95.0 (2026-03-21 / 2026-04-14) |
| `target/debug/ozma-node` | present, built 2026-11-24 09:51 |

## 3. Prerequisite matrix

| # | Prerequisite | Status | Evidence |
|---|---|---|---|
| 1 | A running libvirt domain | **FAIL** | `virsh list --all` (`qemu:///session`) lists no domains; `virsh -c qemu:///system list --all` lists 4 domains, **all `shut off`** |
| 2 | `OZMA_QEMU_DOMAIN` supplied | **FAIL** | unset in this environment |
| 3 | QMP control socket | **FAIL** | `/run/ozma/ozma-gate2.qmp` does not exist; `/run/ozma` and `/var/run/qemu-server` do not exist |
| 4 | VNC endpoint (optional path) | **SKIP** | no `OZMA_VNC_HOST`/`OZMA_VNC_PORT`; no graphics device other than SPICE on the defined domains |
| 5 | ffmpeg runs | **PASS** | `ffmpeg -version` |
| 6 | ffmpeg HLS muxer | **PASS** | `E  hls  Apple HTTP Live Streaming` |
| 7 | ffmpeg `libx264` (HLS) | **PASS** | encoder present |
| 8 | ffmpeg `libx265` / `libsvtav1` / `libaom-av1` / `mjpeg` (codec switch) | **PASS** | all present |
| 9 | ffmpeg VNC demuxer | **FAIL (non-blocking)** | no `vnc` entry in `-demuxers`, `-formats`, `-decoders`, `-protocols`, or `-devices`; the VNC capture path cannot work on this host |
| 10 | WebRTC helper program | **PASS** | `/usr/bin/python3` |
| 11 | `softnode/gst_webrtc_worker.py` present and parses | **PASS** | `py_compile` OK (note: file was being edited concurrently) |
| 12 | Python `gi` + `Gst`, `GstWebRTC`, `GstSdp` | **PASS** | `gi.require_version(...)` + import OK |
| 13 | GStreamer `webrtcbin`, `rtph264pay`, `x264enc`, `opusenc` | **PASS** | all four found via `Gst.ElementFactory` and `gst-inspect-1.0` |
| 14 | GStreamer `nvh264enc`, `vpxenc` (not required) | MISSING | `x264enc`/`av1enc`/`openh264enc` present instead |
| 15 | HLS output dir/parent writable | **PASS** | `/tmp` writable; `/tmp/ozma-gate2-hls` absent and will be created by the node |
| 16 | PipeWire/Pulse reachable | **PASS** | `pactl info` talks to the live PipeWire pulse server; `pactl`, `pw-play`, `paplay`, `pw-cli` installed |
| 17 | Python controller deps installed | **FAIL** | missing `nacl` (PyNaCl), `strawberry` (strawberry-graphql), `toml`; also missing optional `mido`, `argon2`, `mnemonic` |
| 18 | Unmodified controller starts | **FAIL** | `python3 main.py --help` aborts: `ModuleNotFoundError: No module named 'nacl'` (`controller/gaming/moonlight_protocol.py:38`) |
| 19 | Rust node built | **PASS** | `target/debug/ozma-node` present; `cargo test -p ozma-node --test gate2_live` compiles |
| 20 | No Python soft node serving the VM | **PASS** | `pgrep -af 'soft_node\|virtual_node\|desktop_node'` → nothing relevant |
| 21 | Ports 7331 / 7380 / 7382 / 7384 free | **PASS (nothing is running)** | `ss -lntup` shows none of them listening |
| 22 | `scripts/gate2/qemu-preflight.sh` | **FAIL** 11 pass / 2 fail / 1 warn / 1 skip | see §4.4; failures are the missing domain and QMP socket |
| 23 | `scripts/gate2/run-gate2.sh` | **FAIL** (expected) | `preflight_exit=1`, `node_check_exit=1`, `result=FAIL` |

## 4. Captured commands and output (trimmed)

### 4.1 libvirt and VM inventory

```console
$ virsh --version
12.9.0
$ virsh uri
qemu:///session
$ virsh list --all
 Id   Name   State
--------------------
$ virsh -c qemu:///system list --all
 -    reviewpay-autoinstall-test      shut off
 -    reviewpay-autoinstall-test-v7   shut off
 -    reviewpay-autoinstall-test-v8   shut off
 -    winxp                           shut off
$ virsh -c qemu:///system dumpxml reviewpay-autoinstall-test | grep -Ei 'qmp|vnc|graphics type'
    <graphics type='spice' autoport='yes'>
```

All four system domains use **SPICE only**: no `<qemu:commandline>` QMP socket,
no VNC graphics. `/var/run/libvirt/qemu` exists but holds no per-domain monitor
socket because no domain is running.

### 4.2 Sockets, ports, processes

```console
$ ls -d /run/ozma /var/run/ozma /var/run/qemu-server
(no such directories)
$ find /tmp -maxdepth 2 \( -type s -o -name '*qmp*' \) | grep -i qmp
s /tmp/.tmpaGGn5S/qmp.sock   (2026-11-24 00:22 local)
s /tmp/.tmpBH0S5Y/qmp.sock   (2026-11-24 00:24 local)
s /tmp/.tmpcoxXqO/qmp.sock   (2026-11-24 00:22 local)
s /tmp/.tmpDGnLBV/qmp.sock   (2026-11-24 00:24 local)
s /tmp/.tmphdwSba/qmp.sock   (2026-11-24 00:22 local)
$ ss -lx | grep -Ei 'qmp|ozma|qemu'
(no output)
$ ss -lntup | grep -E ':(7331|7380|7382|7384)\b'
(no output)
$ pgrep -af 'soft_node|virtual_node|desktop_node|ozma-node'
(nothing relevant)
```

The five `/tmp/.tmp*/qmp.sock` files are **stale test leftovers** owned by
`matt`: no process is listening on them per `ss -lx`. They were listed only; no
connection was attempted because no socket was supplied via `OZMA_QMP_SOCKET`.

### 4.3 ffmpeg capabilities

```console
$ ffmpeg -hide_banner -version | head -2
ffmpeg version n9.1 Copyright (c) 2000-2026 the FFmpeg developers
built with gcc 16 (GCC)
$ ffmpeg -hide_banner -muxers | grep hls
  E  hls             Apple HTTP Live Streaming
$ ffmpeg -hide_banner -demuxers | grep -i vnc      ; # no output
$ ffmpeg -hide_banner -formats  | grep -i vnc      ; # no output
$ ffmpeg -hide_banner -decoders | grep -i vnc      ; # no output
$ ffmpeg -hide_banner -protocols| grep -i vnc      ; # no output
$ for e in libx264 libx265 libsvtav1 mjpeg libaom-av1; do ...; done
libx264 PRESENT  libx265 PRESENT  libsvtav1 PRESENT  mjpeg PRESENT  libaom-av1 PRESENT
```

**Host VNC demuxer status: absent.** This Arch/CachyOS ffmpeg build has no VNC
demuxer, decoder, protocol, or device, so the transitional VNC capture path
(`-f vnc`, `ozma-capture/src/vnc_display.rs`) fails with `Unknown format 'vnc'`.
The QMP `screendump` capture path is unaffected and remains the supported route.

### 4.4 WebRTC helper and GStreamer

```console
$ python3 -c 'import gi; gi.require_version("Gst","1.0"); gi.require_version("GstWebRTC","1.0")'
(no error)
$ python3 -m py_compile softnode/gst_webrtc_worker.py
(py_compile: OK)
$ python3 - <<'PY'  # Gst.ElementFactory.find per element, after Gst.init
webrtcbin: OK   rtph264pay: OK   x264enc: OK   opusenc: OK
nvh264enc: MISSING   vpxenc: MISSING   av1enc: OK   openh264enc: OK
PY
$ gst-inspect-1.0 --version | head -2
gst-inspect-1.0 version 1.28.9
GStreamer 1.28.9
```

### 4.5 Audio server

```console
$ pactl info | head -8
Server String: /run/user/1000/pulse/native
Server Name: PulseAudio (on PipeWire 1.6.11)
Server Version: 15.0.0
Default Sample Specification: float32le 2ch 48000Hz
$ pgrep -af pipewire
1564 /usr/bin/pipewire
1565 /usr/bin/wireplumber
1811 /usr/bin/pipewire-pulse
```

`pactl`, `pw-play`, `paplay`, `pw-cli` are installed and a server is reachable.
No sink, source, or module was created, moved, or removed.

### 4.6 Python controller dependencies and startup

```console
$ python3 - <<'PY'  # importlib.util.find_spec
fastapi OK  uvicorn OK  pydantic OK  zeroconf OK  aiohttp OK  evdev OK
asyncvnc OK  numpy OK  PIL OK  websockets OK  httpx OK  yaml OK  jwt OK
strawberry MISSING  nacl MISSING  toml MISSING
mido MISSING  argon2 MISSING  mnemonic MISSING
PY
$ cd controller && python3 main.py --help
argon2-cffi not installed — password backup method unavailable
mnemonic not installed — BIP39 export will use hex fallback
Traceback (most recent call last):
  File "/home/matt/ozma/ozma/controller/main.py", line 109, in <module>
    from gaming.moonlight_protocol import MoonlightProtocol
  File ".../controller/gaming/moonlight_protocol.py", line 38, in <module>
    import nacl.secret
ModuleNotFoundError: No module named 'nacl'
```

`controller/requirements.txt` lists `strawberry-graphql[fastapi]`, `pynacl`, and
`toml`; none of the three is installed in this interpreter. The controller
therefore cannot be started for a live run without installing requirements
(the Gate 2 harness never installs or starts it).

### 4.7 `scripts/gate2/qemu-preflight.sh`

Default invocation (no `OZMA_QEMU_DOMAIN`, no `OZMA_QMP_SOCKET`):

```console
$ sh scripts/gate2/qemu-preflight.sh
FAIL  OZMA_QEMU_DOMAIN (or OZMA_LIBVIRT_DOMAIN) is not set; list domains with: virsh list --all
FAIL  QMP control socket path is not set
PASS  ffmpeg 'ffmpeg' runs
WARN  ffmpeg has no VNC demuxer; VNC capture is unavailable (QMP capture can still work)
PASS  ffmpeg has the libx264 encoder required by HLS
PASS  ffmpeg has the HLS muxer
PASS  HLS output parent '/tmp' is writable; the node will create '/tmp/ozma-gate2-hls'
PASS  WebRTC helper program 'python3' is on PATH
PASS  WebRTC helper script '.../softnode/gst_webrtc_worker.py' exists
PASS  WebRTC helper script parses as Python
PASS  Python has GStreamer Gst/GstWebRTC/GstSdp bindings
PASS  GStreamer element 'webrtcbin' is available
PASS  GStreamer element 'rtph264pay' is available
PASS  GStreamer has an H.264 encoder (x264enc or nvh264enc)
SKIP  controller check disabled (OZMA_CONTROLLER_URL is not set)
Summary: 11 passed, 2 failed, 1 warnings, 1 skipped     (exit 1)
```

Tooling-only invocation `OZMA_GATE2_SKIP_LIBVIRT=1 OZMA_GATE2_SKIP_QMP=1` →
`11 passed, 0 failed, 1 warnings, 3 skipped` (exit 0). This is not a Gate 2 pass.

### 4.8 Full harness with placeholder node/controller

```console
$ OZMA_NODE_URL=http://127.0.0.1:7382 \
  OZMA_NODE_ID=ozma-vm-gate2._ozma._udp.local. \
  OZMA_CONTROLLER_URL=http://127.0.0.1:7380 \
  OZMA_QMP_SOCKET=/run/ozma/ozma-gate2.qmp \
  sh scripts/gate2/run-gate2.sh
FAIL  QMP control socket '/run/ozma/ozma-gate2.qmp' does not exist; ...
FAIL  controller 'http://127.0.0.1:7380' did not return a healthy /health response (000)
FAIL  /health returned 000 (curl: (7) Could not connect to server)
FAIL  /api/v1/connection returned 000 (...)
FAIL  /power/state returned 000 (...)
SKIP  HLS state is unavailable ... / SKIP  WebRTC state is unavailable ...
FAIL  controller /health|/api/v1/nodes|node detail|power proxy returned 000
Summary: 0 passed, 7 failed, 0 warnings, 4 skipped
preflight_exit=1  node_check_exit=1  result=FAIL
report=/tmp/opencode/gate2-report/gate2-20261123T231448Z.log
```

## 5. Blockers preventing a live Gate 2 run

Ordered by what must be resolved first:

1. **No gate2 VM exists.** `qemu:///session` is empty; `qemu:///system` has only
   four `shut off` SPICE domains with no QMP socket and no VNC. A live run needs
   a running domain (or direct-QEMU process) exposing
   `-qmp unix:/run/ozma/ozma-gate2.qmp,server=on,wait=off`. Provisioning and
   starting that VM is out of scope for this audit and must not reuse the
   existing `reviewpay-*`/`winxp` domains without an explicit decision.
2. **QMP socket absent.** `/run/ozma` does not exist, so both the preflight QMP
   greeting check and the node's `--qmp-socket` power/input/capture backends
   cannot start.
3. **The unmodified controller cannot start.** `nacl`, `strawberry`, and `toml`
   are missing from the system Python 3.14; `controller/main.py` aborts at
   `import nacl.secret`. Install `controller/requirements.txt`
   (`pynacl>=1.5`, `strawberry-graphql[fastapi]>=0.230`, `toml>=0.10`, and
   optionally `mido`, `argon2-cffi`, `mnemonic`) before any live run.
4. **Nothing is serving the node/controller ports.** 7331/7380/7382/7384 are
   all free, so registration, discovery, and the dashboard proxy cannot be
   exercised.
5. **`OZMA_QEMU_DOMAIN` is not set**, so `virsh domstate` and the libvirt power
   backend cannot be checked. A libvirt-backed run also needs
   `OZMA_LIBVIRT_URI=qemu:///system` here, because the default URI is the
   per-user session bus with zero domains.
6. **ffmpeg has no VNC support** (warning, not a blocker for the default
   path). Use QMP `screendump` capture; do not plan a VNC capture artifact on
   this host.

## 6. Regression runs performed

| Run | Command | Result |
|---|---|---|
| Rust opt-in live test, no env | `cargo test -p ozma-node --test gate2_live -- --ignored --nocapture` | **PASS (clean skip)**: prints `SKIP gate2_live_node_surface: OZMA_GATE2_NODE_URL is not set`, `1 passed` |
| Controller unit suite | `python3 -m pytest tests/unit/controller -q --continue-on-collection-errors` | **2256 passed, 45 failed, 4 skipped, 28 errors** in 8.74 s; 2 collection errors |
| Node reconciler | `python3 -m pytest tests/test_node_reconciler.py -q` | 10 passed |
| Gate 2 preflight | `sh scripts/gate2/qemu-preflight.sh` | FAIL (11 pass / 2 fail / 1 warn / 1 skip) |
| Gate 2 harness | `sh scripts/gate2/run-gate2.sh` | FAIL (expected: no services) |

The gate2_live test was compiled against the concurrent working tree; it only
exercised its documented skip path because `OZMA_GATE2_NODE_URL` was unset.

### 6.1 Representative controller test failures

All 45 failures / 28 errors come from **missing host dependencies or a local
port conflict**, not from node behavior. None of them exercises the Rust node,
and none of them is a Rust-compat failure in itself.

| Test | Outcome | Root cause |
|---|---|---|
| `test_graphql.py::TestGraphQLSchema::test_create_schema` | FAIL | `ModuleNotFoundError: No module named 'strawberry'` (13 failures in this file) |
| `test_graphql_subscriptions.py::TestSubscriptionRegistry::test_register_and_unregister` | FAIL | same `strawberry` import (20 failures in this file) |
| `test_graphql_mutations.py` (collection) | ERROR | same `strawberry` import; entire module cannot be collected |
| `test_transport.py::TestIdentityKeyPair::test_sign_and_verify` | FAIL | `RuntimeError: PyNaCl required for key generation` (9 failures in this file) |
| `test_auth.py::TestJWT::test_create_and_verify` | ERROR | PyNaCl missing during fixture setup (12 errors in this file) |
| `test_pairing.py::TestMeshCA::test_initialise_creates_ca_keypair` | ERROR | PyNaCl missing (14 errors + 1 failure in this file) |
| `test_idp.py::TestJWKS::test_jwks_with_signing_key` | FAIL | PyNaCl missing |
| `test_moonlight_pairing.py::TestMoonlightPairingServer::test_serverinfo_endpoint_unpaired` | FAIL | `OSError: [Errno 98] address already in use` — host port 47984 is already listening (an existing Sunshine/Moonlight service), unrelated to the controller |
| `test_capture_to_moonlight.py` (collection) | ERROR | `ModuleNotFoundError: No module named 'toml'` (`controller/gaming/gstreamer_pipeline.py:44`) |

After installing `pynacl`, `strawberry-graphql`, and `toml`, and stopping or
isolating the 47984 listener for the pairing test, re-run the suite before
treating the controller regression row as evidence.

## 7. Contract mismatches

These are controller↔Rust-node HTTP contract gaps found by static inspection of
the live tree. The P0/P1 snapshot/hints/metrics routes and the P2 aliases/stubs
were subsequently implemented in `ozma-node` (see each item); none has been
validated against a running controller/dashboard in this audit.

### 7.1 Public vs guarded controller routes

`ozma-node` assembles the HTTP app from two compatibility routers. The control
router ([`compat_router`](../ozma-node/src/controller_compat.rs)) is merged
**before** the bearer-token layer; the read-only router (`public_router`) is
merged **after** it, exactly like the HLS `/stream/*` media service. A request
without `Authorization: Bearer <token>` therefore succeeds only on the public
surface; every other controller route still returns 401 before any handler runs.

| Route | Access with `--api-token` | Purpose / controller caller |
|---|---|---|
| `GET /display/snapshot` | public | One-shot JPEG; `controller/stream.py`, `controller/terminal_bridge.py`, `controller/api.py` |
| `GET /display/info` | public | Capture source metadata (read-only) |
| `GET /ui/hints` | public | Empty/no-agent hint shape; `controller/agent_engine.py`, `controller/api.py` |
| `GET /node/status` | public | Connection summary; `controller/mesh_intelligence.py` |
| `GET /agent/metrics` | public | Flat host metrics JSON; `controller/device_metrics.py` |
| `GET /audio/nodes` | public (typed 501) | No graph enumeration; always `audio-nodes-unsupported` |
| `POST /audio/sweep` | public (typed 501) | No room-correction engine; always `audio-sweep-unsupported` |
| `GET /stream/{file}` | public | HLS media; `controller/api.py::stream_hls_proxy` |
| `GET /display/mjpeg` | guarded (401 without token) | Typed 501; no controller caller |
| `GET /api/v1/connection` | guarded | Compatibility status document |
| `GET /power/state`, `POST /power/{action}` | guarded | Power control |
| `POST /input/{key,mouse,type}` | guarded | Input injection |
| `GET/POST /codec` | guarded | Codec status / runtime override |
| `POST /audio/{apply,remove,play,stop}` | guarded | Audio routing control |
| `POST /webrtc/{offer,bitrate}` | guarded | WebRTC helper signaling |
| `/usb`, `/rgb/*`, `/current`, `/session/init`, `/serial/*` | guarded | Typed 501 legacy stubs |

No public response contains the configured API token, an `Authorization`
header, or an environment dump. The metrics document exposes host resource
totals by design (the controller's metrics panel consumes them) and names every
metric it cannot measure instead of reporting a zero.

1. **P0 — `GET /display/snapshot` was missing on the Rust node. Resolved in code
   (not yet live-validated).** The controller consumes a raw-JPEG snapshot at
   several places:
   - `controller/api.py:3464` (`/api/v1/streams/{id}/snapshot` proxy)
   - `controller/api.py:5554` (`/api/v1/remote/{id}/screenshot`)
   - `controller/stream.py:719` (`_SoftNodeStream` frame pull)
   - `controller/terminal_bridge.py:511` (terminal frame source)

   `ozma-node` now serves `GET /display/snapshot` from a one-shot capture
   (`ozma-node/src/display_compat.rs`): it opens the node's first configured
   QMP/D-Bus/VNC/Looking Glass/V4L2/PipeWire source, reads exactly one `Frame`,
   and encodes real pixels with `ozma_encode::encode_jpeg_rgb`/`encode_jpeg_frame`
   (`jpeg_encoder`). RGB24/BGRA are encoded directly and MJPEG is passed through
   only when it carries a JPEG SOI marker; other layouts are a typed 503. The
   response is `Content-Type: image/jpeg` with `Cache-Control: no-store`, and
   capture and JPEG encoding are each bounded by a 5 s default timeout. A
   missing source, busy device, timeout, or unsupported format returns a typed
   JSON 503/504 with `code`, `error`, and `remediation` instead of a fake image. `GET /display/info` reports the source
   id/name, width/height, pixel format, and snapshot availability without opening
   the device. `GET /display/mjpeg` is a guarded typed 501 because no multipart
   stream is implemented.

   **Controller requirement (resolved without a controller change):**
   `display/snapshot` and `display/info` are mounted in
   `controller_compat::public_router`, which `ozma-node` merges **after** the
   bearer-token layer (like `/stream/*`). The unmodified controller's snapshot
   callers therefore work when `--api-token` is set; the control routes remain
   guarded. The dashboard live view has **not** been run against this route yet.
2. **P1 — `/ui/hints` was missing. Resolved in code.** Both
   `controller/agent_engine.py:620` and `controller/api.py:3984` call
   `http://<node>:<api_port>/ui/hints?level=...`. The node now returns the
   minimal shape (`windows`, `focused_window`, `controls`, `focused_control`,
   `level`) with an empty hints list and `available:false`, because no in-guest
   accessibility agent is connected. It never invents windows or controls. It is
   public (merged after the bearer guard) because both callers send no token.
3. **P1 — `/agent/metrics` path mismatch. Resolved as an honest JSON
   document.** `controller/device_metrics.py:340` polls `/agent/metrics`,
   decodes JSON, and calls `update_batch`, which only records top-level numbers.
   The node now returns `200 application/json` with a flat document:
   `cpu_usage` (percent), `ram_used`/`ram_total` and `disk_used`/`disk_total`
   (bytes), `net_rx_rate`/`net_tx_rate` (bytes/s from `/proc/net/dev` deltas),
   and `uptime` (seconds), plus `ok`, `available`, `source`, `scope`,
   `node_id`, `unavailable`, and `notes`. Reads are
   bounded to `/proc/{stat,meminfo,net/dev,uptime}` (256 KiB each) and one
   `statvfs`; there is no subprocess or network poll. Missing values (CPU
   temperature, GPU, fan, power draw, or a first network sample) are omitted
   and listed in `unavailable`; they are never reported as invented zeros. The
   route is public because the controller poller sends no token.
4. **P2 — Other controller-consumed node routes.** `GET /node/status` is
   public and aliases the connection/health summary (`controller/mesh_intelligence.py`
   polls it without a token). `GET /audio/nodes` and `POST /audio/sweep` are
   public typed 501 stubs with the soft-node shape: `AudioRouter` has no graph
   enumeration or room-correction operation, so `/audio/nodes` returns
   `{"nodes":[],"links":[],"default_sink":null,"default_source":null,...}` and
   `/audio/sweep` returns `{"ok":false,"available":false,"profile":null,...}`.
   Neither fabricates a graph or profile. `/usb`, `/rgb/state`, `/rgb/effect`,
   `/current`, `/session/init`, and `/serial/*` return guarded typed 501 JSON
   with `ok:false`, a stable `code`, `error`, and `remediation`; none returns a
   success-shaped no-op. `/bluetooth/pair-keys/import` and `/api/v1/rpa/*`
   remain unimplemented and are listed as known limits rather than parity.
5. **P2 — `/codec` has no controller caller.** The Rust node implements
   `GET/POST /codec`, but the controller's codec endpoints
   (`/api/v1/streams/{id}/codec`) operate on its own `StreamManager` and never
   proxy to the node. The node route is currently exercised only by node tests
   and direct calls; do not count it as controller-verified.
6. **P3 — Doc drift on `audio`.** `docs/gate2-validation.md` §3 expects
   `"audio": "not-implemented"`, but `ConnectionResponse` reports
   `"not-configured"` when no audio router is configured and
   `"running"`/`"failed"` otherwise (`ozma-node/src/controller_compat.rs:1386`).
   Either the doc or the status vocabulary should be reconciled before capturing
   the connection artifact.
7. **Confirmed matching contracts** (no action): direct registration payload
   (`id`/`host`/`port` typed, all TXT fields as strings), `/health` JSON
   (`{"ok":true,...}`, unauthenticated), `/api/v1/connection` `hls`/`webrtc`/
   `input`/`power` shapes, `/power/state`, `/power/{action}` names (`on`/`off`/
   `reset`/`force-off`), `/input/{key,mouse,type}` bodies,
   `/audio/{apply,remove,play,stop}` bodies, `/webrtc/{offer,bitrate}`,
   `/display/{snapshot,info}`, `/ui/hints`, `/node/status`, `/agent/metrics`,
   `/stream/{file}` served without a bearer token, and the `stream_url`
   composition (`http://<host>:<stream_port>/stream/stream.m3u8`). The public/
   guarded split is in §7.1.

## 8. Next-run checklist

Provisioning is separate. Once a disposable VM is running with a QMP socket (and
optionally VNC), and the controller requirements are installed:

```bash
cd /home/matt/ozma/ozma

# 0. Freeze the tree and install controller deps (the only state-changing step).
git status --short                 # expect empty / known-good
python3 -m pip install -r controller/requirements.txt

# 1. Confirm the VM is already running and its QMP socket answers.
export OZMA_QEMU_DOMAIN=ozma-gate2
export OZMA_LIBVIRT_URI=qemu:///system      # default is qemu:///session here
export OZMA_QMP_SOCKET=/run/ozma/ozma-gate2.qmp
virsh --connect "$OZMA_LIBVIRT_URI" domstate "$OZMA_QEMU_DOMAIN"
sh scripts/gate2/qemu-preflight.sh          # expect 0 failed

# 2. Terminal 1: unmodified controller (needs pynacl/strawberry/toml).
cd controller && python3 main.py --host 127.0.0.1 --port 7380
curl -sS http://127.0.0.1:7380/health

# 3. Terminal 2: Rust VM node.
cd /home/matt/ozma/ozma
NODE_NAME=ozma-vm-gate2
NODE_ID="${NODE_NAME}._ozma._udp.local."
NODE=http://127.0.0.1:7382
CTRL=http://127.0.0.1:7380
HLS_DIR=/tmp/ozma-gate2-hls
./target/debug/ozma-node \
  --name "$NODE_NAME" --profile vm \
  --qmp-socket /run/ozma/ozma-gate2.qmp \
  --input-provider qmp --power-provider qmp \
  --http-port 7382 --hid-udp-port 7331 \
  --cap hid,video,audio --machine-class server \
  --hls --hls-dir "$HLS_DIR" --hls-fps 15 --hls-bitrate-kbps 4000 \
  --webrtc-helper /usr/bin/python3 \
  --webrtc-helper-script "$PWD/softnode/gst_webrtc_worker.py" \
  --webrtc-width 1280 --webrtc-height 720 --webrtc-fps 15 \
  --register-url "$CTRL"

# 4. Terminal 3: harness (preflight + node/controller checks).
export OZMA_NODE_URL=$NODE
export OZMA_NODE_ID=$NODE_ID
export OZMA_CONTROLLER_URL=$CTRL
export OZMA_HLS_DIR=$HLS_DIR
export OZMA_WEBRTC_HELPER=/usr/bin/python3
export OZMA_WEBRTC_HELPER_SCRIPT="$PWD/softnode/gst_webrtc_worker.py"
# optional VNC only on a VNC-capable ffmpeg build: this host has none.
scripts/gate2/run-gate2.sh

# 5. Rust opt-in live surface check (skips unless the env is set).
OZMA_GATE2_NODE_URL=$NODE \
OZMA_GATE2_NODE_ID=$NODE_ID \
OZMA_GATE2_WEBRTC_SDP_FILE=/path/to/browser-offer.json \
cargo test -p ozma-node --test gate2_live -- --ignored --nocapture

# 6. Manual evidence rows (also required): /api/v1/nodes JSON + activate,
#    node and controller /power/state, guest-visible KEY_A + pointer,
#    two HLS playlist fetches, browser WebRTC media, dashboard screenshots.
```

Do not set `--api-token`/`OZMA_API_TOKEN` for controller-proxied validation (the
unmodified controller proxy sends no node bearer token). Do not call
`/power/{off,stop,reset}` on a VM you care about. Never use `virsh destroy` on a
VM you did not create for this run.
