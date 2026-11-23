<!-- SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException -->
# Parity fixtures

These files are the **current Python/controller baseline**, written by hand from
the Python implementation. They are not generated from the Rust code and must
not be regenerated from it: doing so would make the test assert that Rust equals
Rust.

| File | Baseline source |
|---|---|
| `registration_baseline.json` | `node/node.py` `run()` TXT construction and `_direct_register()`; `controller/api.py` `DirectRegisterRequest` (lines 203–234) and `register_node()` (lines 1003–1048) |
| `hid_v1_baseline.json` | `softnode/evdev_input.py` `EvdevHIDTranslator.handle_keyboard` / `handle_mouse` / `handle_mouse_relative` (lines 412–502), plus the controller's UDP HID v1 type bytes |
| `http_baseline.json` | The controller-facing `/health` and `/api/v1/connection` surface. The connection document reports input sink availability/injection, read-only codec state, and `audio: "not-implemented"` alongside HLS and WebRTC. HLS is opt-in: `not-configured` without `--hls`, `running` only once `stream.m3u8` exists, `failed` plus `hls_error` otherwise. WebRTC remains `not-implemented`. |

The keyboard entries assert the **normalized** `InputEvent` sequence used by
`ozma-node`, not the Python evdev keycodes. Python maps HID usages directly to
evdev codes; the Rust compatibility adapter keeps the HID usage in
`InputEvent::Key.code` and leaves the evdev mapping to the input sink. The
ordering and diffing rules — modifier changes first, releases before presses,
motion before buttons, then wheel — are the Python semantics this fixture
protects.

## Future work: opt-in live Python comparison

A live harness that starts `node/node.py` or `softnode/evdev_input.py` beside
the Rust node and diffs their output is **not implemented**. It would need
Python, aiohttp, `python-evdev`, uinput access, and mDNS, so it must stay
opt-in and out of the default CI run. Until then, these checked-in fixtures are
the baseline and the matrix rows they feed are at most `partial`.

See `docs/parity-matrix.md` for the gate status and commands.
