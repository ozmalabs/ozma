# Phase 4 V4L2 media follow-up: YUYV/NV12 snapshots and compressed HLS

Date: 2026-11-26 (Australia/Sydney)
Host: CachyOS, Linux 7.3.5-2-cachyos, x86_64
Checkout: `/home/matt/ozma/ozma`
Base HEAD: `c386800` (`feat(phase4): audio graph/sweep, KMS scanout, desktop and
V4L2 fixes`)
Driver: `v4l2loopback` 0.15.4
Producer: ffmpeg n9.1

This report closes the two gaps left by
[`phase4-node-live-fixes-report.md`](phase4-node-live-fixes-report.md):

1. `GET /display/snapshot` and `GET /video` returned the typed
   `display.unsupported_format` for YUYV/NV12 capture.
2. HLS refused every compressed V4L2 layout (H.264/HEVC/AV1).

The original evidence in [`phase4-v4l2-live-report.md`](phase4-v4l2-live-report.md)
is unchanged; its follow-up section now points here.

## Scope and safety

- Product changes are limited to `ozma-encode/src/jpeg.rs`,
  `ozma-capture/src/v4l2.rs`, `ozma-node/src/{hls,display_compat,video,
  controller_compat}.rs`, and docs.
- No commit was created.
- Every live run used a dedicated `v4l2loopback` device (`video_nr=42`,
  `card_label=ozma-capture-test`, `exclusive_caps=1`).
- The node ran in a private mount namespace whose `/dev` contained only
  `/dev/video42` plus the basic character nodes, and whose
  `/sys/class/video4linux` contained only the loopback's name. The host's real
  cameras (`/dev/video0-1`) could not be opened by the node. `/display/info` and
  `/api/v1/connection.capture` reported `/dev/video42` for every run.
- No real camera, module configuration, udev rule, ACL, or host service was
  changed. The module was loaded by an explicit `modprobe` and removed after
  the runs.

## 1. Snapshot and `/video` for YUYV/NV12

### Implementation

`ozma-encode/src/jpeg.rs` now routes `Yuyv` and `Nv12` through the existing
bounded `convert::to_rgb24`, then encodes the packed RGB24 with the normal JPEG
encoder. The JPEG path therefore has one conversion implementation shared with
HLS instead of a second YUV decoder.

- `Rgb24` and `Bgra` are still encoded directly.
- `Mjpeg` is still passed through when it already starts with `FF D8`; the node
  does not re-encode an MJPEG source.
- A short or odd-dimension YUV payload is a typed `JpegError::Convert` (mapped
  to `display.encode_failed`), not a corrupt image.
- Compressed layouts the JPEG encoder cannot render (for example raw H.264
  reaching the snapshot route) keep the typed
  `503 display.unsupported_format`; the remediation now names
  `rgb24, bgra, mjpeg, yuyv, nv12`.

`SnapshotControl`, `/video`, and the controller snapshot route already call
`encode_jpeg_frame`, so no duplicate format logic was added to those modules.
`/video` inherits the conversion for every frame of its cached stream.

### Tests

`ozma-encode --lib`:

- `yuyv_and_nv12_frames_are_converted_and_encoded` decodes the produced JPEG and
  checks real black/white pixels, not just dimensions.
- `short_yuv_payloads_are_typed_conversion_errors`.
- The old “NV12 is unsupported” JPEG test now uses H.264.

`ozma-node --lib`:

- `display_compat::tests::yuyv_and_nv12_sources_are_encoded` exercises the
  one-shot snapshot path for both layouts.
- `display_compat::tests::mjpeg_source_is_passed_through_unchanged` proves a real
  MJPEG frame is returned byte-for-byte, not decoded and re-encoded.
- `controller_compat::tests::display_snapshot_encodes_yuyv_and_nv12_sources`
  exercises the public HTTP `/display/snapshot` route and checks
  `/display/info.pixel_format`.
- `video::tests::yuyv_and_nv12_sources_stream_real_jpeg_parts` starts the real
  `/video` producer task and decodes the first multipart JPEG.
- `video::tests::mjpeg_source_streams_the_original_jpeg_bytes` proves the
  multipart stream carries the original MJPEG bytes.
- `controller_compat::tests::display_snapshot_rejects_an_unsupported_frame_format`
  now uses H.264 and still asserts the typed refusal.

### Live V4L2 YUYV and NV12

Producer, 640x480@30:

```bash
ffmpeg -re -f lavfi -i testsrc2=size=640x480:rate=30 \
  -pix_fmt yuyv422 -f v4l2 /dev/video42
# or -pix_fmt nv12
```

The device reported `'YUYV'` and then `'NV12'` respectively. With HLS disabled
(so the one-shot snapshot was not competing for the device):

| Format | Snapshot | `/display/info` | `/video` |
|---|---|---|---|
| YUYV | `200 image/jpeg`, 27,262 B, 640x480, centre `(17,15,142)` | `pixel_format:"yuyv"`, 640x480, `last-frame` | 20 frames in 4 s at `fps=5`, 640x480, centre `(15,14,141)` |
| NV12 | `200 image/jpeg`, 28,254 B, 640x480, centre `(15,14,141)` | `pixel_format:"nv12"`, 640x480, `last-frame` | 20 frames in 4 s at `fps=5`, 640x480, centre `(15,14,141)` |

The sampled colours match the moving `testsrc2` pattern, so these are real
decoded frames rather than a constant or placeholder image.

A separate HLS run with the same NV12 producer produced
`hls:"running"`, a real `stream.m3u8`, and a 640x480 H.264 segment at 10 fps with
10 decoded frames, confirming the existing YUYV/NV12 conversion path still
works after this change.

Note: with the HLS pipeline running, the snapshot route returns the honest
`capture.unavailable` “Device or resource busy” error because the HLS generation
owns the capture device. Snapshot/video evidence above therefore used the
snapshot-only configuration. This is the existing single-consumer V4L2
behaviour, not a regression.

## 2. HLS H.264/HEVC/AV1 pass-through

### Implementation

`ozma-node/src/hls.rs` distinguishes raw and compressed capture inputs:

- Compressed `H264`, `Hevc`, and `Av1` frames are written to ffmpeg unchanged;
  the feeder never interprets them as pixels.
- The ffmpeg input is the matching raw bitstream demuxer (`-f h264`, `-f hevc`,
  or `-f obu`) with `-r <hls-fps>` as the timestamp base.
- When the configured encoder targets the same codec family, ffmpeg runs with
  `-c:v copy`. No encoder, filter, profile, GOP, or color bitstream option is
  emitted, so ffmpeg cannot silently transcode.
- When the configured encoder targets another codec, the same bitstream input
  is decoded and re-encoded with the existing encoder argv. There is no refusal
  just because the encoder does not match.
- Hardware encoders are not consulted in copy mode: the bitstream is muxed, so
  a missing VAAPI/NVENC/QSV device does not block an otherwise valid copy.
- HEVC copy adds `-bsf:v setts=pts=N:dts=N` and `-tag:v hvc1`. Raw HEVC demuxing
  leaves packet timestamps unset and the mpegts muxer rejects those packets;
  the timestamps are assigned from the access-unit index at the configured
  rate. H.264 and AV1 keep the parser's timestamps.
- AV1 copy uses fragmented-MP4 HLS (`-hls_segment_type fmp4`,
  `init.mp4`, `seg_%05d.m4s`). mpegts muxes AV1 as opaque private data that
  `ffprobe` and players cannot decode; fMP4 carries a real AV1 track.
- Compressed frames are never dropped by `FramePacer`. Dropping a P-frame would
  break the reference chain, so every access unit is written. Raw RGB/YUV
  sources keep the existing wall-clock pacing.
- Segment boundaries still come only from real source keyframes. A compressed
  source without keyframes produces no playlist and status stays failed; no
  segment is invented.

`ozma-capture/src/v4l2.rs` now negotiates `HEVC` and `AV01` after the existing
`MJPG/NV12/YUYV/H264` preferences. The fourcc map already had `HEVC`; `AV01` was
added. A device that only advertises HEVC or AV1 is no longer rejected by
preference, though the snapshot path still refuses those compressed layouts
honestly.

### Tests

`ozma-node --lib hls::tests` (all default-CI, no ffmpeg):

- `h264_source_is_copied_into_hls_without_reencoding`: fake source, asserts the
  exact access-unit bytes reach the sink, argv contains `-f h264 -c:v copy`,
  and contains no `rawvideo`, `libx264`, or `-force_key_frames`.
- `hevc_source_is_copied_with_index_timestamps_and_hvc1_tag`.
- `av1_source_copy_uses_fragmented_mp4_segments`: `-f obu`, `-c:v copy`, fMP4,
  `.m4s` segment pattern.
- `compressed_source_with_a_different_encoder_is_reencoded`: H.264 source with
  HEVC target keeps the bytes for ffmpeg, uses `-c:v libx265`, and does not copy.
- `compressed_copy_does_not_require_the_selected_hardware_encoder`: H.264 copy
  starts with a VAAPI encoder selected even when the probe has no VAAPI encoder
  and the render node is missing, because copy never invokes the encoder.
- `compressed_frames_are_never_paced_away`: five access units at a 1 fps config
  still reach the sink.
- `compressed_input_layouts_are_valid_configuration`: H.264/HEVC/AV1 input
  layouts validate; a YUYV raw layout is still rejected as an internal-only
  conversion.

`ozma-capture --lib v4l2`:

- fourcc mapping covers `H264`, `HEVC`, and `AV01`.
- preference selects HEVC before AV1 and AV1 when it is the only compressed
  option.

### Real ffmpeg + ffprobe test

Opt-in test
`hls::tests::real_ffmpeg_copies_compressed_access_units_with_probeable_segments`
(`OZMA_FFMPEG_TEST=1`, `--ignored`) generates real H.264/HEVC/AV1 bitstreams
with ffmpeg, feeds each as a fake compressed source, and probes the produced
playlist. Result:

```text
test result: ok. 1 passed; 0 failed
```

`ffprobe -count_frames` reported `codec_name=h264/hevc/av1`, 64x48, and
non-zero decoded frames for all three.

### Live V4L2 H.264

ffmpeg's v4l2 muxer can write H.264 to v4l2loopback, so the copy path was
validated against a real device:

```bash
ffmpeg -re -f lavfi -i testsrc2=size=320x240:rate=30 \
  -c:v libx264 -preset ultrafast -tune zerolatency -g 15 -f v4l2 /dev/video42
# loopback advertised: [0]: 'H264' (H.264, compressed), 320x240@30
```

Node:

```bash
target/debug/ozma-node --profile hardware --name ozma-v4l2-media-hls-h264 \
  --http-port 39261 --hid-udp-port 39262 \
  --capture-provider v4l2 --capture-device /dev/video42 \
  --input-provider recording --audio-provider none \
  --display-provider none --power-provider none \
  --hls --hls-dir /tmp/opencode/v4l2-live/hls-h264 \
  --hls-fps 30 --hls-bitrate-kbps 1500 --hls-encoder libx264
```

Evidence:

```text
connection.capture: {source:/dev/video42, name:ozma-capture-test, available:true}
connection.hls:     running
newest segment:     seg_00009.ts, h264 320x240, 30/1 fps, 30 decoded frames
child argv:         ffmpeg -f h264 -r 30 -i - -an -c:v copy -f hls ...
```

The child argv proves ffmpeg was copying, not re-encoding. A live V4L2 HEVC or
AV1 producer was not available on this host: ffmpeg's v4l2 muxer has no HEVC
mapping, and GStreamer's `v4l2sink` rejected `video/x-h265`. HEVC/AV1 copy is
covered by the real-ffmpeg + ffprobe test above.

## Verification commands and results

```bash
cargo test -p ozma-node -p ozma-capture -p ozma-encode --lib
cargo check -p ozma-node -p ozma-capture -p ozma-encode --all-targets
rustfmt --edition 2021 <changed .rs files>
```

Results:

- `ozma-node`: 667 passed, 0 failed, 5 ignored.
- `ozma-capture`: 194 passed, 0 failed, 12 ignored.
- `ozma-encode`: 75 passed, 0 failed.
- `cargo check --all-targets`: success. Remaining warnings are the pre-existing
  unused imports in `ozma-pipewire`.
- `rustfmt`: applied to all changed Rust files.

The opt-in real-ffmpeg test was run separately:

```bash
OZMA_FFMPEG_TEST=1 cargo test -p ozma-node --lib \
  hls::tests::real_ffmpeg_copies_compressed_access_units_with_probeable_segments \
  -- --ignored
```

## Remaining gaps

- Snapshot/`/video` still cannot render a raw H.264/HEVC/AV1 capture frame:
  those layouts keep the typed `display.unsupported_format` because the JPEG
  encoder has no decoder/muxer for them. HLS handles them; the controller's
  preview route does not.
- Live V4L2 HEVC/AV1 pass-through was not possible with the available producer
  tools (`ffmpeg` v4l2 muxer has no HEVC/AV1 mapping; GStreamer `v4l2sink`
  rejected H.265). The copy argv and ffprobe behaviour are covered without a
  V4L2 device.
- Compressed copy uses `--hls-fps` as its timestamp rate. If the operator sets a
  rate different from the source's real frame rate, media time is stretched or
  compressed; frames are not dropped to match it. Raw sources retain the
  existing pacing/drop behaviour.
- HEVC copy assumes decode-order access units (the usual low-latency V4L2
  case). `setts=pts=N:dts=N` assigns display timestamps from packet index; a
  source with B-frame reordering would need a parser that preserves the
  decoder's reorder metadata.
- AV1 copy writes `.m4s` segments and `init.mp4` while H.264/HEVC keep `.ts`.
  The controller's `/stream/*` proxy serves both; players that only accept
  MPEG-TS HLS will not play the AV1 flavour.
- The copy path still requires one capture consumer. Running HLS and a
  simultaneous snapshot/`/video` against the same V4L2 device returns the
  existing typed “Device or resource busy” error.

## Cleanup

- All node and producer processes were stopped; their logs showed a normal
  SIGTERM shutdown.
- `sudo modprobe -r v4l2loopback` removed `/dev/video42` and the module.
- The private namespace, HLS output, snapshots, videos, and producer logs were
  removed from `/tmp/opencode`.
- The host's real cameras remain `/dev/video0` and `/dev/video1`; neither was
  opened by the node during these runs (the namespace exposed only
  `/dev/video42`).
