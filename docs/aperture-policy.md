# Aperture policy for Ozma capability and security negotiation

Status: **plan**; rollout step 1 (`ozma-aperture` crate, policy loader,
receipts, and tests) is implemented. USB, transport, capture, and auth adoption
remain planned.
Inspiration: Ocelot's `observer_aperture` and `aperture_frame` seeds. We borrow
the *model*, not the cognitive engine or its graph runtime.

## 1. Purpose

Ozma negotiates capabilities in several places: USB display modes, UVC formats,
the raw USB link, QUIC/WebTransport/RTSP codecs, capture source selection, and
bandwidth/thermal limits. It also negotiates **security**: permissions, consent,
grants, pairing, and revocation. Today these are spread across ad-hoc flags and
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
| `permissions` | a `PermissionSet` |
| `auth_strength` | `none`, `password`, `ssh-key`, `passkey`, `local` |
| `trust` | `local`, `lan`, `wan`, `relay`, `untrusted` |
| `consent` | `none`, `required`, `granted`, `denied` |
| `zone` | `loopback`, `lan`, `vpn`, `public` |

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

## 3. Security policy (the same model)

Authentication answers **who**; the aperture answers **what may they do now, and
why not more**. It does not replace `ozma-auth` — it is the authorization fold
over the facts `ozma-auth` already produces.

### 3.1 Frames

- **Physiology** — the principal's account/role maximum permission set, and the
  maximum a token could ever carry. A token is already an attenuated frame; the
  physiology is the ceiling.
- **Forced** — machine class (`workstation`, `server`, `kiosk`, `camera`),
  local/node policy, legal or regulatory constraints, network zone, session
  state (`not_ready`, `ended`, `revoked`), token expiry/revocation, device
  capability, privacy/consent requirement, admin lockout. The subject cannot
  lift these.
- **Chosen** — the controller/user/session request: `view`, `control`, `audio`,
  `mic`, `clipboard_read/write`, `file_transfer`, `usb`, `power`, `commands`,
  `display_attach`, `admin`. Optional permissions can be dropped unilaterally.
- **Negotiated** — controller grant ↔ node grant, local consent approval,
  pairing (short code, QR, SSH challenge), multi-party approval. Adoption and
  retraction require **every** bound party; a unilateral accept is a no-op.

### 3.2 Meet

```text
effective = account_ceiling
          ∩ token_scopes
          ∩ session_required
          ∩ machine_policy
          ∩ consent
          ∩ controller_grant
          ∩ node_grant
          ∩ revocation
```

`PermissionSet` in `ozma-protocol` already provides intersection, subset, and
`missing_from`; the aperture uses those directly. Adding a frame can only narrow
the result. An empty intersection is a valid, fail-closed result.

### 3.3 Residual and recourse

| Provenance | Example | Recourse |
|---|---|---|
| `chosen` | user asked for clipboard, policy allows view only | `relax`: drop clipboard, continue view-only |
| `forced` | workstation consent required; token revoked; node policy | `escalate`: consent prompt, local approval, or deny; never silently grant |
| `negotiated` | grant/pairing not yet agreed | `propose`: issue/accept a grant or pairing; adopt only when all parties accept |

The same missing permission has different recourse. A denied `control` because
the *user* didn't request it is `relax`; because *machine policy* forbids it is
`escalate`; because a *grant* is pending is `propose`.

### 3.4 Receipts = audit record

Every authorization decision produces a redacted, tamper-evident receipt:

```json
{
  "principal": "alice",
  "client_id": "web-1",
  "session_id": "sess-…",
  "requested": ["view", "control", "audio"],
  "effective": ["view"],
  "missing": ["control", "audio"],
  "limiting": [
    {"dimension": "control", "frame": "workstation-consent", "provenance": "forced"},
    {"dimension": "audio",   "frame": "session-spec",          "provenance": "chosen"}
  ],
  "recourse": {"control": "escalate", "audio": "relax"},
  "token_claims_hash": "…",
  "policy_version": "workstation-v3",
  "consent": "pending",
  "at_micros": 0
}
```

Receipts are logged, exposed through diagnostics (with secrets removed), and
kept with the audit log. They answer “why was this denied?” without exposing
tokens, passwords, or pairing codes.

### 3.5 Invariants

1. **Monotonicity:** adding a frame never widens the effective aperture or
   permission set. A derived token/grant is always a subset.
2. **No subject relaxation of forced frames:** only an authorized policy change
   or local consent can lift them.
3. **All-party negotiation:** adoption/retraction of a negotiated frame requires
   every bound party; a unilateral accept does nothing.
4. **Fail closed:** unknown dimension, missing policy, expired/revoked token,
   empty intersection, or malformed receipt → deny.
5. **Authentication ≠ authorization:** the aperture never authenticates; it
   consumes verified claims from `ozma-auth`.
6. **No policy from untrusted input:** policy manifests are signed/versioned;
   requests are data, never code.
7. **Atomic revocation:** a revocation frame takes effect immediately for
   in-flight sessions; no queued operation may widen after it.
8. **Receipts are evidence, not secrets:** tokens, passwords, pairing codes,
   and raw claims are hashed or redacted.

### 3.6 Mapping to existing Ozma

| Existing | Aperture role |
|---|---|
| `PermissionSet` | the meet operation and the result |
| `TokenClaims` scopes | an attenuated chosen frame |
| `ConsentPolicy` / `MachineClass` | forced frames |
| `GrantStore` | negotiated frames with expiry/revocation |
| `LocalAuthenticator` (PAM, SSH) | authentication; produces the principal/physiology |
| `PairingStore` / `SshChallengeStore` | negotiated frames with expiry, single use, attempts |
| Node API bearer token | transport authentication, not authorization |
| WebTransport certificate pinning | transport authentication, not authorization |
| Session spec (`audio.mic`, input modes) | chosen/forced session frames |

### 3.7 Security examples

1. **Control on a workstation.** Controller requests `view+control`; machine
   policy requires consent. Effective `view`; residual `control`, forced
   `workstation-consent`, recourse `escalate`. Local approval adds a negotiated
   consent frame accepted by node and user; effective `view+control`. Denial
   leaves `view` only — never a silent grant.
2. **Admin token, restricted node.** Token carries `admin`; node policy caps
   `view`. Effective `view`; residual `admin`, forced `node-policy`; no
   escalation path for a remote requester. Receipt explains it.
3. **Mic not negotiated.** Browser asks for mic but the session was created
   without `audio.mic`. Effective no mic; residual forced `session-spec`; recourse
   `propose` (recreate the session with mic), not a silent fallback.
4. **Pairing.** A short-code/QR/SSH proposal binds node, controller, and user.
   It is adopted only when all accept, expires quickly, is single-use, and is
   attempt-limited. Unilateral acceptance is a no-op.
5. **Revocation.** A forced revocation frame empties the aperture; the session
   drops input/media immediately and emits a receipt.

### 3.8 API shape

```rust
pub struct SecurityRequest {
    pub permissions: PermissionSet,
    pub action: SecurityAction,
}

pub fn authorize(
    principal: &VerifiedPrincipal,
    request: &SecurityRequest,
    frames: &[Frame],          // chosen, forced, negotiated
) -> Admission;                // effective + receipt, or Residual + recourse
```

The node calls this before every session operation that changes permissions or
accepts media/input; the session manager applies the effective set.

## 4. Policy data

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

## 5. Integration

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
| Session/auth | permission meet, consent, grants, pairing, revocation |
| Node API | authorize before session operations and media/input acceptance |
| Diagnostics/UI | receipts and “why is this limited/denied?” explanations |

## 6. Worked examples

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

## 7. Ocelot inspiration

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

## 8. Tests and gates

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
- Security: attenuation is monotonic (adding a frame never widens); forced
  frames cannot be relaxed by the subject; negotiated adoption/retraction needs
  every party; unknown dimension/empty meet/expired/revoked fails closed;
  permission receipts redact secrets; revocation is immediate for in-flight
  sessions.
- Policy changes are versioned; a controller can add a dimension without
  breaking older nodes.

## 9. Rollout

1. `ozma-aperture` crate + policy loader + receipts + tests.
2. Adopt in USB display/UVC/raw negotiation (Phase A of
   [`usb-video-plan.md`](usb-video-plan.md)).
3. Adopt in network transport codec selection.
4. Adopt in capture-source selection and controller policy.
5. Adopt in `ozma-auth`/session authorization: token scopes, machine policy,
   consent, grants, pairing, revocation.
6. Publish receipts in diagnostics, the audit log, and the UI.

## 10. Non-goals

- DisplayLink compatibility (see the USB plan).
- A general-purpose policy engine or graph database.
- Encoding/decoding decisions that belong to the encoder registry; the aperture
  *selects* among real capabilities, it does not invent one.
- Authentication itself. PAM/SSH/pairing/token verification stay in
  `ozma-auth`; the aperture only folds verified claims into an effective policy.
- Executing policy code or accepting unsigned manifests.
