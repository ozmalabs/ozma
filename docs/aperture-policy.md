# Aperture policy for Ozma capability negotiation

Status: **plan**.
Inspiration: Ocelot's `observer_aperture` and `aperture_frame` seeds. We borrow
the *model*, not the cognitive engine or its graph runtime.

## 1. Purpose

Ozma negotiates capabilities in several places: USB display modes, UVC formats,
the raw USB link, QUIC/WebTransport/RTSP codecs, capture source selection, and
bandwidth/thermal limits. Today these are spread across ad-hoc flags and
`if` branches. An **aperture** makes them one declarative policy:

> The effective aperture is the **meet** of what a device can do and every
> frame currently in force. A frame has **provenance** — chosen, forced, or
> negotiated — and provenance determines what you may do about a residual.

This gives one vocabulary for “why can’t I have 1440p60?” and one auditable
decision record for every capability negotiation.

## 2. Model

```text
Aperture        — a set of admissible values per dimension
Frame           — an aperture + provenance (chosen | forced | negotiated)
Physiology      — the endpoint’s maximum aperture (ap_full)
Meet            — fold of Intersect over physiology + every frame in force
Residual        — requested value outside the effective aperture
Recourse        — what the requester may do about the residual
Receipt         — the evidence: inputs, meet, limiting frame, result
```

### 2.1 Dimensions

Start with:

| Dimension | Values |
|---|---|
| `width`, `height` | integer pixels |
| `fps` | integer Hz |
| `codec` | ordered set (`h264`, `hevc`, `mjpeg`, `yuy2`, `opus`, …) |
| `bitrate_kbps` | integer |
| `displays` | integer count |
| `hdr` | boolean / enum |
| `bus` | `usb2`, `usb3`, `network`, … |
| `max_payload` | bytes |
| `latency_class` | `realtime`, `interactive`, `bulk` |
| `audio` | channels/rate/codec |
| `source` | `local-camera`, `client-camera`, `display`, `network` |
| `privacy` | `allow`, `consent-required`, `deny` |

Dimensions are extensible; unknown dimensions are carried through untouched so
a newer controller can add policy without breaking an older node.

### 2.2 Meet

`effective = ap_full ∩ F1 ∩ F2 ∩ … ∩ Fn`

- Commutative, associative, idempotent, order-independent.
- Numeric maxima: the **minimum** wins.
- Sets: the **intersection**; codec preference is a stable ordering, not a
  policy override.
- Booleans/enums: the most restrictive wins.
- An empty intersection is a valid result, not a crash. It carries the
  conflicting frames and a typed `aperture.empty` error.

### 2.3 Provenance and recourse

The same residual has different recourse depending on which frame caused it:

| Provenance | Who imposed it | What a residual means | Recourse |
|---|---|---|---|
| `chosen` | the requester/user/controller | self-imposed limit | `relax`: drop or widen the frame unilaterally, then re-evaluate |
| `forced` | device, bus, thermal, power, privacy/consent, regulatory | a limit you cannot lift alone | `escalate`: emit a diagnostic/concern; operator or policy change required |
| `negotiated` | a proposal binding several parties | a shared limit | `propose`: offer a new aperture; adoption/retraction only when **every** bound party accepts |

A unilateral accept of a negotiated frame does nothing. A forced frame cannot be
relaxed by the requester. A chosen frame can be relaxed by its owner.

This mirrors Ocelot's `af_relax_chosen`, `af_forced_residual_is_a_concern`,
`af_adopt_when_every_party_accepts`, and `af_retract_when_every_party_accepts`.

### 2.4 Receipts

Every evaluation produces a JSON receipt:

```json
{
  "requested": {"width": 2560, "height": 1440, "fps": 60, "codec": "h264"},
  "effective": {"width": 1920, "height": 1080, "fps": 30, "codec": "h264"},
  "limiting": [
    {"dimension": "width",  "frame": "usb2-bandwidth", "provenance": "forced"},
    {"dimension": "height", "frame": "usb2-bandwidth", "provenance": "forced"},
    {"dimension": "fps",    "frame": "usb2-bandwidth", "provenance": "forced"}
  ],
  "residual": {"dimension": "width", "requested": 2560, "allowed": 1920},
  "recourse": "escalate",
  "policy_version": "opi5-usb2-v1",
  "input_hash": "…"
}
```

Receipts are logged, attached to diagnostics, and exposed over the API. They are
stable enough to diff between builds and hardware profiles.

## 3. Policy data

Policy is declarative data, versioned per hardware/profile, not code branches.

```toml
# policy/opi5-usb2.toml
[aperture]
name = "opi5-usb2"
version = "1.0.0"

[aperture.physiology.display]
width = 3840
height = 2160
fps = 60
codecs = ["h264", "hevc", "mjpeg", "yuy2"]
displays = 2
bitrate_kbps = 50000

[aperture.forced.usb2]
bus = "usb2"
max_width = 1920
max_height = 1080
max_fps = 30
max_payload = 1024
note = "isochronous budget measured on OPi5 USB-C"

[aperture.forced.thermal]
max_fps = 30
note = "sustained fanless profile"

[aperture.chosen.default]
codec = "h264"
fps = 30
bitrate_kbps = 12000

[aperture.privacy]
display_capture = "consent-required"
```

Host and controller policy are frames too: a controller may choose 1080p60, a
host driver may force `codec = "mjpeg"`, and a USB bus frame may force 30 fps.
The meet is computed, never hand-written.

## 4. Integration

New crate `ozma-aperture`:

- `Aperture` (dimensions), `Frame { provenance, policy }`, `Meet`, `Residual`,
  `Recourse`, `Receipt`.
- `evaluate(physiology, frames, request) -> Admission { effective, receipt } |
  Residual { limiting, recourse }`.
- `Proposal`/`accept`/`retract` for negotiated frames, with a party set and
  all-must-accept semantics.
- Policy loader (TOML/JSON), profile registry, versioning.
- No I/O, no async runtime, no platform code: pure policy, usable by the node,
  host drivers, controller, and clients.

Consumers:

| Consumer | Use |
|---|---|
| USB display sink | mode/resolution/fps/codec/multi-display negotiation |
| UVC camera output | format and payload selection |
| Raw USB link | stream selection, credits, payload size |
| QUIC/WebTransport/RTSP | codec/resolution/bitrate selection and fallback |
| Capture source | which source is admissible for a session |
| Controller | policy frames, proposals, consent |
| Diagnostics/UI | receipts and “why is this limited?” explanations |

## 5. Worked examples

1. **USB2 1440p60 requested.**
   Physiology allows 4K/60; `forced.usb2` caps 1080p30. Effective 1080p30,
   residual width/height/fps, limiting frame `usb2`, recourse `escalate`.
   The UI says: “USB 2.0 bandwidth; use USB 3 or lower the mode.”

2. **User chose 4K on a 1080p panel.**
   The `chosen.preferred` frame caps 4K; residual, recourse `relax`; the
   controller may widen its own frame, nothing else changes.

3. **Host and node negotiate H.264 1080p60, but the host driver only has MJPEG.**
   The host offers MJPEG; the negotiated frame becomes MJPEG. Unilateral node
   acceptance is a no-op until the host accepts; effective aperture follows.

4. **Privacy denies display capture.**
   `forced.privacy` has an empty display aperture; residual is `escalate`, not a
   silent fallback to a camera. The node must not manufacture a different
   source to satisfy the request.

## 6. Ocelot inspiration

Borrow:

- **Aperture as a frame with provenance.** Not a single authored capability
  list; chosen/forced/negotiated with different recourse.
- **The effective aperture is a meet.** Declare physiology; frames derive the
  rest.
- **The same residual, different recourse.** Testable behavior, not just a
  numeric cap.
- **Data, not branches.** Policy/capability as versioned manifests. The
  decision is a fold, not an `if` ladder.
- **Receipts/evidence.** Every decision leaves an auditable record with
  provenance.
- **Measurement discipline.** Pre-register throughput/latency/quality gates,
  record engine/build/hardware provenance, avoid tuned thresholds that
  manufacture a pass.
- **Thin adapters.** The host driver, USB gadget, and network transports are
  adapters; policy stays in one core.

Do not borrow:

- The cognitive engine, graph runtime, seeds, or substrate.
- Any Ocelot runtime dependency or data format wholesale.

## 7. Tests and gates

- `meet` is commutative, associative, idempotent, and order-independent
  (property tests).
- A chosen/forced/negotiated frame producing the **same residual** yields
  different recourse.
- A negotiated frame is adopted/retracted only when every bound party accepts;
  unilateral accept is a no-op.
- An empty aperture returns `aperture.empty` with the conflicting frames, never
  a panic.
- Receipts are stable across runs and include the limiting frame and policy
  version.
- Integration gate: the USB display negotiation uses the aperture for every
  mode/format decision; no hardcoded resolution/codec branch remains in the USB
  path.
- Policy changes are versioned; a controller can add a dimension without
  breaking older nodes.

## 8. Rollout

1. `ozma-aperture` crate + policy loader + receipts + tests.
2. Adopt in USB display/UVC/raw negotiation (Phase A of
   [`usb-video-plan.md`](usb-video-plan.md)).
3. Adopt in network transport codec selection.
4. Adopt in capture-source selection and controller policy.
5. Publish receipts in diagnostics and the UI.

## 9. Non-goals

- DisplayLink compatibility (see the USB plan).
- A general-purpose policy engine or graph database.
- Encoding/decoding decisions that belong to the encoder registry; the aperture
  *selects* among real capabilities, it does not invent one.
