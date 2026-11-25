# ozma-hardware

HWiNFO-class hardware inventory and sensor collection for Ozma nodes.

## Coverage

| Area | Data |
|---|---|
| CPU | brand/model/socket, sockets, physical/logical cores, base and boost clocks, microcode, instruction flags, cache hierarchy, per-core clock/temperature/voltage/usage/power, package temperature and RAPL power |
| Memory | per-slot size/type/speed/configured speed/manufacturer/part/serial/form factor/bank, timings and XMP profiles when dmidecode exposes them, total memory fallback from `/proc/meminfo` |
| GPU | NVIDIA via `nvidia-smi`, AMD via `amdgpu` hwmon; clocks, power, temperature, hotspot/VRAM temperature, fan, utilization, VRAM use, memory bandwidth |
| Storage | model/serial/firmware/interface/capacity, SMART attributes, NVMe health/wear/unsafe shutdowns/data written |
| Motherboard | vendor/model/version, BIOS version/vendor/date, VRM/PCH/ambient temperatures |
| Cooling | every hwmon fan (RPM, target, PWM capability), every hwmon voltage rail |
| Power | batteries (design/full/current capacity, voltage, charge rate, cycles, health), RAPL/package power |
| Buses | PCIe devices (slot/class/vendor/device/driver/link width/generation), network interfaces and rates |

## API

`ozma-node` exposes the snapshot on the authenticated node API:

```text
GET /hardware                  # full snapshot
GET /api/v1/hardware           # full snapshot
GET /api/v1/hardware/summary   # CPU/memory/GPU/storage/sensors summary
```

Add `?refresh=true` to bypass the short cache.

## Platform notes

- **Linux** is the primary implementation. SPD, DMI, and SMART detail may need
  root; the collector falls back to `/proc` data instead of inventing values.
- **Windows** uses WMI/CIM plus LibreHardwareMonitor's WMI namespace. LHM is
  needed for per-core temperatures, voltages, fan speeds, and power; basic
  inventory works without it. The bundled `LibreHardwareMonitorServer.exe` is
  the same requirement as the previous Python collector.
- **macOS** uses `sysctl` and `system_profiler`; fans and package power require
  `powermetrics` (root). Missing privileges produce empty sections, never fake
  zeroes.

The snapshot is data, not control. Fan PWM is handled separately by
`ozma-thermal`.
