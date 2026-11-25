# Phase 4 — controller-only node surfaces

Status: closed as **typed stubs** for every surface in the Phase 4 P0 blocker 6
list. No surface is reported as implemented, and no stub returns a fabricated
success, empty buffer, or fake image.

This document covers the Rust `ozma-node` side of the controller-only proxies
(`/rpa/*`, `/backup/*`, `/ui/windows`, `/serial/*`, `/rgb/*`, plus `/current`)
and the controller behaviour change that preserves the typed result.

## Decisions

| Surface | Controller consumer | Decision | Exact provider gap |
|---|---|---|---|
| `/api/v1/rpa/*` | `controller/api.py::_node_rpa_proxy`, `node_rpa_screenshot` (business endpoints at `/api/v1/nodes/{id}/rpa/*`) | **Typed stub** (501) | Python `node/rpa.py` is a node-local OCR (bitmap/Tesseract) plus HID/script engine. Rust has capture (`/display/snapshot`) and input injection (`/input/*`) but no OCR, RPA script runner, `wait_for_text`, BIOS automation, or RPA-specific frame grab. A standalone screenshot already exists as `/display/snapshot`. |
| `/backup/*` | `controller/api.py` backup proxy routes (backup snapshots, files, restore, run, apps, restore-apps) | **Typed stub** (501) | Python `agent/backup.py` wraps Restic, DB hooks, app inventory, macOS Time Machine, scheduling, and restore. No Rust backup crate or service exists in this workspace, and the node has no agent backup API. |
| `/ui/windows` | Python desktop agent (`agent/ozma_desktop_agent.py:996`); the controller consumes `/ui/hints` | **Typed stub** (501, public read-only) | Python `agent/ozma_desktop_agent.py` uses an in-guest accessibility provider. `ozma-node` has no in-guest agent and must not invent window titles or positions. `/ui/hints` keeps its existing empty-shape 200 with `available:false` for compatibility; `/ui/windows` is explicitly unsupported. |
| `/serial/*` | `controller/serial_console.py::_poll_node_serial` and legacy panels | **Typed stub** (501) | Python `node/serial_capture.py` owns a real UART/USB-serial reader, ring buffer, SSE stream, and send path. `ozma-node` has no serialport dependency or serial session implementation. |
| `/rgb/*` | `controller/api.py` RGB proxy, `controller/rgb_outputs.py::_push_node` | **Typed stub** (501) | Python `node/rgb_leds.py` drives WS2812B/SK6812 over SPI and runs effects. `ozma-usb`/`ozma-hid` are gadget/HID injectors, not SPI LED drivers; no WS2812 or SPI crate is used by the node. |
| `/current` (and `/current/history`) | `controller/api.py::node_current` | **Typed stub** (501) | Python `node/current_sensor.py` reads an INA219-class power sensor. No Rust sensor driver or ADC source is present. |

No surface was dropped: the controller routes remain, and every one now has a
discoverable typed result.

### Why none of these were implemented for real

The audit hint suggested checking whether a Rust library already exists. None
does in this checkout:

- `ozma-usb` and `ozma-hid` expose ConfigFS gadget, mass-storage/NBD, and HID
  injection primitives. They do not contain an SPI/WS2812 LED output path.
- The node has no `serialport`/`tokio-serial` dependency and no serial session
  buffer to expose.
- Backup requires Restic, platform exclusions, app inventory, and restore
  hooks; there is no Rust equivalent crate or service.
- RPA requires OCR plus a script/automation engine; the only reusable pieces
  (`/display/snapshot`, `/input/*`) are already separate real routes.
- `/ui/windows` requires an in-guest accessibility agent, not a node library.

Implementing a partial surface (for example RGB state tracking without hardware
output) would create exactly the fake success this blocker is meant to remove.

## Typed stub behaviour

All stubs answer `HTTP 501 Not Implemented` with
`Content-Type: application/json` and a stable body:

```json
{
  "ok": false,
  "code": "rpa.unsupported",
  "surface": "/api/v1/rpa/*",
  "capability": "rpa",
  "supported": false,
  "reason": "This node has no RPA/OCR engine or script runner.",
  "error": "This node has no RPA/OCR engine or script runner.",
  "python_reference": "node/rpa.py (node/node.py:437-445)",
  "remediation": "Keep the Python hardware node that owns the RPA/OCR engine, ..."
}
```

`code`, `surface`, `capability`, `supported`, `reason`, and
`python_reference` are the stable fields. `error` mirrors `reason` for existing
controller error handling; `remediation` is operator guidance.

Stable codes:

| Capability | Code |
|---|---|
| `rpa` | `rpa.unsupported` |
| `backup` | `backup.unsupported` |
| `ui_windows` | `ui.windows_unsupported` |
| `serial` | `serial.unsupported` |
| `rgb` | `rgb.unsupported` |
| `current` | `power.current_unsupported` |

Routes:

- guarded control router (`controller_compat::legacy_router`):
  `/api/v1/rpa/*`, `/backup/*`, `/serial/*`, `/rgb/state`, `/rgb/effect`,
  `/rgb/set`, `/rgb/off`, `/current`, `/current/history`
- public read-only router (`controller_compat::public_router`):
  `/ui/windows`

Auth is unchanged from the existing node routes: control surfaces stay behind
the node bearer guard; `/ui/windows` is public read-only, matching `/ui/hints`.
The stubs never inspect or mutate hardware state.

## Discovery without probing

`GET /api/v1/connection` now carries `controller_surfaces`. It is generated from
`controller_compat::CONTROLLER_ONLY_SURFACES`:

```json
"controller_surfaces": [
  {
    "surface": "/api/v1/rpa/*",
    "capability": "rpa",
    "supported": false,
    "reason": "This node has no RPA/OCR engine or script runner.",
    "python_reference": "node/rpa.py (node/node.py:437-445)"
  }
]
```

A controller can read this document once and skip the proxy round-trip for
surfaces it knows are absent. The array is additive; existing connection fields
are unchanged.

## Controller behaviour

`controller/api.py` keeps every route and now preserves the node's typed
`supported:false` response instead of flattening it:

- `_proxy_node_surface` passes a typed 501 body through as JSON. It is used by
  `/current`, `/rgb/state`, `/rgb/set`, `/ui/hints`, the RPA proxy, and the
  backup proxy routes.
- `_proxy_node_raw` fetches `/api/v1/rpa/screenshot` as bytes and checks the
  status first. A typed 501 JSON body is returned as JSON, never labelled
  `image/jpeg`; a real 200 JPEG still streams through unchanged.
- Node 401/403 still maps to the existing token-free controller 502
  explanation; non-typed node failures remain 502, not a hang or a fake 200.
- The backup proxies no longer turn a typed 501 into the generic 503
  "agent unreachable" message.
- `/ui/hints` no longer returns `200 {"error": ...}` when the agent is absent;
  it forwards the typed body (or a real 502 on transport failure).

## Tests

Rust (`cargo test -p ozma-node --lib`):

- `controller_compat::tests::controller_only_surfaces_are_advertised_and_return_typed_501s`
  asserts the connection advertisement, every stub body, and `/ui/windows`.
- `controller_compat::tests::legacy_routes_are_typed_501s_with_remediation`
  covers the legacy RGB/current/serial aliases plus RPA and backup.
- `controller_compat::tests::input_codec_and_audio_routes_are_merged_behind_the_bearer_guard`
  covers the bearer guard for RPA, backup, RGB, current, and serial.

Controller (`python3 -m pytest tests/unit/controller/test_node_api_token_proxy.py`):

- `test_controller_only_surface_proxies_preserve_typed_unsupported` covers RPA
  screen/screenshot/key, current, RGB state/set, backup snapshots/run, and UI
  hints, including bearer-token propagation.
- `test_rpa_screenshot_still_returns_real_jpeg_bytes` proves a real node
  screenshot is not broken by the typed-error path.

## Remaining gaps

- The Python providers remain the only real implementations of RPA, backup,
  serial capture, RGB output, current sensing, and in-guest window hints. A
  future real port needs its own hardware/library work and tests; this change
  only makes their absence honest and discoverable.
- The node registration capability list (`capabilities` TXT/registration field)
  is owned by `main.rs` and is not changed here; discovery uses the connection
  document. A later registration change can mirror the same capability names.
- `controller/serial_console.py` only polls nodes that advertise `cap:serial`
  and `controller/rgb_outputs.py` only pushes to explicitly configured node
  zones, so neither currently probes a Rust node that lacks these capabilities.
  Their existing failure paths log and do not fabricate success.
