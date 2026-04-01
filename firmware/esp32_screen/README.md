# Ozma ESP32 Screen Firmware

Firmware for ESP32 + TFT display modules that renders ozma status
panels locally at 60fps.  The controller pushes a UI layout definition
once; the ESP32 renders widgets from data-only updates over WebSocket.

## Hardware

- ESP32-S3 (recommended) or ESP32
- TFT display: ST7789, ILI9341, or similar SPI display (240x240 to 480x320)
- Optional: WS2812 status LED strip

## Protocol

Connect to `ws://controller:7391` and send:

```json
{"type": "register", "device_id": "esp32-desk-1", "width": 320, "height": 240,
 "capabilities": ["gauge", "bar", "vu_meter", "label", "number"]}
```

Receive layout definition:
```json
{"type": "layout", "layout": { ...widgets... }}
```

Receive data updates at refresh_hz:
```json
{"type": "data", "d": {"cpu_temp": 65.2, "ram_pct": 72}}
```

Receive scenario changes:
```json
{"type": "scenario", "id": "gaming", "name": "Gaming", "color": "#E04040"}
```

## Widget Rendering

The firmware implements local rendering for each widget type:

| Widget | Rendering |
|--------|-----------|
| gauge | Arc with percentage fill, value text, label |
| bar | Horizontal/vertical filled rectangle |
| vu_meter | Segmented level bar with peak hold |
| label | Text with variable interpolation |
| number | Large numeric display with unit |
| sparkline | Line chart from recent values (ring buffer) |

## Build

Uses PlatformIO:
```
cd firmware/esp32_screen
pio run
pio run --target upload
```

## Libraries

- TFT_eSPI (display driver)
- ArduinoJson (protocol parsing)
- WebSocketsClient (connection to controller)
- LVGL (optional — for complex layouts)
