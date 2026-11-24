# Display device adapters

This guide is for anyone building a device that renders an Ozma display:
a browser kiosk, a Raspberry Pi e-ink panel, an ESP32, a Stream Deck, a MIDI
control surface, an RGB controller, or a native OzmaOS surface.

All of these adapters consume the **same protobuf Display protocol**. They do
not need the legacy JSON layout/data frames, and they do not need a code
generation or gRPC build step for the controller to talk to them. The canonical
design is in [`protobuf-core-plan.md`](protobuf-core-plan.md); the browser
reference implementation is in [`web/display/`](../web/display/README.md).

## The device model

The controller sends one definition and then compact updates:

| Direction | Message | Purpose |
|---|---|---|
| Device → controller | JSON registration with `"protocol": "protobuf"` | Negotiate the wire format. This is the only JSON control message. |
| Controller → device | `DisplayDefinition` (tag 1) | Definition document plus compiled field/action tables. Sent once per definition version. |
| Controller → device | `DataUpdate` (tag 5) | Only changed `FieldValue`s, keyed by numeric field id. |
| Device → controller | `ActionInvoke` (tag 6) | A button, key, or control was activated. |
| Device → controller | `InteractionEvent` (tag 7) | Pointer, key, or semantic input, optionally carrying an embedded action. |
| Controller → device | `DefinitionRef` / `DefinitionPatch` (tags 2/3) | Reconnect/cache negotiation and structural updates. |
| Controller → device | `SceneSnapshot` / `SceneCommit` (tags 8/4) | Full scene state or incremental node operations. |

Binary frames are `[tag][protobuf payload]`, with a one-byte tag and an 8 MiB
payload limit. Unknown tags and unknown protobuf fields must be ignored so a
newer controller can talk to an older device.

The steady-state loop is:

```text
registration (JSON)  ->  DisplayDefinition (binary, once)
                     ->  DataUpdate (binary, changed fields only)
button press         ->  ActionInvoke (binary, stable idempotency key)
```

The definition document is JSON, but it is carried as bytes inside the
`DisplayDefinition` protobuf message. A device parses it once, builds a
`field id -> widget/control` map, and then works only from `DataUpdate` values.

## What every adapter needs to do

1. **Register** with a stable `device_id`, name, logical dimensions, and a
   capabilities list. Set both `"protocol": "protobuf"` and
   `"wire_format": "protobuf"` for compatibility with older controllers.
2. **Cache the definition** by `id`, `version`, and `hash`. Do not re-render the
   layout for every data frame.
3. **Decode `DataUpdate`** using the compiled `FieldDef` table. Field ids are
   stable for the definition lifetime and are never reused.
4. **Coalesce updates** to the device's real refresh rate. E-ink panels and
   Stream Deck keys should not repaint on every frame.
5. **Send actions** as `ActionInvoke` with a monotonically increasing
   `idempotency_key`. Send pointer/key/semantic input as `InteractionEvent` when
   the controller needs position or gesture detail.
6. **Reconnect with bounded exponential backoff** and re-register. Treat a
   `DefinitionRef`/`DefinitionPatch` as a signal that a cached definition may be
   stale; request or wait for a full `DisplayDefinition` when the compiled
   field table is needed.
7. **Never let malformed input kill the renderer.** Ignore empty frames,
   unknown tags, truncated varints, and unknown protobuf fields.

## Python `DisplayClient`

[`controller/display_client.py`](../controller/display_client.py) is a small,
reusable async client built on the `websockets` package and
`controller/display_protocol.py`. It handles registration, framing, callbacks,
latest field values, and reconnection.

It is the quickest starting point for a Linux SBC, an e-ink panel, or any host
that can run Python. It supports both package-relative imports
(`controller.display_client`) and running from the controller directory.

```python
import asyncio

from display_client import DisplayClient


async def main() -> None:
    def on_definition(definition) -> None:
        print(f"definition {definition.id} v{definition.version}")
        for field in definition.fields:
            print(f"  {field.id:>4}  {field.key}  type={field.type}")

    def on_data(values, update) -> None:
        # values is keyed by definition field key, e.g. "node.cpu_temp".
        print("update:", values)

    async def on_action(invoke) -> None:
        # The controller invoked an action on this device.
        print("action:", invoke.action_id)

    client = DisplayClient(
        "ws://controller.local:7391",
        device_id="kitchen-panel",
        name="Kitchen Panel",
        width=800,
        height=480,
        capabilities=["label", "number", "gauge", "protobuf"],
        on_definition=on_definition,
        on_data=on_data,
        on_action=on_action,
    )

    # Run the receive loop in the background, then send an action when a
    # physical button is pressed.
    receive = asyncio.create_task(client.run())
    await client.wait_for_definition(timeout=10)
    await client.send_action(
        action_id=1,
        params={"scenario": "gaming"},
        idempotency_key=1,
    )
    await receive


asyncio.run(main())
```

The client also has a small CLI that prints the definition's fields and live
updates, which is useful while wiring up a new panel:

```bash
python controller/display_client.py \
  --url ws://controller.local:7391 \
  --device-id kitchen-panel \
  --width 800 --height 480 \
  --layout panel-status
```

Dependencies are the controller's existing `websockets`, `protobuf`, and
generated `controller/proto_gen` bindings. If the bindings are missing, run
`scripts/gen_proto_python.sh` from the repository root.

## Device class map

| Device class | Rendering / transport | Practical libraries | Notes |
|---|---|---|---|
| Browser or kiosk | WebSocket, Canvas or DOM | Existing [`web/display`](../web/display/README.md) adapter; no npm dependencies | Reference adapter. Renders generic widgets and lists all definition fields. Reuses the same registration/ack and binary frames. |
| Linux SBC or e-ink | Python over WebSocket; Pillow drawing | `DisplayClient`, `Pillow`, `epd-waveshare`; alternatively Rust `epd-waveshare` | Render only on changed fields and at the panel's refresh rate. Prefer full refreshes for big changes and partial refreshes for numbers. Keep a framebuffer cache to avoid flicker. |
| ESP32 / MCU | WebSocket or a small TLS TCP link; on-device drawing | `nanopb` or `protobuf-c`; `LVGL`, `Slint`, or `embedded-graphics` | Use the generated `.proto` sources or a tiny TLV decoder for the subset you need. Keep the definition document in flash, field values in RAM. `postcard` is optional for a separate MCU-to-peripheral link, not for the controller wire protocol. |
| Stream Deck | USB HID; per-key images | `elgato-streamdeck` (Python) or `hidapi` | Render key images from definition actions/fields. Map each key to an action id and send `ActionInvoke`. A definition change means re-rendering the key layout, not the whole protocol. |
| MIDI / OSC control surface | MIDI or UDP; faders, knobs, pads | `mido` + `python-rtmidi`, or `python-osc` | Map CC/note/OSC addresses to definition actions. Send `ActionInvoke` for presses; use `InteractionEvent` when the surface reports continuous pointer-like values. Debounce and use stable idempotency keys. |
| RGB lighting | WLED JSON API, DDP, Art-Net, or sACN | `httpx`/`aiohttp` for WLED, a UDP socket for DDP/Art-Net/sACN | Treat numeric/bool/enum fields as color, brightness, effect, or zone inputs. Map a palette in the adapter; do not put RGB-specific fields in the shared protocol. |
| Native desktop / OzmaOS | Native window or compositor surface | `ozma-client`/`DisplaySession`; Smithay and `wgpu` where applicable | Use the native session API and scene graph for composition. Fall back to the WebSocket Display channel for devices that only need the definition and data stream. |

### Browser and kiosk

The existing [`web/display`](../web/display/README.md) adapter is the reference
consumer. It demonstrates registration, frame decoding, field binding, action
invocation, and reconnect backoff in dependency-free JavaScript. A kiosk build
can serve those files locally; no bundler is required.

### Linux SBC and e-ink

Use `DisplayClient` for the transport and a local renderer for the panel:

- build a widget tree from `client.definition_document` once;
- keep a `field id -> widget region` map from `definition.fields`;
- on `on_data`, mark only the affected regions dirty;
- draw with Pillow into a 1-bit or grayscale buffer;
- push with `epd-waveshare` (or the Rust equivalent), using partial refresh
  where the panel supports it.

E-ink is much slower than the controller's update rate. Coalesce updates and
drop intermediate values; the latest value is the one that matters.

### ESP32 and microcontrollers

MCUs usually cannot afford a full protobuf runtime plus a JSON parser. Two
practical approaches work:

1. **Generated C**: use `nanopb` or `protobuf-c` with the same
   `proto/ozma/v1` sources. Decode only the messages the device renders.
2. **Small hand-written decoder**: implement the tag byte, varints, and the
   `FieldValue` and `DataUpdate` fields used by the definition. Unknown fields
   are skipped by length, as in protobuf.

Render with LVGL, Slint, or `embedded-graphics`. Store the definition JSON in
flash, or compile the handful of field ids and widget positions the device
actually uses. `postcard` can be useful on a secondary MCU-to-display link, but
it is not part of the controller wire protocol.

### Stream Deck

A Stream Deck is a key-image device, not a pixel display:

- map definition actions to keys;
- render each key's label/value from the definition and latest field values;
- on a key press, send `ActionInvoke` with the action id and any configured
  parameters;
- re-render only keys whose bound field changed.

`elgato-streamdeck` exposes key-image updates; `hidapi` is the lower-level
fallback. Keep action ids and key indices in a local table derived from the
definition, not hard-coded per device model.

### MIDI and OSC control surfaces

MIDI controllers and OSC surfaces are input devices first. Map their physical
controls to definition actions:

- note-on or pad press -> `ActionInvoke`;
- CC/fader movement -> `ActionInvoke` with the value in `params`, or an
  `InteractionEvent` when continuous positioning matters;
- OSC address -> action key lookup from the definition.

Use `mido`/`python-rtmidi` for MIDI and `python-osc` for OSC. Debounce noisy
controls and keep a monotonic idempotency key per physical press so a retry
does not activate a scenario twice.

### RGB lighting

RGB adapters usually translate a few definition fields into color effects:

- numeric fields -> brightness, hue, or effect speed;
- bool fields -> effect on/off or zone enable;
- enum fields -> named palette or effect selection.

Use WLED's JSON API, or send DDP/Art-Net/sACN datagrams for low-latency
output. Keep color mapping and palettes in the adapter; the shared Display
protocol carries generic typed values, not device-specific lighting commands.

### Native desktop and OzmaOS

Native clients should use `ozma-client`/`DisplaySession` and the scene graph
for window and surface composition. Smithay and `wgpu` are appropriate when the
client owns the compositor. The WebSocket Display channel remains the simplest
path for a native client that only needs the definition, typed values, and
actions.

## Protocol notes for adapter authors

- **One definition, many updates.** The definition is sent once per version;
  do not expect a full layout with every update.
- **JSON is not the steady-state wire format.** JSON appears in the registration
  handshake, the `registered` acknowledgement, and the definition document
  inside `DisplayDefinition`. Data, actions, interactions, and scenes are
  protobuf.
- **Field ids are stable.** Never infer semantics from field ordering. Use the
  compiled `FieldDef.key`, `type`, `unit`, and `scale`.
- **Scaled integers.** Fields with a `scale` carry integers at wire scale; a
  device may expose `raw / scale` as a human value.
- **Version checks.** Ignore `DataUpdate`s for an older definition version than
  the one you have cached. If a `DefinitionPatch` arrives and you need the new
  field table, wait for a full `DisplayDefinition`.
- **Unknown data is safe.** Ignore unknown tags, message fields, and field ids.
- **Transport security.** Use `wss://` when the controller is not on a trusted
  local link. Registration is JSON but is not a substitute for authentication;
  deploy the screen server behind the same trust boundary as the controller.
- **Do not add a build step to the controller.** Protobuf bindings are generated
  ahead of time. Adapters may generate their own bindings from the same
  `proto/ozma/v1` sources.
