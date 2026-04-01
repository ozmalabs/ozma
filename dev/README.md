# Ozma Dev Harness

QEMU-based development environment that emulates the full hardware stack: a RISC-V node VM with real USB gadget modules, target VMs (vm1, vm2), and the USB/IP chain connecting them.

## Architecture

```
Host (x86_64)
├── controller/  (python uvicorn, port 7380)
├── softnode/    (vm1: QEMU x86_64, QMP /tmp/ozma-vm1.qmp, VNC :5901)
│                (vm2: QEMU x86_64, QMP /tmp/ozma-vm2.qmp, VNC :5902)
└── RISC-V VM    (QEMU riscv64, SSH :2222, HID UDP :7331, API :7382)
    ├── dummy_hcd + ConfigFS HID gadget (/dev/hidg0, /dev/hidg1)
    ├── usbipd (TCP 3240, SLIRP-forwarded to host port 3240)
    └── node.py  (receives UDP HID → writes /dev/hidg0)
         ↕ USB/IP
Host vhci_hcd  (usbip attach -r 127.0.0.1 -b 1-1)
         ↕ QMP device_add
vm1 USB EHCI   (sees 1d6b:0104 Ozma TinyNode)
```

## Prerequisites

Install on the host (Arch Linux):

```bash
sudo pacman -S qemu-system-riscv qemu-system-x86 usbip python3
```

Optional (for cross-compiling kernel modules):

```bash
sudo pacman -S riscv64-linux-gnu-gcc bc flex bison pahole
```

## First-time setup

```bash
cd dev

# 1. Generate SSH key for VM access
make ssh-key

# 2. Build the RISC-V Alpine disk image (requires sudo, ~10 min)
#    Builds: images/riscv-node.qcow2 + images/riscv-vmlinuz-lts + images/riscv-initramfs-lts
make build-node-image

# 3. (Optional) Rebuild kernel modules if the Alpine kernel version changes
bash kernel-build/build-gadget-modules.sh
```

The disk image includes cross-compiled USB gadget modules baked in:
`dummy_hcd`, `libcomposite`, `usb_f_hid`, `usb_f_uac2`, `usbip_core`, `usbip_host`, `vhci_hcd`

## Running the full stack

```bash
# Start RISC-V node VM (boots in ~15s)
make node-vm

# Wait for boot, then connect USB gadget to vm1:
make connect-vms          # default target: vm1
make connect-vms ARGS="--target vm2"   # or vm2

# Disconnect
make disconnect-vms
```

`connect-vms` does the following automatically:
1. Starts the RISC-V VM if not already running
2. Waits for SSH
3. Runs `init-alpine.sh` inside the VM (modules + gadget + usbipd + node.py)
4. Attaches the gadget to the host via USB/IP (`sudo usbip attach -r 127.0.0.1`)
5. Hotplugs the USB device into vm1 via QMP (`device_add usb-host`)

## Common commands

```bash
make logs           # tail node.py log inside the RISC-V VM
make shell-node     # SSH into the RISC-V VM (root@localhost:2222)
make status         # show running VM PIDs
make stop           # stop all VMs

# Manually SSH
ssh -i images/dev_key -p 2222 root@localhost
```

## Network map (SLIRP mode)

| Host port | → VM port | Service |
|-----------|-----------|---------|
| 2222 TCP | 22 | RISC-V VM SSH |
| 7331 UDP | 7331 | HID input to node.py |
| 7382 TCP | 7382 | Node HTTP API |
| 3240 TCP | 3240 | usbipd (USB/IP export) |

## Files

```
dev/
├── Makefile                    Top-level dev commands
├── config.env                  Shared port + resource config (sourced by all scripts)
├── riscv-node/
│   ├── launch.sh               Start the RISC-V QEMU VM
│   ├── init-alpine.sh          Per-boot init: modules + gadget + usbipd + node.py
│   ├── connect-to-vms.sh       Full USB chain setup (USB/IP + QMP hotplug)
│   └── provision.sh            First-time package install in VM (unused with baked image)
├── target/
│   ├── launch.sh               Start an x86_64 Alpine target VM
│   ├── provision.sh            Install usbip on target VM
│   └── attach-usb.sh           Manually attach USB via USB/IP to target
├── scripts/
│   ├── build-riscv-image.sh    Build Alpine RISC-V disk image (needs sudo)
│   ├── check-deps.sh           Verify host tools are installed
│   ├── fetch-images.sh         Download pre-built target VM image
│   ├── feed-video.sh           Push test pattern into v4l2loopback
│   └── setup-tap.sh            Create/destroy TAP network interface
└── kernel-build/
    ├── build-gadget-modules.sh Cross-compile USB gadget modules for Alpine riscv64
    └── alpine-riscv64-lts.config  Alpine kernel .config base
```

## TAP networking (optional)

SLIRP mode works without root but mDNS discovery across VM boundaries doesn't. For TAP mode (real bridge, mDNS works):

```bash
sudo make tap-up
NETWORK_MODE=tap make node-vm
```

## Rebuilding the RISC-V image

The image bakes in:
- Alpine Linux riscv64 minirootfs
- `linux-lts` kernel + initrd (extracted separately for QEMU `-kernel`)
- Cross-compiled USB gadget `.ko.gz` modules
- `node/` + `tinynode/gadget/` source copied to `/root/ozma-node/`
- SSH authorized key from `images/dev_key.pub`
- `/etc/local.d/ozma.start` → runs `init-alpine.sh` on boot

If the Alpine kernel version changes, the cross-compiled modules must be rebuilt with the matching kernel version and `Module.symvers`:

```bash
# Get Module.symvers from the VM
ssh -i images/dev_key -p 2222 root@localhost \
    'find /usr/src/linux-headers-* -name Module.symvers' | head -1

# Then rebuild
bash dev/kernel-build/build-gadget-modules.sh
```
