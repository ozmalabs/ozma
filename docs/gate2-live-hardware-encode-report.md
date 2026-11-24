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
and QSV/V4L2M2M after a real encode check. A later follow-up adds a first-class
`--nvenc-gpu`/`OZMA_NVENC_GPU` selector and reruns both `queen` GPUs with no
external ffmpeg wrapper (§4.4): `-gpu n` is emitted only for NVENC, preserved
across `/codec` restarts, and rejected for non-NVENC startup selections.
A final follow-up (§9) adds a clearly labelled dev/test synthetic capture path
and measures 1080p and 4K at 1-second segments on local VAAPI (H.264 and HEVC
on both Intel and AMD) and both `queen` NVENC GPUs (H.264 and HEVC). It
also fixes VAAPI rate control: the old argv left `rc_mode=auto`, which overshot
the requested bitrate by 22-26% on Intel; naming `-rc_mode CBR` brings the
steady-state mean to +6-7%. NVENC CBR holds at +3-6% for 1080p/4K on both GPUs.
HEVC Main10/p010 encodes directly on both VAAPI devices. A follow-up (§9.4)
extends the HLS path with an opt-in 10-bit/Main10 target for software
libx265/AV1, `hevc_vaapi`, and `hevc_nvenc`, plus HDR color signaling; the
default remains SDR 8-bit. The node's advertised encode envelope now follows
the real capture size (4K for a 4K fake source) instead of a hard-coded 1080p,
and `/codec` reports `max_width`/`max_height`, `hdr`, and `pixel_formats`.

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
| Local git HEAD | `e600c43630185d92df29099eff1931c65b61c0d7` plus the uncommitted selector/test/docs changes |
| Working tree | dirty before this work (unrelated concurrent docs edits); this follow-up changes `ozma-node/src/{hls,config,main,controller_compat}.rs`, `ozma-node/src/platform.rs` (test literal), `ozma-node/tests/parity*`, and these docs, all uncommitted |
| Local kernel | `7.3.5-2-cachyos` x86_64 |
| Local ffmpeg | n9.1 (`/usr/bin/ffmpeg`) |
| QEMU | `qemu-system-x86_64` 11.1.3, TCG |
| `queen` kernel | `6.8.0-147-generic` x86_64 (Ubuntu 24.04) |
| `queen` ffmpeg | 6.1.1-3ubuntu5 (`/usr/bin/ffmpeg`) |
| `queen` GPU driver | NVIDIA 610.57.04, CUDA UMD 13.3 |
| Temp dirs | `/tmp/opencode/ozma-gate2-hw`, `/tmp/opencode/ozma-synth`, `/tmp/queen-nvenc-*`, `/tmp/ozma-gate2-queen`, `/tmp/ozma-gate2-queen2`, `/tmp/ozma-gate2-synth` (all removed) |
| Ports | HTTP 47333–47350 and 47520–47747, HID 60333–60350 and 60520–60747 (released) |

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
possible with that binary; §4.3 and §4.4 later copied a binary built from the
current tree (no package installs, no repo modification).

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
part of the disposable test harness. This wrapper is no longer needed: §4.4
adds the first-class `--nvenc-gpu` flag and reruns both GPUs without it.

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

### 4.4 First-class NVENC GPU selector, no wrapper (follow-up)

The node originally had no GPU selector; §4.3 injected `-gpu` through a
test-only `--ffmpeg-bin` wrapper. This follow-up adds a real option so the
selection and its restrictions are part of the node contract:

| Item | Value |
|---|---|
| CLI | `--nvenc-gpu <INDEX>` / `OZMA_NVENC_GPU`; `--cuda-device` is an alias |
| Stored in | `NodeConfig.nvenc_gpu` and `HlsConfig.nvenc_gpu` (`Option<u32>`, default `None`) |
| argv rule | `-gpu <n>` is appended after `-c:v h264_nvenc`/`-c:v hevc_nvenc` only; software, VAAPI, QSV, and V4L2M2M argv never receive it |
| Non-NVENC CLI | rejected at startup with exit 2: `--nvenc-gpu selects an NVIDIA device and is only used by h264_nvenc/hevc_nvenc; the selected encoder libx264 is software` |
| `/codec` | the stored ordinal is kept across restarts and reported as `nvenc_gpu`; a software restart emits no `-gpu`, and switching back to NVENC targets the same ordinal |
| No configuration | `nvenc_gpu: null`; no ordinal is invented |

The hardware runtime probe uses the same `ffmpeg_argv`, so an out-of-range
value is rejected by config validation and an unusable ordinal fails the real
probe before a pipeline is reported as started or `/codec` reports
`applied:true`.

Fresh `queen` run (binary rebuilt from this working tree; no wrapper):

| Item | Value |
|---|---|
| Binary | local `target/debug/ozma-node`, 282,264,008 B, built 2026-11-24 17:40 AEDT |
| `queen` copy | `/tmp/ozma-gate2-queen2/ozma-node` (removed after the run) |
| SSH | `queen` did not resolve locally during this run, so the same host was reached as `ssh -o BatchMode=yes -o HostName=192.168.40.215 queen` |
| QEMU | 8.2.2, TCG, disposable VGA + one QMP socket, no disk/network |
| ffmpeg | 6.1.1-3ubuntu5; `h264_nvenc`, `hevc_nvenc`, `av1_nvenc` listed |
| Node options | `--profile vm --qmp-socket ... --input-provider recording --power-provider qmp --cap video --hls --hls-fps 5 --hls-bitrate-kbps 1000 --hls-encoder h264_nvenc --nvenc-gpu <0\|1>` |

Live argv observed from the node's own ffmpeg child (720x400 QMP console):

```text
ffmpeg ... -f rawvideo -pix_fmt rgb24 -s 720x400 -r 5 -i - -an \
  -c:v h264_nvenc -gpu 0 -preset p4 -tune ll -rc cbr -b:v 1000k \
  -pix_fmt yuv420p -g 5 -force_key_frames expr:gte(t,n_forced*1) -f hls ...
```

Results (`nvidia-smi --query-compute-apps=pid,gpu_uuid` correlated to the live
ffmpeg child PID, not inferred from the ordinal):

| `--nvenc-gpu` | Selected UUID | nvidia-smi name/index | Node argv | Segment |
|---|---|---|---|---|
| `0` | `GPU-84baadff-eed0-6ee3-4c90-cca7e4182df8` | RTX 3090, index 1 | `-gpu 0` | h264 **Main**, 720x400, **yuv420p**, 5/1 |
| `1` | `GPU-e88e2536-789f-8b56-b2c0-db9e1197497b` | GTX 1660 SUPER, index 0 | `-gpu 1` | h264 **Main**, 720x400, **yuv420p**, 5/1 |

The CUDA ordinal → nvidia-smi index reversal reported in §4.3 is reproducible
and is now explicit operator-visible configuration rather than wrapper
behaviour.

`/codec` preservation on the `--nvenc-gpu 0` node:

```text
GET  /codec -> current_encoder:"h264", available:["h264","hevc"],
               backends:["software","nvenc"], nvenc_gpu:0
POST /codec {"codec":"hevc","hw_accel":"nvenc"}
     -> 200 applied:true encoder:"hevc_nvenc" nvenc_gpu:0 fallback:false
child argv -> -c:v hevc_nvenc -gpu 0 ...
nvidia-smi -> child PID on GPU-84baadff… (RTX 3090)
segment -> hevc Main 720x400 yuv420p
```

The non-NVENC reject was exercised on `queen` with
`--hls-encoder libx264 --nvenc-gpu 0`: exit code 2 and the typed invalid-HLS
configuration message shown above. No ffmpeg wrapper, package change, VM
change, or sudo was used.

Local verification for this state:

```text
cargo test -p ozma-node
  -> 244 passed; 0 failed; 4 ignored
cargo test -p ozma-node --lib hls::
  -> 37 passed; 0 failed; 1 ignored
cargo test -p ozma-node --lib controller_compat::tests::codec
  -> 14 passed; 0 failed; 0 ignored
cargo check --workspace --all-targets
  -> ok
cargo fmt -p ozma-node
  -> completed
```

A later re-run in the shared tree hit unrelated, in-flight `ozma-protocol`
edits (`scene.rs` imported an undeclared `display_method` module), so the same
`ozma-node` patch was re-tested in a clean `HEAD` archive: 198 library tests
plus all `ozma-node` integration targets passed. That failure is not in this
change and was not modified here.

Resolution limit: the disposable QEMU guest has no OS, so its VGA console
stays at 720x400. `queen` had no libvirt domain (`virsh list --all` empty) and
no X display during the run, so a 1080p/4K guest/display was **not** available;
the node-level 1080p/4K, 10-bit, and HDR paths remain unmeasured. The HLS argv
pins `-pix_fmt yuv420p` (8-bit 4:2:0), and nothing in the selector changes the
resolution envelope.

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
   The full wrapper (`--hls-encoder h264_nvenc`/`hevc_nvenc`,
   `--nvenc-gpu 0|1`, and `POST /codec` with `hw_accel:"nvenc"`) ran on both
   the GTX 1660 SUPER and RTX 3090 using a freshly copied current binary
   (§4.3, §4.4). The remaining local gap is that all NVENC evidence is remote.
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
7. **Probe dimensions are 320x240; 1080p/4K is now measured via the
   dev/test source.** The VAAPI and QSV/V4L2M2M checks prove the
   device/driver and encoder wrapper. The advertised envelope remains
   1920x1080 at 60 fps; larger frames still need a device-specific run before
   they are advertised. The synthetic-capture follow-up (§9) measured real
   HLS 1080p and 4K at 30 fps for libx264, Intel/AMD VAAPI H.264, Intel VAAPI
   HEVC, and both `queen` NVENC GPUs with 1-second segments. This is not a
   camera/guest workload, so per-device 4K camera capture remains unmeasured.
   `-pix_fmt yuv420p` is pinned, so HDR and 10-bit HLS output are still not
   implemented; VAAPI Main10/p010 works only in a direct ffmpeg test, not
   through the node argv. NVENC CBR adherence is now measured on both GPUs
   (§9), including at 4K.
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
11. **NVENC GPU selector — fixed in §4.4.** The node now has
    `--nvenc-gpu <n>` / `OZMA_NVENC_GPU` (alias `--cuda-device`), stored in
    `NodeConfig`/`HlsConfig` and emitted as `-gpu <n>` only for
    `h264_nvenc`/`hevc_nvenc`. It is preserved across `/codec` restarts and
    reported as `nvenc_gpu`; a non-NVENC startup selection is a typed exit-2
    error, and no ordinal is invented when unset. Both `queen` GPUs were rerun
    without the external wrapper. CUDA ordinals remain reversed relative to
    nvidia-smi indices on that host, so the operator must map ordinals to
    devices with `nvidia-smi` when they are not identical GPUs.
12. **Binary provenance for the full-node run.** The §4.3 binary was copied
    before local commit `27ccbe1` and not rebuilt after it. The §4.4 selector
    rerun used a binary rebuilt from the current working tree (including the
    selector changes) and copied fresh to `queen`; the tree still has unrelated
    concurrent docs edits, so this is working-tree evidence, not a commit.

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
- Selector follow-up: `/tmp/ozma-gate2-queen2` (copied binary, QEMU
  pid/socket, both HLS dirs, logs) was removed; both recorded node PIDs
  (`670460`, `676747`) and both recorded QEMU PIDs (`667847`, `675718`) were
  killed by explicit PID. Ports 47480–47500/TCP and 60480–60500/UDP are free.
  One cleanup command used `pgrep -f ozma-queen-gpu0`, which matched its own
  SSH shell and ended that shell before the following line ran; no unrelated
  process was signalled by that pattern. Every later kill used a recorded PID
  or the specific live ffmpeg child PID.
- The earlier temporary `queen` host-key entry was removed from the local
  `~/.ssh/known_hosts`; `ssh-keygen` rotated its `.old` backup file. The
  follow-up avoided a persistent known_hosts entry entirely.

> Note: cleanup first used a broad `pkill` pattern that also matched an
> unrelated disposable test node (`ozma-gate2-dbusupdate`, HTTP 47337). That
> process was terminated by the pattern; cleanup then switched to recorded PIDs
> only. No production VM, libvirt domain, persistent service, or non-test
> process was touched.

## 9. Synthetic capture: 1080p/4K and CBR follow-up

**Why:** the disposable QEMU VGA is only 720x400 without a guest, so the
hardware HLS path had no 1080p/4K source. This follow-up adds a clearly labelled
dev/test synthetic capture provider, then measures 1080p and 4K H.264/HEVC with
1-second segments on both local VAAPI devices and both `queen` NVENC GPUs. It is
evidence for the HLS/hardware path only; it is not a camera, guest, or
production source.

**Run provenance:** local HEAD `f645ae9` plus uncommitted working-tree changes
(the tree already contained unrelated edits from concurrent sessions). The
release binary was built from that tree and copied to `queen`; nothing was
committed. Only `ozma-node/src/{config,platform,main,hls}.rs`,
`ozma-capture/src/fake.rs`, and these two docs were changed by this follow-up.

### 9.1 Dev/test synthetic capture

| Item | Value |
|---|---|
| Flag | `--capture-provider fake` |
| Geometry | `--fake-width` (default 1280), `--fake-height` (default 720), `--fake-fps` (default 30); env `OZMA_FAKE_WIDTH/HEIGHT/FPS` |
| Bounds | dimensions 1–8192, fps 1–240 (`NodeConfig::validate`) |
| Profiles | explicit `--profile vm` or `--profile desktop`; **rejected on `hardware`** with a typed exit-2 error |
| Guard | `--fake-width/--fake-height/--fake-fps` without `--capture-provider fake` is an exit-2 error |
| Log | `WARN ... DEV/TEST SYNTHETIC CAPTURE ... not a real camera, guest, or hardware source` |
| Provider | `ozma_capture::FakeCaptureProvider`, id `fake0`, name `<node> synthetic capture (dev/test)`, RGB24 |
| Pattern | `FakePattern::Blocks`: deterministic 32×32 full-color blocks, reseeded each frame |
| Defaults | no profile selects `fake`; the default encoder, bitrate, and segment settings are unchanged |

The 32×32 mosaic was chosen after two extremes made CBR measurement
meaningless: the historical byte ramp is so compressible that VAAPI undershot a
20 Mbps 4K target at 8.6 Mbps (H.264) / 5.6 Mbps (HEVC), while full per-pixel
noise is so expensive that the encoder's minimum-QP floor overshot at 25.9 Mbps
(Intel) / 55.2 Mbps (AMD). A block mosaic sits between those regimes and still
has deterministic, frame-varying detail. `FakePattern::Ramp` remains the
provider default for CI; only the node's dev path calls `with_blocks()`.

A clean local run needs no QEMU or QMP socket:

```bash
target/release/ozma-node --name synth --profile vm \
  --capture-provider fake --fake-width 3840 --fake-height 2160 --fake-fps 30 \
  --input-provider fake-recording --cap video \
  --hls --hls-dir /tmp/opencode/ozma-synth/hls --hls-fps 30 \
  --hls-bitrate-kbps 20000 --hls-encoder h264_vaapi \
  --vaapi-device /dev/dri/renderD128
```

### 9.2 Local 1080p/4K results

Method: release binary, 30 fps, `-hls_time 1`, 12 s steady window after startup;
`ffprobe` reports one 1.000 s video segment per file. The mean excludes the
first two startup segments; min/max are per-segment. CPU is node + live ffmpeg
child, normalized to one core = 100% (so 265% is 2.65 cores). All output was
`yuv420p`; H.264 was **High**, HEVC **Main**.

| Device | Encoder | Requested | Steady mean | Error | Segment min–max | CPU | Segment cadence |
|---|---|---:|---:|---:|---:|---:|---|
| Intel Arc B580 (`renderD128`) | `libx264` | 4,000 kbps | 4,398.9 | +10.0% | 4,287.9–4,630.8 | 90.5% | ~1/s |
| Intel Arc B580 | `h264_vaapi` (`-rc_mode CBR`) | 4,000 | 4,239.8 | +6.0% | 3,981.1–4,784.2 | 28.7% | ~1/s |
| Intel Arc B580 | `hevc_vaapi` (`-rc_mode CBR`) | 4,000 | 4,266.7 | +6.7% | 4,158.6–4,606.8 | 29.4% | ~1/s |
| AMD Radeon 610M (`renderD129`) | `h264_vaapi` (`-rc_mode CBR`) | 4,000 | 4,043.4 | +1.1% | 4,030.7–4,060.8 | 31.2% | ~1/s |
| AMD Radeon 610M (`renderD129`) | `hevc_vaapi` (`-rc_mode CBR`) | 4,000 | 4,406.7 | +10.2% | 4,202.2–4,456.4 | 31.7% | ~1/s |
| Intel Arc B580 | `libx264` | 20,000 | 21,134.4 | +5.7% | 20,929.7–21,420.0 | 265.3% | ~1/s |
| Intel Arc B580 | `h264_vaapi` (`-rc_mode CBR`) | 20,000 | 21,346.5 | +6.7% | 19,088.8–22,948.0 | 70.9% | ~1/s |
| Intel Arc B580 | `hevc_vaapi` (`-rc_mode CBR`) | 20,000 | 21,400.2 | +7.0% | 16,122.9–24,292.6 | 78.5% | ~1/s |
| AMD Radeon 610M | `h264_vaapi` (`-rc_mode CBR`) | 20,000 | 19,958.1 | −0.2% | 19,888.9–20,033.3 | 74.2% | ~1/s |
| AMD Radeon 610M | `hevc_vaapi` (`-rc_mode CBR`) | 20,000 | 22,274.0 | +11.4% | 22,236.6–22,337.4 | 74.6% | ~1/s |

4K was real-time in this path: one 1-second segment completed per wall-clock
second, with 30 fps input. Software 4K used ~2.65 cores; VAAPI used ~0.7–0.8 of
one core on the node+ffmpeg side and encoded on the GPU. Concurrent `gputop`
samples during 4K H.264 showed ffmpeg engine utilization of 2.5–8.1% on the
Intel device and 4.3–35.6% on the AMD device (the driver-specific engine column
labels differ); AMD's `gpu_busy_percent` sysfs value stayed 0 in these short
runs, so it is not used as evidence.

**VAAPI rate-control fix.** With the old argv (`-b:v` only, `rc_mode=auto`),
Intel overshot the same 32×32-block content:

| Case | Before (`auto`) | After (`-rc_mode CBR`) |
|---|---:|---:|
| Intel `h264_vaapi` 1080p | +26.1% (5,044.1 kbps) | +6.0% (4,239.8 kbps) |
| Intel `h264_vaapi` 4K | +22.5% (24,490.1 kbps) | +6.7% (21,346.5 kbps) |
| Intel `hevc_vaapi` 1080p | +6.3% (4,251.1 kbps) | +6.7% (4,266.7 kbps) |
| Intel `hevc_vaapi` 4K | +14.1% (22,810.3 kbps) | +7.0% (21,400.2 kbps) |
| AMD `h264_vaapi` 1080p/4K | +1.1% / −0.2% | unchanged |

The node now emits `-rc_mode CBR` for `h264_vaapi`/`hevc_vaapi`. Per-segment
variation remains (GOP boundaries and VAAPI's rate control), so this is
steady-state accuracy, not a per-segment byte guarantee. A direct ffmpeg check
with `testsrc2`, `smptehdbars`, `testsrc`, and `mandelbrot` at 1080p/4 Mbps also
held at 4.1–4.2 Mbps with CBR.

### 9.3 `queen` NVENC results

The current `target/release/ozma-node` (24,794,480 B; max GLIBC 2.39) was copied
to `/tmp/ozma-gate2-synth/ozma-node`; `queen` has GLIBC 2.39 and resolved every
shared library. No QEMU was used: the synthetic source feeds the node directly.
Same 30 fps, 1-second segments, 12 s window, steady mean excluding two startup
segments.

**CUDA ordinal mapping on `queen` is reversed relative to `nvidia-smi`:**
verbose `h264_nvenc` output lists `GPU #0 - RTX 3090` and
`GPU #1 - GTX 1660 SUPER`, while `nvidia-smi` lists index 0 = GTX 1660 SUPER,
index 1 = RTX 3090. The table uses the node's `--nvenc-gpu`/CUDA ordinal.
`nvidia-smi` encoder utilization and the live ffmpeg child confirmed the
requested device was used.

| `--nvenc-gpu` (CUDA) | Device | Encoder | Requested | Steady mean | Error | Segment min–max | CPU |
|---:|---|---|---:|---:|---:|---:|---:|
| 0 | RTX 3090 | `h264_nvenc` | 4,000 | 4,124.0 | +3.1% | 4,065.3–4,160.1 | 45.9% |
| 0 | RTX 3090 | `hevc_nvenc` | 4,000 | 4,171.9 | +4.3% | 3,997.6–4,500.0 | 46.6% |
| 0 | RTX 3090 | `h264_nvenc` | 20,000 | 20,682.1 | +3.4% | 20,397.2–21,635.0 | 105.9% |
| 0 | RTX 3090 | `hevc_nvenc` | 20,000 | 21,105.3 | +5.5% | 20,448.4–21,462.1 | 106.7% |
| 1 | GTX 1660 SUPER | `h264_nvenc` | 4,000 | 4,126.1 | +3.2% | 4,062.3–4,179.6 | 46.8% |
| 1 | GTX 1660 SUPER | `hevc_nvenc` | 4,000 | 4,171.9 | +4.3% | 3,997.6–4,500.0 | 47.2% |
| 1 | GTX 1660 SUPER | `h264_nvenc` | 20,000 | 20,650.8 | +3.3% | 20,156.6–21,638.0 | 107.6% |
| 1 | GTX 1660 SUPER | `hevc_nvenc` | 20,000 | 20,979.0 | +4.9% | 20,448.4–21,462.1 | 106.1% |

All segments were H.264 **Main** or HEVC **Main**, `yuv420p`, 3840x2160 or
1920x1080, 30 fps, 1.000 s. NVENC kept CBR within +3.1–5.5% including MPEG-TS
overhead, on both GPUs and both resolutions. The GTX 1660 SUPER was
concurrently running Frigate CUDA/ffmpeg work (nvidia-smi 20–45% GPU, 15–18%
encoder during a probe), and the RTX 3090 had ~22.8 GB used by an unrelated
resident workload but 0% utilization; neither prevented the encode or broke the
CBR result. CPU is again node+ffmpeg normalized to one core.

### 9.4 HEVC 10-bit/HDR HLS (implemented follow-up)

The HLS path now has an opt-in 10-bit/Main10 target. Production defaults are
unchanged: no `--hls-*bit`/HDR flag still means `yuv420p`, SDR, 8-bit.

**CLI contract.** `--hls-pixel-format yuv420p|p010` (env
`OZMA_HLS_PIXEL_FORMAT`) or `--hls-10bit` selects the target.
`--hls-hdr` (env `OZMA_HLS_HDR`) requires a 10-bit target and supplies
BT.2020/PQ defaults (`bt2020` / `smpte2084` / `bt2020nc`) for unset fields.
`--hls-color-primaries`, `--hls-color-transfer`, and `--hls-color-matrix`
override individual fields and are checked against the ffmpeg color-name
allowlists. An explicit `yuv420p` plus `--hls-10bit`/`--hls-hdr` is an exit-2
configuration error.

**Encoder mapping.** 10-bit is only accepted where the pipeline has a real
Main10 argv; unsupported combinations return a typed
`HlsError::UnsupportedPixelFormat` before ffmpeg is spawned.

| Encoder | 10-bit argv |
|---|---|
| `libx265` | `-vf format=yuv420p10le -profile:v main10 -pix_fmt yuv420p10le` |
| `libsvtav1` / `libaom-av1` | `-vf format=yuv420p10le -profile:v main -pix_fmt yuv420p10le` |
| `hevc_vaapi` | `-vf format=p010,hwupload -profile:v main10` (no output pin) |
| `hevc_nvenc` | `-profile:v main10 -pix_fmt p010le` |
| `libx264`/`h264_vaapi`/`h264_nvenc`/QSV/V4L2M2M/MJPEG | rejected for `p010` |

A captured P010 source is fed as raw `p010le` with the same 3-byte-per-pixel
semi-planar layout. The dev fake provider also gained a real P010 pattern
(`FakeCaptureProvider::with_p010()`); the live runs below instead used the
default RGB24 fake source and let ffmpeg convert to P010/Main10, so no external
10-bit source or converter was needed.

**HDR signaling.** `-color_primaries/-color_trc/-colorspace` alone were not
enough through the HLS/MPEG-TS path: `ffprobe` reported `color_transfer=unknown`
and `color_primaries=unknown` (the same gap as the earlier direct VAAPI test).
The node therefore also emits a sequence-header rewrite:
`-bsf:v hevc_metadata=colour_primaries=9:transfer_characteristics=16:matrix_coefficients=9`
for HEVC (or `av1_metadata` for AV1) when color metadata is configured. This
preserves the validated primaries/transfer/matrix values in the HLS segment.

Live local validation (debug binary from this tree, 30 fps, 1-second segments,
default fake RGB24 source; all processes and output removed afterwards):

```bash
# Intel VAAPI, 4K, 10-bit HDR
target/debug/ozma-node --name hdrintel --profile vm \
  --capture-provider fake --fake-width 3840 --fake-height 2160 --fake-fps 30 \
  --input-provider fake-recording --cap video \
  --hls --hls-dir /tmp/opencode/ozma-hdr-live/hls --hls-fps 30 \
  --hls-bitrate-kbps 12000 --hls-encoder hevc_vaapi \
  --vaapi-device /dev/dri/renderD128 --hls-10bit --hls-hdr

# Software libx265, 1080p, 10-bit HDR
target/debug/ozma-node --name hdrsoft --profile vm \
  --capture-provider fake --fake-width 1920 --fake-height 1080 --fake-fps 30 \
  --input-provider fake-recording --cap video \
  --hls --hls-dir /tmp/opencode/ozma-hdr-soft-node/hls --hls-fps 30 \
  --hls-bitrate-kbps 8000 --hls-encoder libx265 --hls-10bit --hls-hdr
```

`ffprobe` on the resulting segments:

```text
# /tmp/opencode/ozma-hdr-live/hls/seg_00001.ts (Intel hevc_vaapi, 4K)
codec_name=hevc
profile=Main 10
width=3840
height=2160
pix_fmt=yuv420p10le
color_range=tv
color_space=bt2020nc
color_transfer=smpte2084
color_primaries=bt2020

# /tmp/opencode/ozma-hdr-soft-node/hls/seg_00001.ts (libx265, 1080p)
codec_name=hevc
profile=Main 10
width=1920
height=1080
pix_fmt=yuv420p10le
color_range=tv
color_space=bt2020nc
color_transfer=smpte2084
color_primaries=bt2020
```

Both runs wrote real `.ts` segments and playlists; the 4K VAAPI run produced
`seg_00000.ts` (2,429,900 B) and `seg_00001.ts` (1,110,704 B) with Main10
`yuv420p10le`, and the software run produced a rolling 1080p playlist. No
`queen`/NVENC 10-bit run was made in this follow-up, so `hevc_nvenc` Main10 is
covered by argv/unit tests only.

**Envelope.** `platform::capture_envelope` derives the advertised encode bounds
from the discovered capture source (4K fake source → 4K envelope; 1080p source →
1080p), capped at 4K/60 fps, instead of the previous hard-coded 1080p. Software,
VAAPI, and NVENC capabilities are fitted to that envelope; QSV/V4L2M2M keep
their earlier bound because this tree has no 4K evidence for them. HDR is only
set when the configured HLS target is validated 10-bit HDR and only for the
configured codec family, never inferred from 4K alone. A live `GET /codec` for
the 4K Intel VAAPI HDR configuration returned:

```json
{"current_encoder":"hevc","available":["hevc"],"backends":["vaapi"],
 "max_width":3840,"max_height":2160,"max_fps":30,
 "hdr":true,"pixel_formats":["yuv420p","p010"]}
```

The 10-bit registry hides codec/backend pairs without a Main10 mapping
(H.264/MJPEG/QSV/V4L2M2M), so a controller cannot select a combination the
fixed 10-bit pipeline would reject. The same live run reported
`/api/v1/connection` `hls: "running"` after the manifest appeared.

**Remaining HDR gaps:** no real HDR camera/guest source was used; the synthetic
pattern is not colorimetrically HDR (the metadata is a signaling test). There is
no live NVENC 10-bit run in this follow-up, AV1 10-bit has argv/tests but no HLS
playback evidence, browser/player decode of Main10 HLS is not validated, and
H.264/QSV/V4L2M2M 10-bit remain unsupported rather than silently downconverted.
This is HLS evidence only, not a WebRTC/Gate 2 claim.

### 9.5 Tests added

- `ozma-node/src/config.rs`: fake capture is accepted for VM/desktop, rejected
  for hardware, and geometry/fps bounds are enforced.
- `ozma-node/src/platform.rs`: a VM fake registry advertises id `fake0`,
  requested geometry/fps, kind `Fake`, and opens a source with the same format.
- `ozma-node/src/main.rs`: `--fake-*` flags parse only with
  `--capture-provider fake`.
- `ozma-node/src/hls.rs`: 4K argv carries `3840x2160`, the requested bitrate,
  1-second segments, NVENC `-gpu`/`-rc cbr`, VAAPI `-rc_mode CBR` + `nv12`
  upload, and the 8-bit `yuv420p` pin (the default regression).
- `ozma-node/src/hls.rs`: 10-bit argv for `libx265`, `libsvtav1`, `hevc_vaapi`,
  and `hevc_nvenc` (`p010` upload / `yuv420p10le` / `p010le`, Main10 profile,
  HDR color args + `hevc_metadata`/`av1_metadata` BSF); every H.264/MJPEG/QSV/
  V4L2M2M combination is rejected with `UnsupportedPixelFormat`; HDR requires
  10-bit; invalid color names and `yuv420p + hdr` are rejected; P010 input is
  declared as raw `p010le` and passed through unchanged.
- `ozma-node/src/platform.rs`: a 4K fake source yields a 4K encode envelope
  without HDR; the configured HEVC HLS capability is 4K SDR or 10-bit HDR
  (`main10`) only when configured; an 8K source envelope is capped at 4K.
- `ozma-node/src/main.rs`: `--hls-10bit`/`--hls-pixel-format`/`--hls-hdr`
  resolution, explicit 8-bit conflicts, and all four color flags parse.
- `ozma-capture/src/fake.rs`: the block pattern is deterministic per sequence,
  differs between frames, and the first 32×32 block is uniform; `with_p010()`
  emits a real 10-bit semi-planar layout with frame-varying limited-range luma.

Default tests use no real GPU: the HLS lib suite, `ozma-capture`, platform/main
unit tests, and the parity suite pass without hardware. The required commands
(`cargo test -p ozma-node -p ozma-capture`, `cargo check --workspace
--all-targets`, `cargo fmt`) are recorded in the final verification summary.

### 9.6 Cleanup

Local:

- `/tmp/opencode/ozma-synth` (case HLS dirs, copied segments, logs, raw noise
  and block inputs, direct 10-bit outputs) removed.
- The 10-bit follow-up directories `/tmp/opencode/ozma-hdr-live`,
  `/tmp/opencode/ozma-hdr-soft-node`, `/tmp/opencode/ozma-envelope-live`, and
  the direct `hdr-soft`/`hdr-bsf` probes were removed; no node/ffmpeg child or
  listener remained.
- All synth node processes were killed by process group; no test ffmpeg child
  or listener remained. Ports 47520–47747/TCP and 60520–60747/UDP were only
  used for these runs and are free.
- No package, driver, module, or repo change; no commit.

`queen`:

- `/tmp/ozma-gate2-synth` (copied binary, HLS dirs, segments, logs, probe
  dirs) removed; every synth node process group and its ffmpeg child was
  killed. Ports 47701–47747/TCP are free.
- The existing Frigate workload was not touched; no package, driver, service,
  or repository change was made on `queen`.
