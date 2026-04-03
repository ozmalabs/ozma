# Ozma Hardware

Custom hardware designs for Ozma. All designs are open hardware (KiCad PCB,
Apache-2.0 RTL). See [docs/hardware.md](../docs/hardware.md) for the full
hardware reference including COTS options.

---

## Directory layout

```
hardware/
  rtl/
    dp_rx/        Open DisplayPort receiver RTL (see rtl/dp_rx/README.md)
  dock/           Ozma Dock PCB (KiCad) — not started
  sim/            Simulation testbenches (iverilog)
```

---

## Roadmap

| Version | Design | Status |
|---------|--------|--------|
| V1 HW | ECP5 + TPS65994AD + RK3588S, DP 1.1, 1080p30, 100W PD | RTL in progress |
| V2 HW | CrossLink-NX variant, DP 1.2, 1080p60/4K30, MST, 140W PD | Planned |

COTS node options (no custom PCB needed) are documented in
[docs/hardware.md](../docs/hardware.md).
