# Phase 4 — opt-in serial/UART console report

Status: **implemented and opt-in**. A node started without `--serial-device`
returns a typed 503 `serial.not-configured` from the five implemented routes and
keeps the historical `/serial/*` names as typed 501s. Nothing is auto-detected
and no device is opened unless the operator supplies an explicit path.

The provider now applies modem/flow-control settings on open, reports modem
control lines, offers an opt-in DTR/RTS control route, and reconnects the same
explicit path with bounded exponential backoff after a removal or read error.
There is no fallback to another device.

Source: `ozma-node/src/serial.rs`, wired through
`controller_compat::ConnectionState::with_serial` and
`main.rs` (`--serial-*` flags). Controller proxies live in
`controller/api.py`.

## Configuration

`--serial-device PATH` is required to create the provider. The path must be
absolute and must not contain `.` or `..` components.

| Flag | Default | Validation |
|---|---|---|
| `--serial-baud` | `115200` | 50–4,000,000 |
| `--serial-data-bits` | `8` | 5–8 |
| `--serial-parity` | `none` | `none`, `odd`, `even` |
| `--serial-stop-bits` | `1` | 1–2 |
| `--serial-flow-control` | `none` | `none`, `rtscts`, `xonxoff`; verified after open |
| `--serial-dtr[=BOOL]` | unset | `true` asserts DTR, `false` deasserts it; unset leaves it untouched |
| `--serial-rts[=BOOL]` | unset | `true` asserts RTS, `false` deasserts it; unset leaves it untouched |
| `--serial-control` | off | enables `POST /serial/control` for DTR/RTS only |
| `--serial-timeout-ms` | `100` | 1–10,000 ms; blocking read timeout and write stall bound |
| `--serial-buffer-bytes` | `262144` | 1024–4,194,304 |
| `--serial-reconnect-attempts` | `5` | 0–100; 0 disables reconnect |
| `--serial-reconnect-min-ms` | `1000` | 1–60,000 ms; first retry delay |
| `--serial-reconnect-max-ms` | `30000` | at least the minimum, at most 300,000 ms |

The provider opens the device once at startup and keeps it open in a background
blocking reader thread. Writes use `try_clone()` on the same port, so they share
the same configured line settings. Flow control is requested through
`serialport` and then verified against `SerialPort::flow_control()`; a driver
that silently ignores or rejects the mode produces
`serial.flow-control-unsupported`.

Initial DTR/RTS levels are applied on every open, including after reconnect.
When they are left unset the provider does not touch those pins. Requested
levels are remembered, so a control-route change is reapplied when the same
device reconnects. The provider also clears kernel buffers on each open.

### Reconnect

On device removal, EOF, or a non-timeout read error the reader:

1. marks the session disconnected and drops both port handles;
2. forgets retained receive bytes (absolute offsets stay monotonic, so old
   cursors report `dropped: true`);
3. retries the **same explicit path** with exponential backoff
   (`min × 2^(attempt-1)`, capped at `max`);
4. resets the attempt counter only after a connection stays up for at least
   five seconds, so a device that opens and immediately fails cannot reset the
   budget and storm;
5. stops after `--serial-reconnect-attempts` failures and records
   `serial.reconnect-exhausted`.

With the defaults the delays are 1, 2, 4, 8, and 16 seconds. A retryable open
failure at startup enters the same schedule instead of requiring a restart.
Non-retryable failures (invalid configuration, unsupported flow control or
line control, permission denied) do not loop. `--serial-reconnect-attempts 0`
restores the previous one-shot behaviour. Lifetime `bytes_in`/`bytes_out`
counters are not reset; `reconnect.reconnects` counts successful reopens.
Shutdown sets the stop flag, interrupts a backoff sleep within 25 ms, and joins
the reader thread; a blocking read is bounded by the configured read timeout.

## Node endpoints

All five routes are behind the node bearer guard.

| Endpoint | Behaviour |
|---|---|
| `GET /serial/status` | `ok`, `supported`, `configured`, `connected`, `state` (`connected`/`reconnecting`/`failed`/`stopped`/`disconnected`), `device`, `line`, `flow_control`, `control_enabled`, `lines`, `reconnect`, `read_timeout_ms`, `bytes_in`, `bytes_out`, `buffer`, `limits`, and optional typed `last_error`. |
| `POST /serial/write` | Exactly one of `{"text":"..."}` or `{"hex":"0a 0d"}`. Whitespace, `:` and `-` are ignored in hex; the resulting bytes are written exactly, with no newline added. Maximum 64 KiB. Returns `bytes`, `encoding`, `bytes_out`, and buffer facts. |
| `GET /serial/read?max=N&since=C` | Base64 of exact buffered bytes. `max` is 1–65,536 (default 4,096). `since` is an absolute cursor. Returns `offset`, `next`, `dropped`, `has_more`, and buffer start/end. |
| `GET /serial/stream?since=C&idle_ms=N` | SSE (`text/event-stream`) with `data` events of at most 8 KiB. `idle_ms` is 100–120,000 (default 15,000). Emits `gap` when the cursor predates retained data, then `idle` or `disconnected` and ends. Dropping the client closes only that subscription. |
| `POST /serial/control` | Opt-in via `--serial-control`. Accepts exactly `{"dtr":bool}` and/or `{"rts":bool}`, rejects unknown fields, and is body-capped at 1 KiB. Returns the requested levels plus current `lines`. Without the flag it is a typed 403 `serial.control-disabled`. |

`lines` always has keys `dtr`, `rts`, `cts`, `dsr`, `dcd`, and `ri`. DTR/RTS
are the last commanded output levels (`null` until set); CTS/DSR/DCD/RI are
read-only input pins and are `null` when the device or driver cannot report
them. There is no fabricated pin state.

Other sub-surfaces (`/serial/buffer`, `/serial/send`, `/serial/state`,
`/serial/text`, `/serial/setup`) remain typed 501 `serial.unsupported` with the
stable documented body.

### Bounds and memory behaviour

- Retained receive data is a fixed `VecDeque` ring; eviction advances an absolute
  `start` offset. The ring never exceeds `--serial-buffer-bytes`.
- The reader uses a fixed 4 KiB scratch buffer per read.
- Each read response is capped at 64 KiB.
- Each SSE event is capped at 8 KiB; a slow consumer re-reads from the ring by
  cursor instead of accumulating an unbounded broadcast queue. The broadcast
  channel only carries wake-ups (capacity 32) and lag is recovered by draining
  the ring from the cursor.
- A stream ends on its idle timeout or when the reader marks the device
  disconnected. It has no unbounded producer or replay loop; a continuously
  writing UART still uses fixed memory because old bytes are evicted.
- Reconnect holds no growing queue: one reader thread, one backoff sleep, and a
  fixed retry counter. Retained bytes are cleared on reconnect.

### Typed errors

| Code | HTTP | Meaning |
|---|---|---|
| `serial.not-configured` | 503 | `--serial-device` was not supplied |
| `serial.invalid-device` | 400 | relative/`.`/`..` path or a directory |
| `serial.device-missing` | 503 | explicit path does not exist at startup (retried when enabled) |
| `serial.permission-denied` | 403 | node user cannot open the device |
| `serial.device-busy` | 409 | another process holds the device / it vanished during open |
| `serial.not-connected` | 503 | write/control attempted without an open device |
| `serial.invalid-payload` | 400 | neither or both of `text`/`hex` |
| `serial.invalid-hex` | 400 | odd length or non-hex character |
| `serial.payload-too-large` | 413 | text/hex (or control body) exceeds the configured bound |
| `serial.invalid-limit` | 400 | `max` is zero or above the configured bound |
| `serial.invalid-cursor` | 400 | `since` is beyond the buffered end |
| `serial.invalid-config` | 400 | invalid startup line/buffer/stream/reconnect setting |
| `serial.write-failed` | 502 | write/flush failed after the device was open |
| `serial.read-failed` | 502 | reader thread hit an I/O error or EOF |
| `serial.flow-control-unsupported` | 501 | driver rejected or ignored the requested flow-control mode |
| `serial.line-control-unsupported` | 501 | device/driver cannot set DTR or RTS |
| `serial.reconnect-exhausted` | 503 | the bounded retry budget ran out; no further opens |
| `serial.control-disabled` | 403 | `POST /serial/control` without `--serial-control` |
| `serial.invalid-control` | 400 | missing/unknown/invalid DTR/RTS body |

Errors use `{ok:false,supported:true,configured,available:false,code,error,
remediation}`. The status route keeps the most recent typed failure in
`last_error`.

## Controller proxies

`controller/api.py` adds node-scoped routes for the four original surfaces:

- `GET /api/v1/nodes/{node_id}/serial/status`
- `GET /api/v1/nodes/{node_id}/serial/read` (`max`, `since` forwarded)
- `POST /api/v1/nodes/{node_id}/serial/write` (JSON body forwarded verbatim)
- `GET /api/v1/nodes/{node_id}/serial/stream` (`since`, `idle_ms` forwarded)

`POST /serial/control` is node-only in this change; the controller proxy was
not modified. It remains a bounded, guarded node endpoint.

`_proxy_serial_surface` preserves success JSON and `serial.*` typed errors.
A typed `403 serial.permission-denied` is preserved rather than mistaken for a
node bearer-token rejection; a genuine node 401/403 still maps to the existing
token-free controller 502 explanation. The stream route uses
`httpx.AsyncClient.stream` plus FastAPI `StreamingResponse`, so it does not
buffer the whole serial stream. The node bearer token is only sent in the
upstream request header; it never appears in a response or error detail.
Status/read/stream require `SCOPE_READ`; write requires `SCOPE_WRITE`.

## Tests

`cargo test -p ozma-node --lib serial::tests`:

- `device_and_line_settings_are_validated_before_open` — absolute path, `.`/`..`
  rejection, baud/data-bit/stop-bit bounds, parity/flow-control parsing.
- `reconnect_policy_is_validated_and_backoff_is_capped` — defaults (5 attempts,
  1 s/30 s), 1/2/4/8/16/30 s schedule, attempt/delay bounds.
- `flow_control_initial_lines_and_status_are_reported` — fake backend records
  the requested flow control and path, applies DTR/RTS on open, clears kernel
  buffers, and reports CTS/DSR/DCD/RI without inventing pins.
- `unsupported_flow_control_and_initial_line_are_typed` — fake open returns
  `serial.flow-control-unsupported`; a rejecting DTR yields
  `serial.line-control-unsupported`.
- `control_endpoint_is_opt_in_and_bounded` — `--serial-control` absent gives a
  typed 403; enabled sets DTR/RTS and echoes lines; empty/unknown/oversized
  bodies are typed; unsupported DTR is 501; disconnected is 503.
- `text_and_hex_payloads_are_exact_and_bounded` — exact bytes, separators,
  both/neither payload, odd hex, size overflow.
- `ring_buffer_is_bounded_and_cursor_reads_are_exact` — capacity eviction,
  cursor continuation, dropped data, future-cursor rejection.
- `open_error_mapping_distinguishes_missing_permission_and_busy` — typed
  missing/permission/busy codes and HTTP statuses.
- `not_configured_and_residual_sub_surfaces_are_typed` — implemented routes
  (including control) return typed 503 without configuration.
- `fake_backend_status_read_and_write_validate_bounds` — fake backend status,
  base64 read, invalid limit/cursor, oversized write.
- `reconnect_backoff_reopens_same_path_and_resets_buffer` — fake first port
  reads then fails; the worker reopens the same path, clears retained bytes,
  resets attempts, and keeps lifetime counters.
- `reconnect_cap_stops_and_does_not_storm` — exactly initial + 2 retries, then
  `serial.reconnect-exhausted`, `state:"failed"`, and no further opens.
- `stop_during_backoff_is_prompt_and_leaves_no_worker` — a 60 s backoff sleep is
  interrupted by `stop()` in under a second; no reopen after shutdown.
- `missing_device_is_typed_and_status_stays_observable` — startup
  `serial.device-missing`, status `state:"failed"` with last_error, write
  `serial.not-connected` (reconnect disabled in this fixture).
- `live_socat_pty_status_write_read_stream_and_disconnect` — real `socat` PTY
  pair: open/status, write to peer (`ping\n` exactly), peer→read
  (`boot-ok\n`), live SSE `data` event (`live\n`), then kill socat and observe
  disconnected status plus a finite `event: disconnected` stream.
- `live_socat_pty_removal_and_readd_reconnects` — remove a `socat` PTY, re-add
  the same paths, and observe a reconnect, cleared buffer, and resumed reads.

The live tests skip when `socat` is absent. They write fixed payloads once and
rely on the server's idle timeout, so the test source cannot feed forever.

Controller (`python3 -m pytest tests/unit/controller/test_node_api_token_proxy.py`):

- serial entries in `PROXY_CASES` cover status/read/write token forwarding.
- `test_serial_proxies_forward_bounds_body_and_token_without_leak` covers query
  and body forwarding.
- `test_serial_proxy_preserves_typed_errors` covers 503/403/409/400/413 typed
  bodies, including the typed 403 distinction.
- `test_serial_stream_proxy_forwards_sse_and_token` covers SSE relay, forwarded
  `since`/`idle_ms`, content type, and token-free output.

## Remaining gaps

- One explicit device only; no auto-detection or multi-port sessions. Reconnect
  never falls back to a different path.
- No break control, modem dial/answer sequences, binary logging/rotation, or
  pin-change event stream. `POST /serial/control` sets DTR/RTS only and is
  opt-in.
- Input pins are read opportunistically through `serialport`; a PTY or driver
  that cannot report CTS/DSR/DCD/RI shows `null`, not a guessed value.
- The legacy Python `controller/serial_console.py` poller still requests
  `/serial/buffer`, which remains a typed 501; it has not been migrated to the
  new `/serial/read` cursor API. The controller proxy does not expose
  `/serial/control` yet.
- `/serial/buffer`, `/serial/send`, `/serial/state`, `/serial/text`, and
  `/serial/setup` are intentionally not aliased.
