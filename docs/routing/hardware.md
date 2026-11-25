# Hardware Inventory and Sensors

**Status:** Implemented in `ozma-hardware`, exposed by `ozma-node`.

The node collects a HWiNFO-class hardware tree and sensor snapshot. It is read
only: fan PWM is a separate mechanism in `ozma-thermal`.

## API

Authenticated node routes:

```text
GET /hardware                  # full snapshot
GET /api/v1/hardware           # full snapshot
GET /api/v1/hardware/summary   # smaller CPU/GPU/storage/sensor summary
GET /api/v1/hardware/sensors   # flat raw-sensor list
```

`?refresh=true` bypasses the five-second cache.

The controller proxies the same routes so the fleet API does not need direct
node access:

```text
GET /api/v1/nodes/{node_id}/hardware
GET /api/v1/nodes/{node_id}/hardware/summary
GET /api/v1/nodes/{node_id}/hardware/sensors
```

## Coverage

| Area | Data |
|---|---|
| CPU | brand/model/socket, sockets, cores/threads, base/boost clock, microcode, instruction flags, cache hierarchy, per-core clock/temperature/voltage/usage/power, package temperature, RAPL power |
| Memory | slot size/type/configured speed/manufacturer/part/serial/form factor/bank, timings/XMP where dmidecode exposes them, total fallback from `/proc/meminfo` |
| GPU | NVIDIA `nvidia-smi` and AMD `amdgpu` hwmon; clocks, power, temperatures (core/hot spot/VRAM), fan, utilization, VRAM use, bandwidth |
| Storage | model/serial/firmware/interface/capacity, SMART attributes, NVMe health/wear/data-written/unsafe-shutdown counters |
| Motherboard | vendor/model/version, BIOS, VRM/PCH/ambient temperatures |
| Cooling | every hwmon fan and voltage rail, plus a flat sensor list |
| Power | batteries (design/full/current capacity, voltage, rate, cycles, health), package power |
| Buses | PCIe devices and network interfaces/rates |

The `HardwareSensor` list preserves every temperature, voltage, fan, power, and
current sensor even when it does not map onto a structured field.

## Platform support

- Linux: `/proc`, `/sys` (`hwmon`, `powercap`, `block`, `pci`), `dmidecode`,
  `smartctl`, `nvme`, `nvidia-smi`, `lspci`.
- Windows: WMI/CIM (`Win32_*`) plus LibreHardwareMonitor's WMI namespace for
  per-core temperatures, voltages, fans, and power. Basic inventory works
  without LHM.
- macOS: `sysctl`, `system_profiler -json`, and `powermetrics` for fans/package
  temperatures.

Privileged data (DIMM SPD, DMI, SMART, LHM, `powermetrics`) requires the
corresponding permissions. Missing privileges or tools produce empty sections,
never fabricated zeroes.
