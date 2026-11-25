# Phase 4 live V4L2 capture validation

Date: 2026-11-25 (Australia/Sydney)
Host: CachyOS, Linux 7.3.5-2-cachyos, x86_64
Checkout: `/home/matt/ozma/ozma`
HEAD: `f6d0260feb3b458327d0b1f3da4611c2c5be5943`
Binary: `target/debug/ozma-node` (`ozma-node 0.1.0`, built 2026-11-25 23:42)
Producer: ffmpeg n9.1
Driver: `v4l2loopback` 0.15.4
Module: `/lib/modules/7.3.5-2-cachyos/kernel/drivers/media/v4l2-core/v4l2loopback.ko.zst`

## Verdict

**PARTIAL PASS.** A real v4l2loopback device fed by ffmpeg is discovered and
captured by `ozma-node`; `/display/info`, `/display/snapshot`, and `/video`
work with real MJPEG frames and track a live pattern/size change. Missing,
capture-less, and busy devices produce typed 503s.

**HLS over V4L2 is blocked.** The V4L2 provider negotiates MJPEG/NV12/YUYV/H264,
while the HLS feeder accepts only RGB24/BGRA/P010. The sets do not intersect, so
`--hls` starts as `failed` with a typed error and writes no playlist/segments.
No frames were fabricated to hide this.

**`--capture-device` is not a source selector.** It is registration metadata
only. The hardware registry always enumerates the host and HLS/WebRTC/snapshot
take the first source. With real cameras present, the first source is
`/dev/video0`, not the requested device. This validation used a private mount
namespace to hide `/dev/video0-1`; the node itself has no command-line source-id
selector for the hardware profile.

No product code was changed.

## Follow-up (2026-11-26)

The two blockers below were fixed after this run; the rest of this document keeps
the original pre-fix evidence. See
[`phase4-node-live-fixes-report.md`](phase4-node-live-fixes-report.md) for the
implementation and full commands.

* **HLS from V4L2: fixed.** The capture→HLS bridge now converts MJPEG, YUYV, and
  NV12 to packed RGB24 (and keeps RGB24/BGRA/P010 behavior). A second live run
  with dedicated `v4l2loopback` devices produced `hls:"running"` and a real
  H.264 640x480 10 fps segment for all three producer formats (10 decoded frames
  in the probed segment). H.264/HEVC/AV1 pass-through is still a typed refusal.
* **`--capture-device` selection: fixed.** The configured device path/id/name is
  used by snapshots, `/video`, HLS, WebRTC, the per-console managers, and the
  console broker. A missing device returns typed `capture.unavailable` and never
  falls back to the first enumerated camera. In a namespace where
  `/dev/video0` (`fake camera first`) enumerated first and `/dev/video42`
  (`ozma-capture-test`) was requested, `connection.capture`, `/display/info`,
  snapshots, `/video`, and HLS all used `/dev/video42`; `/dev/video99` failed
  with `"refusing to fall back"` and wrote no playlist.
* Snapshot and `/video` for YUYV/NV12 still return the existing typed
  `503 display.unsupported_format`; only the HLS feeder was extended in this
  fix. The live source selection, missing-device, and HLS results are unchanged
  by that remaining gap.

## PASS / FAIL matrix

| Check | Result | Evidence |
|---|---|---|
| Dedicated v4l2loopback `/dev/video42` | **PASS** | `video_nr=42`, `card_label=ozma-capture-test`, `exclusive_caps=1`; `/dev/video42` appeared as `81,2 root:video 660`, sysfs name `ozma-capture-test` |
| V4L2 discovery/open by `ozma-node` | **PASS** | `/api/v1/connection.capture` = `provider:v4l2`, `kind:v4l2`, `source:/dev/video42`, `name:ozma-capture-test`, `available:true` |
| `/display/info` real geometry | **PASS** | `0x0`/`unknown` before the first frame; `640x480`/`last-frame` after a snapshot |
| `/display/snapshot` | **PASS** | `200 image/jpeg`, 24,644 bytes, 640x480, testsrc2 colour content |
| `/video` MJPEG | **PASS** | 20 parsed JPEG frames in 4 s at `fps=5`, 25,020-25,545 bytes each, 640x480 |
| Pattern/size change tracked | **PASS** | 640x480 testsrc2 -> 320x240 red -> 640x480 green; snapshot, `/video`, and `/display/info` followed each change |
| HLS from V4L2 | **FAIL (blocked)** | `hls:"failed"`, `HLS cannot feed 'mjpeg' frames ... only rgb24 and bgra are supported`; empty HLS dir, no `stream.m3u8` |
| Missing / capture-less device | **PASS (typed)** | `503 capture.unavailable` "does not expose video capture" |
| Busy device | **PASS (typed)** | `503 capture.unavailable` "Failed to set capture format ... Device or resource busy (os error 16)" |
| `--capture-device` selects the source | **FAIL (not wired)** | See "`--capture-device` diagnosis"; flag is advert-only |
| Cleanup | **PASS** | node/producer stopped; module unloaded; `/dev/video42` removed; real cameras unchanged |

## Pre-state

Recorded at `2026-11-25T23:51:18+11:00` before loading anything:

```text
/dev/video0  81,0  root:video 660  Creative Live! Cam Sync 4K: Cre
/dev/video1  81,1  root:video 660  Creative Live! Cam Sync 4K: Cre
/dev/video42 absent
v4l2loopback not loaded
```

No persistent module configuration was present or added (`/etc/modules-load.d`
was not edited; the module was loaded by an explicit `modprobe` only).

## Device setup

```bash
sudo modprobe v4l2loopback video_nr=42 card_label=ozma-capture-test exclusive_caps=1
```

Post-load state:

```text
v4l2loopback 73728 0
crw-rw---- 1 root video 81, 2 /dev/video42
/sys/class/video4linux/video42/name = ozma-capture-test
/sys/module/v4l2loopback/parameters/video_nr = 42,-1,-1,-1,-1,-1,-1,-1
/sys/module/v4l2loopback/parameters/exclusive_caps = Y,N,N,N,N,N,N,N
```

With `exclusive_caps=1` and no producer, the device exposed only `Video Output`
and enumerated no capture formats. A producer must open/write it first.

## Safety isolation

The hardware profile builds `CaptureRegistry::system_with_pipewire()` regardless
of `--capture-provider`/`--capture-device`, and
`open_first_capture_source()` takes the first enumerated source. The real
cameras sort first (`/dev/video0`, name contains `Cam`), so running the node on
the host would have opened a real camera.

To avoid ever opening the user's camera, the node ran in a private mount
namespace that:

- replaced `/dev` with a tmpfs containing the normal null/zero/random/tty/pts
  nodes plus a bind of the real `/dev/video42`;
- replaced `/sys/class/video4linux` with a tmpfs containing only
  `video42/name`.

The namespace log showed only:

```text
namespace /dev video nodes:
crw-rw---- 1 root video 81, 2 /dev/video42
namespace v4l sysfs names:
/sys/class/video4linux/video42/name = ozma-capture-test
```

No host device node, ACL, udev rule, module option, or real camera was changed.
The harness script and namespace are removed after the run.

## Node invocation

```bash
target/debug/ozma-node --profile hardware --name ozma-v4l2-live \
  --http-port 39242 --hid-udp-port 39243 \
  --capture-provider v4l2 --capture-device /dev/video42 \
  --input-provider recording --audio-provider none \
  --display-provider none --power-provider none \
  --hls --hls-dir /tmp/opencode/v4l2-hls --hls-fps 10 \
  --hls-bitrate-kbps 1500 --hls-encoder libx264
```

`/health` returned `{"ok":true,...}`. `/api/v1/connection` reported:

```json
"capture": {
  "provider": "v4l2",
  "kind": "v4l2",
  "source": "/dev/video42",
  "name": "ozma-capture-test",
  "available": true
}
```

## MJPEG capture evidence

Producer (`testsrc2`, 640x480@30, MJPEG):

```bash
ffmpeg -re -f lavfi -i testsrc2=size=640x480:rate=30 \
  -c:v mjpeg -q:v 4 -pix_fmt yuvj420p -f v4l2 /dev/video42
```

Loopback then advertised `[0]: 'MJPG' (Motion-JPEG, compressed) 640x480
30.000 fps`.

### `/display/info` and snapshot

Before the first frame:

```json
{"active":true,"capability_max_height":0,"capability_max_width":0,
 "console":0,"dimensions_source":"unknown","height":0,"last_error":null,
 "ok":true,"pixel_format":null,"snapshot_available":true,
 "snapshot_encoding":"jpeg","source_id":"/dev/video42",
 "source_name":"ozma-capture-test","width":0}
```

`GET /display/snapshot` returned `200 image/jpeg`, `content-length: 24644`,
`cache-control: no-store`. Decoded as 640x480 RGB. Sample pixels matched the
testsrc2 pattern, not a constant/fabricated frame:

```text
(320,240) = (14, 15, 141)
(160,120) = (0, 255, 0)
(480,120) = (255, 0, 252)
(160,360) = (128, 128, 128)
(480,360) = (211, 26, 228)
```

After the snapshot, `/display/info` changed to:

```json
{"active":true,"capability_max_height":0,"capability_max_width":0,
 "console":0,"dimensions_source":"last-frame","height":480,"last_error":null,
 "ok":true,"pixel_format":"mjpeg","snapshot_available":true,
 "snapshot_encoding":"jpeg","source_id":"/dev/video42",
 "source_name":"ozma-capture-test","width":640}
```

### `/video`

`GET /video?fps=5&quality=80` was stopped after 4 s. It produced 20 multipart
frames (485,631 bytes), each a real JPEG:

```text
boundary occurrences: 20
JPEG frames parsed:   20
frame sizes:          25096, 25290, 25125, 25020, 25545, ...
geometry:             640x480
rate:                 ~5 frames/s (matches the requested fps)
```

## Live pattern change

The node stayed running while the producer changed.

| Producer | Snapshot | `/display/info` | `/video` |
|---|---|---|---|
| testsrc2 640x480 MJPEG | 640x480, 24,644 B | 640x480, `mjpeg`, `last-frame` | 20 frames in 4 s at `fps=5` |
| solid red 320x240 MJPEG | 320x240, 674 B, centre `(254,0,0)` | 320x240, `mjpeg` | 9 frames in 3 s at `fps=3`, centre `(254,0,0)` |
| solid green 640x480 MJPEG | 640x480, centre `(0,129,0)` | 640x480, `mjpeg` | not repeated |

The one-shot snapshot path re-enumerates and re-opens the source for each
request, so it tracks the new size and content. The JPEG green value is
`(0,129,0)` because ffmpeg's limited-range YUV conversion does not map pure RGB
green to `(0,255,0)`.

## HLS failure (blocker; fixed in follow-up)

At startup, with the MJPEG source:

```text
WARN ozma_node: HLS unavailable: the local stream pipeline did not start
     error=HLS cannot feed `mjpeg` frames to ffmpeg as raw RGB24; only rgb24 and bgra are supported
```

`/api/v1/connection` reported:

```json
"hls": "failed",
"hls_error": "HLS cannot feed `mjpeg` frames to ffmpeg as raw RGB24; only rgb24 and bgra are supported"
```

The HLS output directory existed but stayed empty: no `stream.m3u8`, no `.ts`
segments, so `ffprobe` had nothing to measure. This is a typed refusal, not a
stalled or fake stream.

Root cause is a format-set mismatch in this tree:

- `ozma-capture/src/v4l2.rs` negotiates
  `PREFERRED_FOURCCS = MJPG, NV12, YUYV, H264` (all it can describe).
- `ozma-node/src/hls.rs::ensure_supported` accepts only
  `Rgb24, Bgra, P010`.

There is no fourcc both sides accept. The same mismatch affects MJPEG→HLS,
YUYV→HLS, NV12→HLS, and H264→HLS. Fixing it requires a real conversion/feed
path (for example YUYV/NV12→RGB24 in the HLS feeder, or an MJPEG decode path),
not a wiring change. It is recorded here rather than patched because this run
was validation-only.

## YUYV and RGB24 producers

YUYV (`testsrc2`, 320x240): the node opened the source, but snapshots and
`/video` failed honestly:

```json
HTTP/1.1 503 Service Unavailable
{"code":"display.unsupported_format","details":"yuyv",
 "error":"The capture source produces `yuyv` frames, which cannot be encoded as JPEG.",
 "ok":false,
 "remediation":"Use a source that emits rgb24, bgra, or mjpeg (QMP, D-Bus, VNC, Looking Glass, or a compatible V4L2 device).",
 "retryable":false}
```

`/display/info` still showed the previous successful MJPEG last frame
(320x240, `pixel_format:"mjpeg"`, `last_error:null`). That is the documented
last-frame cache, but it is easy to misread as current source state after a
failure.

RGB24 (`-pix_fmt rgb24`, 320x240): the loopback advertised `'RGB3' (24-bit RGB
8-8-8)`, and the node refused to open it:

```json
{"code":"capture.unavailable","details":"/dev/video42",
 "error":"V4L2 device `/dev/video42` has no supported pixel format (RGB3).",
 "ok":false,
 "remediation":"A supported device advertises MJPEG, NV12, YUYV, or H.264.",
 "retryable":true}
```

The existing unit tests assert this explicitly
(`fourcc_to_pixel_format(*b"RGB3") == None`,
`preferred_fourcc(&[*b"RGB3"]) == None`), so this is current intended behaviour,
not an accidental omission. It means RGB24-only V4L2 devices cannot use either
snapshot or HLS even though both downstream paths support RGB24.

## Missing and busy device errors

**Capture-less device.** Stopping the producer left the loopback as
`Video Output` only. `GET /display/snapshot` returned:

```json
HTTP/1.1 503 Service Unavailable
{"code":"capture.unavailable","details":"/dev/video42",
 "error":"V4L2 device `/dev/video42` does not expose video capture (driver `v4l2 loopback`, card `ozma-capture-test`).",
 "ok":false,
 "remediation":"Pick a capture node such as a camera or HDMI grabber, not a metadata or output node.",
 "retryable":true}
```

**Busy device.** The producer held the active capture format and one extra
V4L2 consumer was open; `GET /display/snapshot` returned:

```json
HTTP/1.1 503 Service Unavailable
{"code":"capture.unavailable","details":"/dev/video42",
 "error":"Failed to set capture format for V4L2 device `/dev/video42`: Device or resource busy (os error 16)",
 "ok":false,
 "remediation":"Check the device is connected, the node is a character device, and the service user can open it.",
 "retryable":true}
```

The busy remediation text is generic (it suggests checking connectivity rather
than closing the other opener), but the error is typed and does not fabricate a
frame.

## `--capture-device` diagnosis (fixed in follow-up)

The flag is real but advert-only:

- `ozma-node/src/main.rs:445-447` documents it as "Capture device path advertised
  to the controller".
- It is stored in `NodeConfig::capture_device`
  (`main.rs:1450`, `config.rs:1086`) and copied into registration/heartbeat TXT
  (`main.rs:4308-4309`, `tasks/heartbeat.rs:94`).
- The only source-related use is desktop auto-fill from the discovered source id
  (`main.rs:1868-1872`). The hardware profile never reads it for capture.
- `platform.rs:45-49` returns `CaptureRegistry::system_with_pipewire()` for
  `NodeProfile::Hardware`; provider order is V4L2 then PipeWire. `resolve_capture`
  accepts `--capture-provider v4l2` by fall-through, but the hardware registry
  ignores it.
- HLS and WebRTC call `open_first_capture_source()`
  (`main.rs:3457-3474`), which enumerates and takes `.next()` with no id filter.
- Snapshots use `SnapshotControl::from_shared(...)` (`main.rs:2812`) without
  `with_source_id`, even though `SnapshotControl::with_source_id` exists
  (`display_compat.rs:434-438`).

Missing wiring, precisely:

1. **Hardware registry:** no V4L2 capability is filtered/selected by
   `--capture-device` (or by a new source-id flag) before it is handed to HLS,
   WebRTC, or snapshots.
2. **HLS/WebRTC:** `open_first_capture_source` has no source-id parameter; a
   hardware node with more than one V4L2 node always uses the first enumerated
   one.
3. **Snapshot:** the one-shot control never receives the configured device id,
   although the control supports it.

Because `discover_v4l2` sorts by numeric index and the real camera name matches
the capture heuristic, the first source on this host is `/dev/video0`. Therefore
a plain hardware-profile run with `--capture-device /dev/video42` would open the
real camera, not the loopback. The namespace harness avoided that; it did not
fix the node's selection logic.

## Cleanup

- `SIGTERM` to the node: exited promptly; log showed
  `shutdown requested; stopping node tasks signal="SIGTERM"` and
  `Ozma node shutting down`.
- Producer and all test consumers stopped; no `ffmpeg` remained on
  `/dev/video42`.
- `sudo modprobe -r v4l2loopback`: module unloaded and `/dev/video42` removed.
- Real devices after cleanup are byte-for-byte the pre-state devices:

```text
/dev/video0  81,0  root:video 660  Creative Live! Cam Sync 4K: Cre
/dev/video1  81,1  root:video 660  Creative Live! Cam Sync 4K: Cre
```

- No persistent module configuration, udev rule, ACL, or host service was
  added. The namespace, HLS output, producer logs, and captures were removed
  from `/tmp/opencode`.
