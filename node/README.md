# Node

Python daemon that runs on the node device. Listens for HID packets on UDP and writes them to the USB HID gadget (`/dev/hidg0` keyboard, `/dev/hidg1` mouse). Registers with the controller via mDNS or direct HTTP.

## Usage

```bash
python3 node.py \
    --name my-node \
    --register-url http://controller:7380

# In QEMU SLIRP environments where mDNS can't cross the SLIRP boundary:
python3 node.py \
    --name ozma-riscv-node \
    --register-url http://10.0.2.2:7380 \
    --register-host localhost
```

## HID packet format

UDP datagrams to port 7331 (or `--hid-udp-port`):

| Byte | Value | Meaning |
|------|-------|---------|
| 0 | `0x01` | Keyboard report follows |
| 1–8 | 8 bytes | Standard USB HID keyboard report |

| Byte | Value | Meaning |
|------|-------|---------|
| 0 | `0x02` | Mouse report follows |
| 1–6 | 6 bytes | HID absolute pointer report (buttons, X, Y, scroll) |

## HTTP API

The node exposes a status API on port 7382 (`--api-port`):

- `GET /` — node info (name, capabilities, firmware version)
- `GET /usb` — gadget and USB/IP attachment state
- `GET /video` — V4L2 capture device info

## Prerequisites

The node requires a USB HID gadget on the system:

- **Real hardware** (Milk-V Duo S, Raspberry Pi with OTG): hardware UDC, set up via `tinynode/gadget/setup_gadget.sh`
- **QEMU dev harness**: `dummy_hcd` kernel module + ConfigFS, set up by `dev/riscv-node/init-alpine.sh`

Required kernel modules: `libcomposite`, `usb_f_hid`, `dummy_hcd` (or hardware UDC).

## Files

```
node/
├── node.py         Main entry point and HTTP API server
├── usb_hid.py      USB HID gadget writer (/dev/hidg0, /dev/hidg1)
├── usb_audio.py    USB audio gadget interface
├── capture.py      V4L2 video capture
├── hw_detect.py    Hardware platform detection
└── __init__.py
```
