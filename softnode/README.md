# Soft Node

Emulates a hardware node using a QEMU VM. Instead of a real USB gadget, HID input is forwarded to the VM via QMP (keyboard/mouse injection or `usb-host` hotplug). Used for testing the full control path without hardware.

## Usage

Start a soft node against a running QEMU VM:

```bash
python3 softnode/soft_node.py \
    --name vm1 \
    --port 7332 \
    --qmp /tmp/ozma-vm1.qmp \
    --vnc-host 127.0.0.1 \
    --vnc-port 5901
```

The soft node registers with the controller via mDNS as `vm1._ozma._udp.local.` with `capabilities: ["qmp"]`.

## How it works

1. Announces itself via mDNS (`_ozma._udp.local.`) so the controller discovers it
2. Listens for HID UDP packets on `--port`
3. Translates HID reports to QMP keyboard/mouse events and injects them into the QEMU VM
4. Exposes the VM's VNC display coordinates to the controller for UI passthrough

## Files

```
softnode/
├── soft_node.py    Main daemon: mDNS + UDP listener + QMP bridge
├── hid_to_qmp.py  HID report → QMP key/mouse event translation
└── qmp_client.py  Async QMP socket client
```

## Dev harness integration

In `demo/start_vms.sh`, two soft nodes (vm1, vm2) are started alongside their QEMU VMs. The dev `Makefile` targets `make status` and `make stop` cover these.

To start manually:

```bash
# Start QEMU vm1 (Alpine x86_64, VNC :5901)
cd dev && bash target/launch.sh

# Start soft node for vm1
python3 softnode/soft_node.py \
    --name vm1 --port 7332 \
    --qmp /tmp/ozma-vm1.qmp \
    --vnc-host 127.0.0.1 --vnc-port 5901
```
