# Frigate NVR

Embedded NVR with real-time object detection using Hailo-8L or GPU inference.

## Hardware Acceleration

- **Hailo-8L**: Auto-detected when present (USB or M.2)
- **NVIDIA GPU**: Falls back to CUDA if Hailo not detected
- **CPU**: Falls back to CPU-only detection (slow)

## Quick Start

```bash
cd ozmaos/services/frigate
cp env.template .env
# Edit .env with your camera URLs and MQTT settings
docker compose up -d
```

Open http://localhost:5000

## Configuration

Edit `config/config.yml` for camera streams and detection settings. See [Frigate docs](https://docs.frigate.video/).

## Ports

| Port | Service |
|------|---------|
| 5000 | Frigate Web UI |
| 8554 | WebRTC |
| 1883 | MQTT |

## Volumes

- `./config` — Frigate configuration
- `./media` — Recordings and snapshots
- `mqtt-data` — Mosquitto persistence
- `mqtt-logs` — Mosquitto logs
