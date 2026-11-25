# Ozma browser display adapter

A dependency-free browser client for the Ozma **protobuf Display protocol** on
the controller's native screen WebSocket server (port `7391`).

The controller sends a JSON registration acknowledgement and then binary
Display frames over the same socket:

```text
+---------+----------------------------+
| tag (1) | protobuf payload (<=8 MiB) |
+---------+----------------------------+
```

This adapter implements the subset needed to display a definition, stream
typed values, and invoke actions. It has no npm dependencies and no build step.

For non-browser devices (Linux SBC/e-ink, ESP32, Stream Deck, MIDI/OSC, RGB,
and native desktop), see
[`docs/display-device-adapters.md`](../../docs/display-device-adapters.md) and
the reusable Python client in [`controller/display_client.py`](../../controller/display_client.py).

## Files

| File | Purpose |
|---|---|
| `index.html` | Minimal display page with server address, status, fields, and log. |
| `client.js` | WebSocket registration, rendering, data binding, actions, reconnect. |
| `protocol.js` | Pure ES-module Display frame/protobuf codec. No DOM access. |
| `style.css` | Responsive dark display styling. |
| `protocol.test.mjs` | `node:test` codec tests with handcrafted frames. |
| `package.json` | Marks the directory as an ES module package for Node tests. |

## Run the controller screen server

The native screen WebSocket server starts with the controller. From the
repository root:

```bash
python3 controller/main.py --virtual-only
```

or, if you normally run the controller from its own directory:

```bash
cd controller
python3 main.py
```

The screen server listens on `0.0.0.0:7391` and logs:

```text
Screen WebSocket server listening on port 7391
```

The controller needs its Python requirements installed, including the generated
protobuf bindings under `controller/proto_gen`. If those are missing, run
`scripts/gen_proto_python.sh` from the repository root first.

## Serve and open the adapter

Browsers block ES modules loaded from `file://`, so serve the directory over
HTTP:

```bash
cd web/display
python3 -m http.server 8080
```

Open <http://localhost:8080/>. The page auto-connects to
`ws://localhost:7391` and fetches a definition for its default layout.

Use the **Server** field or a URL parameter to point at a different controller:

```text
http://localhost:8080/?server=ws://192.168.1.20:7391
http://localhost:8080/?server=wss://controller.example:7391
```

The last address is remembered in `localStorage`. Add `autoconnect=0` to load
the page without connecting immediately:

```text
http://localhost:8080/?autoconnect=0
```

## What the adapter supports

### Protocol

- `[tag][protobuf]` Display framing and the standard tag constants
  (`definition=1`, `data_update=5`, `action_invoke=6`, `interaction_event=7`,
  and the other known tags).
- **DisplayDefinition (tag 1)**: extracts `id`, `version`, `hash`,
  `definition_json` (field 4), and parses the definition JSON. Unknown fields
  are skipped. A `definition_zstd` payload is flagged but not decompressed.
- **DataUpdate (tag 5)**: `definition_version`, repeated `FieldValue`, optional
  `sequence` and `timestamp_micros`.
- **FieldValue**: sint64 via zigzag, uint64, double (little-endian), bool,
  UTF-8 string, enum, bytes, and signed int64 timestamp.
- **ActionInvoke (tag 6)**: encodes `action_id`, JSON UTF-8 `params`, and
  `idempotency_key`; decodes the same fields plus optional `params_json` and
  timestamp.
- **InteractionEvent (tag 7)**: encodes pointer and semantic events, and can
  embed an ActionInvoke. The browser UI itself sends ActionInvoke frames; the
  interaction helpers are exported for future input adapters.
- Unknown display tags and unknown protobuf fields are ignored. Truncated
  varints, overlong values, bad keys, and length overruns throw explicit
  protocol errors.
- 64-bit values that exceed JavaScript's safe integer range are returned as
  `BigInt`.

### Rendering and UI

- Generic layout containers from `layout.children`, `widgets`, or nested
  `children` (`column`, `row`, `grid`, `stack`, etc.).
- `text` / `label`, `number`, `gauge`, `bar`, `vu_meter`, `icon`, `toggle`,
  `switch`, `scenario`, and `clock`-style cards. Unknown widget types render a
  readable fallback card.
- `vu_meter` uses real meter ballistics when `widgets/vu.js` is present:
  attack/release smoothing, dB-per-second falloff, peak hold/decay, clip latch,
  RMS window, and mono/stereo channels. Add `input: "linear"` for 0..1 sources;
  the default input is dBFS (`min`/`max` default to -60/0 dB).
- Field bindings by numeric `field`/`field_id` or metric key
  (`metric`, `binding`, `data_key`, `metric_key`).
- `{variable}` interpolation in labels/text using current field values.
- A fallback table that lists every field in the definition and updates its
  value live. Data for field IDs not in the table is appended as it arrives.
- Clicking (or pressing Enter/Space on) a widget with an `action` reference or
  `action_id` sends an ActionInvoke with the widget's `params` object and a
  monotonic idempotency key.
- Connection status, frame/update counters, timestamped log, and exponential
  reconnect backoff (1s to 30s, with jitter).

## What is not supported

- Faithful pixel layout: widget `x`/`y`/`w`/`h` coordinates are not honoured.
  The adapter uses a responsive card grid so arbitrary definitions remain
  readable. It is a diagnostic/browser display, not a replacement for a native
  renderer.
- Compressed definitions (`definition_zstd` only). The controller normally
  includes `definition_json`.
- Full sparkline history, image packs, fonts, themes, and vendor widget-pack
  JavaScript. These are rendered as generic cards or skipped.
- Scene snapshots, scene commits, definition refs/patches, and legacy JSON
  layout/data frames. Reconnect with `"protocol": "protobuf"` to receive the
  binary Display channel.
- Keyboard interaction events (`InteractionEvent.key`, field 5) are skipped by
  the decoder; pointer and semantic events are decoded.

## Registration frame

On connect the page sends:

```json
{
  "type": "register",
  "device_id": "browser-xxxxxxxx",
  "name": "Browser Display",
  "width": 1280,
  "height": 720,
  "protocol": "protobuf",
  "wire_format": "protobuf",
  "capabilities": ["label", "text", "number", "gauge", "bar", "vu_meter", "icon", "protobuf"]
}
```

The controller replies with a JSON `registered` acknowledgement and then sends
binary Display frames. Any binary ActionInvoke the page sends is decoded by the
existing controller `NativeRenderProtobufDriver`.

## Tests

```bash
node --test web/display/protocol.test.mjs
```

The tests build frames by hand (independent of the implementation) and cover:

- varint boundaries, truncation, and overlong values;
- zigzag edge cases including 32-bit and 64-bit extremes;
- DataUpdate values of every `FieldValue` type, sequence, and timestamp;
- DisplayDefinition JSON, id/version/hash, unknown fields, and invalid JSON;
- ActionInvoke encode/decode round-trip;
- pointer/semantic InteractionEvent encoding and decoding;
- unknown tags, unknown fields, groups, truncation, zero keys, and invalid wire
  types.

Syntax checks:

```bash
node --check web/display/protocol.js
node --check web/display/client.js
node --check web/display/protocol.test.mjs
```
