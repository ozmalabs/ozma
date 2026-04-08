# Appendices

**Status:** Draft
**RFC 2119 Conformance:** The key words "MUST", "MUST NOT", "REQUIRED", "SHALL",
"SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

This document contains worked examples illustrating the Ozma routing
specification in practice. All appendices are implementation notes
(non-normative) and do not impose requirements on conforming implementations.
They demonstrate how the routing graph, intents, format negotiation, and intent
composition operate in realistic scenarios.

---

## Appendix A: Gaming Scenario Worked Example

*Implementation notes (non-normative).*

### Scenario: User switches from "Desktop" to "Gaming"

**Setup**: Controller with two hardware nodes (Node A -> Workstation, Node B ->
Gaming PC). Both on the same LAN. Node B has a capture card (USB 3.0, supports
1080p60 MJPEG). Node B is connected to a GPU with NVENC.

**Graph** (relevant subset):

```
Controller
  +-- Port: kbd-capture (HID source, evdev)
  +-- Port: mouse-capture (HID source, evdev)
  +-- Port: audio-out (audio sink, PipeWire)
  +-- Port: udp-7331 (HID sink, network)

Node B (LAN: 10.0.100.12, overlay: 10.200.0.12)
  +-- Port: udp-7331 (HID sink, network)
  +-- Port: hidg0 (HID sink, USB gadget -> Gaming PC)
  +-- Port: v4l2-cap (video source, capture card output)
  +-- Port: vban-6980 (audio source, network)
  +-- Port: usb-audio (audio source, UAC2 gadget <- Gaming PC)

Gaming PC (target)
  +-- Port: hdmi-out (video source -> capture card)
  +-- Port: usb-hid (HID sink, sees Node B as keyboard/mouse)
  +-- Port: usb-audio (audio source, sees Node B as speaker)
```

**Intent**: `gaming`

**Router computation**:

1. **HID pipeline**: Controller:kbd-capture -> udp-aead -> Node B:udp-7331 ->
   internal -> Node B:hidg0 -> usb-gadget -> Gaming PC:usb-hid
   - Latency: <1ms (evdev) + <1ms (LAN UDP) + <1ms (gadget write) = ~2ms (< 5ms target)
   - Format: boot protocol keyboard, 8 bytes, 1000 Hz

2. **Video pipeline**: Gaming PC:hdmi-out -> hdmi-cable -> Node B:capture-card:hdmi-in ->
   internal -> Node B:capture-card:usb-out -> v4l2 -> Node B:v4l2-cap ->
   ffmpeg(MJPEG->H.264, NVENC) -> udp-aead -> Controller:video-sink
   - Latency: ~1ms (capture) + ~3ms (NVENC encode) + ~1ms (LAN UDP) = ~5ms (< 16ms target)
   - Format: 1080p60 H.264 high profile, ~15 Mbps
   - Note: MJPEG from capture card is transcoded to H.264 by NVENC (1 conversion)

3. **Audio pipeline**: Gaming PC:usb-audio -> Node B:usb-audio -> pipewire ->
   Node B:vban-6980 -> vban -> Controller:audio-in -> pipewire -> Controller:audio-out
   - Latency: ~5ms (UAC2 buffer) + ~5ms (VBAN frame) + ~1ms (LAN) = ~11ms (< 20ms target)
   - Format: PCM 48000 Hz, stereo, 16-bit (VBAN native, no conversion)

**Result**: Three pipelines, all satisfying `gaming` intent constraints. Total
HID latency ~2ms, video ~5ms, audio ~11ms. Zero degradation needed.

**Switching**: When the user presses the switch hotkey, the controller:
1. Deactivates Node A's HID pipeline (stops sending UDP to Node A:7331)
2. Activates Node B's pre-computed HID pipeline (starts sending UDP to Node B:7331)
3. Activates Node B's video pipeline (starts ffmpeg capture + transcode + stream)
4. Activates Node B's audio pipeline (starts VBAN stream)

Steps 1-2 complete in <5ms (UDP redirect). Steps 3-4 complete in <100ms
(process startup). HID switching is near-instantaneous; video/audio follow
within one frame.

---

## Appendix B: Format Negotiation Worked Example

*Implementation notes (non-normative).*

### Capture card format selection

**Port capabilities**:

```yaml
# Capture card V4L2 output port
capabilities:
  formats:
    - codec: mjpeg
      resolution: { min: {w: 640, h: 480}, max: {w: 1920, h: 1080} }
      framerate: { min: 1, max: 60 }
    - codec: raw
      resolution: { min: {w: 640, h: 480}, max: {w: 1920, h: 1080} }
      framerate: { min: 1, max: 30 }  # raw limited by USB bandwidth

# Network transport (udp-aead)
capabilities:
  formats:
    - codec: [h264, h265, mjpeg, av1]  # any compressed format
      resolution: { min: {w: 1, h: 1}, max: {w: 7680, h: 4320} }
      framerate: { min: 1, max: 240 }

# Hardware encoder (NVENC)
capabilities:
  formats:
    - input: { codec: [raw, mjpeg] }  # accepts raw or MJPEG
      output: { codec: [h264, h265], resolution: { max: {w: 4096, h: 4096} } }
```

**Intent**: `gaming` -- wants <=16ms latency, prefers hardware codec, prefers
lower latency over higher quality.

**Negotiation**:

1. **Enumerate**: Capture outputs MJPEG or raw. NVENC accepts both, outputs
   H.264 or H.265. Network accepts any compressed format.

2. **Restrict**: Intent forbids nothing. Intersection: MJPEG->H.264, MJPEG->H.265,
   raw->H.264, raw->H.265 are all viable paths through the encoder.

3. **Fixate**:
   - MJPEG input preferred over raw (less USB bandwidth, capture card hardware
     compresses it)
   - H.264 output preferred over H.265 (lower encode latency on NVENC, intent
     prefers lower latency)
   - Resolution: 1080p (max capture card supports)
   - Framerate: 60 (intent preference, capture card supports it with MJPEG)

   **Selected**: Capture(MJPEG 1080p60) -> NVENC(MJPEG->H.264 1080p60) -> UDP

---

## Appendix C: Intent Composition Example

*Implementation notes (non-normative).*

### `control` + `fidelity_audio`

User is typing on a headless audio workstation -- needs HID input and
uncompressed multichannel audio, no video.

**Composition**:

```yaml
# Merged from control + fidelity_audio
streams:
  - media_type: hid
    required: true                    # from control
    constraints:
      max_latency_ms: 5              # from control
      max_loss: 0.001                # from control
    preferences:
      prefer_lower_latency: true     # from control

  - media_type: audio
    required: true                    # from fidelity_audio
    constraints:
      max_latency_ms: 10             # from fidelity_audio
      forbidden_formats:
        - { lossy: true }            # from fidelity_audio
    preferences:
      target_channels: 8             # from fidelity_audio
      target_sample_rate: 96000      # from fidelity_audio
      target_bit_depth: 32           # from fidelity_audio
      prefer_lossless: true          # from fidelity_audio

  - media_type: video
    required: false                   # both say false

priority: 90                          # max(control:90, fidelity_audio:60)
```

The router builds two pipelines: one for HID (tiny, fast), one for audio
(high bandwidth, lossless). No video pipeline is created.
