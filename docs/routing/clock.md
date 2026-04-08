# Clock Model

**Status:** Draft

**RFC 2119 Conformance:** The key words "MUST", "MUST NOT", "REQUIRED", "SHALL",
"SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

This document specifies the clock model for the Ozma routing system. Devices in
the Ozma graph have independent clocks. Unlike PipeWire (where a single driver
clock governs each subgraph), Ozma MUST synchronise across network boundaries.
The specification covers clock domains, PTP synchronisation with hardware
timestamp detection, QoS marking, jitter buffer depth selection, drift
compensation strategies, and video frame timing.

---

## Specification

### 1. Clock Domains

A **clock domain** is a set of devices that share a common time reference. On a
single machine, all devices MAY share the system clock. Across machines, clocks
diverge.

```yaml
ClockDomain:
  id: string
  reference: ClockReference     # what this domain synchronises to
  members: DeviceRef[]          # devices in this domain
  drift_ppm: float?             # measured clock drift if known
```

**ClockReference**:

```yaml
ClockReference: enum
  system_monotonic   # local monotonic clock (CLOCK_MONOTONIC)
  ptp                # IEEE 1588 Precision Time Protocol
  ntp                # NTP-synchronised wall clock
  audio_device       # locked to an audio device's word clock
  free_running       # no synchronisation (each device runs independently)
```

### 2. Synchronisation Strategy

**Same machine**: All devices SHOULD share `system_monotonic`. No synchronisation
is needed. Latency measurements are directly comparable.

**LAN (same broadcast domain)**: PTP (IEEE 1588v2) provides sub-microsecond
synchronisation on standard Ethernet — no proprietary hardware is REQUIRED. This
is the same clock mechanism used by Dante, AES67, and SMPTE ST 2059. The
controller SHOULD run a PTP grandmaster; nodes MUST synchronise to it.

PTP accuracy depends on hardware timestamping support:

| NIC capability | PTP accuracy | Audio quality achievable | Hardware examples |
|---------------|-------------|------------------------|-------------------|
| Hardware timestamps (PHC) | <1us | Sample-accurate at 192kHz (1 sample = 5.2us) | Intel I210, I225, I226; Broadcom BCM5720; Realtek RTL8125B (some) |
| Software timestamps only | 10-100us | Sample-accurate at 48kHz (1 sample = 20.8us); sub-sample at 96kHz | Most USB Ethernet, WiFi adapters, budget NICs |
| NTP only (no PTP) | 1-10ms | Adequate for KVM audio; not for pro audio sync | Any network interface |

The controller SHOULD detect PTP hardware timestamp support automatically via
`ethtool -T` (Linux) and MUST select the best available clock source. Hardware
PTP is preferred; software PTP is the fallback; NTP is the last resort. The
`InfoQuality` on the clock sync MUST reflect this: hardware PTP = `measured`,
software PTP = `measured` (lower confidence), NTP = `reported`.

**QoS marking**: Audio packets SHOULD be marked with DSCP EF (Expedited
Forwarding, value 46) to receive priority treatment on managed switches. This is
the same QoS classification used by Dante and AES67. The transport plugin MUST
set the DSCP value on the socket (`setsockopt IP_TOS`). On unmanaged switches,
DSCP has no effect but causes no harm.

**Latency classes**: Like Dante's selectable latency, Ozma's audio transport
offers configurable jitter buffer depth that determines the latency/reliability
trade-off:

| Jitter buffer | One-way latency added | Tolerance | Use case |
|--------------|----------------------|-----------|----------|
| 0.25ms (12 samples @ 48k) | 0.25ms | Very tight — REQUIRES PTP + dedicated/managed switch | Local monitoring |
| 0.5ms (24 samples) | 0.5ms | Tight — PTP RECOMMENDED | Studio recording |
| 1ms (48 samples) | 1ms | Moderate — software PTP sufficient | Live performance |
| 2ms (96 samples) | 2ms | Comfortable — works on any decent LAN | General pro audio |
| 5ms (240 samples) | 5ms | Relaxed — tolerates WiFi jitter | Non-critical audio |
| 20ms (960 samples) | 20ms | Very relaxed — tolerates internet jitter | Remote audio |

The intent's `max_latency_ms` constraint determines which buffer depth is
acceptable. The router MUST select the smallest buffer that the measured link
jitter can sustain — if the link has 0.3ms p99 jitter, a 0.5ms buffer works;
if jitter is 2ms, the buffer MUST be at least 2ms.

**Dante-equivalent on commodity hardware**: On a wired Gigabit LAN with a
managed switch (QoS enabled) and a NIC with hardware PTP timestamps, Ozma
achieves the same sync accuracy and latency as Dante — without Audinate
licensing, without proprietary chipsets, without vendor lock-in. The
difference is purely protocol: Dante is a closed standard; Ozma's audio
transport is open and interoperable with AES67 (which Dante itself
supports as a compatibility mode).

**Remote (via Connect relay)**: NTP synchronised to a common reference. Clock
offset MUST be measured during session establishment and applied to latency
calculations. Accuracy is lower (tens of milliseconds) but sufficient for remote
desktop intents.

### 3. Drift Compensation

When audio crosses clock domains, sample rate drift causes buffer underrun or
overrun. Compensation strategies:

| Strategy | Latency impact | Quality impact | Use case |
|----------|---------------|----------------|----------|
| Adaptive resampling | +1ms | Inaudible (SRC quality) | Default for cross-machine audio |
| Buffer padding | +5-20ms | None | High-fidelity where resampling is unacceptable |
| Drop/duplicate samples | 0ms | Audible clicks on drift | Emergency fallback only |

The router MUST select compensation strategy based on the intent:
- `fidelity_audio`: buffer padding (MUST NOT resample)
- `gaming` / `desktop`: adaptive resampling
- `preview` / `observe`: not applicable (audio not REQUIRED or low priority)

### 4. Video Timing

Video frame timing across clock domains uses a simpler model: frames MUST be
timestamped at source and played at destination with a jitter buffer. The
jitter buffer depth MUST be derived from the intent's latency budget minus the
transport latency:

```
jitter_buffer_ms = intent.max_latency_ms - transport.latency_ms - codec.latency_ms
```

If the result is negative, the intent MUST NOT be satisfied on this path.
