# Ozma Display Architecture — Virtual & Passthrough

## Two display scenarios

### Scenario A: Virtual display (QEMU renders)

The VM has no physical GPU. QEMU emulates a VGA/virtio-gpu device and renders
to a framebuffer in host memory. The soft node reads this framebuffer directly.

```
QEMU process (host)
  │
  ├─ virtio-gpu device → framebuffer in host memory
  │                          │
  │                      soft node reads directly
  │                          │
  │                      ffmpeg → HLS/MJPEG → dashboard
  │
  ├─ input-linux (evdev) ← keyboard/mouse from controller
  │
  └─ QMP control socket ← power/status only
```

**No VNC. No SPICE. No remote desktop protocol.**

The framebuffer is just pixels in memory. Three ways to access it:

1. **QMP screendump** — `{"execute": "screendump", "arguments": {"filename": "/dev/stdout"}}` 
   writes a PPM to a file/pipe. Simple but slow (file I/O per frame).

2. **QEMU display D-Bus** — `-display dbus` exposes the framebuffer via D-Bus.
   The soft node connects as a D-Bus client and receives frame updates.
   Supports scanout (DMA-BUF), listeners, and resolution change notifications.

3. **Shared memory** — custom QEMU display plugin that writes frames to a
   shared memory region. The soft node mmap()s it and reads at will. Fastest.

4. **SPICE (fallback)** — SPICE is lighter than VNC and has a proper library
   API for frame capture + input injection. Good intermediate step.

### Scenario B: GPU passthrough (agent captures)

The VM has a physical GPU passed through via VFIO. QEMU can't see the
framebuffer — the GPU renders directly to its own VRAM. The **agent inside
the VM** captures the screen.

```
QEMU VM
  │
  ├─ GPU (VFIO passthrough) → renders to VRAM
  │                               │
  │                           Agent inside VM
  │                               │
  │                           ┌───┴────────────────────┐
  │                           │ Screen Capture          │
  │                           │                         │
  │                           │ Windows: DXGI Dup/NVFBC │
  │                           │ Linux: PipeWire/x11grab │
  │                           │ macOS: avfoundation     │
  │                           └───┬────────────────────┘
  │                               │
  │                           ffmpeg → stream
  │                               │
  │                           Agent HTTP API
  │                               │
  └─────────────────────────── Controller ← dashboard
```

**The critical problem: virtual display**

When a GPU is passed through but no physical monitor is connected, Windows
won't create a desktop — there's "no display". The agent has nothing to capture.

**Solution: Virtual Display Driver (IddCx)**

Windows 10+ supports Indirect Display Drivers (IddCx) — kernel drivers that
create virtual monitors. Windows thinks a real monitor is connected and renders
a full desktop to it. The agent captures this virtual display.

This is exactly how Sunshine/Apollo handles headless streaming:
1. Install an IddCx virtual display driver
2. Windows creates a desktop on the virtual monitor
3. Sunshine captures via DXGI Desktop Duplication
4. Encodes with NVENC and streams

**The ozma agent needs:**
1. A virtual display driver (ship with the agent installer)
2. DXGI Desktop Duplication capture (already in `screen_capture.py` via DXcam)
3. ffmpeg encoding → stream to controller
4. The controller treats it the same as any other stream source

## Implementation plan

### Phase 1: QEMU direct framebuffer (Scenario A)

Replace VNC with direct framebuffer access for virtual display VMs.

**Option 1: QMP screendump pipeline (simplest)**
```python
# soft_node.py — capture loop
while True:
    # QMP screendump to a pipe
    await qmp_control.screendump("/tmp/ozma-frame.ppm")
    # Read the PPM, convert to JPEG, push to MJPEG stream
    frame = read_ppm("/tmp/ozma-frame.ppm")
    await stream.push_frame(frame)
    await asyncio.sleep(1/20)  # 20 fps
```
Pro: works today, no new dependencies.
Con: file I/O per frame, limited to ~15fps.

**Option 2: SPICE (intermediate)**
```bash
qemu-system-x86_64 ... \
    -spice port=5930,disable-ticketing=on \
    -display none
```
Use `spice-gtk` or `pyvirt` to connect and receive frame updates.
SPICE also handles keyboard/mouse natively — replaces both VNC input AND evdev.
Pro: proper frame callbacks, resolution change handling, input built in.
Con: extra dependency (spice-gtk).

**Option 3: Custom display plugin (fastest)**
Write a QEMU display backend as a shared library (`.so`) that:
- Receives frame updates from QEMU's display subsystem
- Writes them to a Unix socket or shared memory region
- The soft node reads frames with zero copy

This is the V2 approach — maximum performance, zero overhead.

### Phase 2: Agent virtual display (Scenario B)

Ship a virtual display driver with the Windows agent.

**IddCx virtual display driver:**
- Kernel-mode driver (`.sys` + `.inf`)
- Creates a virtual monitor at configurable resolution
- Windows renders desktop to it via GPU
- Agent captures via DXGI Desktop Duplication

**Open source options:**
- Sunshine's virtual display driver (GPLv3)
- Virtual-Display-Driver (MIT) — community IddCx implementation
- ParsecVDD — Parsec's virtual display
- IddSampleDriver — Microsoft's sample

The agent installer (`ozma-agent.exe install`) should:
1. Install the IddCx driver
2. Configure resolution to match the controller's display settings
3. Start capturing from the virtual display
4. Stream to the controller

### Phase 3: Unified stream interface

The controller shouldn't care whether frames come from:
- QEMU direct framebuffer (Scenario A)
- Agent DXGI capture (Scenario B)  
- Hardware node HDMI capture (physical machine)
- VNC (legacy fallback)

All paths produce a stream URL. The dashboard plays it the same way.

```
Controller StreamManager
  │
  ├─ QEMUDirectCapture  — reads QEMU framebuffer via socket/SHM
  ├─ AgentStreamSource   — pulls from agent's HTTP stream endpoint  
  ├─ HardwareCapture     — ffmpeg from V4L2 capture card
  └─ VNCCapture          — legacy fallback (asyncvnc)
```

## Display + Input matrix

| VM Type | Display Capture | Input Injection | Dependencies |
|---------|----------------|-----------------|--------------|
| Virtual (no GPU) | QEMU framebuffer direct | evdev input-linux | None (host-side) |
| GPU passthrough | Agent DXGI + IddCx VDD | Agent SendInput | IddCx driver + agent |
| Physical machine | HDMI capture card | USB HID gadget | Hardware node |
| Legacy/fallback | VNC | VNC keyboard/mouse | asyncvnc |
