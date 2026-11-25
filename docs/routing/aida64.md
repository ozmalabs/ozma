# AIDA64-compatible sensors and panels

**Status:** Implemented as an opt-in node listener.

The node publishes the same hardware snapshot in AIDA64's documented sensor
vocabulary. This lets existing AIDA64 RemoteSensor viewers, ESP32 panels, and
phone/tablet dashboards consume Ozma data, while the generated Ozma layout uses
the existing gauge/bar/number widget schema.

## Enable

```bash
ozma-node --profile desktop \
  --aida64-port 8080 \
  --aida64-bind 0.0.0.0
```

`--aida64-port 0` (the default) disables the listener. The listener is
unauthenticated because AIDA64 RemoteSensor clients do not send an
`Authorization` header; enable it only on a trusted network or bind it to
loopback behind a local reverse proxy.

## Endpoints

| Path | Purpose |
|---|---|
| `/` | AIDA64 RemoteSensor-compatible HTML panel with `SimpleN` spans |
| `/sse` | `text/event-stream`, `Page0|{\|}Simple1|value|{\|}...` |
| `/api` | JSON sensor map; supports `?sensors=TCPU,SCPUUTI` and `?callback=fn` JSONP |
| `/sensors` | Full JSON snapshot with ids, labels, values, units, colors |
| `/panel` | Ozma `ScreenLayout` JSON generated from semantic `WidgetIR` |
| `/panel/data` | `@active.<AIDA id>` field values for the panel |

AIDA64 values use the documented IDs, including `SDATE`, `STIME`, `SUPTIME`,
`SCPUUTI`, `SCC-1-01`, `SMEMUTI`, `SUSEDMEM`, `SMOBONAME`, `TCPU`, `TCPUPKG`,
`TCC-1-01`, `TGPU1`, `TGPU1HOT`, `SGPU1UTI`, `TMOBO`, `TVRM`, `TPCH`, `FCPU`,
`FGPU1`, `VCPU`, `VP12V`, `PCPUPKG`, `PGPU1`, `THDD1`, and `SNIC1DL`.

Values that the hardware snapshot cannot provide are omitted rather than
reported as fake zeroes.

## Ozma widget panel

`GET /panel` emits a layout like:

```json
{
  "id": "aida64-panel",
  "name": "AIDA64 Sensor Panel",
  "width": 680,
  "height": 480,
  "background": "#0a0a0f",
  "refresh_hz": 2,
  "fields": [
    { "key": "@active.TCPU", "type": "real", "unit": "°C", "purpose": "value" }
  ],
  "widgets": [
    {
      "type": "gauge", "ir_kind": "gauge", "ir_id": "aida64-tcpu",
      "metric": "@active.TCPU", "label": "CPU", "min": 0, "max": 100,
      "warn_color": "#f0b94a", "crit_color": "#f26464"
    }
  ]
}
```

`/panel` is built from `WidgetIR` (`panel_ir_widgets`), then mapped through the
same `ScreenLayout` projection as other Ozma sources. Each widget carries
`ir_kind`/`ir_id`, thresholds become `warn_color`/`crit_color`, and fields are
the canonical array form. The browser display adapter, controller screen
manager, and any other widget renderer already understand this schema.

The node also merges numeric AIDA IDs into `/agent/metrics`, so the controller's
metric bus can resolve `@active.TCPU`, `@active.SGPU1UTI`, `@active.SMEMUTI`,
and the other IDs directly.

## Windows shared memory / Registry bridge (optional)

**Status:** Portable protocol implemented; the Win32 backend is implemented but
off by default and is not started by the node or by the AIDA64 HTTP listener.

AIDA64's native Windows exports are private implementation details. In
particular, the `AIDA64_SensorValues` shared-memory record, AIDA64's Registry
values, and its WMI classes are not documented by FinalWire as stable public
formats. This bridge **does not claim compatibility with any of those native
layouts** and hard-codes no AIDA64 field offsets. It publishes the same
`AidaSnapshot` model through a documented Ozma layout for Windows consumers.

The AIDA64-compatible surface remains the RemoteSensor HTTP/SSE listener above.
It works on every platform and does not require this bridge. Consumers that
expect AIDA64's own shared memory, Registry, or WMI objects will not find them
here.

### Shared memory

The Windows backend creates the named mapping
`Local\Ozma_Aida64_SensorValues` with `CreateFileMappingW`/`MapViewOfFile`.
The mapping contains a 24-byte header followed by the encoded snapshot:

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | magic `OZMASHRM` |
| 8 | 2 | header version `1` (little-endian) |
| 10 | 2 | flags, currently `0` |
| 12 | 4 | sequence counter (odd while writing, even when stable) |
| 16 | 4 | payload length in bytes |
| 20 | 4 | reserved, currently `0` |
| 24 | n | encoded snapshot payload |

Writers use a seqlock-style sequence: the counter goes odd, the length and
payload are copied, and the counter goes even. Readers should retry if the
sequence is odd or changes while reading, then validate the payload header.

The payload is a versioned binary map with its own header:

| Offset | Size | Field |
|---|---|---|
| 0 | 8 | magic `OZMAAIDA` |
| 8 | 2 | protocol version `1` |
| 10 | 2 | flags, currently `0` |
| 12 | 4 | sensor count |
| 16 | 8 | snapshot timestamp as little-endian `f64` |
| 24 | n | sensors, sorted by id |

Each sensor stores bounded UTF-8 strings for `id`, `label`, `unit`, `kind`,
and `color` (each as a `u16` byte length followed by bytes), then a value tag:
`0` means a little-endian `f64`, `1` means a bounded UTF-8 string. Encoding is
deterministic, strings and sensor counts are bounded, and truncated, oversized,
duplicate-id, malformed-UTF-8, and non-finite inputs are rejected.

### Registry

The backend creates `HKCU\Software\Ozma\Aida64\SensorValues` (no administrator
rights required) and writes a flat, deterministic value map:

| Value | Type | Meaning |
|---|---|---|
| `@format` | `REG_SZ` | `ozma-aida64-sensor-map-v1` |
| `@timestamp` | `REG_BINARY` | snapshot timestamp, 8-byte little-endian `f64` |
| `@sensor_count` | `REG_BINARY` | sensor count as an `f64` |
| `sensor.<id>.value` | `REG_BINARY` or `REG_SZ` | numeric `f64` or text value |
| `sensor.<id>.label` | `REG_SZ` | sensor label |
| `sensor.<id>.unit` | `REG_SZ` | unit |
| `sensor.<id>.kind` | `REG_SZ` | sensor kind |
| `sensor.<id>.color` | `REG_SZ` | display color |

Numeric Registry values are stored as 8-byte little-endian `f64` in
`REG_BINARY`; strings are UTF-16 `REG_SZ`. Each publish overwrites the same
names, so the last write wins.

### API and limits

On Windows, Rust callers can opt in explicitly (`start_bridge` is compiled only
on Windows; the protocol types are portable):

```rust
use ozma_node::aida64_windows::{start_bridge, WindowsBridgeConfig};

let config = WindowsBridgeConfig {
    shared_memory: true,
    registry: true,
    max_bytes: 1024 * 1024,
};
tokio::spawn(start_bridge(hardware, shutdown, config));
```

`WindowsBridgeConfig::default()` leaves both transports off. `max_bytes` is
clamped to 4 KiB–16 MiB, and the encoded payload itself has a 4 MiB hard cap.
The bridge polls the hardware snapshot once per second and publishes; publish
and mapping failures are logged and returned as typed errors rather than
panicking. The shared-memory backend cannot be exercised on Linux; only the
portable protocol and fake-sink tests run there.

### WMI

WMI is explicitly **not implemented**. A WMI provider is not a file or Registry
writer: it is a COM server plus a MOF class declaration that must be registered
with WMI, normally by an installer with the required privileges. Supplying one
would require a separate installable provider component, not just this bridge.
The bridge therefore makes no WMI claims.
