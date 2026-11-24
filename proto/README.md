# Ozma protocol proto schema

`proto/` is the source of truth for the Ozma session protocol. Rust bindings
are generated at build time by the `ozma-protocol-proto` crate; other language
bindings are generated from the same files.

## Layout

```text
proto/ozma/v1/
  version.proto    # ProtocolVersion, WireFormat
  display.proto    # RenderingTier, DisplayMethod, DataSchema, capabilities
  scene.proto      # SceneSnapshot, SceneCommit, SceneNode, SceneOp
  data.proto       # DisplayDefinition, FieldDef, DataUpdate, FieldValue
  action.proto     # ActionDef, ActionInvoke, InteractionEvent
  control.proto    # Hello, HelloAck, Session, ApiError, ControlMessage
  transport.proto  # TransportMode, ChannelKind, ChannelDescriptor
```

All files use `package ozma.v1` and proto3.

## Conventions

- Field and enum numbers are part of the wire contract. Never reuse a number
  after removing a field or value; reserve it instead.
- Use `snake_case` for fields and `UPPER_SNAKE_CASE` for enum values. The zero
  enum value is always `*_UNSPECIFIED`.
- Use `optional` when field presence is meaningful (for example negotiated
  values and nullable scene structure).
- Use `oneof` for polymorphic content: scene node content, scene operations,
  field values, interaction events, and the control envelope.
- JSON is carried as `bytes` only for display definitions and definition
  patches. Steady-state scene, data, and action traffic is protobuf.
- New channel kinds are appended after existing values so channel identifiers
  do not shift.

## Related crates

- `ozma-protocol-proto` — generated prost and pbjson bindings, plus the embedded
  `FileDescriptorSet`.
- `ozma-display-protocol` — stable tag + protobuf codec for the reliable
  Display channel, domain/protobuf conversions, and the client-side display
  session state machine.
- `ozma-display-definition` — canonical JSON parsing, BLAKE3 hashing, RFC 6902
  patching, and compiled field/action tables.

## Building the Rust bindings

```sh
cargo test -p ozma-protocol-proto
```

`ozma-protocol-proto/build.rs` compiles every `proto/ozma/v1/*.proto` file with
prost-build, writes a `FileDescriptorSet` to `OUT_DIR/ozma_v1_descriptor.bin`,
and generates pbjson serde implementations. The descriptor set is re-exported as
`ozma_protocol_proto::FILE_DESCRIPTOR_SET`.

The build uses `protoc-bin-vendored` unless `PROTOC` is already set in the
environment, so a system `protoc` is not required.

## Linting and compatibility

`buf` is not required to build or test the crate. Install buf 1.75.0 (the
version CI pins) to run the schema checks:

```sh
buf lint
buf format --diff --exit-code
buf breaking --against proto/baseline.bin
```

`buf.yaml` uses the v2 module syntax with `STANDARD` lint and `FILE` breaking
categories. `buf.gen.yaml` is an optional cross-language generation path; see
the comments in that file for prerequisites.

### Compatibility baseline

`proto/baseline.bin` is the committed Buf image (wire-compatible with a
`FileDescriptorSet`) that `buf breaking` compares against. It is generated from
the current schema with:

```sh
buf build --exclude-source-info -o proto/baseline.bin
```

Source info is excluded so the baseline stays small (about 11 KiB) and
deterministic for a given schema and buf version. The `proto` CI job runs lint,
format, and `buf breaking --against proto/baseline.bin` on every push and pull
request, so a wire-incompatible change fails until it is fixed or the baseline
is intentionally advanced.

### Updating the baseline

```sh
scripts/update_proto_baseline.sh
```

The script runs `buf lint`, regenerates `proto/baseline.bin` from the repository
root, and prints its SHA-256. Run it only for an intentional, wire-compatible
release baseline: regenerating the file accepts the current schema as the new
compatibility floor and discards the previous one. Within a major protocol
version, prefer additive changes (new fields/messages; reserve removed field
numbers and names). Reset the baseline only when a major version intentionally
changes the wire contract, and note that in the commit message.
