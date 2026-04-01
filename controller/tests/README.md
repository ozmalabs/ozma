# Local testing without hardware

## Quick start (two terminals)

**Terminal 1** — virtual node:
```bash
cd controller
python tests/virtual_node.py
```

**Terminal 2** — run the demo:
```bash
cd controller
python tests/inject_input.py
```

`inject_input.py` sends UDP packets directly to port 7331 — no kernel input
devices, nothing touches your display server or physical keyboard.

---

## Full stack test (three terminals)

Verify the Controller discovery, activation, and forwarding path.

**Terminal 1** — virtual node (mDNS + UDP listener):
```bash
python tests/virtual_node.py
```

**Terminal 2** — Controller in virtual-only mode:
```bash
python main.py --debug --virtual-only
```

`--virtual-only` limits evdev capture to `ozma-virtual-*` devices only.
Your physical keyboard and mouse are left alone.

**Terminal 3** — activate the node, then inject:
```bash
# Check what was discovered (wait ~5s after starting):
curl http://localhost:7380/api/v1/nodes | python -m json.tool

# Activate:
curl -X POST http://localhost:7380/api/v1/nodes/virtual-node-1._ozma._udp.local./activate

# Run demo — sends UDP directly to the virtual node via the Controller path:
python tests/inject_input.py

# Or interactive:
python tests/inject_input.py --interactive
```

Interactive commands:
```
key A
key ENTER
key F5
ctrl+c
ctrl+z
type Hello World
mouse 16383 16383
click left
click right
scroll 3
scroll -2
quit
```

---

## What to verify

- Virtual node shows decoded keyboard and mouse packets
- `h-e-l-l-o` = HID usage IDs `0x0B 0x08 0x0F 0x0F 0x12`
- Ctrl+C = `mod=LCtrl keys=[0x06]`
- Mouse coords in 0–32767 range
- WebSocket events fire on activation:
  ```bash
  websocat ws://localhost:7380/api/v1/events
  ```
