# Gate 2 live hardware-encode report

**Scope:** real hardware H.264/HEVC encoding through the HLS path, on this host
and on the NVIDIA host `queen`, using a disposable QEMU capture. This is live
evidence for the `/codec` and `--hls-encoder` hardware claims, not a full Gate 2
pass. It does not cover discovery/registration, the Python controller,
dashboard/browser playback, WebRTC, guest input, or audio.

**Verdict:** VAAPI H.264 and HEVC encode a real, probeable HLS segment on both
the Intel Arc B580 and the AMD Radeon 610M. NVENC H.264 and HEVC produce real
HLS segments on `queen` (GTX 1660 and RTX 3090) with the node's exact ffmpeg
argv. QSV and V4L2M2M are advertised by ffmpeg on this host but fail a real
encode; they are now reported as typed errors instead of a false success.
Two bugs were found and fixed in `ozma-node/src/hls.rs`: the hardware "probe"
was only `ffmpeg -encoders`, and RGB24 input silently negotiated 4:4:4 chroma
that many HLS clients cannot decode. A follow-up fixed the `/codec`
advertisement gaps in §6.4: detection now honours `--vaapi-device` and
advertises only device-proven VAAPI H.264/HEVC, NVENC with a CUDA/NVML device,
and QSV/V4L2M2M after a real encode check.

> No `sudo`, package install, driver/module change, VM definition, or commit was
> made. The only VM was a direct, headless `qemu-system-x86_64` process with a
> temporary QMP socket and no disk/network device. `queen` was accessed over
> SSH with read-only inventory plus temporary `/tmp` encodes. All test
> processes, sockets, ports, and temp directories were removed; see
> [Cleanup](#cleanup).

## 1. Run metadata

| Item | Value |
|---|---|
| Date | 2026-11-24 (AEDT) |
| Local git HEAD | `436b80c7b384b80e8db9163ad294af571f06641a` |
| Working tree | dirty before this work (unrelated concurrent edits); the live-run code change is `ozma-node/src/hls.rs`; the §6.4 advertisement follow-up also changes `ozma-encode` and `ozma-node` codec wiring, all uncommitted |
| Local kernel | `7.3.5-2-cachyos` x86_64 |
| Local ffmpeg | n9.1 (`/usr/bin/ffmpeg`) |
| QEMU | `qemu-system-x86_64` 11.1.3, TCG |
| `queen` kernel | `6.8.0-147-generic` x86_64 (Ubuntu 24.04) |
| `queen` ffmpeg | 6.1.1-3ubuntu5 (`/usr/bin/ffmpeg`) |
| `queen` GPU driver | NVIDIA 610.57.04, CUDA UMD 13.3 |
| Temp dirs | `/tmp/opencode/ozma-gate2-hw`, `/tmp/queen-nvenc-*`, `/tmp/ozma-gate2-queen` (all removed) |
| Ports | HTTP 47333–47350, HID 60333–60350 (released) |

## 2. Device and tool inventory

### 2.1 Local host

| Device | Driver | Capability | Encode evidence |
|---|---|---|---|
| `/dev/dri/renderD128` (`03:00.0`, `8086:e20b`) | `xe`, Intel iHD 26.3.5 | Intel Arc B580; VAAPI H.264/HEVC encode entrypoints | **PASS** real encode |
| `/dev/dri/renderD129` (`13:00.0`, `1002:13c0`) | `amdgpu`, Mesa 26.3.1 | AMD Radeon 610M; VAAPI H.264/HEVC encode entrypoints | **PASS** real encode |
| `/dev/dri/renderD130` (`platform/hermes-kms`) | `hermes-kms` | Not a VAAPI device | **FAIL** (`Failed to initialise VAAPI connection`) |
| `/dev/video0`, `/dev/video1` | `uvcvideo` | Creative Live! Cam Sync 4K, capture/metadata only | No mem2mem encoder |
| NVIDIA | none | `/dev/nvidia*` absent; no `nvidia-smi` | **UNAVAILABLE** (`Cannot load libcuda.so.1`) |

Permissions: the user is in the `video` group (985) but not `render`; the render
nodes are world read/write (`crw-rw-rw-`), so VAAPI opens succeed without
privilege changes. Nothing was chmodded or added to a group.

`vainfo` (abridged):

```text
renderD128: VA-API 1.24, Intel iHD 26.3.5
  VAProfileH264High     : VAEntrypointEncSlice
  VAProfileHEVCMain     : VAEntrypointEncSlice
  VAProfileHEVCMain10   : VAEntrypointEncSlice
renderD129: VA-API 1.24, Mesa 26.3.1 (radeonsi)
  VAProfileH264High     : VAEntrypointEncSlice
  VAProfileHEVCMain     : VAEntrypointEncSlice
renderD130: vaInitialize failed (unknown libva error)
```

Installed media stack: `intel-media-driver 26.3.5`, `libvpl 2.17.0`,
`libmfx 23.2.2`, `libva 2.24.1`, `libva-utils 2.24.0`.

`ffmpeg -encoders` advertises all eight hardware wrappers tested:

```text
h264_vaapi  hevc_vaapi  h264_nvenc  hevc_nvenc
h264_qsv    hevc_qsv    h264_v4l2m2m hevc_v4l2m2m
```

Listing is **not** evidence of availability: QSV, NVENC, and V4L2M2M all fail
the real encode on this host.

### 2.2 `queen`

| Item | Value |
|---|---|
| Host | `queen`, AMD Ryzen 9 5900X, 24 threads, x86_64 |
| GPU 0 | NVIDIA GeForce GTX 1660 (6 GB), driver 610.57.04 |
| GPU 1 | NVIDIA GeForce RTX 3090 (24 GB), driver 610.57.04 |
| Devices | `/dev/nvidia0`, `/dev/nvidia1`, `/dev/nvidiactl`, `/dev/nvidia-uvm` |
| ffmpeg | 6.1.1-3ubuntu5; `h264_nvenc`, `hevc_nvenc`, `av1_nvenc` listed |
| Repo | `/home/matt/ozma` → `/applications/data/projects/ozma`, HEAD `79b41c73`, dirty |
| Binary | `/home/matt/ozma/target/debug/ozma-node` dated Apr 14 2026 |

The `queen` binary reports `ozma-node 0.1.0` but its `--help` has **no**
`--hls`, `--hls-encoder`, `--hls-dir`, or `--vaapi-device` flags: it predates
the HLS compatibility path. A full node NVENC run there was therefore not
possible without building/copying the current tree, which this run did not do
(no package installs, no large artifact copy, no repo modification).

## 3. Direct ffmpeg ground truth (local)

Synthetic source: `testsrc2=size=720x400:rate=30`, 2 s, encoded to Matroska.
A successful row means real bytes plus a valid `ffprobe` codec/dimensions.

| Encoder | Invocation | Result | ffprobe |
|---|---|---|---|
| `h264_vaapi` | `-vaapi_device /dev/dri/renderD128 -vf format=nv12,hwupload` | PASS | h264 High 720x400 yuv420p, 585633 B |
| `hevc_vaapi` | same, renderD128 | PASS | hevc Main 720x400 yuv420p, 619244 B |
| `h264_vaapi` | renderD129 (AMD) | PASS | h264 High 720x400 yuv420p, 473902 B |
| `hevc_vaapi` | renderD129 (AMD) | PASS | hevc Main 720x400 yuv420p, 527855 B |
| `h264_qsv` | plain `-c:v h264_qsv` | **FAIL** | `Error creating a MFX session: -9` |
| `hevc_qsv` | plain | **FAIL** | `Error creating a MFX session: -9` |
| `h264_nvenc` | plain | **FAIL** | `Cannot load libcuda.so.1` |
| `hevc_nvenc` | plain | **FAIL** | `Cannot load libcuda.so.1` |
| `h264_v4l2m2m` | plain | **FAIL** | `Could not find a valid device` |
| `hevc_v4l2m2m` | plain | **FAIL** | `Could not find a valid device` |
| `libx264` | control | PASS | h264 High |

QSV was retried with `-init_hw_device qsv=hw`, `-qsv_device
/dev/dri/renderD128`, and `qsv=hw@va`; all returned the same MFX session
`-9` / device-creation failure. This is a host media-stack/driver result, not a
node argv result. No driver or package was changed.

## 4. `queen` NVENC validation

### 4.1 Direct NVENC on both GPUs

Commands (one per GPU/codec, temp dir auto-removed):

```bash
ffmpeg -hide_banner -f lavfi -i testsrc=size=1280x720:rate=30 -t 2 \
  -c:v h264_nvenc -gpu <0|1> -preset p4 -tune ll -rc cbr -b:v 4000k \
  -f h264 out.h264
ffmpeg -hide_banner -f lavfi -i testsrc=size=1280x720:rate=30 -t 2 \
  -c:v hevc_nvenc -gpu <0|1> -preset p4 -tune ll -rc cbr -b:v 4000k \
  -f hevc out.hevc
```

| GPU | Codec | Result | ffprobe |
|---|---|---|---|
| GTX 1660 (0) | h264_nvenc | PASS | h264, 1280x720, 60 frames, 962692 B |
| GTX 1660 (0) | hevc_nvenc | PASS | hevc, 1280x720, 60 frames, 512603 B |
| RTX 3090 (1) | h264_nvenc | PASS | h264, 1280x720, 60 frames, 962692 B |
| RTX 3090 (1) | hevc_nvenc | PASS | hevc, 1280x720, 60 frames, 512603 B |

(The direct test used `testsrc` RGB without a pixel-format target, so these
particular outputs were High 4:4:4 / `gbrp`. That is the pre-fix behaviour; see
§6.)

### 4.2 Exact current node HLS argv on `queen`

The current node argv is built by `ffmpeg_argv`; it now includes
`-pix_fmt yuv420p`. The input was a real 4 s RGB24 rawvideo file, exactly the
shape the QMP capture feeder writes:

```bash
ffmpeg -hide_banner -loglevel warning \
  -f rawvideo -pix_fmt rgb24 -s 720x400 -r 30 -i - -an \
  -c:v h264_nvenc -preset p4 -tune ll -rc cbr -b:v 4000k \
  -pix_fmt yuv420p -g 30 -force_key_frames 'expr:gte(t,n_forced*1)' \
  -f hls -hls_time 1 -hls_list_size 4 -hls_flags delete_segments \
  -hls_segment_filename "$D/hls_h264/seg_%05d.ts" "$D/hls_h264/stream.m3u8" \
  < "$D/raw.rgb"
```

`hevc_nvenc` was run with the same shape. Both passed with `ffmpeg_rc=0`, a
4-entry playlist, and four 1-second segments:

| Codec | Segment | ffprobe |
|---|---|---|
| `h264_nvenc` | `seg_00000.ts`–`seg_00003.ts`, 463796–527904 B | h264 **Main**, 720x400, **yuv420p**, 30/1, 30 frames, 1.000000 s |
| `hevc_nvenc` | `seg_00000.ts`–`seg_00003.ts`, 463044–554036 B | hevc **Main**, 720x400, **yuv420p**, 30/1, 30 frames, 1.000000 s |

Exact playlist shape:

```text
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:1
#EXT-X-MEDIA-SEQUENCE:0
#EXTINF:1.000000,
seg_00000.ts
...
#EXT-X-ENDLIST
```

**What this proves:** NVENC H.264 and HEVC really encode the node's argv,
including `-preset p4 -tune ll -rc cbr`, the GOP/`force_key_frames` shape, and
the chroma fix. It did not, by itself, prove the full node wrapper. That was
completed in §4.3 with the current local binary copied to `queen`; `queen`'s
checked-out binary predates `--hls`.

### 4.3 Full-node NVENC on `queen`

`queen`'s `/applications/data/projects/ozma` checkout is HEAD `79b41c73`
(2026-04-14) and its binary has no `--hls`/`--hls-encoder`, so the current local
`target/debug/ozma-node` was copied instead:

| Item | Value |
|---|---|
| Binary | local `target/debug/ozma-node`, 282,311,152 B, built 2026-11-24 15:19 AEDT |
| Compatibility | max GLIBC 2.39; `queen` has Ubuntu GLIBC 2.39; all shared libs resolved |
| Provenance | built from the local working tree that already contained the NVENC registry detection (later committed locally as `27ccbe1`); not rebuilt after that commit |
| `queen` copy | `/tmp/ozma-gate2-queen/ozma-node` (removed after the run) |
| QEMU | 8.2.2, TCG, disposable VGA + one QMP socket |
| Node options | `--profile vm --qmp-socket ... --input-provider recording --power-provider qmp --cap video --hls --hls-fps 5 --hls-bitrate-kbps 1000` |
| Encoders | `--hls-encoder h264_nvenc`/`hevc_nvenc`; `/codec` with `hw_accel:"nvenc"` |

Disposable VM and node:

```bash
qemu-system-x86_64 -name ozma-gate2-queen -machine pc,accel=tcg -m 256 \
  -nodefaults -device VGA -display none \
  -qmp unix:/tmp/ozma-gate2-queen/qmp.sock,server=on,wait=off \
  -pidfile /tmp/ozma-gate2-queen/qemu.pid -daemonize

OZMA_TEST_NVENC_GPU=0 /tmp/ozma-gate2-queen/ozma-node \
  --name ozma-queen-nvenc --profile vm \
  --qmp-socket /tmp/ozma-gate2-queen/qmp.sock \
  --input-provider recording --power-provider qmp --cap video \
  --http-port 47468 --hid-udp-port 60468 \
  --hls --hls-dir /tmp/ozma-gate2-queen/hls --hls-fps 5 \
  --hls-bitrate-kbps 1000 --hls-encoder h264_nvenc \
  --ffmpeg-bin /tmp/ozma-gate2-queen/ffmpeg-gpu.py
```

The node has no NVENC GPU-selector flag, so a test-only `--ffmpeg-bin` wrapper
inserted `-gpu <N>` immediately after `-c:v h264_nvenc|hevc_nvenc` and
`exec`ed `/usr/bin/ffmpeg`. No node code was changed for this; the wrapper is
part of the disposable test harness.

Results (both codecs through the CLI and `/codec`, both GPUs):

| nvidia-smi GPU | UUID | CLI start | `/codec` request | `applied` | Segment after restart |
|---|---|---|---|---|---|
| RTX 3090 (index 1) | `GPU-84baadff…` | `h264_nvenc` | `{"codec":"hevc","hw_accel":"nvenc"}` | **true**, HTTP 200 | hevc **Main**, 720x400, **yuv420p**, 5/1, 5 frames, 1.000000 s |
| GTX 1660 (index 0) | `GPU-e88e2536…` | `hevc_nvenc` | `{"codec":"h264","hw_accel":"nvenc"}` | **true**, HTTP 200 | h264 **Main**, 720x400, **yuv420p**, 5/1, 5 frames, 1.000000 s |

The GPU was identified by correlating the node's live ffmpeg child PID with
`nvidia-smi --query-compute-apps=pid,gpu_uuid`, not by the `-gpu` number alone.
On this host CUDA ordinals are reversed relative to nvidia-smi indices:
`-gpu 0` (and `CUDA_VISIBLE_DEVICES=0`) selected the RTX 3090 (UUID 84ba),
while `-gpu 1` selected the GTX 1660 (UUID e88e).

`/stream` was fetched over HTTP from the node:

```text
GET /stream/stream.m3u8  -> 200 (playlist; first segment listed)
GET /stream/seg_00000.ts -> 200, h264 Main 720x400 yuv420p, 5 frames, 1.000 s
POST /codec hevc+nvenc   -> 200 applied:true, encoder hevc_nvenc
GET /stream/seg_00018.ts -> 200, hevc Main 720x400 yuv420p, 5 frames, 1.000 s
GET /stream/seg_00017.ts -> 200, h264 Main 720x400 yuv420p, 5 frames, 1.000 s
```

After the HEVC restart, `/api/v1/connection` reported
`available:["h264","hevc"]`, `backends:["software","nvenc"]`,
`current_encoder:"hevc_nvenc"`, `applied:true`, `pending_restart:false`.

Honesty checks:

- `POST /codec {"codec":"h264","hw_accel":"qsv"}` → HTTP 400
  `encoder.unavailable` / `requested_backend_unavailable`; the HEVC NVENC
  pipeline stayed `running`. No silent fallback.
- `POST /codec {"codec":"hevc","hw_accel":"hevc_nvenc"}` → HTTP 400
  `invalid_request`: `hw_accel` accepts the alias `nvenc`, not an ffmpeg
  encoder name. The error lists the real advertised capabilities.
- The `/codec` restart returned `applied:true` only after the node's hardware
  runtime probe encoded a segment, and a post-restart segment fetched over
  `/stream` decoded as the requested codec.

Local test evidence for the same tree:

```text
cargo test -p ozma-node --lib hls::
  test result: ok. 34 passed; 0 failed; 1 ignored
cargo test -p ozma-node --lib controller_compat::tests::codec
  test result: ok. 13 passed; 0 failed; 0 ignored
cargo test -p ozma-encode
  lib: 68 passed; 0 failed; 0 ignored
  integration: 17 passed; 0 failed; 0 ignored
```

## 5. Live local node HLS runs

### 5.1 Disposable QEMU and node command

```bash
GDIR=/tmp/opencode/ozma-gate2-hw/live
qemu-system-x86_64 \
  -name ozma-gate2-hw -machine pc,accel=tcg -m 256 -nodefaults \
  -device VGA -display none \
  -qmp "unix:$GDIR/qmp.sock,server=on,wait=off" \
  -pidfile "$GDIR/qemu.pid" -daemonize

./target/debug/ozma-node \
  --name ozma-gate2-hw --profile vm \
  --qmp-socket "$GDIR/qmp.sock" --cap video \
  --http-port 47333 --hid-udp-port 60333 \
  --hls --hls-dir "$GDIR/hls" --hls-fps 5 --hls-bitrate-kbps 1000 \
  --hls-encoder h264_vaapi --vaapi-device /dev/dri/renderD128 \
  --ffmpeg-bin /usr/bin/ffmpeg
```

`/health` → 200. `/api/v1/connection` reported `"hls":"running"` and
`"hls_error":null`. QMP capture produced 720x400 frames.

### 5.2 CLI encoder matrix

| `--hls-encoder` | Device | Result | Segment evidence |
|---|---|---|---|
| `h264_vaapi` | renderD128 (Intel) | **PASS** | h264 High 720x400 yuv420p, 5/1, 1.000 s, 5 segments |
| `hevc_vaapi` | renderD128 (Intel) | **PASS** | hevc Main 720x400 yuv420p, 5/1, 1.000 s, 5 segments |
| `h264_vaapi` | renderD129 (AMD) | **PASS** | h264 High 720x400 yuv420p, 5/1, 1.000 s |
| `h264_qsv` | renderD128 | **FAIL (typed)** | no `pipeline started`; `h264_qsv ... failed a real encode probe ... MFX session: -9`; 0 segments |
| `h264_nvenc` | none | **FAIL (typed)** | `Cannot load libcuda.so.1`; 0 segments |
| `h264_v4l2m2m` | none | **FAIL (typed)** | `Could not find a valid device`; 0 segments |
| `hevc_v4l2m2m` | none | **FAIL (typed)** | `Could not find a valid device`; 0 segments |
| `h264_vaapi` | `/dev/dri/renderD999` (absent) | **FAIL (typed)** | `VAAPI device ... is unavailable: the path does not exist`; no spawn |
| `h264_vaapi` | `/dev/dri/renderD130` (exists, not VAAPI) | **FAIL (typed)** | `Failed to initialise VAAPI connection`; no spawn |

Before the fix, the QSV/NVENC/V4L2M2M cases logged
`HLS pipeline started`, then failed a moment later with
`writing a frame to ffmpeg failed: Broken pipe`. After the fix they fail
*before* a pipeline is created, with the real ffmpeg stderr in the typed error
and zero segments.

### 5.3 `POST /codec`

`/codec` is guarded only by the node's normal API guard; this run had no token,
so a loopback POST was accepted.

| Request | HTTP | Response | Result |
|---|---|---|---|
| `{"codec":"h264","bitrate":"2M","hw_accel":"vaapi"}` | 200 | `applied:true`, `encoder:"h264_vaapi"`, `vaapi_device:"/dev/dri/renderD128"`, `hls:"running"` | **PASS**; next segment h264 High yuv420p 720x400 |
| `{"codec":"h265","bitrate":"2M","hw_accel":"vaapi"}` | 400 | `encoder.unavailable`, `reason:"requested_codec_unavailable"` | HEVC is not in this node's registry (see §7) |
| `{"codec":"h264","hw_accel":"nvenc"}` | 400 | `encoder.unavailable`, `reason:"requested_backend_unavailable"` | no fallback; previous vaapi pipeline stayed `running` |
| `{"codec":"h264","hw_accel":"qsv"}` | 400 | same | no fallback |
| `{"codec":"h264","hw_accel":"v4l2m2m"}` | 400 | same | no fallback |
| `{"codec":"h264","hw_accel":"nvenc","allow_software_fallback":true}` | 200 | `applied:true`, `encoder:"libx264"`, `fallback:false`, receipt `applied:false`, `fallback_reason:"requested_backend_unavailable"` | **PASS with a reporting inconsistency** (§8) |

Wrong `--vaapi-device` scenario, after the fix:

```text
POST /codec {"codec":"h264","hw_accel":"vaapi"}   (device /dev/dri/renderD130)
HTTP 502  encoder.unavailable
  "... h264_vaapi ... failed a real encode probe ...
   [VAAPI] Failed to initialise VAAPI connection: -1 (unknown libva error)
   ... restored the previous pipeline with encoder `libx264`"
```

Three seconds after that response `/api/v1/connection` still reported
`"hls":"running"`, `"applied":false`, `"current_encoder":"h264"`. Before the
fix the same request returned `applied:true, hls:"running"` and then the
pipeline died.

> These local `/codec` rows were captured before the §6.4 registry
> advertisement fix. On the current binary and on `queen`, `/codec` advertises
> software H.264 plus device-proven VAAPI/NVENC H.264+HEVC; the full-node NVENC
> evidence is in §4.3.

## 6. Bugs found and fixed

The live run changed only `ozma-node/src/hls.rs`. The advertisement follow-up
in §6.4 additionally changes `ozma-encode` detection and `ozma-node` codec
wiring; `hls.rs` is untouched by that follow-up.

### 6.1 Hardware "probe" was only `ffmpeg -encoders`

`SystemEncoderProbe` proved that a wrapper was compiled in, not that the device
or driver could encode. `HlsPipeline::start_with_clock` returns after spawning
ffmpeg, so `HlsController::restart_with_encoder_fallback` could return success
(and `POST /codec` could return `applied:true`) for a pipeline that died on the
first frame. This was reproduced live with an existing-but-not-VAAPI
`/dev/dri/renderD130`.

**Fix:** `EncoderProbe` gained a default-no-op `runtime_check`. Production
`SystemEncoderProbe` now runs the exact `ffmpeg_argv` on three synthetic 320x240
RGB24 frames in a throwaway directory, with a 10 s bound, and requires a clean
exit plus a non-empty `.ts` segment. The frames are written to a temp rawvideo
file (not a stdin pipe, which could block if the encoder never reads), and the
last 4 KiB of ffmpeg stderr is kept for the typed error. `validate_encoder_runtime`
calls it after the device/listing checks and turns any failure into
`HlsError::EncoderUnavailable` with the captured ffmpeg stderr. The `available`
list on a runtime failure contains only software encoders that ffmpeg
advertises, so unproven hardware is never presented as available.

Test probes keep the default no-op, so deterministic tests and fake spawners do
not need a GPU.

### 6.2 RGB24 input silently produced 4:4:4 HLS

Without an explicit output pixel format, ffmpeg negotiated
`libx264 → yuv444p` (High 4:4:4) and NVENC → `gbrp` (High 4:4:4) from the
node's RGB24 feeder. `ffprobe` accepts these, but many HLS/browser decoders do
not. VAAPI was already NV12 because of `format=nv12,hwupload`.

**Fix:** `HlsEncoder::hls_pixel_format()` plus a `-pix_fmt` argument in
`ffmpeg_argv`:

| Encoder | Target |
|---|---|
| `libx264`, `libx265`, `libsvtav1`, `libaom-av1` | `yuv420p` |
| `h264_nvenc`, `hevc_nvenc`, `*_v4l2m2m` | `yuv420p` |
| `h264_qsv`, `hevc_qsv` | `nv12` |
| `h264_vaapi`, `hevc_vaapi` | unchanged (`nv12` via upload filter) |
| `mjpeg` | unchanged |

Verified on `queen`: the exact NVENC argv now produces h264 **Main/yuv420p**
and hevc **Main/yuv420p** segments.

### 6.3 Tests

Added/updated in `ozma-node/src/hls.rs`:

- `hardware_start_rejects_a_failed_runtime_probe_before_spawning` — a probe
  that advertises `h264_vaapi` but fails `runtime_check` returns
  `EncoderUnavailable` and never spawns ffmpeg.
- `ffmpeg_argv_pins_a_decodable_chroma_format` — every encoder's output
  `-pix_fmt` is asserted, while the rawvideo input stays `rgb24`.
- `ffmpeg_argv_matches_the_compatibility_pipeline` — exact argv now includes
  `-pix_fmt yuv420p`.

Evidence:

```text
cargo test -p ozma-node --lib hls::
  test result: ok. 30 passed; 0 failed; 1 ignored

cargo test -p ozma-node
  lib: 184 passed; 0 failed; 3 ignored
  all integration binaries: 0 failed

cargo test --workspace
  all test binaries: 0 failed
```

`rustfmt --edition 2021 --check ozma-node/src/hls.rs` is clean.

### 6.4 Encoder advertisement ignored `--vaapi-device` and most real encoders

The live run exposed two related gaps: `EncoderRegistry::with_detected_encoders()`
advertised only software H.264 plus VAAPI H.264, and its VAAPI probe used
`VaapiProbe::new()`'s hard-coded `/dev/dri/renderD128` regardless of the
configured `--vaapi-device`. `GET /codec.available` was therefore `["h264"]` on
this host, and `POST /codec` rejected HEVC+VAAPI and could never select
NVENC/QSV/V4L2M2M even though the HLS CLI accepts those names.

**Fix (`ozma-encode`, `ozma-node`; `hls.rs` unchanged):**

- Added `EncoderEnvironment`, a plain value carrying the existing VAAPI render
  node, exact `ffmpeg -encoders` names, CUDA/NVML presence, and hardware
  encoders that passed a real one-frame encode check.
  `EncoderEnvironment::detect(Some(path))` honours only that path (an absent
  configured device does not silently probe a different node); `None` keeps the
  normal renderD128/lowest-render-node detection.
- Added `EncoderRegistry::with_detected_encoders_for_device(Option<&Path>)`.
  `with_detected_encoders()` remains as the no-explicit-device entry point.
  Tests use `with_detected_encoders_and_probe(&environment, &FakeProbe…)`, so no
  GPU or ffmpeg process runs in CI.
- Advertisement rules, all gated by device evidence:

  | Encoder | Advertised when |
  |---|---|
  | software H.264 | always (the crate's real fallback) |
  | VAAPI H.264 | the configured/default render node exists **and** a real VAAPI H.264 encode probe passes |
  | VAAPI HEVC | that device probe passed **and** ffmpeg lists `hevc_vaapi` |
  | NVENC H.264/HEVC | ffmpeg lists the wrapper **and** `/dev/nvidia*`/NVML is present; no full encode in the registry |
  | QSV H.264/HEVC | ffmpeg lists the wrapper **and** a real one-frame `testsrc2` encode succeeds |
  | V4L2M2M H.264/HEVC | ffmpeg lists the wrapper **and** a real one-frame encode succeeds |
  | `Fake` | never in real detection |

- `--vaapi-device` now flows through `NodeConfig.vaapi_device` into both the HLS
  config and the `/codec` registry, so the advertised and applied device agree.
- `GET /codec` now adds `backends` (unique advertised backends such as
  `["software","vaapi","nvenc"]`); `available` remains the unique video codec
  names (`["h264","hevc"]` on a VAAPI+NVENC host). `POST /codec` maps every
  advertised `(codec, backend)` to its HLS encoder, but still returns
  `applied:true` only after a successful HLS restart whose runtime probe encoded
  a real segment. A failed runtime probe is a typed 502 and restores the
  previous pipeline.
- Added `EncoderRegistry::select_buildable` for the session/native-media path so
  an advertised HLS-only backend (NVENC/QSV/V4L2M2M, which `ozma-encode` has no
  concrete adapter for) cannot be selected by `SessionBackend::build_encoder`.

**Tests added:**

- `ozma-encode`: configured-device resolution with a temp render-node path;
  VAAPI H.264+HEVC advertisement with a fake passing probe; no device/no
  passing probe; HEVC requires the ffmpeg listing; NVENC listed/absent and
  device present/absent; QSV/V4L2M2M require a verified encode; no invented
  hardware or `Fake`; `select_buildable` skips HLS-only backends; ffmpeg table
  parsing.
- `ozma-node`: `/codec` lists unique codecs and backends from a fake registry
  with VAAPI H.264+HEVC and NVENC; every advertised VAAPI/NVENC/QSV/V4L2M2M
  H.264/HEVC entry maps and is stored without a false `applied`; an advertised
  HEVC VAAPI entry whose runtime probe fails is a typed 502 and never returns
  `applied:true`.

Evidence from this follow-up:

```text
cargo test -p ozma-encode -p ozma-node
  ozma-encode lib: 68 passed; 0 failed
  ozma-node lib:  191 passed; 0 failed; 3 ignored
  all integration binaries: 0 failed

cargo check --workspace --all-targets: ok
cargo fmt -p ozma-encode -p ozma-node -- --check: clean
```

**Honesty limits:** VAAPI HEVC is advertised because the same device/driver
passed a real H.264 encode and ffmpeg lists `hevc_vaapi`; the registry does not
run a separate HEVC encode. NVENC likewise relies on the listing plus a device
node, because the HLS runtime probe is the gate immediately before
`applied:true`. QSV/V4L2M2M are omitted on this host because their one-frame
checks fail, matching §7.2 and §7.3.

## 7. Remaining hardware gaps and findings

1. **No local NVIDIA device; full-node NVENC is now validated on `queen`.**
   The full wrapper (`--hls-encoder h264_nvenc`/`hevc_nvenc` and
   `POST /codec` with `hw_accel:"nvenc"`) ran on both the GTX 1660 and RTX 3090
   using the copied current binary (§4.3). The remaining local gap is that all
   NVENC evidence is remote.
2. **QSV fails locally.** ffmpeg lists `h264_qsv`/`hevc_qsv`, `vainfo` shows
   VAAPI encode, but oneVPL/MFX returns `Error creating a MFX session: -9`
   (device creation failed). QSV argv/rate control therefore remains
   unvalidated. No driver/package change was attempted.
3. **V4L2M2M has no encoder device.** Only the UVC camera (`/dev/video0/1`,
   capture/metadata) exists; there is no mem2mem node. Only the honest failure
   path is validated.
4. **`/codec` advertised less than the CLI supports — fixed in §6.4.**
   Before the follow-up, `EncoderRegistry::with_detected_encoders()` probed
   software H.264 plus VAAPI H.264 only. `POST /codec` therefore rejected
   HEVC+VAAPI with `requested_codec_unavailable`, and never selected
   NVENC/QSV/V4L2M2M even when the CLI accepts those names.
   `GET /codec.available` was correspondingly narrow (`["h264"]` on this host).
   The registry now advertises software H.264, device-proven VAAPI H.264+HEVC,
   NVENC when listed with a CUDA/NVML device, and QSV/V4L2M2M only after a real
   one-frame encode check.
5. **VAAPI detection ignored `--vaapi-device` — fixed in §6.4.** The old
   `with_detected_encoders()` used `VaapiProbe::new()`'s hard-coded
   `/dev/dri/renderD128`, regardless of the configured device. The codec
   registry and HLS config now both use `NodeConfig.vaapi_device`; an absent
   configured path is not replaced by another render node.
6. **Fallback reporting inconsistency.** With
   `allow_software_fallback:true` and an unregistered backend (NVENC), the
   policy selects `libx264` and the receipt correctly records
   `applied:false` + `fallback_reason:"requested_backend_unavailable"`, but the
   top-level response reports `fallback:false` and
   `requested_encoder:"libx264"`. A controller that only reads the top level
   cannot tell the request fell back. The unit tests cover the HLS-level
   fallback path (where the receipt and top level agree), not this
   policy-level path.
7. **Probe dimensions are 320x240.** The VAAPI and QSV/V4L2M2M checks prove the
   device/driver and encoder wrapper, not 1080p/4K, HEVC 10-bit, or HDR. The
   advertised envelope remains 1920x1080 at 60 fps; larger frames need a
   device-specific run. Full-node NVENC is validated on both `queen` GPUs at
   720x400, 5 fps, 1 Mbps. Its registry advertisement relies on the ffmpeg
   listing plus a CUDA/NVML node; the HLS runtime probe is the gate before
   `applied:true`. NVENC rate-control accuracy (CBR adherence) and 10-bit/HDR
   remain unmeasured.
8. **Post-probe failure is still possible.** `applied:true` now requires a real
   probe segment, but a device that disappears after the probe can still make
   the live pipeline fail later; `/api/v1/connection` then reports `failed`.
   The probe narrows, but does not eliminate, that window.
9. **No browser/controller playback.** Segments were fetched over the node's
   real HTTP `/stream/*` route on `queen` and decoded with `ffprobe`; no
   browser, GStreamer, or controller-proxy decode session was run for hardware
   HLS.
10. **Probe cost.** A hardware start/restart now blocks for one short encode
    (bounded at 10 s) before reporting success. On this host it completed in
    well under a second for VAAPI.
11. **No node NVENC GPU selector.** The node exposes no `--nvenc-gpu`/`-gpu`
    flag, so the two-GPU run injected `-gpu` through a test-only
    `--ffmpeg-bin` wrapper. CUDA ordinals were reversed relative to nvidia-smi
    indices on `queen` (`-gpu 0` → RTX 3090, `-gpu 1` → GTX 1660), so a raw
    device number is not self-describing. This is a usability gap for
    multi-GPU hosts, not a correctness gap.
12. **Binary provenance for the full-node run.** The copied binary was built
    from the local working tree immediately before local commit `27ccbe1` and
    was not rebuilt after that commit. Its `/codec` output already showed the
    NVENC registry detection, but a rebuild and re-run at the final HEAD is
    still the clean reproducibility step.

## 8. Cleanup

Local:

- All `ozma-node` test processes were terminated by their recorded PIDs; the
  disposable QEMU was killed via its pidfile.
- Every port used by this run was released; a final `ss` check found no
  listener from this run. (An unrelated concurrent test process later used
  47337/47339; it was not started or touched by this run.)
- `/tmp/opencode/ozma-gate2-hw` was removed.
- Probe temp dirs (`/tmp/ozma-hls-encoder-probe-*`) are removed by the probe
  itself on every path.

`queen`:

- SSH used `-J ha.hrdwrbob.net matt@queen` with a temporary
  `UserKnownHostsFile` under `/tmp`; that file was removed locally. No
  package, driver, service, or repository change was made.
- `/tmp/queen-nvenc-*` was removed by the remote `trap`; no test ffmpeg
  processes remain.
- Full-node follow-up: `/tmp/ozma-gate2-queen` (copied binary, `ffmpeg-gpu.py`
  test wrapper, QEMU pid/socket, HLS dirs, fetched segments, logs) was removed
  after killing the disposable QEMU and all test nodes. Ports
  47460–47469/TCP and 60460–60469/UDP are free. `queen`'s repo status was
  unchanged; nothing under `/applications/data/projects/ozma` was edited.
- The earlier temporary `queen` host-key entry was removed from the local
  `~/.ssh/known_hosts`; `ssh-keygen` rotated its `.old` backup file. The
  follow-up avoided a persistent known_hosts entry entirely.

> Note: cleanup first used a broad `pkill` pattern that also matched an
> unrelated disposable test node (`ozma-gate2-dbusupdate`, HTTP 47337). That
> process was terminated by the pattern; cleanup then switched to recorded PIDs
> only. No production VM, libvirt domain, persistent service, or non-test
> process was touched.
