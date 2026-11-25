# Phase 4 — opt-in Bluetooth surface

Status: **implemented behind `--bluetooth`**. The Rust node now owns a bounded
BlueZ surface instead of leaving `/bluetooth/*` to a Python-only provider or a
fake success. Every mutating call names exactly one MAC address, pairing never
trusts a device, and a missing adapter/`bluetoothctl` is a typed 503.

The controller keeps its existing controller-local
`/api/v1/bluetooth/*` routes, and adds node-scoped proxies under
`/api/v1/nodes/{node_id}/bluetooth/*` that preserve the node's typed documents.

## Opt-in and configuration

| Flag | Meaning |
|---|---|
| `--bluetooth` (`OZMA_BLUETOOTH`) | Create the bounded BlueZ provider and advertise the `bluetooth` registration capability after a read-only adapter probe. Without it every `/bluetooth/*` route returns 503 `bluetooth.not-configured`. |
| `--bluetooth-adapter MAC` | Use an explicit adapter. Omit it to use BlueZ's `[default]` controller or the first listed controller. |
| `--bluetooth-program PATH` | Override the `bluetoothctl` executable (default `bluetoothctl`). |

The provider never powers a radio on/off, never changes discoverable/pairable
state, and never edits `/var/lib/bluetooth` or system BlueZ configuration. A scan
is the only state-changing operation, and it is bounded and cleaned up.

## Endpoints

All routes are mounted in `controller_compat::compat_router`, so they use the
same bearer-token guard as `/input/*`, `/power/*`, `/backup/*`, and `/usb/*`.
They are **not** part of the unauthenticated `public_router`.

| Endpoint | Behaviour |
|---|---|
| `GET /bluetooth/status` | Bounded adapter list plus the selected adapter's `powered`, `discoverable`, `discovering`, and `pairable` flags. Reports scan bounds and active-scan state. |
| `GET /bluetooth/devices?limit=N` | Known devices from `bluetoothctl devices`, enriched with `info` for up to 32 rows. Returns `address`, `name`, `alias`, `paired`, `bonded`, `trusted`, `connected`, `blocked`, optional `rssi`/`icon`/`class`/`address_type`, and `detailed`. `limit` is 1–64 (default 64); the underlying list is always capped at 64. |
| `POST /bluetooth/scan` | Bounded scan, 1–15 s (default 8). Accepts `{"duration_seconds": N}` (also `seconds`/`timeout` for controller compatibility). Returns known devices after the scan plus `seconds`, `cleanup`, and `truncated`. No pairing/trust/connect is attempted. |
| `POST /bluetooth/pair` | `{"address":"AA:BB:CC:DD:EE:FF"}` → exact argv `bluetoothctl pair AA:BB:CC:DD:EE:FF`. **Never calls `trust`.** Response states `"auto_trust": false`. |
| `POST /bluetooth/connect` | `{"address":"..."}` → exact argv `bluetoothctl connect <addr>`. |
| `POST /bluetooth/disconnect` | `{"address":"..."}` → exact argv `bluetoothctl disconnect <addr>`. |
| `POST /bluetooth/forget` | `{"address":"..."}` → exact argv `bluetoothctl remove <addr>` (BlueZ's forget operation). |

### Status shape

```json
{
  "ok": true,
  "supported": true,
  "configured": true,
  "available": true,
  "program": "bluetoothctl",
  "adapter": "80:84:89:F6:E2:E8",
  "adapters": [
    {
      "address": "80:84:89:F6:E2:E8",
      "name": "SFFPC",
      "alias": "SFFPC",
      "default": true,
      "powered": true,
      "discoverable": false,
      "discovering": false,
      "pairable": true
    }
  ],
  "discovering": false,
  "scan": {"active": false, "default_seconds": 8, "max_seconds": 15}
}
```

### Device shape

```json
{
  "address": "68:8F:C9:0C:1D:F1",
  "name": "MuOS Gamepad",
  "paired": false,
  "bonded": false,
  "trusted": false,
  "connected": false,
  "blocked": false,
  "rssi": -61,
  "icon": "input-gaming",
  "detailed": true
}
```

`rssi` and other detail fields are omitted when `bluetoothctl info` did not
provide them; they are never invented.

## Safety invariants

- **Explicit address only.** The request body must contain one six-octet
  colon-separated MAC. It is validated and uppercased before any process starts.
  Shell metacharacters, extra arguments, hostnames, and partial addresses are
  rejected with 400 `bluetooth.invalid-address`.
- **Exact argv, no shell.** Production starts `bluetoothctl` directly with
  `pair|connect|disconnect|remove <validated-address>`; there is no `sh -c`, no
  string concatenation, and no caller-controlled flags.
- **No auto-trust.** Pairing never invokes `bluetoothctl trust`. The response
  makes that explicit (`auto_trust:false`).
- **No implicit device selection.** Status/scan may list devices, but no mutating
  route chooses a "best" or "last seen" device. Pair/connect/disconnect/forget
  require the caller to name one.
- **No destructive radio changes.** The node does not power-cycle adapters,
  change discoverability/pairability, block devices, or write pair-key files.
- **Bounded work.** Adapters ≤ 8, devices ≤ 64, per-device `info` ≤ 32 with an
  8 s overall enrichment budget, scan 1–15 s, retained stdout ≤ 256 KiB and
  stderr ≤ 32 KiB, and every child has a timeout and `kill_on_drop(true)`.
- **Discovery cleanup.** A scan always attempts `scan off` afterwards, including
  after a scan timeout or failure. BlueZ may return
  `Failed to stop discovery` when discovery is already being torn down; that
  cleanup result is intentionally ignored and the scan response reports
  `"cleanup":"scan-off"`. The node never leaves its own scan running.
- **Single scan at a time.** A second concurrent scan is 409 `bluetooth.busy`;
  scan/device status still reports the active state.

## Typed errors

Error bodies are `{ok:false,supported:true,configured,available,code,error,remediation}`:

| Code | Status | Meaning |
|---|---|---|
| `bluetooth.not-configured` | 503 | Node was not started with `--bluetooth`. |
| `bluetooth.unavailable` | 503 | `bluetoothctl` is absent, no adapter exists, a command reports no controller, or the configured adapter was not found. |
| `bluetooth.invalid-address` | 400 | The address is not exactly `AA:BB:CC:DD:EE:FF`. |
| `bluetooth.invalid-limit` | 400 | `limit=0`; limits above 64 are clamped. |
| `bluetooth.busy` | 409 | Another bounded scan is already running. |
| `bluetooth.timeout` | 504 | A `bluetoothctl` invocation exceeded its operation timeout. |
| `bluetooth.pair-failed` | 502 | BlueZ reported pairing failure (for example `AuthenticationFailed`). |
| `bluetooth.connect-failed` | 502 | BlueZ reported connection failure. |
| `bluetooth.disconnect-failed` | 502 | BlueZ reported disconnection failure. |
| `bluetooth.forget-failed` | 502 | BlueZ reported removal failure. |
| `bluetooth.command-failed` | 502 | Another `bluetoothctl` command failed. |
| `bluetooth.io` | 500 | Local process/spawn I/O failed. |

Non-zero exits and BlueZ error text (for example `Failed to pair`,
`AuthenticationFailed`, `Failed to connect`) are typed failures even when
`bluetoothctl` itself exits 0. A zero exit is also not enough for success: the
node requires BlueZ's positive confirmation (`Pairing successful`/`Paired: yes`,
`Connection successful`/`Connected: yes`, a disconnect confirmation, or
`Device has been removed`). Empty or unrecognised output is a typed failure, so
the routes never return a success-shaped response for an action BlueZ did not
confirm.

## Capability advertisement

- With `--bluetooth` and a successful read-only startup probe, `main.rs` adds the
  `bluetooth` name to the registration `cap` string via `set_capability`.
- `GET /api/v1/connection.controller_surfaces` advertises the seven implemented
  routes (`bluetooth.status`, `bluetooth.devices`, `bluetooth.scan`,
  `bluetooth.pair`, `bluetooth.connect`, `bluetooth.disconnect`,
  `bluetooth.forget`) plus an additive `bluetooth` summary containing the
  configured adapter and `max_scan_seconds`.
- Without `--bluetooth`, the connection document advertises
  `/bluetooth/*` with `supported:false` and reason
  “Bluetooth is opt-in; start the node with --bluetooth…”. The routes remain
  mounted and return 503 `bluetooth.not-configured`, so a probe sees the typed
  reason rather than a bare 404.
- If the probe fails because `bluetoothctl` or an adapter is missing, the
  service stays installed (routes return 503 `bluetooth.unavailable`) but the
  `bluetooth` registration capability is not advertised.

## Controller proxies

`controller/api.py` adds node-scoped proxies:

- `GET  /api/v1/nodes/{node_id}/bluetooth/status`
- `GET  /api/v1/nodes/{node_id}/bluetooth/devices` (forwards `limit`)
- `POST /api/v1/nodes/{node_id}/bluetooth/scan`
- `POST /api/v1/nodes/{node_id}/bluetooth/pair`
- `POST /api/v1/nodes/{node_id}/bluetooth/connect`
- `POST /api/v1/nodes/{node_id}/bluetooth/disconnect`
- `POST /api/v1/nodes/{node_id}/bluetooth/forget`

`_proxy_bluetooth_surface` returns success documents verbatim and preserves
`bluetooth.*` typed errors with their node status, code, error, and remediation.
Read routes require `SCOPE_READ`; scan/pair/connect/disconnect/forget require
`SCOPE_WRITE`. The node bearer token is forwarded only in the request header and
is never copied into a response or error text. Request timeouts are 20 s for
status/devices, 30 s for scan, 40 s for pair, 30 s for connect, and 25 s for
disconnect/forget, comfortably above the node's own bounds.

The pre-existing controller-local `/api/v1/bluetooth/*` routes still manage the
controller host through `controller/bluetooth.py`; they are unchanged.

## Verification

### Rust

Commands run in the working tree:

```
cargo check -p ozma-node --all-targets
cargo test -p ozma-node --lib
cargo test -p ozma-node --lib bluetooth::
```

Results:

- `cargo check -p ozma-node --all-targets`: passed.
- `cargo test -p ozma-node --lib`: **689 passed, 0 failed, 5 ignored** (the
  ignored tests need GStreamer/Python helpers).
- `cargo test -p ozma-node --lib bluetooth::`: **18 passed**, including:
  - `addresses_are_validated_before_any_command`
  - `invalid_address_starts_no_process`
  - `actions_use_exact_argv_and_never_trust`
  - `pair_and_connect_failures_are_typed`
  - `action_without_an_adapter_is_a_typed_503`
  - `zero_exit_without_confirmation_is_not_a_fake_success`
  - `status_parses_adapters_and_flags`
  - `devices_parse_flags_rssi_and_are_bounded`
  - `scan_is_clamped_and_stops_discovery`
  - `scan_timeout_still_stops_discovery`
  - `concurrent_scan_is_busy`
  - `missing_binary_and_no_adapter_are_typed_503`
  - `router_with_no_service_returns_typed_not_configured`
  - `router_preserves_typed_errors`
  - `ansi_and_scan_logs_are_parsed`
  - `error_detail_truncation_respects_utf8_boundaries`
  - `output_parsers_are_bounded_and_tolerate_missing_names`
  - `live_status_and_bounded_scan_when_hci0_is_present`
- `controller_compat::tests`: **148 passed**, including the new
  `connection_advertises_configured_bluetooth_without_probing` and the bearer
  guard list that now covers all seven `/bluetooth/*` routes.

### Live host evidence (`hci0`)

The live test runs only when `/sys/class/bluetooth/hci0` exists; it skips rather
than fabricating an adapter in containers/CI. On the target host it passed and
performed a real 2 s scan through the service, returning
`cleanup:"scan-off"`.

Independent `bluetoothctl` evidence from the same host:

```
$ bluetoothctl list
Controller 80:84:89:F6:E2:E8 SFFPC [default]

$ bluetoothctl show 80:84:89:F6:E2:E8
	Name: SFFPC
	Powered: yes
	Discoverable: no
	Pairable: yes
	Discovering: no

$ bluetoothctl --timeout 3 scan on   # bounded scan
# ... discovery log ...
$ bluetoothctl scan off             # BlueZ: failed because teardown was already in progress
$ bluetoothctl show ... | grep Discovering
	Discovering: no

$ bluetoothctl devices | wc -l
6
```

No real device was paired, connected, disconnected, trusted, or forgotten
during validation. The host's known MuOS Gamepad was only read through
`bluetoothctl info` (`Paired: no`, `Trusted: no`, `Connected: no`).

### Controller

```
python3 -m pytest tests/unit/controller/test_node_api_token_proxy.py -q
```

Result: **111 passed**. New tests cover status JSON/token propagation, scan
body forwarding with a typed 504, pair with a typed 400 invalid-address, and
forget with a typed 503 not-configured document.

### Formatting

`rustfmt` was run on every touched Rust file.

## Remaining gaps

- **Pair-key migration is not implemented in the Rust node.** The controller's
  `POST /api/v1/bluetooth/pair-keys/push-to-node` still targets
  `/bluetooth/pair-keys/import`, which this node does not serve. Implementing it
  would require careful `/var/lib/bluetooth` ownership/permission validation and
  is intentionally out of this safety-focused cut.
- **No trust management.** There is deliberately no `trust`/`untrust` route;
  pairing cannot silently make a device trusted. An operator must use BlueZ
  tooling for that explicit action.
- **No radio management.** Adapter power, discoverable/pairable mode, device
  blocking, and system BlueZ configuration are not exposed.
- **Authentication prompts depend on BlueZ.** Non-interactive
  `bluetoothctl pair` can only complete devices that do not require a
  confirmation/PIN agent. A PIN/passkey-required device returns a typed
  `bluetooth.pair-failed` instead of pretending to pair.
- **One selected adapter for status.** `list` reports up to eight adapters, but
  the scan/device operations use the configured/default adapter. Multi-adapter
  per-command selection is not exposed.
- **Device detail is best-effort.** At most 32 rows are enriched by `info` and
  the enrichment budget is 8 s. `details_complete:false` or a missing `rssi`
  means detail was unavailable, not that the device is absent.
- **No PipeWire/A2DP routing integration.** The surface manages BlueZ
  pairing/connection state; codec/sink selection remains PipeWire's job.
- **Controller-local Bluetooth routes are unchanged.** `/api/v1/bluetooth/*`
  still operates on the controller host; the new node-scoped proxies are the
  only path to a Rust node.
