# Video over USB: virtual display sink, UVC camera, and raw Ozma link

Status: **design/plan** (not implementation).
Related: [`rust-reconciliation-plan.md`](rust-reconciliation-plan.md),
[`routing/node-definition.md`](routing/node-definition.md) §4.4,
[`architecture.md`](architecture.md).

## 0. Decision record

- **DisplayLink compatibility is a non-goal.** No DisplayLink protocol, VID/PID,
  descriptor clone, or "compatible" claim. It is proprietary, patent-encumbered,
  and actively enforced.
- The display path is an **open vendor device + open host driver + documented
  protocol**. The basic mode is our driver with standard EDID modes; the raw mode
  is the same driver plus the full agent.
- The only standard, driver-free "true monitor" option is **DP alt mode over
  USB-C** (video on DP lanes, not USB data). Keep it as a hardware option, not
  the default.
- UVC remains the driver-free camera path; it is not a monitor.

## 1. Goal

The node's USB device port can carry video in several roles. They are not the
same device:

- **USB display sink (host → node).** The dongle enumerates as a **display
  adapter**. The host's OS sees an extra monitor with an EDID and renders its
  desktop to it; the dongle receives the framebuffer over USB and streams it to
  the controller/client. This is a capture path for the node and a display
  output for the host. It can replace an HDMI capture card for any target that
  can run the host driver, and it gives headless machines a display.
  - **Basic mode** — a small open-source virtual-display driver plus a
    documented baseline protocol (raw/RLE or MJPEG), standard EDID modes, one
    display. No Ozma viewer required on the host.
  - **Raw Ozma mode** — the full host agent negotiates H.264/HEVC (hardware
    encode when available), multi-display, adaptive bitrate, audio, and
    control.
- **USB video output (node → host).** The host sees a camera/capture device:
  - **Basic mode** — standard UVC: MJPEG/YUY2 (and H.264 UVC where supported).
    Works with any camera app, no driver.
  - **Raw Ozma mode** — vendor interface carrying H.264/HEVC/Opus with
    sequence, keyframe, timestamp, and flow control.
- **Host media input (host → node).** The vendor interface is full-duplex: a
  host can publish camera/mic over USB, and the node forwards it to the
  target-facing UVC/UAC2 functions, the controller, or both.

The source/sink is otherwise source-agnostic: local camera, client-published
camera, network feed, captured display, or a virtual monitor.

### 1.1 USB display sink: design constraints

**There is no open, cross-OS USB display class.** DisplayLink is proprietary
and patent-encumbered; Synaptics has actively enforced its DisplayLink patents
against open-source implementations. Do **not** implement DisplayLink protocol
compatibility, reuse its VID/PID, or ship a "DisplayLink-compatible" claim.

Instead:

| Path | Host driver | Software on host | Notes |
|---|---|---|---|
| Basic virtual display | our open driver (evdi / IddCx / CGVirtualDisplay) | yes, the basic driver | True monitor; standard EDID modes; baseline codec |
| Raw Ozma display | same driver + agent | yes, the full agent | H.264/HEVC, multi-display, adaptive bitrate |
| DP alt mode (hardware option) | none | none | True monitor over USB-C DP lanes; dongle captures DP; video is not on USB data lanes |
| UVC camera fallback | standard UVC | none | Not a monitor; apps see a camera |

Recommended sequence: **basic virtual display on Linux first** (open driver,
real monitor), then raw Ozma, then Windows IddCx, then evaluate macOS and the
DP-alt-mode hardware option. Avoid DisplayLink entirely.

Host driver matrix:

- **Linux:** `evdi` (open-source DRM virtual display used by DisplayLink's
  Linux product) plus a user-space daemon. Ship evdi as DKMS/source and respect
  the GPL kernel boundary; the daemon uses the user-space API.
- **Windows:** IddCx indirect display driver plus a user-mode companion over
  WinUSB (Microsoft OS 2.0 descriptors). Test-sign for development; WHQL
  signing is a later step.
- **macOS:** `CGVirtualDisplay` (private API) or a ScreenCaptureKit fallback.
  Treat macOS as a phase/risk, not a first-release guarantee.

Basic EDID modes: 1280×720@60, 1920×1080@60, 1920×1200@60, 2560×1440@60 where
bandwidth allows; 30 Hz variants for USB 2.0. One display in basic mode;
multiple display IDs in raw mode.

Encoding location: **the host**, not the dongle. The host has the GPU
(VAAPI/NVENC/QSV/VideoToolbox); the dongle forwards the encoded stream to the
controller or client without re-encoding. Basic mode uses a cheap baseline
(RLE/MJPEG) when no hardware encoder is available. This keeps dongle CPU free
and preserves quality.

Data path:

```text
host compositor
  → virtual display driver (damage rects)
  → baseline MJPEG/RLE  or  H.264/HEVC hardware encode
  → USB vendor bulk IN
  → dongle (forward, or local decode/record)
  → controller/client session
```

Bandwidth: USB 2.0 high-speed cannot carry 1080p60 raw; use MJPEG/H.264.
USB 3.x allows higher fps, raw/RLE, and multiple displays. Negotiate on the
vendor channel; advertise only modes the bus and host encoder sustain.

### 1.2 Aperture policy

Every mode/format/stream decision in this plan is an **aperture** decision, not
an ad-hoc flag comparison. See [`aperture-policy.md`](aperture-policy.md).

- **Physiology:** the device's maximum (resolutions, fps, codecs, displays,
  endpoints, payload).
- **Forced frames:** USB2/USB3 isochronous budget, UDC endpoint budget, thermal
  and power limits, privacy/consent, protected content. These cannot be lifted
  by the requester.
- **Chosen frames:** the user/controller/agent preference (e.g. 1080p60 H.264,
  lowest latency, battery saving). These can be relaxed unilaterally.
- **Negotiated frames:** host-driver and node agree on codec, resolution, fps,
  bitrate, display count. Adoption/retraction requires every bound party.
- **Effective aperture = meet.** The narrowest frame wins.
- **Residual + recourse.** A request outside the meet returns a typed residual
  naming the limiting frame and provenance: `relax` (chosen), `escalate`
  (forced), or `propose` (negotiated). No silent downgrade to a camera or a
  different source.
- **Receipts.** Every decision is logged and exposed through diagnostics:
  requested, effective, limiting frame, policy version, input hash.

This applies to the UVC basic formats, the raw vendor stream selection, the
virtual-display EDID/mode set, and the network transports. The same policy core
answers “why can't this host have 1440p60?” with evidence.

**Security is the same aperture.** Permissions, consent, grants, pairing, and
revocation fold through the same meet with the same provenance rules: a forced
machine policy cannot be relaxed by the requester, a chosen permission can be
dropped, and a negotiated grant/pairing requires every party. USB privacy
(`display_capture = deny`), raw-client admission, and camera/mic permission are
security frames; their receipts are audit records. See
[`aperture-policy.md`](aperture-policy.md) §3.

Ocelot is the inspiration for this model: aperture as a frame with provenance,
the effective aperture as a meet, and the same residual with different recourse
(`af_effective_aperture`, `af_relax_chosen`,
`af_forced_residual_is_a_concern`,
`af_adopt_when_every_party_accepts`). We borrow the model, not Ocelot's
cognitive engine or graph runtime. Its measurement discipline also applies:
pre-register throughput/latency/quality thresholds, record build/hardware
provenance, and do not tune a threshold to manufacture a pass.

## 2. Current state

Exists:

- HID gadget (`ozma-input::HidGadgetSink`) and UAC2 bridge
  (`ozma-node/src/usb_audio.rs`).
- Camera/mic uplink from browser and native clients into `ClientCaptureHub`.
- H.264 encoders (VAAPI, OpenH264), Opus, decode, playback, transport, auth.
- Kernel gadget modules for HID/UAC2 in
  `dev/kernel-build/build-gadget-modules.sh`.
- Hardware docs already specify `HID + UAC + UVC (configfs)` and
  `uvc_camera` as a node function.

Missing:

- `CONFIG_USB_CONFIGFS_F_UVC` / `CONFIG_USB_F_UVC` and a UVC userspace feeder.
- A configfs gadget composition owned by the Rust stack (the referenced
  `tinynode/gadget/setup_gadget.sh` does not exist in the tree).
- MJPEG/JPEG encode output.
- A vendor USB interface and raw protocol.
- A host-side USB client transport.
- Tests on `dummy_hcd` and real UDC hardware.

## 3. USB composition

One composite configuration, with functions selectable at build/run time:

```text
Configuration 1
  ├─ HID keyboard  (/dev/hidg0)          existing
  ├─ HID mouse     (/dev/hidg1)          existing
  ├─ UAC2 speaker/mic                    existing
  ├─ UVC camera                          output (basic mode, device → host)
  └─ Vendor (FunctionFS)                 bidirectional (raw mode)
       EP IN  bulk   video / audio / events      node → host (output)
       EP OUT bulk   control / input / camera    host → node (input)
       EP IN  interrupt (optional events)
```

UVC is inherently one-way (device → host). Anything the host must send —
client camera, microphone, control, input — uses the vendor OUT endpoint. This
keeps one descriptor set for both the output and input roles.

- UVC is an IAD group with a streaming interface and alternate settings.
- UVC creates a V4L2 **output** node (`/dev/videoN`); a userspace feeder owns
  the UVC probe/commit/stream-on events and writes frames.
- The vendor function is a separate interface. Host software claims it and
  speaks the raw protocol; the OS remains free to use the UVC interface.
- Windows: vendor interface auto-binds to WinUSB via **Microsoft OS 2.0
  descriptors** (WCID), so no custom driver installation. Linux uses usbfs;
  macOS uses IOKit/libusb on the unclaimed vendor interface.
- VID/PID, serial, and strings are stable and documented. A device serial is
  required for multi-node hosts.

### 3.0 Port roles and topology

A USB device port faces exactly one host. Choose the role per port:

| Role | Port faces | Node sends | Node receives |
|---|---|---|---|
| Virtual monitor (capture) | target | HID/UAC2 | raw display frames (host → node) |
| Target camera | target | UVC + raw video/audio | — (input stays on HID/UAC2) |
| Local-client display | client laptop/PC | UVC camera + raw video/audio | raw display frames if the client is also a capture target |
| Full local console | client | UVC + raw video/audio | display frames, control, keyboard/mouse, camera/mic |

- If the board has **one** device-capable OTG port (e.g. RK3588S USB-C), it can
  serve one role at a time. The Magic Dock use case (USB-C to the laptop,
  HDMI capture from it) can be either the **virtual monitor** role (laptop
  renders to the dongle) or the **target camera** role (laptop sees a webcam),
  selected in software; it cannot be both simultaneously on one port.
- A **local-client display** role needs a second device-capable port or a
  deliberate role switch; it cannot share the same port with target HID/audio.
  Validate the board's OTG/host mux before promising both simultaneously.
- With two device ports, one can face the target (HID/UAC2/UVC) and one the
  client (UVC/raw), enabling a true no-network USB KVM console.
- Document the supported topology per hardware model rather than implying all
  roles are simultaneous.

### 3.1 UVC basic modes

Start conservative, then extend:

| Format | Resolution | FPS | Notes |
|---|---|---|---|
| MJPEG | 640×480 | 30 | Universal fallback |
| MJPEG | 1280×720 | 30 | Primary basic mode |
| MJPEG | 1920×1080 | 30 | USB2 isoc budget permitting; verify per UDC |
| YUY2 | 640×480 | 30 | Uncompressed fallback |
| H.264 (UVC 1.5) | 1920×1080 | 30 | Optional; modern Windows/macOS/Linux; not a universal guarantee |

Rules:

- Advertise only modes that the UDC and encoder can sustain. Do not advertise
  1080p if isochronous bandwidth or CPU cannot hold it.
- MJPEG is the compatibility baseline; YUY2 only at low resolution.
- H.264 UVC is offered as an extra mode for capable hosts, not as the fallback.
- The feeder must handle host format changes at runtime without dropping the
  gadget.

### 3.2 Raw vendor protocol

A small framed protocol, reusing `ozma-protocol` types where they fit:

```text
UsbMessageHeader {
  magic:      [u8;4] = "OZUS",
  version:    u16,
  kind:       u8,     // 1 control, 2 video, 3 audio, 4 event
  flags:      u8,     // keyframe, end-of-stream, ...
  sequence:   u32,
  pts_micros: u64,
  length:     u32,
}
```

Control messages (JSON payloads, or a compact binary form):

- `hello { version, client, capabilities }`
- `caps { codecs, modes, max_frame, displays, audio, directions }`
- `select { stream, codec, width, height, fps, bitrate, display }`
- `selected { ... }`, `start`, `stop`, `keyframe_request`, `stats`,
  `keepalive`, `error`
- Host → node: `client_camera { codec, width, height, fps }`,
  `client_mic { sample_rate, channels }`, `client_camera_stop`,
  `client_mic_stop`, plus input/control messages when the host is an Ozma
  client.

`directions` advertises which media the host may send (`none`, `camera`,
`camera+mic`). A node that cannot accept host video (no target UVC/UAC2 or no
controller sink) rejects the request with a typed error.

Media:

- One encoded access unit per message (no 1100-byte video fragmentation needed
  over bulk). Chunk very large frames at ~1 MiB to keep latency bounded.
- Video: H.264/HEVC/AV1 Annex B, keyframe flag and sequence preserved.
- Audio: Opus packets.
- Host → node: framed camera access units and Opus packets on the OUT endpoint,
  with the same header; the node validates codec/resolution and forwards them
  to `ClientCaptureHub` or the target-facing UVC/UAC2 sink.
- Flow control both ways: bounded queues with credit/window feedback; drop
  non-keyframes before keyframes; never block the encoder or the host.
- Reconnect: `hello` resets the stream; the sender starts with a keyframe.

Mode arbitration:

- If a raw client completes `hello` within N seconds of enumeration, start raw.
- Otherwise start UVC basic.
- Optional "both": UVC basic at reduced fps while raw is active, if bandwidth
  and CPU allow.
- A raw client can request `stop` to return to basic mode.

## 4. Rust implementation

New crate `ozma-usb` (or `ozma-node/src/usb/`) with:

- `gadget.rs` — configfs composition: create functions, link to a
  configuration, set strings/IDs, bind the UDC; idempotent teardown; clear
  errors; retries while the UDC is muxed away (RK3588S OTG/host mux).
- `display.rs` — vendor display sink: FunctionFS/descriptor set for a virtual
  display, EDID and mode management, damage/frame reassembly from the host,
  multi-display IDs, hotplug, bandwidth/rate negotiation, forwarding to the
  controller/client.
- `uvc.rs` — V4L2 output feeder: enumerate formats, handle UVC events
  (probe/commit/stream-on/off), buffer management, frame pacing, format
  conversion (I420/NV12 → MJPEG/YUY2/H.264), frame timestamps.
- `vendor.rs` — FunctionFS mount/descriptors, endpoint reader/writer tasks,
  protocol codec, credits, reconnect.
- `source.rs` — select a video source: `LocalCamera`, `ClientCaptureHub`
  (browser/native uplink), `NetworkFeed`, `DisplayCapture`; source changes are
  allowed while running.
- `sink.rs` — accept host-published media from the vendor OUT endpoint and
  route it:
  - display frames → the capture/session pipeline (and optionally a local
    preview);
  - camera/mic → target-facing UVC/UAC2 functions, `ClientCaptureHub`, the
    controller, or a combination.
  Apply the same authorization and bounds as the network camera uplink.
- `encode.rs` — MJPEG encoder (ffmpeg initially, then a Rust/V4L2 M2M path) and
  reuse of the existing H.264/Opus encoders.
- `config.rs` — `--usb-video`, mode list, max fps, source selection, UDC name,
  vendor interface on/off, tokens.

`ozma-node` integration:

- `--profile hardware|vm|desktop` plus a `usb-video` feature.
- The USB video output subscribes to the same encoded/raw frame stream as the
  network transports; it does not fork the capture pipeline.
- The camera uplink path becomes end-to-end in either direction:
  browser/native camera → `ClientCaptureHub` → USB UVC to the target, and
  host camera over USB → `ClientCaptureHub` → target/controller.
- The output path reuses the same capture/encode stream as the network
  transports: target display/camera → UVC basic and/or raw vendor.
- Metrics: UVC frames/drops, raw bytes/credits, reconnects, mode switches.

Gadget setup is owned by the Rust service, not by an external missing script.
The kernel build script gains UVC options and V4L2/media dependencies.

## 5. Host/client software

- **Display host driver (Linux first):** a user-space daemon using evdi:
  create the virtual monitor, receive damage/frame updates, encode with
  VAAPI when raw mode is selected, send the vendor protocol. A minimal
  "basic" mode uses MJPEG/RLE and no Ozma viewer.
- **No software (UVC only):** the target sees a UVC camera. Validate with
  `v4l2-ctl --list-formats-ext`, `ffmpeg -f v4l2`, `guvcview`, Chrome
  `getUserMedia`, the Windows Camera app, macOS Photo Booth/FaceTime.
- **Rust host transport:** new `ozma-transport-usb` using `nusb` (preferred,
  pure Rust, cross-platform) or `rusb`:
  - enumerate VID/PID and the vendor interface; claim it; open endpoints;
  - `hello`/`caps`/`select`; receive frames; reuse `ozma-decode` and
    `ozma-playback`;
  - send input/control over the OUT endpoint when the host is an Ozma client;
  - send host camera/mic (`client_camera`, `client_mic`) when requested and
    permitted, reusing the existing camera/encoder code;
  - expose a `ChannelTransport`-like adapter so the existing session driver can
    run over USB instead of QUIC, in either direction.
- **Optional browser path:** WebUSB on Chromium for the vendor interface, no
  native driver. Not required for the first release.
- **Optional target-side agent:** an Ozma agent inside the target can select
  raw mode and republish frames to a virtual camera (v4l2loopback,
  DirectShow/Media Foundation, AVFoundation) when an app cannot use UVC/H.264.

## 6. Kernel and hardware

Kernel options to add (node and dev kernel):

```text
CONFIG_MEDIA_SUPPORT=y
CONFIG_VIDEO_DEV=y
CONFIG_V4L2_FOPS=y
CONFIG_USB_CONFIGFS_F_UVC=y
CONFIG_USB_F_UVC=y
CONFIG_USB_CONFIGFS_F_FS=y
CONFIG_USB_F_FS=y
CONFIG_USB_CONFIGFS_F_HID=y
CONFIG_USB_F_HID=y
CONFIG_USB_CONFIGFS_F_UAC2=y
CONFIG_USB_F_UAC2=y
```

- USB 2.0 high-speed is the floor. 720p30 MJPEG fits comfortably; 1080p30
  MJPEG needs a careful isochronous budget. USB 3.x (RK3588S OTG) removes most
  pressure and is preferred for raw bulk.
- UVC isochronous endpoints and vendor bulk endpoints must fit the UDC's
  endpoint budget together with HID and UAC2.
- The RK3588S OTG/host mux must be validated per board revision; the design
  assumes the USB-C port is a gadget port to the target while HDMI capture is
  a host port.
- Power/PD is orthogonal; the gadget must enumerate on bus power and survive
  PD renegotiation.

## 7. Security

- UVC basic mode is intentionally open and one-way.
- Raw mode requires a handshake; optionally a short local token displayed on
  the node or provisioned into the host software. Reject unknown clients.
- Bound every length; fuzz the protocol; never allocate based on an unchecked
  `length`.
- No HID injection via the vendor interface by default; input travels over the
  existing authorized session path.
- A raw client must not be able to observe other sessions' media.

## 8. Tests

- **dummy_hcd:** the existing dev harness can create a gadget and a host
  controller. Validate descriptors, UVC formats, stream-on/off, the vendor
  display sink, and the vendor interface without hardware.
- **Linux virtual display:** evdi creates an Ozma monitor visible in
  `xrandr`/DRM; move windows/change resolution and assert frames arrive; measure
  glass-to-glass latency and CPU; compare against HDMI capture.
- **Multi-display:** two virtual monitors, independent EDIDs, display IDs in
  the protocol.
- **UVC compatibility:**
  - `v4l2-ctl --list-formats-ext` shows exactly the advertised modes;
  - `ffmpeg -f v4l2 -i /dev/videoN -t 10` decodes 720p30/1080p30 with no
    corruption;
  - Chrome `getUserMedia` and a native app show live frames;
  - Windows and macOS camera apps (manual/CI runner where available).
- **Raw protocol:** Rust host client over dummy_hcd; H.264 1080p30; sequence and
  keyframe continuity; credit/backpressure; reconnect; no-client fallback to
  UVC.
- **End-to-end (output):** browser/native camera → node hub → UVC webcam
  visible inside a QEMU target VM; audio via UAC2. Captured target display →
  UVC/raw on a local-client port.
- **End-to-end (input):** host camera/mic over the vendor OUT endpoint →
  target-facing UVC/UAC2 visible inside a QEMU target VM; reject when the node
  has no sink.
- **Soak:** 1-hour streams for UVC and raw; memory/fd/handle leaks; UDC
  unbind/rebind.
- **Security:** oversized length, unknown kind, bad version, fuzz corpus.

## 9. Phases and estimates

| Phase | Deliverable | Estimate |
|---|---|---|
| A0 | `ozma-aperture` crate: meet, chosen/forced/negotiated frames, residual/recourse, receipts, policy loader, tests | 1 wk |
| A | Kernel config + configfs composition; **Linux virtual display sink** (evdi daemon + vendor bulk + basic MJPEG/RLE 1080p30); aperture-driven mode negotiation; dummy_hcd and real UDC tests | 3–4 wk |
| B | Raw display mode: host-side VAAPI/NVENC encode, H.264/HEVC, multi-display, adaptive bitrate; UVC camera output and fallback | 3–4 wk |
| C | Windows IddCx driver + WinUSB; host camera/mic input; UAC2; reconnect; macOS spike; packaging/signing | 4–5 wk |
| D | Hardware integration (OPi5/Zero 3), PD/mux, optional DP-alt-mode sink, docs, CI | 2–3 wk |

Roughly **9–12 engineer-weeks** for one contributor; A and B can overlap once
the gadget composition is stable.

## 10. Acceptance gates

- **U1:** a clean Linux target enumerates the composite device; UVC shows
  720p30 MJPEG and (where supported) 1080p30; 10-minute stream with no
  corruption; YUY2 fallback works.
- **U2:** a Rust host client completes `hello/select`, receives 1080p30 H.264
  with continuous keyframes/sequence, and can `stop` back to UVC. No client
  after 3 s still gives UVC.
- **U3:** a browser-published camera appears as a webcam in a QEMU target VM
  and in Chrome inside that VM; UAC2 audio works alongside.
- **U3b:** a host-published camera/mic over USB appears as a webcam/microphone
  in the target (or in a second client), with the same authorization and bounds
  as the network camera uplink.
- **U4:** Windows auto-binds WinUSB (no driver install) and macOS/Linux clients
  work; install docs and CI cover the supported matrix.
- **A0:** aperture meet is commutative/idempotent/order-independent; the same
  residual under chosen/forced/negotiated frames yields different recourse; a
  negotiated frame requires every party; receipts name the limiting frame and
  policy version.
- **D1:** on Linux, the host sees an Ozma virtual monitor in `xrandr`, renders
  to it, and the captured desktop appears in the client at 1080p30 with stable
  frame rate and bounded latency. No DisplayLink code or IDs are used, and mode
  selection goes through the aperture (no hardcoded resolution branch).
- **D2:** raw display mode carries H.264/HEVC 1080p30 over USB 2.0 and
  1080p60 over USB 3.x; multi-display works; adaptive bitrate responds to
  congestion.
- **D3:** Windows IddCx virtual monitor works in a VM with test signing;
  macOS has a documented decision (supported path or deferral).

## 11. Risks

| Risk | Mitigation |
|---|---|
| UVC gadget userspace is fiddly (events, alt settings, isoc) | Start from the kernel `uvc-gadget` pattern; validate on dummy_hcd before hardware; keep UVC optional at runtime |
| USB2 isochronous bandwidth | Advertise 720p30 as the baseline; gate 1080p on measured throughput; prefer USB3 |
| MJPEG encode cost | Hardware/ffmpeg M2M first; cap fps/resolution; offer H.264 UVC where supported |
| Windows vendor interface | Microsoft OS 2.0 descriptors for WinUSB; test on a clean Windows image |
| Endpoint/interface budget with HID+UAC2 | Reserve a fixed composition; make functions configurable and validate descriptors on the UDC |
| OTG/host mux on RK3588S | Per-board validation; fail clearly if the UDC disappears |
| One device port cannot face target and client at once | Document role selection; require a second device port for simultaneous roles |
| Host→node video path increases descriptor/bandwidth pressure | Separate vendor OUT endpoint; explicit `directions` capability; reject unsupported sinks |
| Raw protocol drift | Versioned header; reuse `ozma-protocol` types; fuzz and golden tests |
| Security of a local USB channel | Handshake/token, bounded lengths, no cross-session access, no HID tunneling by default |
| DisplayLink patents and enforcement | Do not implement or claim DisplayLink compatibility; clean-room open protocol/driver; legal review before release |
| Host driver signing (Windows) | Test-sign for development; budget WHQL signing and CI |
| macOS virtual display APIs are private/restricted | Spike early; ScreenCaptureKit fallback or defer macOS |
| evdi GPL/kernel boundary | Keep the kernel module GPL and the daemon user-space; ship source/DKMS |
| USB2 bandwidth for a display | Host-side encode; advertise only sustainable modes; USB3 for 60 fps/multi-display |

## 12. Relationship to the Rust migration

This is a new **local transport/output**, not a replacement for the soft-node
work:

- It reuses the Rust capture/encode/audio stack and the camera-uplink hub.
- It adds `ozma-usb` and `ozma-transport-usb`, keeping the native session plane
  (QUIC/WebTransport/RTSP) primary.
- It gives hardware nodes a zero-install video path (UVC) and a high-quality
  path (raw) that does not depend on the Python controller.
- Soft nodes can expose the same source as a UVC device if the hypervisor
  passes a virtual USB controller/gadget through, but the first target is the
  hardware node.
