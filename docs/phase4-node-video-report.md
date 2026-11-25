# Phase 4 — `ozma-node` `/video` MJPEG stream

Status: implemented, unit/integration tested with the deterministic fake capture
provider. No real camera/QMP/VNC/D-Bus device was used.

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

`GET /video?fps=<1..30>&quality=<1..100>&console=<N|label|Console_N|source-id>`

`GET /display/mjpeg` accepts the same query.

| Query | Default | Accepted / clamping |
|---|---|---|
| `fps` | `10` | `1..=30`; numeric values outside the range are clamped. |
| `quality` | `85` | `1..=100`; numeric values outside the range are clamped. |
| `console` | active console | Same resolver as `/display/snapshot?console=`: an advertised index, label, `Console_N`, or capture source id. |

A non-numeric `fps`/`quality` is a typed `400` (`video.invalid_query`) rather
than a silently ignored parameter.

### Success

- Status `200`.
- `Content-Type: multipart/x-mixed-replace; boundary=ozma-frame`.
- `Cache-Control: no-store`, `Pragma: no-cache`,
  `X-Accel-Buffering: no`.
- `X-Ozma-Fps` and `X-Ozma-Quality` report the clamped values.
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

### Typed failures

| Status | Code | When |
|---|---|---|
| `400` | `video.invalid_query` | `fps`/`quality` is not an integer. |
| `404` | `not_found` | `?console=` names a console that is not advertised. |
| `409` | `conflict` | A single-head QMP/VNC/Looking Glass provider was asked for a non-active console (`details.single_head:true`). |
| `501` | `capture.unavailable` | `?console=` was requested but the node has no console selector/broker. |
| `503` | `capture.unavailable` | No snapshot control is configured, no source is discoverable, the source cannot be opened, or the first frame cannot be read. |
| `503` | `video.stream_limit` | All concurrent stream slots are in use. |
| `504` | `display.snapshot_timeout` / `display.encode_timeout` | The existing snapshot capture/encode bound elapsed. |

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

- Default maximum: **4** simultaneous streams (`VideoStreams::default()`).
  `ConnectionState::with_video_streams()` replaces the allowance for tests or
  constrained nodes.
- A request over the cap receives a typed `503 video.stream_limit`; it does not
  wait indefinitely or start a fifth source.
- Each stream has a **one-slot** frame channel. `try_send` drops a newer frame
  when the client is not reading, so memory and latency stay bounded instead of
  accumulating an unbounded queue.
- Frame pacing uses the clamped `fps`. A fast fake/file source cannot spin the
  encoder; a slow source simply yields fewer frames.
- Dropping the response (client disconnect) closes the receiver. The producer
  task observes the closed channel, stops the cached capture source, and drops
  its stream guard. This releases both the provider connection and the
  concurrent-stream slot.
- A per-frame capture/encode failure or the existing snapshot timeout ends the
  stream cleanly; it does not fabricate a replacement frame.

## Status advertisement

`GET /api/v1/connection` now includes an additive `video` object:

```json
{
  "supported": true,
  "available": true,
  "route": "/video",
  "alias": "/display/mjpeg",
  "encoding": "jpeg",
  "active_streams": 0,
  "max_streams": 4,
  "default_fps": 10,
  "min_fps": 1,
  "max_fps": 30,
  "default_quality": 85
}
```

- `available` is true when a snapshot/capture control is configured. It does not
  claim that enumeration will find a source; the route still returns a typed
  `503` in that case.
- `active_streams` is the live count of reserved stream slots and is `0` when
  idle. It is not a promise of a running source when no client is connected.

## Bearer behaviour

`/video` and `/display/mjpeg` are read-only media, like
`/display/snapshot` and `/display/info`. They live in
`controller_compat::public_router`, which is merged **after** the bearer guard.
They therefore remain readable without a node token, matching the unmodified
controller's snapshot polling. Mutating routes (`/display/select`, `/input/*`,
`/power/*`, `/codec`, `/usb`, `/webrtc/*`) remain guarded.

## Tests

New coverage is fake-source only; no `/dev/video*`, QMP, VNC, or D-Bus device is
touched.

`ozma-node/src/video.rs` unit tests:

- defaults are bounded;
- `fps`/`quality` clamp at both ends;
- non-numeric values are typed errors;
- stream slots are capped and released on guard drop;
- multipart parts carry a real length and the supplied JPEG bytes.

`ozma-node/src/controller_compat.rs` route tests:

- `video_stream_has_multipart_headers_and_real_changing_frames` — `200`,
  exact multipart content type, no-store/quality/fps headers, at least three
  decodable JPEGs with changing bytes;
- `video_stream_honours_fps_bound_and_stops_on_disconnect` — three frames at
  `fps=2` take at least the paced interval, then the slot returns to zero;
- `video_stream_caps_concurrent_clients_and_recovers_after_disconnect` — second
  client is a typed `503 video.stream_limit`; after the first body is dropped a
  new stream starts;
- `video_stream_reports_unavailable_sources_and_bad_queries_typed` — empty
  registry and missing control are typed `503`; invalid `fps` is `400`;
- `video_stream_honours_console_selection` — `Console_1` streams its broker
  source (64x48 fake frames), `console=0` streams the active 32x24 source, and an
  unknown console is typed `404`;
- `connection_reports_video_availability_without_claiming_a_stream` — idle
  `active_streams` is `0`; it becomes `1` while a stream is open; idle again
  after disconnect.

`ozma-node/src/api.rs` guard test now fetches `/video` and `/display/mjpeg`
without a token (both `200` multipart) and no longer expects `/display/mjpeg`
to be guarded.

## Verification commands

```bash
cargo test -p ozma-node --lib
cargo check -p ozma-node --all-targets
rustfmt --edition 2021 --check --config skip_children=true \
  ozma-node/src/video.rs ozma-node/src/controller_compat.rs \
  ozma-node/src/display_compat.rs ozma-node/src/display_broker.rs \
  ozma-node/src/api.rs ozma-node/src/lib.rs
```

Result on 2026-11-25:

- `cargo test -p ozma-node --lib`: **521 passed, 0 failed, 3 ignored** (the
  ignored tests are the pre-existing live GStreamer helper tests).
- `cargo check -p ozma-node --all-targets`: **passed** (existing warnings only).
- `rustfmt --edition 2021 --check --config skip_children=true ...`: **passed**.

(`skip_children=true` keeps rustfmt from following `lib.rs`'s module tree into
unrelated files.)

## Remaining video gaps

- MJPEG only. `/video` does not encode H.264/H.265/AV1; high-resolution/high-fps
  streams consume substantially more bandwidth than `/stream/*` HLS or WebRTC.
  Capture layouts are not limited to RGB/BGRA/MJPEG: YUYV and NV12 are converted
  to RGB24 through `ozma_encode::to_rgb24` before JPEG encoding, and MJPEG is
  passed through. See [`phase4-v4l2-media-report.md`](phase4-v4l2-media-report.md).
- One capture source per stream. Providers that only allow a single client (for
  example an unshared VNC server) may reject a second stream with a typed error
  or end it when the provider fails; the node does not share frames between
  clients or fabricate them.
- An already-open stream does not hot-switch when `/display/select` changes the
  active console. A new `/video?console=` request (or reconnecting the stream)
  picks up the new selection.
- There is no adaptive frame rate/quality, client-side bandwidth estimate, or
  per-stream authentication beyond the public read-only surface.
- No live hardware/QMP/VNC/D-Bus stream run is part of this report; all tests
  use `FakeCaptureProvider`. Real-device latency and long-run disconnect
  behaviour still need a live check.
- The Python `node/README.md` still describes `/video` as V4L2 device info, and
  the current Python hardware node has no `/video` route. This implementation
  follows the Phase 4 task's MJPEG contract; it does not add a V4L2-info JSON
  shape at the same path.
