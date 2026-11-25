# Phase 4 — node RPA report

Status: real RPA primitives implemented in `ozma-node`; the script/BIOS engine
remains a typed 501. No OCR text, input event, or screenshot is fabricated.

## Summary

The Python hardware node (`node/rpa.py` + `node/node.py:380-445`) has a full
node-local RPA engine: frame grab, bitmap/Tesseract OCR, HID key/type/click,
`wait_for_text`, script execution, and BIOS presets. The Rust node already had
the underlying primitives (capture registry/console broker, JPEG encoder, shared
input sink, layout-aware typing), so this change wires an honest subset of the
controller's `/api/v1/rpa/*` surface to them.

| Endpoint | State | Backing primitive |
|---|---|---|
| `POST /api/v1/rpa/type` | Implemented | `SharedSink` + `keyboard_layout` mapping |
| `POST /api/v1/rpa/key` | Implemented | `SharedSink` + HID usage mapping |
| `GET /api/v1/rpa/screenshot` | Implemented | capture registry / console broker + JPEG encoder |
| `POST /api/v1/rpa/screenshot` | Implemented | same as GET |
| `POST /api/v1/rpa/ocr` | Implemented when `tesseract` is installed | captured frame + `tesseract` CLI |
| `GET /api/v1/rpa/screen` | Typed 501 | bitmap OCR / text regions not ported |
| `POST /api/v1/rpa/click` | Typed 501 | click-by-coordinate/text helper not ported |
| `POST /api/v1/rpa/wait_for_text` | Typed 501 | text-wait loop not ported |
| `POST /api/v1/rpa/script` | Typed 501 | script runner not ported |
| `POST /api/v1/rpa/enter_bios` | Typed 501 | BIOS heuristics not ported |
| `POST /api/v1/rpa/set_boot_usb` | Typed 501 | BIOS navigation not ported |

The remaining sub-surfaces keep the existing stable 501 schema
(`ok:false`, `code:"rpa.unsupported"`, `surface`, `capability`, `supported:false`,
`reason`, `error`, `python_reference`, `remediation`).

## Implemented routes

### Type

`POST /api/v1/rpa/type`

```json
{"text": "Hello!", "delay": 0.03}
```

- Same layout-aware mapping and timing as `/input/type`.
- Validates the whole string before sending anything; unsupported characters,
  dangling dead keys, and over-limit text return 400 and type nothing.
- `delay` is optional and bounded to 100 ms/character.
- No input sink: 503 `input-unavailable` with remediation.
- Response: `{"ok":true,"sink":"...","events":N}` plus `injected:false` for a
  recording-only sink.

### Key

`POST /api/v1/rpa/key`

```json
{"key": "enter"}
{"key": "?", "modifier": 2}
{"key": "c", "modifiers": ["ctrl"]}
{"keys": ["ctrl", "c"]}
```

- Named keys mirror `node/rpa.py` (`enter`, `return`, `esc`, `backspace`, `tab`,
  `space`, `delete`, `insert`, arrows, home/end, page up/down, `f1`..`f12`,
  print-screen, scroll-lock, pause, caps-lock, num-lock, menu, and common
  punctuation names).
- Single printable characters use the selected guest layout, including Shift
  and AltGr requirements.
- `modifier` is the Python node's USB HID boot-keyboard bitmask; `modifiers` and
  `keys` accept names.
- Invalid/ambiguous keys return 400 and send nothing; no sink returns 503
  `input-unavailable`.

### Screenshot

`GET|POST /api/v1/rpa/screenshot[?console=Console_N]`

- Real `image/jpeg` bytes from the active capture source, with
  `Cache-Control: no-store`.
- `?console=` uses the same selection/broker path as `/display/snapshot`; the
  active console uses the shared registry snapshot control, non-active consoles
  use the bounded broker.
- No source is 503 `capture.unavailable`; unknown console is 404; single-head
  conflict is 409; capture/encode timeout is 504/503. Nothing is synthesized.

### OCR

`POST /api/v1/rpa/ocr[?console=Console_N]`

- Captures one real frame, then runs:

  ```text
  tesseract stdin stdout --psm 3 -l eng
  ```

  with the JPEG piped on stdin (no temporary image file).
- Bounds: 16 MiB JPEG, 8192x4096 pixels, 20 s timeout, 1 MiB stdout.
- Response includes the exact engine text plus capture geometry/source:

  ```json
  {
    "ok": true,
    "available": true,
    "engine": "tesseract",
    "text": "exact engine output",
    "full_text": "exact engine output",
    "width": 1920,
    "height": 1080,
    "source_id": "...",
    "source_name": "...",
    "text_regions": []
  }
  ```

- Empty screen -> empty `text`/`full_text`. `text_regions` stays empty because
  this route does not compute bounding boxes.
- No tesseract: 503 `rpa.ocr-unavailable`.
- Oversized frame: 413 `rpa.ocr-frame-too-large`.
- Timeout: 504 `rpa.ocr-timeout`.
- Process/output failure: 502 `rpa.ocr-failed` / `rpa.ocr-output-too-large`.
- No frame source: the same typed capture errors as screenshot.

## Authorization and capability advertisement

- All implemented RPA routes live in `controller_compat::rpa_router`, merged
  into `compat_router` **before** the node API bearer guard, matching the
  existing `/input/*`, `/power/*`, and `/codec` behaviour. They remain
  bearer-guarded.
- `GET /api/v1/connection.controller_surfaces` now lists both:
  - `RPA_IMPLEMENTED_SURFACES` with `supported:true` for `rpa.type`, `rpa.key`,
    `rpa.screenshot`, and `rpa.ocr`;
  - `CONTROLLER_ONLY_SURFACES` with `supported:false` for the remaining RPA
    fallback and the other controller-only surfaces.
- The controller's existing typed-error and raw-JPEG proxy paths already handle
  both a real 200 JPEG and a typed 501/503 body; no proxy change was required.

## Tests and verification

Rust:

```bash
cargo test -p ozma-node --lib
cargo check -p ozma-node --all-targets
rustfmt --edition 2021 --check ozma-node/src/rpa.rs \
  ozma-node/src/controller_compat.rs ozma-node/src/lib.rs
```

Focused tests added:

- `rpa_type_route_dispatches_layout_aware_events_through_the_sink`
- `rpa_key_route_supports_named_keys_characters_and_combos`
- `rpa_screenshot_returns_real_jpeg_and_honours_console_selection`
- `rpa_ocr_returns_exactly_the_engine_text` (fake CLI, no tesseract needed)
- `rpa_ocr_runs_installed_tesseract_over_a_real_frame` (skips if absent)
- `rpa_ocr_without_tesseract_is_a_typed_503`
- `controller_only_surfaces_are_advertised_and_return_typed_501s` (updated)
- `legacy_routes_are_typed_501s_with_remediation` (updated)
- `input_codec_and_audio_routes_are_merged_behind_the_bearer_guard` (updated)
- `rpa::tests::*` for key/modifier mapping, event ordering, absent OCR, and
  oversized-frame rejection.

Controller tests were not changed because no proxy code changed. The existing
`tests/unit/controller/test_node_api_token_proxy.py` still covers typed-body
pass-through and real-JPEG pass-through.

## Remaining gaps

- **Script engine:** no Rust execution of `node/rpa.py::run_script` commands.
- **Text waits/click:** no `wait_for_text`, `wait_for_any_text`, or
  `click_text` loop.
- **Screen read:** no bitmap/VGA-font BIOS OCR and no region coordinates, so
  `/api/v1/rpa/screen` remains typed 501.
- **BIOS automation:** `enter_bios` and `set_boot_usb` remain heuristic Python
  workflows; the Rust node does not pretend to navigate firmware.
- **OCR portability:** requires `tesseract` plus `eng` traineddata on the node.
  Nodes without it report `rpa.ocr-unavailable`.
- **Registration capabilities:** the mDNS/registration capability list still
  describes the node at a coarse level; the detailed implemented/unsupported
  split is advertised through `/api/v1/connection.controller_surfaces`.
