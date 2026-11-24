# Gate 2 live QMP/capture/HLS report

**Scope:** the Rust node's QMP `screendump` → capture → HLS path and the
one-shot `/display/snapshot` path, exercised against a real, disposable QEMU
process with **no guest OS**.

**This is not a Gate 2 pass.** It covers only the QMP/capture/HLS/snapshot
portion. It does not cover discovery/registration, the unmodified Python
controller, the dashboard, guest-visible input, WebRTC, audio, or power
*actions*. Those still need a guest and/or controller. See
[Remaining Gate 2 blockers](#08-remaining-gate-2-blockers-after-the-rerun).

> No libvirt domain was started, stopped, defined, edited, or destroyed. No
> `virsh` mutating verb was used. No `sudo` or package install was used. The
> only VM was a direct, headless `qemu-system-x86_64` process with a temporary
> QMP Unix socket and no disk or network device. Every process and temporary
> file was removed at the end; see [Cleanup](#07-cleanup).

> **Post-fix status (2026-11-24 02:47–02:51Z):** the shared-QMP single-socket
> rerun now passes power, HLS, snapshots, and QMP input concurrently. See
> [§0 Post-fix rerun](#0-post-fix-rerun). Sections 1–10 below are the
> **before-fix historical run** and are retained as evidence.

## 0. Post-fix rerun

**Result: PASS for the shared-QMP single-socket path.** With one `--qmp-socket`,
`--power-provider qmp`, `--input-provider qmp`, and `--hls`, the node served
power, HLS, snapshots, and QMP input concurrently without a second QMP
connection or a greeting timeout. D-Bus display capture still fails; see
[§0.5](#05-d-bus-still-fails).

### 0.1 Build and environment

| Item | Value |
|---|---|
| Run | 2026-11-24 13:47–13:51 AEDT (2026-11-24T02:47–02:51Z) |
| Git HEAD | `1e48641f347c871fbaf5b0dbe2eb4a160e7d018e` |
| Working tree | dirty: the shared-QMP/HLS fixes plus unrelated concurrent edits; nothing was committed |
| Build | `cargo build -p ozma-node` → exit 0; `ozma-node` 0.1.0, binary timestamp 13:45 |
| QEMU | `qemu-system-x86_64` 11.1.3 |
| KVM | available; **TCG used** (`-machine pc,accel=tcg`) |
| ffmpeg | n9.1 |
| Temp dir | `/tmp/opencode/ozma-gate2-rerun` (removed) |
| Ports | HTTP/TCP 47233, QUIC/UDP 47235, HID/UDP 60271 |

### 0.2 Disposable QEMU and manual QMP

A USB keyboard was added so the QMP input test has a real input device behind
it; the key was sent only to this disposable, headless VM.

```bash
GDIR=/tmp/opencode/ozma-gate2-rerun
qemu-system-x86_64 \
  -name ozma-gate2-rerun \
  -machine pc,accel=tcg \
  -m 256 \
  -nodefaults \
  -device VGA \
  -device qemu-xhci,id=xhci \
  -device usb-kbd,bus=xhci.0 \
  -display none \
  -qmp "unix:$GDIR/qmp.sock,server=on,wait=off" \
  -pidfile "$GDIR/qemu.pid" \
  -daemonize
```

Exit `0`, pid `3824558`, empty stderr log. Manual QMP check:

```text
greeting: QEMU 11.1.3, capabilities ["oob"]
qmp_capabilities: {"return": {}}
query-status: {"return": {"running": true, "status": "running"}}
screendump: {"return": {}}
input-send-event key down: {"return": {}}
input-send-event key up:   {"return": {}}
PPM: P6 720x400 maxval=255, size=864015, sha256=8c27567f…36c8
```

### 0.3 Node command

```bash
RUST_LOG=info,ozma_input=debug,ozma_qmp=debug ./target/debug/ozma-node \
  --name ozma-gate2-rerun \
  --profile vm \
  --qmp-socket "$GDIR/qmp.sock" \
  --input-provider qmp \
  --power-provider qmp \
  --http-port 47233 \
  --hid-udp-port 60271 \
  --cap hid,video \
  --hls \
  --hls-dir "$GDIR/hls" \
  --hls-fps 5 \
  --hls-bitrate-kbps 1000 \
  --ffmpeg-bin /usr/bin/ffmpeg
```

`/health` returned 200 and `/api/v1/connection` reported all roles ready:

```json
"power":{"available":true,"status":"running"},
"input":{"sink_kind":"qmp","available":true,"injecting":true},
"hls":"running","hls_error":null
```

Log excerpt:

```text
INFO ozma_qmp::client: QMP control connected path=.../qmp.sock
INFO ozma_node: QMP control socket: one shared client for power, capture, snapshots, and input owner="node-shared-control"
INFO ozma_node: Input sink ready sink="qmp"
INFO ozma_node: QMP power backend uses the shared control client
INFO ozma_node: HLS pipeline started ...
```

`ss -x` showed exactly one `ESTAB` connection on `qmp.sock` throughout the
concurrent checks, and no second control connection was opened.

### 0.4 Concurrent live results

| Check | Result | Evidence |
|---|---|---|
| Single shared QMP connection | **PASS** | one `u_str ESTAB ... qmp.sock`; no second connection |
| `/power/state` while HLS runs | **PASS** | 200 `{"available":true,"status":"running"}` at 02:49:36 and 02:50:03 |
| HLS target / segment duration | **PASS** | `#EXT-X-TARGETDURATION:1`, `#EXTINF:1.000000`, ffprobe `duration=1.000000` |
| HLS playlist advance | **PASS** | `MEDIA-SEQUENCE:74` → `79` in 0.8 s; later `146`, `231` |
| HLS segment over HTTP | **PASS** | 200 `video/vnd.dlna.mpeg-tts`, 17,296 bytes, H.264 720×400, TS sync `0x47` |
| Two consecutive snapshots while HLS runs | **PASS** | two 200 `image/jpeg`, 19,944 bytes, 720×400; third also 200 |
| `POST /input/key` through shared client | **PASS** | down/up 200 `{"ok":true,"sink":"qmp"}` |
| Input actually sent on shared connection | **PASS** | `DEBUG ozma_input::qmp: QMP input events sent on shared client count=1` twice |
| QMP greeting/timeout/failure log lines | **PASS** | none found by the error scan below |

Playlist sample during the run:

```text
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:1
#EXT-X-MEDIA-SEQUENCE:74
#EXTINF:1.000000,
seg_00074.ts
#EXTINF:1.000000,
seg_00075.ts
#EXTINF:1.000000,
seg_00076.ts
#EXTINF:1.000000,
seg_00077.ts
```

After 0.8 s the same endpoint returned `MEDIA-SEQUENCE:79` with
`seg_00079.ts` … `seg_00082.ts`. A later fetch/segment pair returned:

```text
GET /stream/stream.m3u8 -> 200, #EXT-X-TARGETDURATION:1,
                           #EXTINF:1.000000, seg_00146..00149
GET /stream/seg_00149.ts -> 200, content-length: 17296
ffprobe: h264, 720x400, yuv444p, duration 1.000000, size 17296
TS: 474011150042f0250001c10000ff01ff, sync 0x47 at 0 and 188
```

Snapshot headers and geometry while HLS was running:

```text
HTTP/1.1 200 OK
content-type: image/jpeg
cache-control: no-store
content-length: 19944

snapshot 1: 720x400, SOF0, FF D8 FF … FF D9, sha256 56b26817…9960
snapshot 2: 720x400, SOF0, FF D8 FF … FF D9, sha256 56b26817…9960
snapshot 3: 720x400, SOF0, FF D8 FF … FF D9, sha256 56b26817…9960
```

Input responses:

```text
POST /input/key {"keycode":30,"down":true}  -> 200 {"ok":true,"sink":"qmp"}
POST /input/key {"keycode":30,"down":false} -> 200 {"ok":true,"sink":"qmp"}
DEBUG ozma_input::qmp: QMP input events sent on shared client count=1
DEBUG ozma_input::qmp: QMP input events sent on shared client count=1
```

The live ffmpeg argv confirms the keyframe fix:

```text
/usr/bin/ffmpeg -hide_banner -loglevel warning -f rawvideo -pix_fmt rgb24 \
  -s 720x400 -r 5 -i - -an -c:v libx264 -preset veryfast -tune zerolatency \
  -b:v 1000k -g 5 -keyint_min 5 -sc_threshold 0 \
  -force_key_frames expr:gte(t,n_forced*1) \
  -f hls -hls_time 1 -hls_list_size 4 -hls_flags delete_segments \
  -hls_segment_filename .../hls/seg_%05d.ts .../hls/stream.m3u8
```

The QMP error scan was clean:

```text
$ grep -Ein 'greeting|timed out|QMP connect failed|QMP input send.*failed|QMP.*failed|ERROR' node.log
(no QMP greeting/timeout/failure/ERROR lines)
```

One pacing caveat remains: QMP capture still produces frames faster than the
declared `--hls-fps 5` input timebase (playlist sequence advanced roughly six
segments per wall-clock second), so media time is not wall-clock time. The
segment duration itself is now correct at 1.0 s.

### 0.5 D-Bus still fails

QEMU accepted `-display dbus,p2p=yes` in a second disposable TCG VM, but the
current tree's ignored real-D-Bus test still fails identically:

```text
thread 'dbus_display::tests::real_qemu_dbus_display_pushes_a_frame' panicked:
called `Result::unwrap()` on an `Err` value:
Capture { id: "dbus0",
          message: "timed out after 15s establishing the QEMU D-Bus display connection" }
test result: FAILED. 0 passed; 1 failed; ... finished in 15.00s
```

No `ozma-capture/src/dbus_display.rs` change was part of the two fixes, and the
D-Bus handoff still uses its own QMP `getfd`/`add_client` path rather than the
shared control client. D-Bus is therefore a **remaining failure**, not a
regression from the post-fix QMP work.

### 0.6 Post-fix disposition of the historical findings

* **Finding 1 (one QMP chardev = one client):** resolved for the single-socket
  QMP path by `SharedQmpClient`. Power, QMP capture, snapshots, and QMP input
  serialized through one connection in this run. D-Bus is still not multiplexed.
* **Finding 2 (50-second HLS segments):** resolved. The argv now carries
  `-g 5 -keyint_min 5 -sc_threshold 0 -force_key_frames expr:gte(t,n_forced*1)`,
  and ffprobe reports 1.000000-second segments. The media-time/wall-clock
  pacing caveat above remains.
* **Finding 3 (stale `delete_segments` note):** still open as a documentation
  bug. `docs/gate2-validation.md` §7.4 still says the argv has no
  `-hls_flags delete_segments`, while the live argv does.

### 0.7 Cleanup

| Process | PID |
|---|---|
| Main disposable QEMU (TCG, VGA + USB keyboard) | 3824558 |
| `ozma-node` shared-QMP run | 3824984 |
| D-Bus disposable QEMU | 3826422 |

Final checks after removing `/tmp/opencode/ozma-gate2-rerun` and the build log:

```text
$ ps -C ffmpeg -o pid,stat,args
(no rows)

$ ps -C qemu-system-x86_64 -o pid,stat,args
(no rows)

$ ps -eo pid,args | grep -E '[o]zma-node .*ozma-gate2-rerun'
(none)

$ ss -lntup | grep -E ':(47233|47235|60271)\b'
(none)

$ ss -lx | grep -F 'ozma-gate2-rerun'
(none)

$ test -e /tmp/opencode/ozma-gate2-rerun && echo EXISTS || echo ABSENT
ABSENT
```

An unrelated `<defunct>` `ozma-node` process (PID 3823613, parent
`opencode-pty`) predated this run and belongs to another session; it is not a
process from this validation. No sudo, libvirt, or existing domain was used.

### 0.8 Remaining Gate 2 blockers after the rerun

The single-socket QMP architecture and HLS keyframe bugs are resolved. The
remaining Gate 2 work is outside this component rerun:

* **Guest OS and guest-visible behavior:** real guest framebuffer, resolution
  changes, and a key/mouse event observed inside a guest. This run proved QMP
  accepts `input-send-event`, not that a guest reacts to it.
* **Unmodified Python controller:** discovery/registration, `/api/v1/nodes`,
  HLS/snapshot/input/power proxies, dashboard live view, and the controller
  regression suite. No controller was started here.
* **Power actions:** only `query-status`/`/power/state` was exercised. Stop,
  reset, force-off/destroy, and ACPI behavior still need a disposable guest.
* **WebRTC:** helper startup, real SDP offer/answer, browser ICE/DTLS, media.
* **Audio:** still not implemented on this node surface.
* **D-Bus display capture:** still times out establishing the QEMU D-Bus
  connection (§0.5). The D-Bus QMP `getfd`/`add_client` handoff is not routed
  through `SharedQmpClient`.
* **HLS pacing:** segments are now 1.0 s, but capture still runs faster than
  the declared 5 fps input timebase, so media time advances faster than
  wall-clock time. Decide whether capture should be rate-limited to the
  declared HLS rate.
* **Documentation:** `docs/gate2-validation.md` §7.4 still claims there is no
  `delete_segments` flag; the live argv has it.
* **Evidence:** re-run on a frozen tree before attaching the final Gate 2
  evidence to the parity matrix.

---

## Historical before-fix run (2026-11-24 02:16–02:22Z)

The sections below are retained as **before-fix** evidence. Where the rerun
supersedes a result, §0 says so explicitly.

## 1. Run metadata

| Item | Value |
|---|---|
| Run date | 2026-11-24, 13:16–13:22 AEDT (2026-11-24T02:16–02:22Z) |
| Host kernel | `7.3.5-2-cachyos` x86_64 |
| Repo | `/home/matt/ozma/ozma` |
| Git HEAD at run | `38e7dfa1628f9af61914cf0013899aa5444baeb4` |
| Node binary | `target/debug/ozma-node` 0.1.0, built 2026-11-24 10:36 |
| QEMU | `qemu-system-x86_64` 11.1.3 |
| ffmpeg | n9.1 (GCC 16), with `libx264` and the `hls` muxer |
| KVM | available (`/dev/kvm` readable/writable); **TCG was used deliberately** for reproducibility and to avoid host device state |
| Temporary dir | `/tmp/opencode/ozma-gate2-live` (removed) |
| Test ports | HTTP/TCP 45571, QUIC/UDP 45573, HID/UDP 39767 (checked free before use) |

### Working-tree caveat

`git status --porcelain` was empty at the start of the run. While the run was in
progress another process modified the tree. At the end it showed:

```text
 M ozma-node/Cargo.toml
 M ozma-node/src/api.rs
 M ozma-node/src/controller_compat.rs
 M ozma-node/src/lib.rs
 M ozma-node/src/main.rs
?? docs/gate2-live-device-report.md
?? ozma-node/src/agent_metrics.rs
```

Those changes were not made by this run and were not touched. The validated
binary was built at 10:36, before those edits, so these artifacts are tied to
HEAD `38e7dfa` plus that binary, **not** to the current working tree. Re-run on a
frozen tree before attaching Gate 2 evidence.

## 2. Disposable QEMU

```bash
GDIR=/tmp/opencode/ozma-gate2-live
qemu-system-x86_64 \
  -name ozma-gate2-live \
  -machine pc,accel=tcg \
  -m 256 \
  -nodefaults \
  -device VGA \
  -display none \
  -qmp "unix:$GDIR/qmp.sock,server=on,wait=off" \
  -pidfile "$GDIR/qemu.pid" \
  -daemonize
```

Result: exit `0`, pid `3713759`, QMP socket present, empty stderr log. The
machine booted SeaBIOS with no boot device, so the display was a static
`No bootable device` text screen at **720×400**. That is enough to prove the
capture path; it is not a guest OS.

## 3. Manual QMP verification — PASS

A small Python client connected to the QMP socket, read the greeting, sent
`qmp_capabilities`, `query-status`, and `screendump`.

```json
{"QMP": {"capabilities": ["oob"], "version": {"package": "", "qemu": {"major": 11, "micro": 3, "minor": 1}}}}
{"return": {}}
{"return": {"running": true, "status": "running"}}
{"return": {}}
```

The screendump was requested to `manual-screendump.ppm`:

| Property | Value |
|---|---|
| Size | 864,015 bytes |
| SHA-256 | `8c27567ffb3f23a19edf2a2ebec2243531ff231e2898392f36ab7c913db036c8` |
| Magic / geometry | `P6`, 720×400, maxval 255 |
| Header bytes | `P6\n720 400\n255\n` |
| Raster | 864,000 bytes (720×400×3), exact match |
| Distinct RGB colors | 2 (static black/white SeaBIOS text screen) |

An independent Python P6 parser, matching the Rust parser's documented grammar
(comments, arbitrary header whitespace, one separator before the raster),
accepted the file with no trailing bytes.

## 4. Real-QMP ignored tests

### `ozma-qmp` — PASS

```bash
OZMA_QMP_SOCKET="$GDIR/qmp.sock" \
  cargo test -p ozma-qmp --test qmp_client -- --ignored --nocapture \
  real_qemu_query_status
```

```text
running 1 test
test real_qemu_query_status ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 22 filtered out
```

### `ozma-power` — PASS

```bash
OZMA_TEST_QMP_SOCKET="$GDIR/qmp.sock" \
  cargo test -p ozma-power --test qmp_power -- --ignored --nocapture \
  real_qemu_query_status
```

```text
running 1 test
test real_qemu_query_status ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 7 filtered out
```

### `ozma-capture` real QMP test — N/A

The task brief expected an ignored real-QMP test in `ozma-capture`. It does not
exist in this tree. `ozma-capture/tests/qmp_display.rs` drives a **fake** QMP
server and has **0 ignored tests**:

```text
$ cargo test -p ozma-capture -- --ignored --list
v4l2::tests::real_v4l2_capture_yields_monotonic_frames: test
vnc_display::tests::real_vnc_ffmpeg_captures_a_frame: test
...
     Running tests/qmp_display.rs (...)
0 tests, 0 benchmarks
```

The real QMP `screendump` capture path was therefore validated live through the
node (below), not through an `ozma-capture` ignored test. No test was added or
modified.

## 5. Node runs

All nodes used:

```bash
./target/debug/ozma-node \
  --profile vm \
  --qmp-socket "$GDIR/qmp.sock" \
  --http-port 45571 \
  --hid-udp-port 39767 \
  --cap hid,video \
  --ffmpeg-bin /usr/bin/ffmpeg
```

plus the per-run flags below. Each node was stopped with `SIGTERM` and reaped.

### 5.1 The suggested flags (`recording` input + QMP power + HLS) — FAIL (before fix)

```bash
./target/debug/ozma-node \
  --name ozma-gate2-live-suggested \
  --profile vm \
  --qmp-socket "$GDIR/qmp.sock" \
  --input-provider recording \
  --power-provider qmp \
  --http-port 45571 \
  --hid-udp-port 39767 \
  --cap hid,video \
  --hls \
  --hls-dir "$GDIR/hls-suggested" \
  --hls-fps 5 \
  --hls-bitrate-kbps 1000 \
  --ffmpeg-bin /usr/bin/ffmpeg
```

Observed after `/health` returned 200:

```json
"power": {"available": true, "status": "running"},
"input": {"sink_kind": "fake-recording", "available": true, "injecting": false},
"hls": "failed",
"hls_error": "capture source `qmp0` failed: capture source `qmp0` failed: QMP connect failed: QMP command \"control greeting\" timed out after 3s"
```

```text
GET /power/state        -> 200 {"available":true,"status":"running"}
GET /stream/stream.m3u8 -> 404
GET /display/snapshot   -> 503 capture.unavailable
                           QMP command "control greeting" timed out after 3s
```

The node log confirms the order: `QMP power: connected`, then
`HLS pipeline started`, then the capture source could not connect. This is the
single-QMP-client limitation described in [Finding 1](#finding-1-one-qmp-chardev--one-client).
It describes the binary from this run; the current tree shares one control
client instead of opening independent power/HLS/snapshot clients.

An earlier run using the brief's other suggested input
(`--input-provider qmp --power-provider qmp`) behaved the same way in the
opposite order: the QMP **input** sink connected and held the socket, the power
backend then timed out, and HLS failed:

```json
"power": "not-configured",
"input": {"sink_kind": "qmp", "available": true, "injecting": true},
"hls": "failed",
"hls_error": "... QMP command \"control greeting\" timed out after 3s"
```

### 5.2 HLS-only run — PASS

To give QMP capture the socket, QMP power and QMP input were disabled for this
run:

```bash
./target/debug/ozma-node \
  --name ozma-gate2-live-hls \
  --profile vm \
  --qmp-socket "$GDIR/qmp.sock" \
  --input-provider recording \
  --power-provider none \
  --http-port 45571 \
  --hid-udp-port 39767 \
  --cap hid,video \
  --hls \
  --hls-dir "$GDIR/hls" \
  --hls-fps 5 \
  --hls-bitrate-kbps 1000 \
  --ffmpeg-bin /usr/bin/ffmpeg
```

`/health` → 200. `GET /api/v1/connection` reported `hls: "running"` after about
8 seconds (the manifest did not exist before the first segment):

```json
{"ok":true,"profile":"vm","hls":"running","hls_error":null,
 "input":{"sink_kind":"fake-recording","available":true,"injecting":false},
 "power":"not-configured","webrtc":"not-configured","audio":"not-configured"}
```

The first playlist fetch was 200 with `content-type: audio/x-mpegurl`:

```text
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:50
#EXT-X-MEDIA-SEQUENCE:0
#EXTINF:50.000000,
seg_00000.ts
```

A later fetch showed the playlist had advanced and retained four entries:

```text
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:50
#EXT-X-MEDIA-SEQUENCE:1
#EXTINF:50.000000,
seg_00001.ts
#EXTINF:50.000000,
seg_00002.ts
#EXTINF:50.000000,
seg_00003.ts
#EXTINF:50.000000,
seg_00004.ts
```

Latest segment fetched over the node's unauthenticated `/stream` route:

```text
HTTP/1.1 200 OK
content-type: video/vnd.dlna.mpeg-tts
content-length: 66364
```

| Property | Value |
|---|---|
| Size | 66,364 bytes |
| SHA-256 | `0aa82043034b35aff5904bf38a5616ddcafd0351ce7c6646efd8c74d78df1a80` |
| MPEG-TS sync | `0x47` at offset 0 and offset 188 |
| First bytes | `474011140042f0250001c10000ff01ff` |
| `ffprobe` | `h264`, 720×400, `yuv444p`, duration 50.0 s |

Observed ffmpeg argv:

```text
/usr/bin/ffmpeg -hide_banner -loglevel warning -f rawvideo -pix_fmt rgb24 \
  -s 720x400 -r 5 -i - -an -c:v libx264 -preset veryfast -tune zerolatency \
  -b:v 1000k -f hls -hls_time 1 -hls_list_size 4 \
  -hls_flags delete_segments \
  -hls_segment_filename .../hls/seg_%05d.ts .../hls/stream.m3u8
```

This proves QMP `screendump` → RGB24 → ffmpeg/libx264 → HLS over HTTP works.
The 50-second segment duration is a real behavior, not a pass for 1-second
segments; see [Finding 2](#finding-2-hls-segments-are-50-seconds-not-1-second).

While HLS held the only QMP client, a concurrent snapshot failed (historical
binary; the current tree shares the control client between HLS and snapshots):

```text
GET /display/snapshot -> 503
{"code":"capture.unavailable","details":"qmp0",
 "error":"QMP connect failed: QMP command \"control greeting\" timed out after 3s",
 "ok":false,"retryable":true,...}
```

### 5.3 Snapshot-only run — PASS

With HLS disabled, `/display/snapshot` got the QMP socket:

```bash
./target/debug/ozma-node \
  --name ozma-gate2-live-snapshot \
  --profile vm \
  --qmp-socket "$GDIR/qmp.sock" \
  --input-provider recording \
  --power-provider none \
  --http-port 45571 \
  --hid-udp-port 39767 \
  --cap hid,video
```

`GET /display/info` → 200:

```json
{"ok":true,"source_id":"qmp0","source_name":"ozma-gate2-live-snapshot VM display",
 "width":1920,"height":1080,"pixel_format":"rgb24",
 "snapshot_available":true,"snapshot_encoding":"jpeg",
 "dimensions_source":"capability-max","last_error":null}
```

Two consecutive `GET /display/snapshot` calls both returned 200:

```text
HTTP/1.1 200 OK
content-type: image/jpeg
cache-control: no-store
content-length: 19944
```

| Property | Value |
|---|---|
| Size | 19,944 bytes each |
| SHA-256 (both) | `56b268177e98eccbe3f4441e902a93e7230d50dcffb937b49677725345609960` |
| Magic / EOI | `FF D8 FF` … `FF D9` |
| SOF marker | `0xC0` (baseline) |
| Dimensions | 720×400 |

The two snapshots are byte-identical because the SeaBIOS screen is static. The
second call also proves the one-shot QMP connection is closed and reusable
between requests when nothing else holds the socket.

### 5.4 QMP power query in isolation — PASS

A node with `--input-provider recording --power-provider qmp` and no HLS or
snapshot traffic reported:

```text
GET /power/state -> 200 {"available":true,"status":"running"}
```

and `/api/v1/connection` contained `"power":{"available":true,"status":"running"}`.
A snapshot attempted against that node returned 503 with the same QMP greeting
timeout, confirming that QMP power itself works but monopolizes the chardev.

## 6. Optional D-Bus display test — FAIL (not part of the required path)

QEMU advertises the backend:

```text
$ qemu-system-x86_64 -display help
Available display backend types:
none
gtk
sdl
egl-headless
curses
spice-app
dbus
```

A second disposable QEMU was started with:

```bash
qemu-system-x86_64 \
  -name ozma-gate2-dbus \
  -machine pc,accel=tcg \
  -m 256 \
  -nodefaults \
  -device VGA \
  -display dbus,p2p=yes \
  -qmp "unix:$GDIR/dbus/qmp.sock,server=on,wait=off" \
  -pidfile "$GDIR/dbus/qemu.pid" \
  -daemonize
```

It started successfully (exit 0). The ignored real-D-Bus test then failed:

```bash
OZMA_DBUS_TEST_QMP="$GDIR/dbus/qmp.sock" \
  timeout 90s cargo test -p ozma-capture --lib -- --ignored --nocapture \
  real_qemu_dbus_display_pushes_a_frame
```

```text
thread 'dbus_display::tests::real_qemu_dbus_display_pushes_a_frame' panicked:
called `Result::unwrap()` on an `Err` value:
Capture { id: "dbus0",
          message: "timed out after 15s establishing the QEMU D-Bus display connection" }
test result: FAILED. 0 passed; 1 failed; ... finished in 15.00s
```

QEMU accepted `-display dbus,p2p=yes`; the handshake/listener setup in
`DBusDisplaySource::connect` did not complete within its 15-second bound. The
root cause was not investigated further because D-Bus is outside this run's
required QMP/capture/HLS/snapshot scope. No D-Bus success is claimed.

## 7. Before-fix findings (historical)

These are the findings from the before-fix run. §0 records the post-fix rerun;
each finding below notes its current disposition.

### Finding 1: one QMP chardev = one client

**Post-fix status:** resolved for the single-socket QMP path by
`ozma-qmp::SharedQmpClient`; see §0. D-Bus display capture still performs its
own QMP handoff and is not multiplexed through the shared client.

QEMU 11.1.3 accepts **one client at a time** on a single `-qmp` chardev. While
the node held the socket, a second manual connection failed immediately:

```text
BlockingIOError: [Errno 11] Resource temporarily unavailable
```

After the node stopped, the same connection returned the greeting (111 bytes).
The node logs show the same thing as a 3-second greeting timeout.

The historical node opened independent QMP clients for:

* input (`ozma-qmp` input sink),
* power (`QmpPower::connect`),
* HLS capture (`QmpDisplaySource`),
* each snapshot (`SnapshotControl` opened a fresh source).

Consequently the documented VM setup with **one** `--qmp-socket` could not run
input + power + HLS + snapshot concurrently. This explains the failures in §5.1
and the concurrent-snapshot 503s in §5.2. It also contradicted
`docs/gate2-validation.md` §7.5, which said "QMP screendump normally supports
multiple connections"; that is true of multiple QEMU chardevs, not one.

**Current architecture (fix implemented, live rerun pending):**

1. `ozma-qmp::SharedQmpClient` wraps one `QmpClient` plus one async command
   mutex. `main` creates exactly one shared client when `--qmp-socket` is set.
2. `QmpPower::with_shared_client`, `QmpDisplayProvider::with_shared_client`
   (and `CaptureRegistry::with_qmp_display_shared`), and
   `QmpSink::spawn_with_shared` all use that client. Power queries, HLS
   `screendump`, snapshots, and input `input-send-event` serialize; none opens
   a second control connection.
3. `--qmp-input-socket` selects `QmpSink::spawn_dedicated_input`: input uses
   the dedicated socket only, keeping the control socket single-owner.
4. A capture source opened by HLS and another opened by snapshot still each run
   their own `screendump` loop, but both call the same shared client. The
   command mutex covers the QMP round trip and the PPM read happens after it is
   released, so there is no cross-path deadlock.
5. Fake single-connection QMP tests assert that concurrent power, screendump,
   and input calls succeed and that no second connection is attempted.

**Remaining limitation:** D-Bus display capture still connects to QMP itself
for `getfd`/`add_client` and is not routed through `SharedQmpClient`. A
single-socket VM should use QMP `screendump` when power/input/snapshot must run
concurrently, or provision separate QMP chardevs. The fix above still needs a
live QEMU rerun (and the stale multi-connection claim needed correcting); see
[Remaining work](#9-remaining-work).

### Finding 2: HLS segments are 50 seconds, not 1 second

**Post-fix status:** resolved. The rerun's ffmpeg argv includes
`-g 5 -keyint_min 5 -sc_threshold 0 -force_key_frames expr:gte(t,n_forced*1)`,
and ffprobe reports 1.000000-second segments; see §0.4.

The observed ffmpeg argv has `-hls_time 1` but no `-g`/forced-keyframe option.
libx264's default keyframe interval is 250 frames; at `-r 5` that is a 50-second
GOP. The first segment and every playlist entry therefore report
`#EXTINF:50.000000` and `#EXT-X-TARGETDURATION:50`. The capture source also runs
faster than the declared 5 fps input timebase (QMP polling defaults to 30 fps),
so the playlist advanced quickly in wall-clock time while the media timeline
advanced in 50-second chunks. This matches the warning in
`docs/gate2-validation.md` §7.1 that HLS timestamps use `--hls-fps`, not
wall-clock capture timing, but the concrete keyframe consequence was not
documented.

If 1-second segments are required, the pipeline needs a keyframe interval tied
to the segment duration (for example `-g <fps>` or forced keyframes), and a
decision on how capture rate and declared input rate should relate.

### Finding 3: stale `delete_segments` note

**Post-fix status:** still open as a documentation bug; the current
`docs/gate2-validation.md` §7.4 still says the argv has no
`-hls_flags delete_segments`, while the live argv does.

The current argv **does** include `-hls_flags delete_segments`, but
`docs/gate2-validation.md` §7.4 says it does not. The code and that document
disagree; the playlist retained four entries as configured.

## 8. Before-fix pass / fail summary

| Check | Result | Notes |
|---|---|---|
| QMP greeting + `qmp_capabilities` + `query-status` | **PASS** | live QEMU 11.1.3 |
| QMP `screendump` valid P6 720×400 | **PASS** | 864,000-byte raster, exact header |
| `ozma-qmp` real-QMP ignored test | **PASS** | 1 passed |
| `ozma-power` real-QMP ignored test | **PASS** | 1 passed |
| `ozma-capture` real-QMP ignored test | **N/A** | test does not exist; only fake-server tests |
| Suggested single-socket node (`recording` + QMP power + HLS) | **FAIL (historical)** | binary from this run; current tree shares one control client |
| QMP power query | **PASS** | `/power/state` 200, `running` |
| QMP capture → HLS (QMP reserved for capture) | **PASS** | `hls:"running"`, 200 playlist/segment, H.264 720×400 |
| HLS playlist advance | **PASS** | media sequence 0 → 1, four retained segments |
| Concurrent HLS + snapshot on one QMP socket | **FAIL (historical)** | binary from this run; current tree serializes both through `SharedQmpClient` |
| One-shot JPEG snapshot (QMP reserved for capture) | **PASS** | two 200s, 720×400 JPEG, `FF D8 FF` … `FF D9` |
| D-Bus real display test (optional) | **FAIL** | 15 s D-Bus connection timeout; not required here |
| Cleanup | **PASS** | no processes, ports, sockets, or temp files left |

## 9. Before-fix remaining work (historical)

These are not covered by this report and still need a guest and/or controller:

* **Guest OS and guest display:** real framebuffer content, mode changes,
  resolution changes, and non-static frames. The SeaBIOS text screen is enough
  for transport validation only.
* **Unmodified Python controller:** discovery and one-shot registration,
  `/api/v1/nodes` visibility, controller HLS proxy, snapshot proxy, input
  proxy, power proxy, dashboard live view, and the controller regression suite.
  No controller process was started here.
* **Guest-visible input:** QMP `input-send-event` and uinput key/mouse events
  observed inside a guest, plus the controller UDP HID path. Only the QMP
  input sink's connection/injecting status was observed; no event was sent.
* **Power actions:** only `query-status`/`/power/state` was verified. Stop,
  reset, force-off/destroy, and ACPI behavior remain manual and need a
  disposable guest.
* **WebRTC:** external helper startup, real SDP offer/answer, browser
  ICE/DTLS, and media. No WebRTC helper was configured.
* **Audio:** not implemented on this node surface; not exercised.
* **QMP client architecture:** the shared-client serialization fix is
  implemented and covered by fake single-connection tests. Re-run the
  one-socket setup (power query + HLS + concurrent snapshot + QMP input) on a
  live QEMU before recording it as a pass. D-Bus capture still opens its own
  QMP handoff connection, so a single-socket VM should not combine D-Bus
  capture with concurrent QMP control roles.
* **D-Bus display:** investigate and fix the 15-second handshake failure if
  D-Bus is intended to be the lower-latency VM path.
* **Full Gate 2:** re-run on a frozen tree and attach artifacts to the parity
  matrix. This report alone must not be used to claim Gate 2.

## 10. Before-fix cleanup

All spawned processes were direct children of this run, not libvirt domains.
The following PIDs were stopped and verified dead:

| Process | PID |
|---|---|
| Main disposable QEMU (TCG, `-display none`) | 3713759 |
| Node, initial `input=qmp` + `power=qmp` run | 3716819 |
| Node, HLS-only run | 3723690 |
| Node, snapshot-only run | 3725499 |
| D-Bus disposable QEMU | 3726059 |
| Node, power-only run | 3727146 |
| Restart disposable QEMU for the exact suggested flags | 3728555 |
| Node, exact suggested-flags run | 3728560 |

Final checks after removing `/tmp/opencode/ozma-gate2-live`:

```text
$ ps -eo pid,comm,args | grep -E '[q]emu-system-x86_64|[o]zma-node |[f]fmpeg '
(no matching processes)

$ pgrep -x ozma-node
(none)

$ pgrep -x ffmpeg
(none)

$ ss -lntu | grep -E ':(45571|45573|39767)\b'
(none)

$ test -e /tmp/opencode/ozma-gate2-live && echo EXISTS || echo ABSENT
ABSENT
```

No QMP Unix socket, HLS output, PPM, JPEG, TS segment, playlist, or log remains
under the temporary path. The only persistent source-tree write from this run
is this report; `cargo test` may have refreshed build artifacts under
`target/`. The unrelated untracked `docs/gate2-live-device-report.md` and the
source edits made by the concurrent process were left untouched.
