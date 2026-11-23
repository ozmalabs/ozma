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

## Current implementation

- **Native QUIC.** Scene v1 starts with a single full-display surface on the
  existing video channel. The node accepts an optional startup scene producer
  and sends its initial updates after submitting the first video frame. A
  placement-only update can reuse an existing PNG. The authenticated QUIC
  test exercises this path. The loopback test exercises automatic presentation
  when a resource arrives after its commit. Resources are bounded to 65,535
  bytes including the ID, and the receiver starts a fresh cache per connection.
- **Xpra.** The fixture mapper accepts decoded geometry, whole-window raster
  references, placement deltas, and removal. An Xpra packet decoder, region
  copies against a raster backing store, stacking metadata, and a live peer
  test are pending.
- **Browser.** The standalone fixture emits one PNG resource with native
  resource framing, ordered placement commits, and independent canvas video.
  Serve `ozma-protocol/examples/browser-scene` on localhost and open
  `index.html`. The fixture publishes DOM events. A browser-to-node bridge,
  arbitrary browser paint capture, and desktop compositing are pending.
- **Adaptive execution.** The tested selector ranks eligible representations
  by transfer and processing cost. It retains the current representation
  through small changes when it can still meet the presentation deadline.
  Runtime telemetry and presentation-boundary representation switches are
  pending.

## Acceptance measurements

Run the same scrolling image and video workload in full-display video and
hybrid mode. Measure cold and warm time to first presentation, input-to-present
latency, bytes on the wire by channel, cache hits and resource uploads, missed
presentation deadlines, client and host load, and text and image fidelity.
Capture the first frame and each representation switch in the trace. Verify
that geometry and stacking agree across profiles and that a missing resource,
video loss, or reconnect cannot leave a permanently stale scene.
