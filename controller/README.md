# Controller

FastAPI daemon that manages the node inventory, routes HID input to the active node, and exposes a REST + WebSocket API.

## Running

```bash
uv pip install -r requirements.txt
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

### Supervisor multi-VM view (read-only)

Set `OZMA_SUPERVISOR_STATUS_URL` to a supervisor's status document, for example
`http://127.0.0.1:9555/status`. The controller fetches that document and returns
it unchanged; it does not append routes, parse supervisor internals, poll the
per-VM children, or expose any control action.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/supervisor/status` | Proxied supervisor `/status`: `{ok, configured, status}` |

`OZMA_SUPERVISOR_STATUS_TOKEN` optionally adds `Authorization: Bearer ...` to
the upstream request; the token is never returned to the browser or logged.
With no URL configured the route returns `503`; an unreachable or invalid
supervisor returns `502` with a clear `detail`. The dashboard's "Virtual
Machines" panel (Advanced/Amazing modes) renders the proxied VM names, ports,
supervisor state, child HLS/WebRTC/audio/power/input summary, and honest
`last_seen`/`stale`/error fields. There are no per-VM reset/restart controls yet.

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
