# qemu-kvmfr — QEMU display backend for Looking Glass

A QEMU display backend that writes the VM's framebuffer to IVSHMEM shared
memory in Looking Glass KVMFR format. Any Looking Glass client can read
frames from the host side — **no guest software needed** for emulated GPUs.

## What this solves

When a VM uses an emulated GPU (VGA, virtio-gpu, QXL), QEMU has the
framebuffer in memory but the only way to get it out is VNC, SPICE,
or D-Bus — all protocol-based with encoding overhead.

This backend writes raw pixels directly to shared memory in the same
format that Looking Glass uses. Zero protocol overhead, zero encoding,
sub-millisecond frame availability.

## Architecture

```
QEMU process
  │
  ├─ Emulated GPU renders to DisplaySurface (pixman image)
  │
  ├─ kvmfr display backend (this code)
  │     │
  │     ├─ Copies dirty regions to IVSHMEM shared memory
  │     ├─ Writes KVMFR frame headers (compatible with Looking Glass)
  │     ├─ Writes cursor position + shape
  │     └─ Runs LGMP heartbeat (10ms timer)
  │
  └─ IVSHMEM PCI device (-device ivshmem-plain)
        │
        └─ Guest sees a PCI BAR mapped to the same SHM file
           (guest-side Looking Glass host can write here too —
            for GPU passthrough, the guest writes; for emulated
            GPU, this backend writes)
```

Both the display backend and the IVSHMEM device access the same
`/dev/shm/looking-glass` file. They don't interact with each other.

## Usage

```bash
# 1. Create shared memory
truncate -s 32M /dev/shm/looking-glass

# 2. Run QEMU with kvmfr backend + ivshmem device
KVMFR_SHM_PATH=/dev/shm/looking-glass KVMFR_SHM_SIZE=32 \
qemu-system-x86_64 \
    -display none \
    -object memory-backend-file,id=lg-mem,share=on,\
            mem-path=/dev/shm/looking-glass,size=32M \
    -device ivshmem-plain,memdev=lg-mem \
    -vga virtio \
    ...

# 3. Read frames from any Looking Glass client, or ozma:
python3 -c "
from softnode.looking_glass import LookingGlassCapture
lg = LookingGlassCapture('vm1')
print('available:', lg.available)
"
```

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `KVMFR_SHM_PATH` | `/dev/shm/looking-glass` | Path to shared memory file |
| `KVMFR_SHM_SIZE` | `32` | SHM size in MB |

## Multi-monitor

QEMU VMs with multiple display heads (e.g., `virtio-gpu-pci,max_outputs=2`)
create multiple `QemuConsole` instances. The backend registers a separate
`DisplayChangeListener` for each console. Currently all consoles share one
SHM region; a future version will support separate regions per head.

## Emulated GPU vs GPU passthrough

| Scenario | Who writes to SHM? | Guest software needed? |
|----------|--------------------|-----------------------|
| Emulated GPU (VGA, virtio-gpu) | **This backend** (host-side) | No |
| GPU passthrough (VFIO) | Looking Glass Host (guest-side) | Yes |
| Passthrough + RAMFB (boot phase) | **This backend** (RAMFB is emulated) | No |

For passthrough VMs, this backend provides boot-phase display (BIOS/UEFI
via RAMFB). Once the passthrough GPU driver loads and the guest-side
Looking Glass Host starts, it takes over writing to the same SHM region.

## Building

This is a QEMU in-tree module. It needs to be built as part of QEMU:

```bash
# Link into QEMU source tree
make setup QEMU_SRC=~/qemu

# Add to ~/qemu/ui/meson.build:
#   kvmfr_ss = ss.source_set()
#   kvmfr_ss.add(when: [pixman], if_true: files('kvmfr.c'))
#   ui_modules += {'kvmfr' : kvmfr_ss}

# Build
cd ~/qemu/build && ninja
```

## Wire format

The shared memory uses the LGMP (Looking Glass Message Protocol) format
with KVMFR user data. This is binary-compatible with Looking Glass clients.

See `kvmfr.c` for the full structure definitions. Key types:

- `LGMPHeader` — magic, version, session ID, queue descriptors
- `KVMFRFrame` — frame dimensions, format (BGRA), stride, damage rects
- `FrameBuffer` — atomic write pointer + raw pixel data
- `KVMFRCursor` — cursor position, hotspot, shape (RGBA pixels)

Frame format is `FRAME_TYPE_BGRA` (matches QEMU's `PIXMAN_x8r8g8b8`).
Double-buffered: two frame slots, toggled per frame.

## License

GPL-2.0-or-later (required for QEMU modules that use internal headers).

Copyright (C) 2024-2026 Ozma Labs Pty Ltd.
