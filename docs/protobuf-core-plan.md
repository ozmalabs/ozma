# Protobuf-core protocol and display-method implementation plan

**Status:** Approved direction; Phase 1 is being implemented.

## 1. Decision

The Ozma protocol contract is **protobuf-first**:

- Session negotiation, capabilities, transport, display methods, scene state,
  input, actions, consent, aperture policy, diagnostics, and audit metadata are
  protobuf messages.
- JSON is retained in exactly two places:
  1. The canonical **display definition document** (schema, widget/layout tree,
     action catalog, extension field table), carried as a `bytes` payload in a
     protobuf envelope and sent on first use or when it changes.
  2. The **REST/GraphQL compatibility binding**, generated from the protobuf
     schema with `pbjson` (or an equivalent) rather than maintained as a second
     hand-written model.
- Compact steady-state display traffic is protobuf. Data updates, scene
  operations, input, and action invocations never repeat JSON keys.
- There must not be two independent protocol implementations. The `.proto`
  files are the source of truth; Rust, Python, TypeScript, and device SDK types
  are generated from them.

## 2. Why this split

JSON is excellent for a definition document: readable, diffable, schema-
validatable, easy for widget-pack authors, and easy to patch. It is poor for
per-frame or per-value traffic because it repeats field names, numbers, and
framing.

Protobuf is the right fit for the core because it provides:

- stable field numbers and unknown-field skipping;
- compact varint/zigzag encoding;
- oneofs for polymorphic scene and action content;
- code generation across Rust, Python, TypeScript, C/C++, Kotlin, and Swift;
- `optional` presence semantics for negotiated values;
- a mature lint/breaking-change story through `buf`.

The protocol version scheme stays `major.minor`. A new major may break the
wire contract; a new minor adds messages or fields and must remain readable by
older decoders.

## 3. Layer model

| Layer | Format | Sent when |
|---|---|---|
| Handshake, version, session, transport, capabilities | Protobuf | Every connection |
| Display method advertisement and selection | Protobuf | Connection setup and device change |
| Scene commits and node operations | Protobuf | Every revision |
| Typed data updates | Protobuf | Change, rate-limited/deadbanded |
| Input events | Protobuf | Every event |
| Action descriptors | Protobuf envelope containing JSON schema | Definition time |
| Action invocations | Protobuf | Every action |
| Widget/layout/data definition | JSON inside `DisplayDefinition.definition_json` | First use or definition change |
| Definition patches | JSON Patch / Merge Patch inside `DefinitionPatch` | Structural change |
| Resources (images, fonts, packs) | Binary blobs on a reliable bulk/display channel | Cache miss |
| REST/GraphQL | JSON mapping of protobuf messages | API boundary |

The scene graph and display definitions are logically separate:

- **Definition**: what a device can render and what controls exist.
- **Scene**: what is currently presented.
- **State**: changed data values and node properties.
- **Interaction**: pointer/touch/key and semantic action events.

## 4. Proto package layout

```text
proto/ozma/v1/
  version.proto        # ProtocolVersion, WireFormat
  ids.proto            # SessionId, NodeId, DisplayId, TrackId
  error.proto          # ApiError, ErrorCode
  capability.proto     # NodeCapabilities, capture/encode/audio/input/display
  transport.proto      # TransportMode, ChannelDescriptor, SessionOffer
  session.proto        # SessionSpec, SessionState, consent, privacy
  display.proto        # DisplayMethod, DataSchema, RenderingTier
  scene.proto          # SceneSnapshot, SceneCommit, SceneNode, SceneOp
  data.proto           # DisplayDefinition, FieldDef, DataUpdate, FieldValue
  action.proto         # ActionDef, ActionInvoke, InteractionEvent
  input.proto          # InputEvent, pointer/touch/pen
  control.proto        # Hello, HelloAck, Event, Error, Bye, Ping
  aperture.proto       # capability/provenance/consent frames
  diagnostics.proto    # checks and runtime metrics
```

`buf.yaml` and `buf.gen.yaml` live at the repository root. CI runs
`buf lint` and `buf breaking` against the previous release baseline.

## 5. Core negotiation messages

```proto
syntax = "proto3";
package ozma.v1;

enum WireFormat {
  WIRE_FORMAT_UNSPECIFIED = 0;
  WIRE_FORMAT_PROTOBUF = 1;
  WIRE_FORMAT_JSON = 2; // compatibility and debug only
}

message Hello {
  string session_id = 1;
  string token = 2;
  ProtocolVersion protocol = 3;
  repeated WireFormat wire_formats = 4;
  DisplayCapabilities display = 5;
}

message HelloAck {
  Session session = 1;
  WireFormat wire_format = 2;
  DisplayProfile display = 3;
}
```

`HelloAck.wire_format` selects the encoding for the rest of the connection.
Native QUIC/WebTransport sends protobuf payloads directly. The JSON option
exists only for legacy clients and diagnostics.

## 6. Display methods and tiers

```proto
enum RenderingTier {
  RENDERING_TIER_UNSPECIFIED = 0;
  RENDERING_TIER_PIXEL_SINK = 1;
  RENDERING_TIER_RENDERED_UI = 2;
  RENDERING_TIER_NATIVE_WIDGETS = 3;
  RENDERING_TIER_DATA_DRIVEN = 4;
}

message DisplayMethod {
  string id = 1;
  repeated string accepts = 2;
  repeated string schemas = 3;
  MethodLocation location = 4;
  repeated Format outputs = 5;
  repeated Interaction interactions = 6;
  optional string configuration_schema = 7;
}

message DisplayCapabilities {
  repeated uint32 scene_versions = 1;
  repeated DisplayMethod methods = 2;
  repeated RenderingTier tiers = 3;
  repeated string schemas = 4; // schema ids; definitions travel separately
  optional uint32 max_update_rate_hz = 5;
  uint64 resource_budget_bytes = 6;
}
```

A tier is a hint. The advertised `DisplayMethod` is authoritative: a device
may support several tiers, and a tier number never implies support for a
particular widget, schema, or interaction.

## 7. Dynamic definitions, field tables, and compact updates

Runtime-extensible fields must not force dynamic `.proto` generation. The
definition is JSON; its compiled field table is protobuf.

```proto
message DefinitionRef {
  string id = 1;
  uint32 version = 2;
  bytes hash = 3;              // proves the client already has the document
}

message DisplayDefinition {
  string id = 1;
  uint32 version = 2;
  bytes hash = 3;              // BLAKE3 of canonical JSON
  bytes definition_json = 4;   // schema, layout, actions, extension metadata
  bytes definition_zstd = 5;   // optional compressed form
  repeated FieldDef fields = 6;
  repeated ActionDef actions = 7;
}

enum ValueType {
  VALUE_TYPE_UNSPECIFIED = 0;
  VALUE_TYPE_SINT = 1;
  VALUE_TYPE_UINT = 2;
  VALUE_TYPE_REAL = 3;
  VALUE_TYPE_BOOL = 4;
  VALUE_TYPE_STRING = 5;
  VALUE_TYPE_ENUM = 6;
  VALUE_TYPE_BYTES = 7;
  VALUE_TYPE_TIMESTAMP = 8;
}

message FieldDef {
  uint32 id = 1;               // stable for the definition lifetime; never reused
  string key = 2;              // namespaced, e.g. "org.ozma/node.cpu_temp"
  ValueType type = 3;
  optional string unit = 4;
  optional sint32 scale = 5;   // 100 => hundredths
}

message FieldValue {
  uint32 field_id = 1;
  oneof value {
    sint64 integer = 2;
    uint64 unsigned = 3;
    double real = 4;
    bool flag = 5;
    string text = 6;
    int32 enum_value = 7;
    bytes blob = 8;
    int64 timestamp_micros = 9;
  }
}

message DataUpdate {
  uint32 definition_version = 1;
  repeated FieldValue values = 2; // only changed fields
}
```

Rules:

- Field IDs are declared in the JSON definition, assigned once, and never
  reused. Do not derive IDs from sorted key order.
- Values are sent only when changed, subject to `max_update_rate_hz` and a
  schema-declared deadband.
- Use scaled integers and enums where possible; avoid floats for fixed
  physical units.
- Strings that are labels or enum text belong in the definition, not in each
  update.
- The controller may send `DisplayDefinition` again when a device reconnects
  cold, when the schema changes, or when a definition patch cannot be applied.
- On a definition change, send `DefinitionPatch` (JSON Patch) when possible;
  otherwise send a full `DisplayDefinition`. Updates carry the new
  `definition_version`.

## 8. Scene and action messages

```proto
message SceneCommit {
  uint64 revision = 1;
  repeated SceneOp operations = 2;
}

message SceneOp {
  uint64 node_id = 1;
  oneof operation {
    SceneNode upsert = 2;
    NodeMove move = 3;
    bool remove = 4;
  }
}

message SceneNode {
  uint64 id = 1;
  optional uint64 parent = 2;
  Transform transform = 3;
  optional Rect clip = 4;
  int32 z = 5;
  oneof content {
    VideoSurface video = 6;
    ResourceRef image = 7;
    TextContent text = 8;
    WidgetNode widget = 9;
    DataBinding data = 10;
  }
  optional InteractionTarget interaction = 11;
}

message ActionDef {
  uint32 id = 1;
  string key = 2;                    // "org.ozma.scenario.activate"
  bytes params_schema_json = 3;
  repeated string permissions = 4;
  bool requires_confirmation = 5;
}

message ActionInvoke {
  uint32 action_id = 1;
  bytes params = 2;                  // typed message or JSON/CBOR extension
  uint64 idempotency_key = 3;
}
```

Use `oneof` for polymorphic content and `optional` for presence. Hot-path
node references use `uint64`; human-facing session/node identifiers may remain
strings.

## 9. Transport and channel changes

- Keep `CONTROL` for session lifecycle and negotiation.
- Add `ChannelKind::Display` for definitions, definition patches, scene
  commits, data updates, and action descriptors/invocations.
- Keep resources on `BULK` (or a later dedicated resource channel).
- Append the new channel after existing channels in
  `ChannelMap::for_tracks` so old channel IDs do not shift.
- `Display` is reliable and ordered. Data updates are coalesced at the
  application layer, not by the transport.
- Scene video continues on the existing media channels.

## 10. Compatibility and migration phases

### Phase 0 — schema foundation

- Add `proto/` and `buf.yaml`/`buf.gen.yaml`.
- Add the `ozma-protocol-proto` crate with `prost-build` and `pbjson-build`.
- Generate Rust types and JSON mappings.
- Add round-trip tests and `buf lint`.

### Phase 1 — negotiation and display domain

- Add `WireFormat` negotiation to `Hello`/`HelloAck`.
- Add `DisplayMethod`, `DataSchema`, `RenderingTier`, `Interaction`.
- Add `DisplayDefinition`, `FieldDef`, `DataUpdate`, `FieldValue`.
- Add scene node operations and action descriptors/invocations.
- Add `ChannelKind::Display`.
- Keep JSON v1 compatibility.

### Phase 2 — native transport

- Switch `ozma-node` and `ozma-client-core` native sessions to protobuf when
  both peers select it.
- Keep the JSON control path behind `WIRE_FORMAT_JSON`.
- Add golden vectors for both encodings.

### Phase 3 — definition and resolver

- Implement the definition registry, field-table assignment, JSON Schema
  validation, JSON Patch application, and method resolver.
- Implement a reference device/adapter that consumes a definition once and then
  binary updates.

### Phase 4 — controller and adapters

- Generate Python types (or use `betterproto`).
- Update `controller/screen_server.py`, `screen_manager.py`, and
  `screen_widgets.py` to negotiate definitions and send binary updates.
- Update web, embedded, Stream Deck, MIDI/OSC, e-ink, and RGB adapters to
  advertise methods and interactions.

### Phase 5 — remove duplicate JSON wire

- Make protobuf canonical for all session traffic.
- Retain JSON only for definitions, REST/GraphQL bindings, and explicit debug
  output.
- Keep a legacy JSON compatibility window documented in `docs/protocols.md`.

## 11. Work packages for this execution

### Package A — proto schema and generated crate

Files:

- `proto/ozma/v1/*.proto`
- `proto/README.md`
- `buf.yaml`, `buf.gen.yaml`
- `ozma-protocol-proto/Cargo.toml`, `build.rs`, `src/lib.rs`, tests
- root `Cargo.toml` workspace member/dependencies

Acceptance:

- `cargo test -p ozma-protocol-proto` passes.
- Generated types cover version, wire format, display methods, scene, data,
  action, and the core control messages.
- A round-trip test encodes and decodes `Hello`, `DisplayDefinition`,
  `DataUpdate`, `SceneCommit`, and `ActionInvoke`.
- `buf lint` passes when `buf` is installed; document the command.

### Package B — display domain model and resolver

Files:

- `ozma-protocol/src/display_method.rs`
- `ozma-protocol/src/lib.rs`
- `ozma-protocol/src/capability.rs` (only if needed)
- `ozma-protocol/Cargo.toml`
- tests in the same crate

Acceptance:

- `DisplayMethod`, `RenderingTier`, `DataSchema`, `Interaction`, `FieldDef`,
  `ActionDef`, and `InteractionTarget` exist with serde derives.
- `MethodRegistry` and `resolve` match content type, schema, and required
  interactions.
- A tier alone never satisfies a method requirement.
- Unit tests cover exact match, schema-only match, unsupported interaction,
  and a renderer-service fallback.

### Package C — transport negotiation and display channel

Files:

- `ozma-protocol/src/transport.rs`
- `ozma-transport/src/channel.rs`
- adjacent tests

Acceptance:

- `WireFormat` is negotiated in transport/session vocabulary.
- `ChannelKind::Display` exists.
- `ChannelMap` appends Display after existing channels so old IDs do not move.
- Tests cover default map stability, lookup by kind, and explicit negotiation of
  protobuf vs JSON.

### Package D — documentation

Files:

- `docs/protocols.md`
- `docs/remote-scene-integration.md`
- `docs/protobuf-core-plan.md` (this file)

Acceptance:

- Documents state that protobuf is canonical and JSON is a definition/REST
  binding.
- Migration and compatibility rules are explicit.

## 12. Testing and acceptance

- Protobuf round-trip and golden-vector tests for every core message.
- Unknown-field skip tests: an old decoder reads a newer message without
  failing.
- JSON definition fixture tests: the same definition produces the same field
  table and update values in Rust and Python.
- Definition cache tests: cold connect sends JSON, warm reconnect sends only a
  `DefinitionRef`, definition change sends a patch, incompatible change
  resends the full definition.
- Resolver tests: content/schema/interaction matching and renderer fallback.
- Channel-map stability tests: existing channel IDs are unchanged.
- Legacy JSON tests remain green until Phase 5.
- `buf breaking` in CI once the first release baseline exists.

## 13. Risks and decisions

- **`prost` unknown fields:** `prost` skips unknown fields when decoding but
  does not preserve them by default. Endpoints may drop them; relays that must
  forward unknown extensions must keep the raw frame or use `prost-reflect`.
  Do not promise transparent forwarding without one of those.
- **Generated vs hand-written types:** Phase 1 may keep both temporarily, but
  the target is generated types as canonical plus `pbjson` for JSON. Do not
  let two models drift indefinitely.
- **`protoc` availability:** use `protoc-bin-vendored` or document a pinned
  `protoc`/`buf` version so builds are reproducible.
- **Field-number discipline:** reserve removed fields and names; `buf breaking`
  must gate merges.
- **JSON definition size:** compress the definition blob with `zstd` once, not
  every update; constrained devices may receive a precompiled binary
  definition.
- **Security:** definitions and widget packs are untrusted input. Validate
  schema depth/size, sign packs, and sandbox executable widget code.
- **Performance:** benchmark JSON vs protobuf for a realistic scene and data
  workload before removing the JSON path. The expected win is largest for
  repeated data keys and small updates.

## 14. Implementation status

Completed in this execution:

- `proto/ozma/v1` covers version/wire format, display methods, display
  capabilities, scene graph/operations, definitions/field tables, actions,
  interactions, control negotiation, and transport channels.
- `Hello`/`HelloAck` negotiate a wire format. Both native clients advertise
  protobuf and JSON; the node selects protobuf when available and falls back to
  JSON for legacy peers.
- The node sends scene snapshots as protobuf on the reliable `Display` channel
  when protobuf is selected. `ozma-client` decodes them into the existing scene
  receiver; the JSON control path remains available.
- `ozma-protocol-proto` generates prost and pbjson bindings at build time and
  embeds a `FileDescriptorSet`.
- `ozma-protocol` has the display-method domain model and resolver; rendering
  tiers never satisfy resolution on their own.
- `ozma-transport` allocates a reliable `Display` channel after existing
  channels, and `WireFormat` negotiation prefers protobuf.
- `ozma-display-protocol` provides the stable one-byte-tag protobuf codec for
  definition, patch, scene, data, action, and interaction messages, plus
  domain/protobuf conversions and a `DisplaySession` state machine that applies
  definitions, patches, data updates, and scene revisions atomically.
- `ozma-display-definition` parses and validates canonical JSON, hashes it with
  BLAKE3, applies RFC 6902 patches, compiles field/action tables, converts to
  protobuf, and encodes typed field values.
- `DisplaySession` accepts bounded zstd-compressed definitions and patches,
  prefers the uncompressed payload when both are present, and cross-checks
  non-empty protobuf field/action tables against the parsed JSON document.
- A shared `tests/fixtures/display_definition.json` fixture proves that the Rust
  and Python canonicalizers produce identical bytes and BLAKE3 hashes.
- `controller/display_protocol.py` and `controller/proto_gen` implement the
  Python side: frame codec, canonical hashing, definition construction, typed
  data updates, and inbound action/interaction decoding.
- `ScreenWebSocketServer` accepts an opt-in protobuf registration and keeps
  JSON as the legacy default. `NativeRenderProtobufDriver` sends the definition
  once and then changed-only binary data updates.
- The native `ozma-client` owns a `DisplaySession`, applies definitions,
  patches, data updates, actions, and interactions, exposes
  `ClientEvent::Display`, and can send `ActionInvoke`/`InteractionEvent` on the
  Display channel.
- `web/display/` is a dependency-free browser adapter: protobuf framing,
  definition rendering, live typed values, action invocation, and reconnect.
- `controller/display_client.py` is a reusable async Python device client with
  definition/data callbacks and action/interaction send support; the
  [device adapter guide](display-device-adapters.md) maps it to SBC/e-ink, MCU,
  Stream Deck, MIDI/OSC, and RGB implementations.
- `proto/baseline.bin` is committed and CI runs `buf breaking` against it.
- `docs/protocols.md` and `docs/remote-scene-integration.md` describe protobuf
  as canonical and JSON as the definition/REST binding.

Verification run:

```sh
cargo test -p ozma-protocol-proto -p ozma-display-protocol \
  -p ozma-display-definition -p ozma-protocol -p ozma-transport \
  -p ozma-client-core
cargo check -p ozma-node --tests -p ozma-client -p ozma-web
cargo check --workspace
cargo test -p ozma-node --test quic_session --test scene_reconnect \
  --test client_capture --test webtransport
cargo test -p ozma-client --test quic_session --test cli_connect \
  --test cli_probe
node --test web/display/protocol.test.mjs
python -m pytest tests/unit/controller/test_display_protocol.py \
  tests/unit/controller/test_screen_server_protobuf.py
buf lint
buf format --diff --exit-code
buf breaking --against proto/baseline.bin
```

Remaining work is compatibility cleanup and hardware-specific adapters:

- Keep the legacy JSON control path for the documented compatibility window,
  then remove it once old clients are no longer supported.
- Add dedicated protobuf adapters for embedded firmware, e-ink, Stream Deck,
  MIDI/OSC, and RGB. The controller WebSocket path and `web/display/` are the
  reference implementations; those device classes can consume the same frames.
- Full end-to-end latency and compression benchmarks remain to be measured.
