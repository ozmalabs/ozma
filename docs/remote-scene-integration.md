# Remote scene graph integration plan

Make the scene graph the native Ozma display model. A full-display video
surface is the baseline representation. Richer sources can preserve placement,
media, and drawing semantics and let the client render selected subtrees.
The [node and Hermes roadmap](node-hermes-parity.md) still governs delivery of
the real capture path, native desktop client, consent, and virtual monitor.

## Presentation contract

The host owns scene state. A scene revision describes stable node IDs,
parentage, transforms, clipping, stacking, and references to resources or timed
surfaces. Resources have IDs and versions independent of places where they
appear. An image can be uploaded once and placed multiple times. A video
surface has a stable identity, codec configuration generation, and presentation
timestamps. One commit publishes a coherent scene revision. A client can
continue presenting the previous revision if required resources are unavailable,
or reuse a previous video frame where the commit explicitly allows it.

Treat compositing dependencies as part of the split. When effects such as
backdrop blur span nodes, either evaluate the required group on the client or
flatten that group on the host. Switch a subtree's representation at a commit
boundary with a self-contained image, keyframe, or complete resource set.
Reconnect starts with a fresh snapshot and confirms any cache entries that
will be reused. Reset epochs on restart so stale media cannot bind to a new
surface.

## Outbound display adapters

The authoritative scene feeds pluggable output adapters. Each connection
negotiates its own capabilities and receives the richest representation the
adapter can preserve. The output adapter owns protocol framing, acknowledgements,
and per-client cache state. The shared renderer flattens unsupported subtrees
into surfaces and can produce a complete framebuffer when the receiver needs
pixels. Adapters consume the same scene revisions, so the capture pipeline
and producer are independent of the viewer protocol.

| Output | Initial mapping |
| --- | --- |
| Ozma | Scene commits, resource references, and timed video surfaces over native QUIC. |
| Xpra | Window geometry and raster updates. Preserve window boundaries where the producer exposes them. [Xpra encoding](https://xpra-org.github.io/xpra/Subsystems/Encoding.html) |
| RDP | Map compatible scene groups to graphics surfaces, cached bitmaps, and frame boundaries. Composite groups with unsupported effects before sending them. [RDP graphics pipeline](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/f75a7d5c-141d-4bbc-8b27-3299378f8ac6) |
| SPICE | Use display surfaces, supported drawing operations, and video streams. Rasterize groups that require effects the peer cannot reproduce. [SPICE protocol](https://spice-space.org/spice-protocol.html) |
| VNC | Render the scene to a framebuffer and send changed rectangles using the viewer's negotiated RFB encodings. [RFB protocol](https://www.rfc-editor.org/rfc/rfc6143) |
| Google Cast | Compose the selected display or scene to a live media stream. The Cast sender starts playback on a receiver using a receiver-reachable URL. [Google Cast receiver options](https://developers.google.com/cast/docs/web_receiver) |

The same session can drive several outputs concurrently. Each output gets its
own pacing, damage history, and resource lifetime. If a slow viewer misses
updates, coalesce changes against that viewer's last delivered revision or
send a fresh snapshot. Map viewer input back to the shared display coordinates
and enforce the session's control, consent, and disconnect policy across all
adapters. Inbound bridges and outbound servers share the scene boundary and
each implement their own protocol role.

## Integration sequence

1. **Local contract fixture.** Build a deterministic presentation test with a
   cached still image beside an independently updated video surface. Move the
   image through placement-only commits and verify the cached payload remains
   unchanged. Verify that a failed commit preserves the previous scene and
   that a full-display video surface can represent the output. This step
   validates state transitions with simulated media.
   Run `cargo run -p ozma-protocol --example scene_demo`.
2. **Native display transport.** Extend the shared `ozma-protocol` session
   contract and existing QUIC transport. Add optional display capabilities to
   `Hello` and the selected profile to `HelloAck` on the existing reliable
   control stream. Treat absent capabilities as the current video profile.
   Advertise scene version, codec support, and resource limits. Use reliable streams for
   scene commits and resources, keeping large uploads off the commit stream.
   Carry deadline-sensitive video on the media channel. Make the first display
   update self-contained and allow richer resources to arrive afterward. Gate
   scene updates, assets, and input on existing session authorization and
   consent. Exercise a legacy video-only client, a scene-capable client, packet
   loss, and reconnect. Keep RTSP in the video compatibility path.
3. **Xpra bridge.** Map a small inbound subset of window creation, geometry,
   scroll, raster updates, and removal to Ozma surfaces. Start with recorded
   fixtures and validate them against a live Xpra session. Flatten unsupported
   operations at the window surface. Use SPICE as the next reference for
   bridging drawing commands and video, and RDP as a reference for cache and
   presentation semantics. [Xpra encoding](https://xpra-org.github.io/xpra/Subsystems/Encoding.html) ·
   [SPICE protocol](https://spice-space.org/spice-protocol.html) ·
   [RDP graphics pipeline](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/f75a7d5c-141d-4bbc-8b27-3299378f8ac6)
4. **Browser producer.** Use a controlled test page or instrumented browser paint
   source to emit an image asset, its placements, and an independent video
   surface. Bind export to the authorized visible scene and send authorized
   media bytes through Ozma. Scope reusable private assets to the session.
   Keep unsupported effects and protected content subject to browser capture
   policy. First show that scrolling changes placement without resending the
   image, then test repeated placements, clipping, and mixed stacking.
   [Microsoft browser multimedia redirection](https://learn.microsoft.com/en-us/azure/virtual-desktop/multimedia-redirection-video-playback-calls)
5. **Adaptive execution.** Choose representations per subtree using capability,
   cache state, bandwidth, decode and render cost, and presentation deadlines.
   Apply hysteresis and measure under bandwidth changes and CPU or GPU
   pressure. Start with raster, still assets, and bounded 2D commands. Evaluate
   glTF with MPEG-I timed media for later 3D import. Consider predefined effects
   before accepting general shader programs. [MPEG-I Scene Description](https://www.mpeg.org/standards/MPEG-I/14/) ·
   [GSlider](https://www.computer.org/csdl/journal/tc/2026/12/11685353/2jFlivNmmOc)

6. **Outbound compatibility.** First drive native Ozma and a VNC framebuffer
   adapter from the same scene. Reuse the reference compositor for an initial
   implementation and verify with a stock viewer that scrolling, overlapping
   layers, and overlay removal agree with the native output. Check that a slow
   VNC viewer cannot delay native presentation and that a view-only connection
   cannot inject input. Then use Xpra as the first structured output and add
   RDP and SPICE adapters using existing server libraries where practical.
   Verify each output against a real client before claiming interoperability.

7. **Cast output.** Use the shared compositor to stream a selected display or
   scene to Google's Default Media Receiver. Start with live HLS using H.264
   and optional AAC. The Ozma sender handles receiver selection, start, and
   stop. Serve manifests and segments from a URL the receiver can reach, with
   trusted HTTPS and compatible CORS. Issue a short-lived playback grant for
   the selected output and revoke it when casting stops. Keep the Cast output
   view-only and continue accepting authorized input through the original
   Ozma session. Validate playback, audio sync, stop and revocation, and
   measure first-picture and source-to-screen latency on a physical receiver.
   Evaluate a Custom Web Receiver later if retained overlays or tighter
   presentation timing justify the additional receiver app. [Supported Cast media](https://developers.google.com/cast/docs/media) ·
   [Custom Web Receiver](https://developers.google.com/cast/docs/web_receiver/basic)

## Current implementation

- **Native QUIC.** The node sends the first video frame before scene updates.
  It supports optional producers and polls for newer scene revisions. A
  placement-only update reuses its cached image. The native client can opt in
  with an encoded-resource cache budget and receives presentation events from
  the negotiated scene. It has a CPU reference compositor for compositing PNG
  overlays over decoded video frames, clipping to the output. Desktop GPU
  integration and full network-loss testing remain open. The authenticated
  reconnect test uses the same session offer to attach a new receiver, verifies
  it receives the replay resource again and reaches the same placement, and
  checks that video arrives on both connections.
- **Xpra.** The adapter reads bounded uncompressed rencodeplus records for
  window geometry, whole-window inline PNG draws, and removal. Negotiation,
  authentication, compressed records, out-of-band chunks, region copies,
  stacking metadata, and a live peer test remain open. The outbound encoder
  emits geometry, PNG draws, and removals. It has per-viewer window state
  and omits PNG data when only an image placement moves. Wire round trips
  exercise the Xpra packet mapper. [Xpra record format](https://xpra-org.github.io/xpra/Network/Protocol.html)
- **Browser.** Serve `ozma-protocol/examples/browser-scene` on localhost. Start
  the fixture, scroll, then download its JSONL scene replay. Run
  `ozma-node --scene-replay ozma-scene.jsonl` on a development node to replay
  that trusted fixture to scene-capable sessions. The node uses its own video
  channel. Live browser paint ingestion and browser video passthrough remain open.
- **Outbound frame stream.** `scene_rgba` decodes a native session and
  publishes the composed frame over a bounded latest-frame handoff. Each
  record is `OZFR`, three big-endian u32 values for width, height and stride,
  then `stride × height` RGBA8 bytes. The development bridges use fixed
  dimensions per connection.
- **Outbound RDP.** The `rdp_output` example pipes the composed frames to an
  isolated Xvfb display and exposes that display with FreeRDP shadow. It is
  loopback-only and view-only. A stock FreeRDP viewer displayed the scene
  overlay. A direct graphics-surface adapter remains open.
- **Outbound SPICE.** The development bridge feeds composed frames to
  libspice-server's QXL display interface. It binds to loopback and exposes
  a fixed-size, view-only display. The stock SPICE screenshot client showed
  the Ozma overlay and successive frame changes. Structured drawing, audio,
  and managed authentication remain open. [SPICE protocol](https://spice-space.org/spice-protocol.html)
- **Outbound Xpra.** The `xpra_output` example forwards native scene placements
  and cached PNGs to a loopback, view-only TCP peer. Wire tests cover hello,
  draw acknowledgement, and a placement-only move. The adapter currently
  exposes the structured image layer. Video, compressed records, and stock
  client interoperability remain open. [Xpra handshake](https://xpra-org.github.io/xpra/Porting/Handshake.html)
- **Outbound VNC.** The `vnc_output` example consumes a native QUIC
  session, decodes H.264, composites scene overlays, and serves the latest
  framebuffer over loopback RFB 3.8. It responds to raw framebuffer
  requests and discards viewer input. Run
  `cargo run -p ozma-client --example vnc_output -- --offer offer.json --node http://127.0.0.1:7382 --scene`.
  Connect a viewer to `127.0.0.1:5900`. Open a fresh viewer connection after
  a display size change. Incremental requests compare against the viewer's
  last delivered pixels for the requested area and send a bounded damage
  rectangle.
- **Google Cast.** The `cast_hls` example writes live HLS from the composed
  output, with optional Opus-to-AAC conversion. The development media origin
  scopes manifests and segments to an expiring playback URL. The Cast sender
  asks the Default Media Receiver to load the HTTPS URL and revokes the
  development grant on Stop. Local playback verifies the overlay and AAC
  rendition. Physical receiver playback, audio sync, and controller integration
  remain open. [Cast fixture instructions](../ozma-client/examples/cast-output/README.md)
- **Adaptive execution.** The selector ranks eligible representations by
  transfer and processing cost. Client-side presentation is tested for moving
  and removing overlays. A presentation model also keeps video active while an
  image or drawing waits for resources and promotes the newest ready revision.
  Feeding live measurements and applying the model to real subtree switches
  remain open.

## Run the development outputs

Use a native session offer with view access. Add `--scene` to receive scene
overlays. The RDP bridge requires Xvfb, FFplay, and FreeRDP shadow.

```sh
cargo run -p ozma-client --example scene_rgba -- \
  --node http://127.0.0.1:7382 --offer offer.json --scene | \
  cargo run -p ozma-client --example rdp_output -- --shadow-port 3390

cargo run -p ozma-client --example xpra_output -- \
  --node http://127.0.0.1:7382 --offer offer.json --scene

cc -Wall -Wextra ozma-client/examples/spice-output/spice-output.c \
  $(pkg-config --cflags --libs spice-server glib-2.0) -o /tmp/ozma-spice-output
cargo run -p ozma-client --example scene_rgba -- \
  --node http://127.0.0.1:7382 --offer offer.json --scene | \
  /tmp/ozma-spice-output 5930
```

The RDP viewer connects to `127.0.0.1:3390` using TLS. The SPICE viewer
connects to `spice://127.0.0.1:5930`. Both are local development endpoints
without viewer authentication. The Xpra development peer listens on
`127.0.0.1:14500` and expects an uncompressed rencodeplus hello advertising
PNG.

## Acceptance measurements

Use `ozma-client probe --offer offer.json --node http://127.0.0.1:7382 --json`
for the video baseline. For a node started with `--scene-replay`, append
`--scene-revision 2` to wait for revision 2 or later. The report measures from
QUIC connection start to receipt of a complete encoded frame and to readiness
of the requested scene. These timestamps precede client decode and display.

A five-run loopback smoke test with fake capture and one PNG measured 12–15 ms
to a complete frame in video-only mode and 10–13 ms to both the complete frame
and scene revision 2 in scene mode. The fixture checks the startup path.
It does not isolate the cost of scene negotiation or estimate display latency.

Run the same scrolling image and video workload in full-display video and
hybrid mode. Measure cold and warm time to first presentation, input-to-present
latency, bytes on the wire by channel, cache hits and resource uploads, missed
presentation deadlines, client and host load, and text and image fidelity.
Capture the first frame and each representation switch in the trace. Verify
that geometry and stacking agree across profiles and that a missing resource,
video loss, or reconnect cannot leave a permanently stale scene.
