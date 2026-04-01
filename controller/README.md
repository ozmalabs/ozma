# Controller

FastAPI daemon that manages the node inventory, routes HID input to the active node, and exposes a REST + WebSocket API.

## Running

```bash
pip install -r requirements.txt
python -m uvicorn main:app --host 0.0.0.0 --port 7380 --reload
```

## API

Base URL: `http://<controller>:7380`

### Nodes

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/nodes` | List all registered nodes |
| `POST` | `/api/v1/nodes/register` | Node self-registration |
| `POST` | `/api/v1/switch/{node_id}` | Set the active node |
| `GET` | `/api/v1/nodes/{node_id}/usb` | USB gadget state for a node |

### HID input

HID packets are sent directly to the active node's UDP port (7331). The controller proxies keyboard/mouse events from the web UI → active node.

### WebSocket events

Connect to `ws://<controller>:7380/api/v1/events` for real-time events:

| Event | Meaning |
|-------|---------|
| `node.online` | Node registered |
| `node.offline` | Node stopped responding |
| `node.switched` | Active node changed |
| `scenario.activated` | Scenario activated |

## Discovery

The controller listens for mDNS announcements on `_ozma._udp.local.`. Nodes that can't use mDNS (e.g., QEMU SLIRP) POST to `/api/v1/nodes/register` directly with the same payload.

## Files

```
controller/
├── main.py         FastAPI app factory and startup
├── api.py          REST + WebSocket route handlers
├── discovery.py    mDNS discovery (zeroconf)
├── hid.py          HID keyboard/mouse forwarding
├── state.py        Node inventory and active-node state
├── stream.py       HLS video stream management
├── rgb.py          RGB peripheral control
├── scenarios.py    Scenario execution
├── keycodes.py     HID keycode mappings
├── config.py       Configuration management
├── scenarios.json  Scenario definitions
├── requirements.txt
└── tests/
    ├── inject_input.py   Send test HID packets to a node
    └── virtual_node.py   Minimal node stub for controller testing
```
