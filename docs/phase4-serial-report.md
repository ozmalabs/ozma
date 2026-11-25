# Phase 4 — opt-in serial/UART console report

Status: **implemented and opt-in**. A node started without `--serial-device`
returns a typed 503 `serial.not-configured` from the four implemented routes and
keeps the historical `/serial/*` names as typed 501s. Nothing is auto-detected
and no device is opened unless the operator supplies an explicit path.

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
| `--serial-timeout-ms` | `100` | 1–10,000 ms; used as the blocking read timeout and write stall bound |
| `--serial-buffer-bytes` | `262144` | 1024–4,194,304 |

The provider opens the device once at startup and keeps it open in a background
blocking reader thread. Writes use `try_clone()` on the same port, so they share
the same configured line settings. A failure to open is retained as a typed
`last_error`; the service stays mounted so `GET /serial/status` can explain the
state. No reconnect loop is started.

## Node endpoints

All four routes are behind the node bearer guard.

| Endpoint | Behaviour |
|---|---|
| `GET /serial/status` | `ok`, `supported`, `configured`, `connected`, `state` (`connected`/`failed`/`disconnected`), `device`, `line`, `read_timeout_ms`, `bytes_in`, `bytes_out`, `buffer`, `limits`, and optional typed `last_error`. |
| `POST /serial/write` | Exactly one of `{"text":"..."}` or `{"hex":"0a 0d"}`. Whitespace, `:` and `-` are ignored in hex; the resulting bytes are written exactly, with no newline added. Maximum 64 KiB. Returns `bytes`, `encoding`, `bytes_out`, and buffer facts. |
| `GET /serial/read?max=N&since=C` | Base64 of exact buffered bytes. `max` is 1–65,536 (default 4,096). `since` is an absolute cursor. Returns `offset`, `next`, `dropped`, `has_more`, and buffer start/end. |
| `GET /serial/stream?since=C&idle_ms=N` | SSE (`text/event-stream`) with `data` events of at most 8 KiB. `idle_ms` is 100–120,000 (default 15,000). Emits `gap` when the cursor predates retained data, then `idle` or `disconnected` and ends. Dropping the client closes only that subscription. |

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

### Typed errors

| Code | HTTP | Meaning |
|---|---|---|
| `serial.not-configured` | 503 | `--serial-device` was not supplied |
| `serial.invalid-device` | 400 | relative/`.`/`..` path or a directory |
| `serial.device-missing` | 503 | explicit path does not exist at startup |
| `serial.permission-denied` | 403 | node user cannot open the device |
| `serial.device-busy` | 409 | another process holds the device / it vanished during open |
| `serial.not-connected` | 503 | write attempted without an open device |
| `serial.invalid-payload` | 400 | neither or both of `text`/`hex` |
| `serial.invalid-hex` | 400 | odd length or non-hex character |
| `serial.payload-too-large` | 413 | text/hex exceeds the configured write bound |
| `serial.invalid-limit` | 400 | `max` is zero or above the configured bound |
| `serial.invalid-cursor` | 400 | `since` is beyond the buffered end |
| `serial.invalid-config` | 400 | invalid startup line/buffer/stream setting |
| `serial.write-failed` | 502 | write/flush failed after the device was open |
| `serial.read-failed` | 502 | reader thread hit an I/O error or EOF |

Errors use `{ok:false,supported:true,configured,available:false,code,error,
remediation}`. The status route keeps the most recent typed failure in
`last_error`.

## Controller proxies

`controller/api.py` adds node-scoped routes:

- `GET /api/v1/nodes/{node_id}/serial/status`
- `GET /api/v1/nodes/{node_id}/serial/read` (`max`, `since` forwarded)
- `POST /api/v1/nodes/{node_id}/serial/write` (JSON body forwarded verbatim)
- `GET /api/v1/nodes/{node_id}/serial/stream` (`since`, `idle_ms` forwarded)

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
  rejection, baud/data-bit/stop-bit bounds, parity parsing.
- `text_and_hex_payloads_are_exact_and_bounded` — exact bytes, separators,
  both/neither payload, odd hex, size overflow.
- `ring_buffer_is_bounded_and_cursor_reads_are_exact` — capacity eviction,
  cursor continuation, dropped data, future-cursor rejection.
- `open_error_mapping_distinguishes_missing_permission_and_busy` — typed
  missing/permission/busy codes and HTTP statuses.
- `not_configured_and_residual_sub_surfaces_are_typed` — all four implemented
  routes return typed 503 without configuration.
- `fake_backend_status_read_and_write_validate_bounds` — fake backend status,
  base64 read, invalid limit/cursor, oversized write.
- `missing_device_is_typed_and_status_stays_observable` — startup
  `serial.device-missing`, status `state:"failed"` with last_error, write
  `serial.not-connected`.
- `live_socat_pty_status_write_read_stream_and_disconnect` — real `socat` PTY
  pair: open/status, write to peer (`ping\n` exactly), peer→read
  (`boot-ok\n`), live SSE `data` event (`live\n`), then kill socat and observe
  disconnected status plus a finite `event: disconnected` stream.

The live test skips when `socat` is absent. It writes a fixed payload once and
relies on the server's idle timeout, so the test source cannot feed forever.

Controller (`python3 -m pytest tests/unit/controller/test_node_api_token_proxy.py`):

- serial entries in `PROXY_CASES` cover status/read/write token forwarding.
- `test_serial_proxies_forward_bounds_body_and_token_without_leak` covers query
  and body forwarding.
- `test_serial_proxy_preserves_typed_errors` covers 503/403/409/400/413 typed
  bodies, including the typed 403 distinction.
- `test_serial_stream_proxy_forwards_sse_and_token` covers SSE relay, forwarded
  `since`/`idle_ms`, content type, and token-free output.

## Remaining gaps

- One explicit device only; no auto-detection or multi-port sessions.
- No automatic reconnect after removal/crash; restart the node after fixing the
  device (the status/last_error remains observable until then).
- No modem control-line management, hardware/software flow-control selection,
  break control, or binary logging/rotation.
- The legacy Python `controller/serial_console.py` poller still requests
  `/serial/buffer`, which remains a typed 501; it has not been migrated to the
  new `/serial/read` cursor API.
- `/serial/buffer`, `/serial/send`, `/serial/state`, `/serial/text`, and
  `/serial/setup` are intentionally not aliased.
