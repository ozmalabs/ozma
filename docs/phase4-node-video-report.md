# Phase 4 — `ozma-node` `/video` MJPEG and H.264/HEVC streams

Status: implemented. MJPEG is unit/route tested with the deterministic fake
capture provider. H.264/HEVC MPEG-TS is tested with fake sources and a recording
ffmpeg spawner, plus an ignored live test that pipes a fake RGB source through
real `ffmpeg` and validates the result with `ffprobe`. No real camera, QMP, VNC,
or D-Bus device was used.

## Why

The Phase 4 cutover audit lists `/video` as a Rust-node gap. The Python hardware
node README advertises `GET /video` (V4L2 device info), but the current Python
`node/node.py` does not actually register that route. The soft node does serve a
real MJPEG stream at `/display/mjpeg`, which is the controller-facing media
surface. This change adds a real MJPEG stream for browser/controller consumers
at both paths:

- canonical: `GET /video`
- alias: `GET /display/mjpeg` (the Python soft-node path)

No placeholder JPEG, synthetic frame, or success-shaped empty stream is sent.

## Endpoint contract

`GET /video?codec=<mjpeg|h264|hevc>&fps=<1..30>&quality=<1..100>&bitrate=<100..50000>&console=<N|label|Console_N|source-id>`

`GET /display/mjpeg` accepts the same query. Without `codec`, the response is
the historical MJPEG multipart stream.

| Query | Default | Accepted / clamping |
|---|---|---|
| `codec` | `mjpeg` | `mjpeg`, `h264`, `hevc` (aliases `jpeg`, `avc`, `h265`). An unknown value is a typed `400`. |
| `fps` | `10` | `1..=30`; numeric values outside the range are clamped. |
| `quality` | `85` | `1..=100`; numeric values outside the range are clamped. MJPEG only. |
| `bitrate` | `4000` | `100..=50000` kbps; numeric values outside the range are clamped. H.264/HEVC only. |
| `console` | active console | Same resolver as `/display/snapshot?console=`: an advertised index, label, `Console_N`, or capture source id. |

A non-numeric `fps`/`quality`/`bitrate` or an unknown `codec` is a typed `400`
(`video.invalid_query`) rather than a silently ignored parameter.

### MJPEG success

- Status `200`.
- `Content-Type: multipart/x-mixed-replace; boundary=ozma-frame`.
- `Cache-Control: no-store`, `Pragma: no-cache`,
  `X-Accel-Buffering: no`.
- `X-Ozma-Codec: mjpeg`, `X-Ozma-Fps`, and `X-Ozma-Quality` report the clamped
  values.
- Each part is:

  ```text
  --ozma-frame\r\n
  Content-Type: image/jpeg\r\n
  Content-Length: <n>\r\n
  \r\n
  <n JPEG bytes>\r\n
  ```

The first frame is captured **before** the `200` is committed. If opening or
reading the source fails, the route returns the same typed JSON error as
`/display/snapshot` instead of starting an empty stream. Subsequent capture
failures end the body; no error/filler frame is injected.

### H.264/HEVC success

- Status `200`.
- `Content-Type: video/mp2t`.
- `Cache-Control: no-store`, `Pragma: no-cache`,
  `X-Accel-Buffering: no`.
- `X-Ozma-Codec: h264|hevc`, `X-Ozma-Encoder: libx264|libx265|<hw encoder>|copy`,
  `X-Ozma-Stream-Copy: true|false`, `X-Ozma-Fps`, and `X-Ozma-Bitrate`.
- The body is a continuous MPEG-TS byte stream (`-f mpegts pipe:1`).

The response is not committed until ffmpeg has emitted a real keyframe
(H.264 IDR / HEVC IRAP) into the MPEG-TS stream. The handler parses complete TS
packets and PES video payloads (`stream_id 0xe0..=0xef`) for the Annex-B NAL
type before returning `200`. Bytes before that keyframe (PAT/PMT/SPS/PPS) are
retained and sent as the first chunk. If the encoder exits, writes no keyframe,
or exceeds the pre-keyframe buffer bound, the route returns a typed error and
stops the source.

The MPEG-TS path reuses the HLS encoder construction:

- the same `HlsConfig` fps/bitrate/ffmpeg/VAAPI/NVENC fields;
- the same raw-input conversion (`bgra`, `mjpeg`, `yuyv`, `nv12` → `rgb24`,
  `p010` passed through);
- the same H.264/HEVC software encoders (`libx264`/`libx265`), hardware
  variants (`*_vaapi`, `*_nvenc`, `*_qsv`, `*_v4l2m2m`), profiles, pixel format
  pinning, GOP/keyframe forcing, and color signalling;
- the same encoder availability probe. A configured hardware backend is used
  when ffmpeg advertises it and it passes the runtime encode probe; otherwise
  the matching software encoder is selected. If neither is available, the
  route returns `503 video.encoder_unavailable` before spawning ffmpeg.

Compressed capture pass-through is selected automatically. When the source's
declared layout is H.264 and `codec=h264` (or HEVC/HEVC), ffmpeg receives the
capture access units with `-c:v copy` and only muxes them. A compressed source
of the other family is decoded and re-encoded through the same input path
(`-f h264`/`-f hevc`). AV1 and MJPEG are not live MPEG-TS codecs on this route.

### Typed failures

| Status | Code | When |
|---|---|---|
| `400` | `video.invalid_query` | `codec` is not `mjpeg|h264|hevc`, or `fps`/`quality`/`bitrate` is not an integer. |
| `404` | `not_found` | `?console=` names a console that is not advertised. |
| `409` | `conflict` | A single-head QMP/VNC/Looking Glass provider was asked for a non-active console (`details.single_head:true`). |
| `501` | `capture.unavailable` | `?console=` was requested but the node has no console selector/broker. |
| `503` | `capture.unavailable` | No snapshot control is configured, no source is discoverable, the source cannot be opened, or the first frame/keyframe cannot be read. |
| `503` | `video.encoder_unavailable` | `codec=h264|hevc` was requested without an encoder configuration, ffmpeg does not advertise the selected encoder, a hardware encoder/device is unusable, or the source cannot be converted for that codec. |
| `503` | `video.stream_failed` | ffmpeg/source failed after setup but before the first keyframe. |
| `503` | `video.stream_limit` | All concurrent stream slots are in use. |
| `504` | `video.keyframe_timeout` | The live MPEG-TS setup bound elapsed with no decodable keyframe. |
| `504` | `display.snapshot_timeout` / `display.encode_timeout` | The existing snapshot capture/encode bound elapsed (MJPEG). |

All error bodies are JSON with `ok:false`, `code`, `error`, and remediation
where applicable. They are produced by the existing snapshot error mapping, so
`/video` never invents a stream when the capture registry has nothing to show.

## Capture and console reuse

- `SnapshotControl::streaming()` creates one independent cached control per
  stream. It follows the same capture registry (including the runtime console
  registry slot), applies the requested JPEG quality, opens one provider
  connection on the first frame, and reuses it for later frames.
- With no `?console=`, the stream follows the active shared-registry snapshot
  control.
- `?console=<active>` also uses the active path.
- `?console=<non-active>` uses `ConsoleBroker::streaming_control()`, which
  applies the broker's advertised-console and single-head rules and the D-Bus
  per-console source cache. Ending the stream stops only the stream's own source;
  the broker's cached snapshot source and other streams are unaffected.

## Concurrency, backpressure, and cleanup

- Default maximum: **4** simultaneous streams (`VideoStreams::default()`),
  shared by MJPEG and H.264/HEVC. `ConnectionState::with_video_streams()`
  replaces the allowance for tests or constrained nodes.
- A request over the cap receives a typed `503 video.stream_limit`; it does not
  wait indefinitely or start a fifth source.
- MJPEG streams have a **one-slot** frame channel. `try_send` drops a newer
  frame when the client is not reading, so memory and latency stay bounded
  instead of accumulating an unbounded queue.
- Encoded streams never drop TS bytes (that would corrupt the bitstream). They
  use a bounded four-chunk queue and await the client; a slow reader
  back-pressures ffmpeg stdout and then the source feeder. Memory remains
  bounded instead of growing an unbounded queue.
- Frame pacing uses the clamped `fps`. A fast fake/file source cannot spin the
  encoder; a slow source simply yields fewer frames. Compressed access units
  are never paced away because dropping one would break the reference chain.
- Dropping the response (client disconnect) closes the receiver. The producer
  task observes the closed channel, kills ffmpeg, stops the capture source, and
  drops its stream guard. This releases both the provider connection and the
  concurrent-stream slot.
- A per-frame capture/encode failure, a source end, or a disconnect ends the
  stream; MJPEG never fabricates a replacement frame and MPEG-TS never
  synthesizes a keyframe. A clean source end lets ffmpeg flush its remaining
  encoded packets before the body closes.

## Status advertisement

`GET /api/v1/connection` now includes an additive `video` object:

```json
{
  "supported": true,
  "available": true,
  "route": "/video",
  "alias": "/display/mjpeg",
  "encoding": "jpeg",
  "codecs": ["mjpeg", "h264", "hevc"],
  "default_codec": "mjpeg",
  "active_streams": 0,
  "active_encoded_streams": 0,
  "max_streams": 4,
  "default_fps": 10,
  "min_fps": 1,
  "max_fps": 30,
  "default_quality": 85,
  "default_bitrate_kbps": 4000,
  "min_bitrate_kbps": 100,
  "max_bitrate_kbps": 50000
}
```

- `available` is true when a snapshot/capture control is configured. It does not
  claim that enumeration will find a source; the route still returns a typed
  `503` in that case.
- `codecs` lists MJPEG always and adds `h264`/`hevc` when the node has a live
  encoder configuration. Advertisement does not prove a specific encoder
  binary exists; a missing/unusable encoder is the typed
  `503 video.encoder_unavailable` at request time.
- `active_streams` is the live count of reserved stream slots (both codecs) and
  is `0` when idle. `active_encoded_streams` is the H.264/HEVC subset.

## Bearer behaviour

`/video` and `/display/mjpeg` are read-only media, like
`/display/snapshot` and `/display/info`. They live in
`controller_compat::public_router`, which is merged **after** the bearer guard.
They therefore remain readable without a node token, matching the unmodified
controller's snapshot polling. Mutating routes (`/display/select`, `/input/*`,
`/power/*`, `/codec`, `/usb`, `/webrtc/*`) remain guarded.

## Tests

Default coverage is fake-source/fake-ffmpeg only; no `/dev/video*`, QMP, VNC, or
D-Bus device is touched. The one explicitly ignored live test uses real ffmpeg
on the fake source.

`ozma-node/src/video.rs` unit tests:

- defaults are bounded (including codec and bitrate);
- `fps`/`quality`/`bitrate` clamp at both ends;
- non-numeric values and unknown codecs are typed errors;
- stream slots are capped, encoded slots are counted, and both release on guard
  drop;
- multipart parts carry a real length and the supplied JPEG bytes.

`ozma-node/src/hls.rs` MPEG-TS tests:

- `mpegts_argv_reuses_the_hls_codec_path_and_muxes_to_stdout` — H.264/HEVC argv
  keeps the HLS encoder/filters/GOP and ends with `-f mpegts pipe:1`;
- `mpegts_argv_stream_copies_matching_compressed_sources_and_reencodes_mismatches`
  — H.264 source + H.264 encoder emits `-c:v copy`; H.264 source + HEVC encoder
  emits `-f h264` + `libx265` with no copy;
- `mpegts_keyframe_detector_recognizes_h264_idr_and_hevc_irap` — real TS packet
  parsing, not a byte-pattern guess over arbitrary data;
- `mpegts_stream_returns_a_real_keyframe_and_stops_on_disconnect` — the
  committed prefix contains an IDR, the source `stop()` runs, and ffmpeg stdin
  closes after the receiver is dropped;
- `mpegts_stream_fails_before_commit_without_a_keyframe` — no fabricated
  response;
- `mpegts_stream_rejects_mjpeg_and_unavailable_encoders` — typed
  `UnsupportedStreamCodec`/`EncoderUnavailable` and no ffmpeg spawn;
- `mpegts_stream_stream_copies_a_compressed_h264_source` — pass-through works
  without any advertised encoder.

Ignored live test (run explicitly):

- `hls::tests::live_mpegts_stream_is_ffprobe_decodable_h264_and_hevc` — a fake
  160×120 RGB capture source goes through the real system ffmpeg spawner and
  encoder probe; the MPEG-TS body is written to a temporary file and `ffprobe`
  asserts `codec_name` (`h264`/`hevc`), `width=160`, `height=120`, at least ten
  decoded frames, and a clean `ffmpeg -f null -` decode.

`ozma-node/src/controller_compat.rs` route tests:

- the existing MJPEG tests are unchanged; the default response still has the
  exact multipart content type and `X-Ozma-Codec: mjpeg`;
- `video_h264_and_hevc_are_mpegts_with_a_real_first_keyframe` — both codecs
  return `200 video/mp2t`, the encoder/copy/fps/bitrate headers, whole TS
  packets, and a real keyframe through the fake spawner;
- `video_encoded_selection_uses_compressed_passthrough_or_reencode` — route
  headers and ffmpeg argv select `copy` vs re-encode from the source layout;
- `video_encoded_unavailable_and_bad_codecs_are_typed` — missing encoding or
  missing ffmpeg encoder is `503 video.encoder_unavailable`; `codec=vp9` is
  `400 video.invalid_query`; no ffmpeg starts when unavailable;
- `video_encoded_disconnect_releases_slot_and_stops_ffmpeg` — encoded stream
  holds an encoded slot while open, releases it on body drop, and closes ffmpeg
  stdin;
- `connection_reports_video_availability_without_claiming_a_stream` — idle
  `active_streams` is `0`; it becomes `1` while a stream is open; idle again
  after disconnect.

`ozma-node/src/api.rs` guard test fetches `/video` and `/display/mjpeg`
without a token (both `200`) and no longer expects `/display/mjpeg` to be
guarded.

## Verification commands

```bash
cargo test -p ozma-node --lib
cargo check -p ozma-node --all-targets
cargo test -p ozma-node --lib \
  hls::tests::live_mpegts_stream_is_ffprobe_decodable_h264_and_hevc \
  -- --ignored --exact
rustfmt --edition 2021 --check --config skip_children=true \
  ozma-node/src/video.rs ozma-node/src/hls.rs \
  ozma-node/src/controller_compat.rs ozma-node/src/display_compat.rs \
  ozma-node/src/main.rs
```

Result on 2026-11-26:

- `cargo test -p ozma-node --lib`: **733 passed, 0 failed, 6 ignored** (the
  ignored tests are the pre-existing live GStreamer/restic/bluetooth probes plus
  the new live MPEG-TS probe).
- `cargo check -p ozma-node --all-targets`: **passed** (existing warnings only).
- The live `ffprobe` test: **passed** for H.264 and HEVC (4.6 s).
- `rustfmt --edition 2021 --check --config skip_children=true ...`: **passed**.

(`skip_children=true` keeps rustfmt from following `lib.rs`'s module tree into
unrelated files.)

## Remaining video gaps

- No real camera/QMP/VNC/D-Bus live run is part of this report. The live probe
  uses a deterministic fake RGB source; real-device latency, v4l2loopback
  compressed access units, and long-run disconnect behaviour still need a
  hardware check. The compressed pass-through path is proven at argv/route
  level and by the HLS compressed tests, not by a live V4L2 capture.
- Only MJPEG, H.264, and HEVC are exposed on `/video`. AV1 remains HLS-only; the
  MPEG-TS keyframe detector has no AV1 mapping.
- The live MPEG-TS path is 8-bit SDR (`yuv420p`); the HLS 10-bit/P010 and HDR
  configuration is deliberately not inherited. A future `?codec=hevc&hdr=1`
  would need its own validation and keyframe/metadata checks.
- A frame-size change on a raw source ends the encoded stream rather than
  restarting ffmpeg mid-response. HLS restarts generations; live MPEG-TS keeps
  one process so a decoder never sees an unannounced discontinuity.
- One capture source per stream. Providers that only allow a single client (for
  example an unshared VNC server) may reject a second stream with a typed error
  or end it when the provider fails; the node does not share frames between
  clients or fabricate them.
- An already-open stream does not hot-switch when `/display/select` changes the
  active console. A new `/video?console=` request (or reconnecting the stream)
  picks up the new selection.
- There is no adaptive frame rate/quality, client-side bandwidth estimate, or
  per-stream authentication beyond the public read-only surface.
- The Python `node/README.md` still describes `/video` as V4L2 device info, and
  the current Python hardware node has no `/video` route. This implementation
  follows the Phase 4 task's media-stream contract; it does not add a V4L2-info
  JSON shape at the same path.
