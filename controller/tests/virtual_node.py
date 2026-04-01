#!/usr/bin/env python3
"""
Virtual Ozma compute node for local testing.

Announces _ozma._udp.local. via mDNS so the Controller discovers it,
then listens on UDP 7331 and prints decoded HID packets to stdout.

Usage:
  python tests/virtual_node.py [--port 7331] [--name test-node]

The Controller will pick it up automatically. Switch to it with:
  curl -X POST http://localhost:7380/api/v1/nodes/<name>/activate
"""

import argparse
import signal
import socket
import struct
import sys
import time
from zeroconf import ServiceInfo, Zeroconf

PROTO_VERSION = 1

# Button bit positions in mouse report byte 0
MOUSE_BUTTONS = {0: "LEFT", 1: "RIGHT", 2: "MIDDLE"}


def decode_keyboard(payload: bytes) -> str:
    if len(payload) < 8:
        return f"[short keyboard payload: {payload.hex()}]"
    modifier = payload[0]
    keys = [payload[i] for i in range(2, 8) if payload[i] != 0]
    mods = []
    mod_names = ["LCtrl", "LShift", "LAlt", "LGui", "RCtrl", "RShift", "RAlt", "RGui"]
    for i, name in enumerate(mod_names):
        if modifier & (1 << i):
            mods.append(name)
    return f"KBD mod={'+'.join(mods) or '0'} keys=[{', '.join(f'0x{k:02X}' for k in keys)}]"


def decode_mouse(payload: bytes) -> str:
    if len(payload) < 6:
        return f"[short mouse payload: {payload.hex()}]"
    buttons = payload[0]
    x = payload[1] | (payload[2] << 8)
    y = payload[3] | (payload[4] << 8)
    scroll = struct.unpack_from("b", payload, 5)[0]  # signed
    btn_names = [MOUSE_BUTTONS[i] for i in range(3) if buttons & (1 << i)]
    return f"MOUSE x={x:5d} y={y:5d} scroll={scroll:+d} buttons=[{','.join(btn_names) or 'none'}]"


def run(host: str, port: int, name: str) -> None:
    # Bind UDP socket first so we fail fast if port is in use
    import sys
    sys.stdout.reconfigure(line_buffering=True)

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind((host, port))
    sock.settimeout(1.0)
    print(f"[virtual-node] UDP listening on {host}:{port}")

    # Announce via mDNS
    local_ip = socket.gethostbyname(socket.gethostname())
    service_name = f"{name}._ozma._udp.local."
    info = ServiceInfo(
        "_ozma._udp.local.",
        service_name,
        addresses=[socket.inet_aton(local_ip)],
        port=port,
        properties={
            "proto": str(PROTO_VERSION),
            "role": "compute",
            "hw": "virtual",
            "fw": "0.0.1-test",
            "cap": "",
        },
    )
    zc = Zeroconf()
    zc.register_service(info)
    print(f"[virtual-node] mDNS announced as '{service_name}' @ {local_ip}:{port}")
    print(f"[virtual-node] Activate with:")
    print(f"  curl -X POST http://localhost:7380/api/v1/nodes/{service_name}/activate")
    print()

    stop = False

    def _on_signal(sig, _frame):
        nonlocal stop
        stop = True

    signal.signal(signal.SIGINT, _on_signal)
    signal.signal(signal.SIGTERM, _on_signal)

    last_mouse_x = -1
    last_mouse_y = -1

    while not stop:
        try:
            data, addr = sock.recvfrom(64)
        except socket.timeout:
            continue
        except OSError:
            break

        if not data:
            continue

        ptype = data[0]
        payload = data[1:]

        if ptype == 0x01:
            decoded = decode_keyboard(payload)
            # Only print non-trivial reports (suppress all-zero)
            if payload[0] != 0 or any(payload[2:]):
                print(f"  {decoded}")
        elif ptype == 0x02:
            decoded = decode_mouse(payload)
            x = payload[1] | (payload[2] << 8)
            y = payload[3] | (payload[4] << 8)
            # Suppress mouse move spam — only print if position changed significantly
            if abs(x - last_mouse_x) > 50 or abs(y - last_mouse_y) > 50 or payload[0]:
                print(f"  {decoded}")
                last_mouse_x = x
                last_mouse_y = y
        else:
            print(f"  UNKNOWN type=0x{ptype:02X} payload={payload.hex()}")

    print("\n[virtual-node] Shutting down")
    zc.unregister_service(info)
    zc.close()
    sock.close()


def main() -> None:
    p = argparse.ArgumentParser(description="Virtual Ozma compute node")
    p.add_argument("--port", type=int, default=7331)
    p.add_argument("--host", default="0.0.0.0")
    p.add_argument("--name", default="virtual-node-1")
    args = p.parse_args()
    run(args.host, args.port, args.name)


if __name__ == "__main__":
    main()
