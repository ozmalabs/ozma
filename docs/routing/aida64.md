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

## AIDA64 shared memory / Registry / WMI

AIDA64's native Windows exports (`AIDA64_SensorValues` shared memory, Registry,
WMI, RTSS) are not emulated. The HTTP/SSE RemoteSensor surface is the portable
protocol and works on every node platform. A Windows-only shared-memory writer
can be added later without changing the sensor model.
