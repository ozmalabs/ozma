# Video over USB: UVC basic mode + raw Ozma vendor mode

Status: **design/plan** (not implementation).
Related: [`rust-reconciliation-plan.md`](rust-reconciliation-plan.md),
[`routing/node-definition.md`](routing/node-definition.md) §4.4,
[`architecture.md`](architecture.md).

## 1. Goal

The node's USB device port presents video to the **target host**:

- **Basic mode** — a standard UVC camera. Any OS or app sees a webcam with no
  driver or Ozma software: MJPEG/YUY2 (and H.264 UVC where the host supports
  it). This covers OS installers, video-call apps, browsers, and quick tests.
- **Raw Ozma mode** — a vendor-specific USB interface carrying an efficient
  H.264/HEVC/Opus stream with sequence numbers, keyframe flags, timestamps,
  metadata, and control. Used when Ozma host software is running on the target.
  Higher resolution/fps, lower latency, multiple streams/displays, and explicit
  flow control.

The video source is source-agnostic:

- a local camera on the node (Camera Node / USB camera),
- a client-published camera (the browser/native camera uplink already
  implemented; `ClientCaptureHub`),
- a network/controller feed,
- optionally a captured display, if a deployment wants screen video as a
  webcam.

The same gadget can expose UAC2 audio (already present) so the target sees a
webcam + microphone as one composite device.

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
  ├─ UVC camera                          new (basic mode)
  └─ Vendor (FunctionFS)                 new (raw mode)
       EP IN  bulk   video
       EP IN  bulk   audio/events
       EP OUT bulk   control/input
       EP IN  interrupt (optional events)
```

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
- `caps { codecs, modes, max_frame, displays, audio }`
- `select { stream, codec, width, height, fps, bitrate, display }`
- `selected { ... }`, `start`, `stop`, `keyframe_request`, `stats`,
  `keepalive`, `error`

Media:

- One encoded access unit per message (no 1100-byte video fragmentation needed
  over bulk). Chunk very large frames at ~1 MiB to keep latency bounded.
- Video: H.264/HEVC/AV1 Annex B, keyframe flag and sequence preserved.
- Audio: Opus packets.
- Flow control: a bounded queue with credit/window feedback; drop non-keyframes
  before keyframes; never block the encoder.
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
- `uvc.rs` — V4L2 output feeder: enumerate formats, handle UVC events
  (probe/commit/stream-on/off), buffer management, frame pacing, format
  conversion (I420/NV12 → MJPEG/YUY2/H.264), frame timestamps.
- `vendor.rs` — FunctionFS mount/descriptors, endpoint reader/writer tasks,
  protocol codec, credits, reconnect.
- `source.rs` — select a video source: `LocalCamera`, `ClientCaptureHub`
  (browser/native uplink), `NetworkFeed`, `DisplayCapture`; source changes are
  allowed while running.
- `encode.rs` — MJPEG encoder (ffmpeg initially, then a Rust/V4L2 M2M path) and
  reuse of the existing H.264/Opus encoders.
- `config.rs` — `--usb-video`, mode list, max fps, source selection, UDC name,
  vendor interface on/off, tokens.

`ozma-node` integration:

- `--profile hardware|vm|desktop` plus a `usb-video` feature.
- The USB video output subscribes to the same encoded/raw frame stream as the
  network transports; it does not fork the capture pipeline.
- The camera uplink path becomes end-to-end: browser/native camera →
  `ClientCaptureHub` → USB UVC to the target (and/or controller stream).
- Metrics: UVC frames/drops, raw bytes/credits, reconnects, mode switches.

Gadget setup is owned by the Rust service, not by an external missing script.
The kernel build script gains UVC options and V4L2/media dependencies.

## 5. Host/client software

- **No software:** the target sees a UVC camera. Validate with
  `v4l2-ctl --list-formats-ext`, `ffmpeg -f v4l2`, `guvcview`, Chrome
  `getUserMedia`, the Windows Camera app, macOS Photo Booth/FaceTime.
- **Rust host transport:** new `ozma-transport-usb` using `nusb` (preferred,
  pure Rust, cross-platform) or `rusb`:
  - enumerate VID/PID and the vendor interface; claim it; open endpoints;
  - `hello`/`caps`/`select`; receive frames; reuse `ozma-decode` and
    `ozma-playback`;
  - send input/control over the OUT endpoint when the host is an Ozma client;
  - expose a `ChannelTransport`-like adapter so the existing session driver can
    run over USB instead of QUIC.
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
  controller. Validate descriptors, UVC formats, stream-on/off, and the vendor
  interface without hardware.
- **UVC compatibility:**
  - `v4l2-ctl --list-formats-ext` shows exactly the advertised modes;
  - `ffmpeg -f v4l2 -i /dev/videoN -t 10` decodes 720p30/1080p30 with no
    corruption;
  - Chrome `getUserMedia` and a native app show live frames;
  - Windows and macOS camera apps (manual/CI runner where available).
- **Raw protocol:** Rust host client over dummy_hcd; H.264 1080p30; sequence and
  keyframe continuity; credit/backpressure; reconnect; no-client fallback to
  UVC.
- **End-to-end:** browser/native camera → node hub → UVC webcam visible inside
  a QEMU target VM; audio via UAC2.
- **Soak:** 1-hour streams for UVC and raw; memory/fd/handle leaks; UDC
  unbind/rebind.
- **Security:** oversized length, unknown kind, bad version, fuzz corpus.

## 9. Phases and estimates

| Phase | Deliverable | Estimate |
|---|---|---|
| A | Kernel config + configfs composition + UVC feeder; MJPEG 720p30 from the local camera; dummy_hcd and real UDC tests | 2–3 wk |
| B | FunctionFS vendor interface + raw protocol + Rust host transport; H.264 1080p30; mode negotiation and UVC fallback | 3–4 wk |
| C | Client-published camera source, UAC2 audio, reconnect, Windows WinUSB descriptors, macOS/Linux validation | 2–3 wk |
| D | Hardware integration (OPi5/Zero 3), PD/mux validation, packaging, docs, CI | 2 wk |

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
- **U4:** Windows auto-binds WinUSB (no driver install) and macOS/Linux clients
  work; install docs and CI cover the supported matrix.

## 11. Risks

| Risk | Mitigation |
|---|---|
| UVC gadget userspace is fiddly (events, alt settings, isoc) | Start from the kernel `uvc-gadget` pattern; validate on dummy_hcd before hardware; keep UVC optional at runtime |
| USB2 isochronous bandwidth | Advertise 720p30 as the baseline; gate 1080p on measured throughput; prefer USB3 |
| MJPEG encode cost | Hardware/ffmpeg M2M first; cap fps/resolution; offer H.264 UVC where supported |
| Windows vendor interface | Microsoft OS 2.0 descriptors for WinUSB; test on a clean Windows image |
| Endpoint/interface budget with HID+UAC2 | Reserve a fixed composition; make functions configurable and validate descriptors on the UDC |
| OTG/host mux on RK3588S | Per-board validation; fail clearly if the UDC disappears |
| Raw protocol drift | Versioned header; reuse `ozma-protocol` types; fuzz and golden tests |
| Security of a local USB channel | Handshake/token, bounded lengths, no cross-session access, no HID tunneling by default |

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
