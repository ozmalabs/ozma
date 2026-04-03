# Multi-Seat Single PC — Implementation Plan

> 4 users, 1 PC, no VMs. Windows + Linux. Each seat = an ozma node.

## Architecture

```
Physical PC running ozma-agent
┌─────────────────────────────────────────────────────┐
│  SeatManager                                        │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐          │
│  │  Seat 0  │  │  Seat 1  │  │  Seat 2  │  ...     │
│  │ Display 0│  │ Display 1│  │ Display 2│          │
│  │ KB+Mouse │  │ KB+Mouse │  │ KB+Mouse │          │
│  │ Audio    │  │ Audio    │  │ Audio    │          │
│  │ UDP:7331 │  │ UDP:7332 │  │ UDP:7333 │          │
│  │ HTTP:7382│  │ HTTP:7383│  │ HTTP:7384│          │
│  └──────────┘  └──────────┘  └──────────┘          │
│                                                     │
│  USBTopologyScanner — groups devices by hub          │
│  DisplayManager — enumerates + creates displays      │
│  InputRouter — routes physical devices to seats       │
│  AudioMixer — per-seat virtual sinks                  │
└─────────────────────────────────────────────────────┘
         │              │              │
    Controller     Controller     Controller
    (sees 3 independent nodes — zero controller changes)
```

### Core Principle

Each seat registers as a separate ozma node via `POST /api/v1/nodes/register`. The controller sees N independent nodes and routes to them identically to physical machines or VMs. All multi-seat complexity lives in the agent.

### Cross-Platform Abstraction Layers

| Subsystem | Interface | Linux Backend | Windows Backend |
|-----------|-----------|---------------|-----------------|
| Display enumeration | `DisplayBackend` | Xrandr / DRM | DXGI / SetupAPI |
| Virtual display | `VirtualDisplayBackend` | Xorg multi-screen / xf86-video-dummy | IddCx driver |
| Screen capture | `SeatCaptureBackend` | x11grab per-screen / PipeWire | DXGI Desktop Duplication per-output |
| Input routing | `InputRouterBackend` | evdev device assignment | Raw Input API per-device |
| USB topology | `USBTopologyBackend` | sysfs `/sys/bus/usb/devices/` | SetupAPI / WMI |
| Audio isolation | `SeatAudioBackend` | PipeWire virtual sinks | WASAPI virtual endpoints |
| HID injection | `SeatHIDInjector` | uinput (per-seat X display) | SendInput (per-seat monitor region) |

---

## File Layout

```
agent/
  multiseat/
    __init__.py
    seat_manager.py        # SeatManager: lifecycle, auto-detection, CLI
    seat.py                # Seat dataclass + per-seat node lifecycle
    display_backend.py     # ABC for display enumeration + virtual display
    display_linux.py       # Xrandr/DRM display backend
    display_windows.py     # DXGI + IDD display backend
    capture_backend.py     # ABC for per-output screen capture
    capture_linux.py       # x11grab/PipeWire per-screen capture
    capture_windows.py     # DXGI Desktop Duplication per-output
    input_router.py        # ABC for input device → seat routing
    input_linux.py         # evdev device assignment
    input_windows.py       # Raw Input API routing
    usb_topology.py        # Cross-platform USB hub grouping
    audio_backend.py       # ABC for per-seat audio
    audio_linux.py         # PipeWire virtual sinks
    audio_windows.py       # WASAPI virtual endpoints
    gamepad_router.py      # XInput/evdev gamepad → seat mapping
    seat_profiles.py       # gaming/workstation/media profiles

drivers/
  idd/
    ozma-idd/              # Rust IddCx driver (virtual-display-rs fork)
    ozma-idd-installer/    # NSIS/WiX installer
    README.md              # Build + signing instructions
```

---

## Phase Breakdown

### Phase 1: Foundation + Linux (2 weeks)

Seat abstraction, USB topology scanning, working multi-seat on Linux.

- **`seat.py`** — Seat dataclass: display index, input devices, audio sink, UDP port, HTTP port, node ID. Lifecycle: `start()`, `stop()`, `register()`.
- **`seat_manager.py`** — Replaces single `DesktopSoftNode` when `--multi-seat` flag is passed. Discovers displays + inputs, creates one Seat per display, each runs its own UDP listener + HTTP server + node registration.
- **`usb_topology.py`** — Groups USB devices by shared hub. Linux: sysfs. Windows: SetupAPI (stub for Phase 2).
- **`display_linux.py`** — Enumerate via `xrandr --listactivemonitors`. Map each to an X screen (`:0.0`, `:0.1`).
- **`input_linux.py`** — Assign evdev devices to seats via Xorg `InputDevice` sections.
- **`audio_linux.py`** — PipeWire null sink per seat (reuses existing `AudioBackendLinux` pattern).
- **`capture_linux.py`** — Per-screen x11grab: `ffmpeg -f x11grab -i :0.N`.

**Deliverable**: `ozma-agent --multi-seat` on Linux with 2+ monitors → 2+ nodes in dashboard, independent capture/audio/input.

### Phase 2: Windows Display + Capture (3 weeks)

DXGI Desktop Duplication for per-output capture. Physical multi-head first (no IDD yet).

- **`display_windows.py`** — Enumerate via DXGI (`IDXGIFactory::EnumAdapters` + `EnumOutputs`). Pure ctypes.
- **`capture_windows.py`** — DXGI Desktop Duplication per-output. The existing `dxcam` backend already supports `output_idx=N`. Multi-seat creates one camera per seat.
- **`input_windows.py`** — Raw Input API via ctypes. `RegisterRawInputDevices` with per-device handles. Manual assignment initially, auto-detection in Phase 4.
- **`audio_windows.py`** — HDMI audio follows display. USB audio follows USB topology. Virtual audio deferred.

**Deliverable**: Windows PC with 2+ physical monitors → `ozma-agent --multi-seat` creates independent nodes with DXGI capture.

### Phase 3: IDD Virtual Display Driver (4 weeks, parallel with Phase 4)

Virtual monitors on Windows without physical displays. This is the hardest part.

**Development path:**
- Fork `virtual-display-rs` (Rust IddCx, MIT licensed)
- Modify for multiple virtual monitors with configurable resolution
- Build with Windows Driver Kit (WDK)
- Install in test-signing mode (`bcdedit /set testsigning on`)

**Production signing:**
- **Attestation signing** (Microsoft Partner Center): ~$200, 1-2 weeks. Signs for Windows 10 1607+.
- **WHQL** (HLK testing): ~$500, 2-4 weeks. Required for Windows 11 Secure Core.

**IDD control interface:** Named pipe (`\\.\pipe\ozma-idd`). Agent sends:
- `ADD_MONITOR {width} {height} {refresh}` → create virtual display
- `REMOVE_MONITOR {index}` → destroy it
- `LIST` → enumerate active virtual monitors

DXGI DD captures virtual displays identically to physical ones.

**Fallbacks if IDD is blocked:**
- Dummy HDMI plugs ($3 each) — physical monitors not needed
- Parsec Virtual Display Driver (widely deployed, already signed)
- amyuni USB Mobile Monitor (signed, free for open source)

### Phase 4: Auto-Detection (2 weeks)

Plug in keyboard+mouse → seat appears within 3 seconds.

- **USB hotplug monitoring**: `pyudev` (Linux), `RegisterDeviceNotification`/WMI (Windows)
- **Hub grouping algorithm**: devices on same hub → candidate seat → find next available display → create seat
- **Seat persistence**: `seats.json` — remembers USB path → seat mapping across reboots
- **Virtual display on-demand**: if no unclaimed physical display, create IDD virtual monitor

### Phase 5: Audio Isolation (2 weeks)

Per-seat audio on both platforms.

**Linux**: Already solved in Phase 1 via PipeWire virtual sinks. Enhancement: per-process routing via `pw-metadata` or `PIPEWIRE_RUNTIME_DIR` scoping.

**Windows** (three tiers):
1. HDMI/DP audio follows display (free, works today)
2. USB audio follows USB topology (free, USB headsets/DACs)
3. Per-process routing via `IAudioSessionControl::SetProcessDefaultAudio` (Windows 11 22H2+) or `IPolicyConfig` (older Windows)

Virtual audio driver deferred — tiers 1+2 cover most physical setups.

### Phase 6: Game Launcher + Profiles (2 weeks)

One-click game launch from the dashboard, scoped to a seat.

- **Launch environment**: `DISPLAY=:0.N` (Linux) or monitor affinity (Windows) + `PULSE_SINK=ozma-seat-N` + `SDL_VIDEO_FULLSCREEN_HEAD=N`
- **Steam integration**: parse `libraryfolders.vdf`, launch via `steam://rungameid/XXXXX`
- **Profiles**: gaming (60fps, low-latency), workstation (15fps, standard), media (4K, surround)
- **Agent API**: `POST /seats/{id}/launch` with `{"command": "steam -applaunch 730"}`

### Phase 7: WebRTC Streaming (2 weeks)

Sub-100ms streaming to remote endpoints (thin clients, tablets, phones).

- Per-seat WebRTC endpoint via `aiortc` or GStreamer WebRTC
- H.264 hardware encoding: NVENC (NVIDIA) / AMF (AMD) / QuickSync (Intel)
- Controller proxies signaling via existing WebSocket infrastructure
- Targets: any browser, dedicated thin client app, mobile (future)

---

## Key Technical Details

### Windows Input: Raw Input API

```python
# Register for per-device raw input
RAWINPUTDEVICE = struct.pack("HHPI",
    0x01, 0x06,  # Usage Page: Generic Desktop, Usage: Keyboard
    RIDEV_INPUTSINK, hwnd)
RegisterRawInputDevices(...)

# Each WM_INPUT message includes device handle
# Agent maintains device_handle → seat_index mapping
```

For gamepad: XInput device indices (0-3) map directly to seats.

### USB Topology Grouping

**Linux** — sysfs:
```
/sys/bus/usb/devices/1-1/        # Hub
/sys/bus/usb/devices/1-1.1/      # Device on port 1 (keyboard)
/sys/bus/usb/devices/1-1.2/      # Device on port 2 (mouse)
→ Group: {keyboard, mouse} on hub 1-1 = Seat candidate
```

**Windows** — SetupAPI:
```python
parent = CM_Get_Parent(device_instance)
# Devices with same parent = same hub = same seat
```

### Windows Display Capture: DXGI Desktop Duplication

The existing `dxcam` integration already supports per-output:
```python
camera = dxcam.create(output_idx=seat.display_index, output_color="BGR")
```
Zero-copy GPU-side. Works with fullscreen D3D games. Supports up to 240fps. Virtual displays (IDD) are captured identically.

### HID Injection Scoping

**Linux**: Each seat's uinput devices target a specific X screen via `DISPLAY=:0.N`.

**Windows**: `SendInput` with `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` targets specific monitor regions. Keyboard input goes to the foreground window of the target monitor.

---

## Risk Areas and Fallbacks

| Risk | Severity | Mitigation | Fallback |
|------|----------|------------|----------|
| IDD driver signing | HIGH | Attestation signing ($200), test-signing for dev | Dummy HDMI plugs ($3), Parsec VDD |
| Windows input isolation | MEDIUM | Raw Input API per-device, absolute coords per-monitor | Windows MultiPoint SDK, fast user switching |
| Game compatibility | MEDIUM | Borderless windowed, per-game profiles | `SetDisplayConfig` before launch |
| GPU VRAM pressure | LOW | 4x 1080p ≈ 12GB, recommend 16GB+ GPU | Auto-degrade to 720p/30fps |
| Audio without virtual driver | LOW | HDMI follows display, USB follows topology | VB-Audio Cable (free) |
| NVIDIA vs AMD differences | LOW | `display_backend.py` abstracts vendor | Vendor-specific example configs |

---

## CLI Interface

```bash
# Auto-detect: one seat per connected display
ozma-agent --multi-seat

# Explicit: 4 seats, virtual displays
ozma-agent --multi-seat --seats 4

# With controller registration
ozma-agent --multi-seat --controller https://ozma.local:7380

# Manual seat config
ozma-agent --multi-seat --seat-config seats.json
```

---

## Testing Strategy

### Unit Tests (`tests/multiseat/`)
- `test_usb_topology.py` — Mock sysfs/SetupAPI, verify grouping
- `test_seat_manager.py` — Seat lifecycle, port allocation
- `test_display_linux.py` — Mock xrandr output parsing
- `test_display_windows.py` — Mock DXGI enumeration
- `test_input_router.py` — Device → seat mapping

### Integration Tests
- Linux 2-seat: two monitors, two keyboards → two independent nodes
- Windows 2-seat: same with DXGI capture
- USB hotplug: plug/unplug hub → seat auto-create/destroy within 3s
- Game launch: game on seat 1, correct display + audio routing

### CI/CD
- Linux: GitHub Actions (Xvfb + virtual monitors)
- Windows: self-hosted runner with GPU (DXGI DD needs real GPU)
- IDD: Windows VM in test-signing mode

---

## Timeline

| Phase | Duration | Platform | Deliverable |
|-------|----------|----------|-------------|
| 1. Foundation | 2 weeks | Linux | Multi-seat works, seat abstraction |
| 2. Windows Display+Capture | 3 weeks | Windows | DXGI DD per-output, physical multi-head |
| 3. IDD Virtual Display | 4 weeks | Windows | Virtual monitors (parallel with 4) |
| 4. Auto-Detection | 2 weeks | Both | USB hotplug creates seats |
| 5. Audio Isolation | 2 weeks | Both | Per-seat audio on Windows |
| 6. Game Launcher | 2 weeks | Both | One-click game launch per seat |
| 7. WebRTC Streaming | 2 weeks | Both | Sub-100ms to any device |

**Total: ~17 weeks** to full parity. Phases 1-2 (5 weeks) deliver a usable product.

---

## vs ASTER Multiseat

| Feature | ASTER ($60) | Ozma (free) |
|---------|-------------|-------------|
| Windows multi-seat | ✓ | ✓ |
| Linux multi-seat | ✗ | ✓ |
| Stream to remote devices | ✗ | ✓ (WebRTC) |
| Audio isolation | Partial | Full (per-seat) |
| Auto-detect peripherals | ✗ | ✓ (USB topology) |
| Game launcher | ✗ | ✓ (dashboard) |
| Virtual displays | ✗ | ✓ (IDD) |
| Open source | ✗ | ✓ (AGPL) |
| Ecosystem integration | ✗ | ✓ (scenarios, RGB, AI, widgets) |

This is a standalone product-level feature that drives adoption far beyond the KVM niche.
