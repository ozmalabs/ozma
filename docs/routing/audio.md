# Audio Routing

**Status:** Draft
**RFC 2119 Conformance:** The key words "MUST", "MUST NOT", "REQUIRED", "SHALL",
"SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in
this document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

This document specifies the audio routing model for the Ozma routing graph.
The routing graph handles audio as pipelines between source and sink ports.
For basic KVM audio (stereo from target machine to desk speakers), the
pipeline model is sufficient. Professional audio requires additional
primitives: mix buses, monitor controllers, channel mapping, insert chains,
metering, and precision processing. This specification defines these
primitives as virtual devices in the routing graph, covering channel mapping,
mix buses, monitor controllers, cue sends, insert chains, latency
compensation, metering, gain staging, dither, professional audio transports,
active redundancy, sample-accurate synchronisation, and spatial audio with
speaker arrangement and room acoustics.

## Specification

### 1. Virtual Audio Devices

Audio routing primitives are modelled as **virtual devices** in the routing
graph. They do not correspond to physical hardware, but they have ports,
participate in pipelines, consume resources, and contribute latency. On
machines running PipeWire, each virtual device MUST map to a PipeWire node.

### 2. Channel Mapping

Channel mapping MUST use ITU-R BS.2051 labels. `AudioFormat` specifies
channel count but not channel assignment. Pro audio requires knowing which
channel carries what signal.

```yaml
AudioFormat:
  # ... existing fields ...
  channel_layout: string?       # named layout (see table below)
  channel_map: string[]?        # explicit per-channel assignment, ITU-R BS.2051 labels

  # If channel_layout is set, channel_map is derived from it.
  # If channel_map is set, it overrides channel_layout.
  # If neither is set, channels are positional (ch0=left, ch1=right, etc.)
```

If `channel_layout` is set, implementations MUST derive `channel_map` from
it. If `channel_map` is explicitly set, it MUST override `channel_layout`.
If neither is set, implementations MUST treat channels as positional
(`ch0=left`, `ch1=right`, etc.).

**Standard channel layouts:**

| Layout | Channels | Map |
|--------|----------|-----|
| `mono` | 1 | `["FC"]` |
| `stereo` | 2 | `["FL", "FR"]` |
| `stereo_lfe` | 3 | `["FL", "FR", "LFE"]` |
| `quad` | 4 | `["FL", "FR", "RL", "RR"]` |
| `5.1` | 6 | `["FL", "FR", "FC", "LFE", "SL", "SR"]` |
| `7.1` | 8 | `["FL", "FR", "FC", "LFE", "SL", "SR", "RL", "RR"]` |
| `7.1.4` | 12 | `["FL", "FR", "FC", "LFE", "SL", "SR", "RL", "RR", "TFL", "TFR", "TRL", "TRR"]` |
| `custom` | N | Explicit `channel_map` required |

Channel labels follow ITU-R BS.2051: `FL` (front left), `FR` (front right),
`FC` (front centre), `LFE` (low frequency effects), `SL`/`SR` (side
left/right), `RL`/`RR` (rear left/right), `TFL`/`TFR`/`TRL`/`TRR`
(top/height channels).

Implementations MUST support all standard layouts listed above. When a
`custom` layout is specified, `channel_map` MUST be provided.

When a link connects ports with different channel layouts, the converter
plugin MUST perform channel remixing -- upmix, downmix, or remap. The
format negotiation system handles this: if the source produces 7.1 and the
sink accepts stereo, a downmix converter MUST be inserted.

### 3. Mix Bus

A mix bus is a virtual device that sums multiple audio inputs with per-input
gain, pan, and mute. It is the fundamental building block for monitor
controllers, headphone mixes, cue sends, and multi-source mixing.

```yaml
MixBusDevice:
  type: virtual
  virtual_type: "mix_bus"
  name: string                  # "Main Mix", "Headphone Mix A", "Cue Send 1"
  ports:
    - id: input_0               # one sink port per input source
      direction: sink
      media_type: audio
    - id: input_1
      direction: sink
      media_type: audio
    # ... up to N inputs
    - id: output                # one source port (summed output)
      direction: source
      media_type: audio
  config: MixBusConfig

MixBusConfig:
  output_format: AudioFormat    # output sample rate, bit depth, channels
  inputs: MixBusInput[]
  master_gain_db: float         # master output gain
  master_mute: bool

MixBusInput:
  source: PortRef               # which port feeds this input
  gain_db: float                # per-input gain (-inf to +12 dB)
  pan: float                    # -1.0 (full left) to +1.0 (full right), 0.0 = centre
  mute: bool
  solo: bool                    # solo-in-place (mute all non-soloed inputs)
  phase_invert: bool            # invert polarity
  channel_routing: ChannelRouting?  # custom channel routing for this input
```

A mix bus MUST map to a PipeWire node with N input ports and 1 output port
group. PipeWire's native port-level linking handles the per-channel
connections; gain/pan/mute MUST be applied via PipeWire stream volume
controls or a `pw-filter-chain` summing node.

When one or more inputs have `solo: true`, all non-soloed inputs MUST be
muted (solo-in-place behaviour).

### 4. Monitor Controller

A monitor controller is a compound virtual device that provides source
selection, speaker set switching, and monitoring utilities. It replaces
a hardware monitor controller.

```yaml
MonitorControllerDevice:
  type: virtual
  virtual_type: "monitor_controller"
  ports:
    # Source inputs (any number of stereo/multichannel sources)
    - id: source_0              # e.g., "DAW Mix"
      direction: sink
      media_type: audio
    - id: source_1              # e.g., "Reference Player"
      direction: sink
      media_type: audio
    - id: source_2              # e.g., "Node Audio (target machine)"
      direction: sink
      media_type: audio
    # Speaker outputs (multiple speaker sets)
    - id: speakers_a            # e.g., "Main Monitors"
      direction: source
      media_type: audio
    - id: speakers_b            # e.g., "Secondary Monitors"
      direction: source
      media_type: audio
    - id: speakers_c            # e.g., "Headphones"
      direction: source
      media_type: audio
    - id: sub                   # subwoofer output (optional)
      direction: source
      media_type: audio
    # Talkback
    - id: talkback_mic          # engineer's microphone
      direction: sink
      media_type: audio
    - id: talkback_out          # routed to selected cue outputs
      direction: source
      media_type: audio
  config: MonitorControllerConfig

MonitorControllerConfig:
  active_source: uint           # which source input is selected (0-indexed)
  active_speakers: string[]     # which speaker sets are active (can be multiple)
  volume_db: float              # master volume
  dim: bool                     # reduce level by dim_amount_db
  dim_amount_db: float          # typically -20 dB
  mono: bool                    # sum to mono (for mono compatibility check)
  mute: bool                    # mute all outputs
  sub_enabled: bool             # route LFE to sub output
  sub_crossover_hz: float?      # crossover frequency (if bass management is active)
  talkback: TalkbackConfig

TalkbackConfig:
  mode: string                  # "momentary" (PTT), "latching", "auto" (voice-activated)
  destinations: string[]        # which outputs receive talkback ("cue_1", "cue_2")
  dim_on_talk: bool             # dim main monitors during talkback
  dim_amount_db: float          # how much to dim
  level_db: float               # talkback mic level
```

Monitor controller source selection MUST be atomic -- changing the active
source MUST switch instantaneously without audible glitches or intermediate
states. Multiple speaker sets MAY be active simultaneously.

The monitor controller is a routing device -- it is a managed switch for
audio, with additional processing (dim, mono, talkback). On PipeWire, it
MUST map to a combination of `pw-link` operations (source selection), volume
controls (dim/level), and a filter-chain node (mono sum, crossover).

### 5. Cue Sends / Aux Sends

Cue sends allow per-source, per-destination level control -- "send 50% of
source A to headphone mix B". These are modelled as mix buses with a
specific purpose.

```yaml
CueSendConfig:
  name: string                  # "Cue 1", "Headphone Mix A"
  mix_bus: MixBusDevice         # the underlying mix bus
  sends: CueSend[]              # per-source send levels
  output: PortRef               # where this cue mix goes (headphone amp, VBAN to node, etc.)

CueSend:
  source: PortRef               # which audio source
  send_level_db: float          # send level (-inf to +12 dB)
  pre_fader: bool               # true = level is independent of source's main fader
  pan: float                    # pan in the cue mix
  mute: bool
```

Pre-fader sends MUST tap the signal before the main mix bus gain stage.
This MUST map to PipeWire as a separate mix bus node with per-input volumes.

### 6. Insert Chain

An insert chain is an ordered sequence of audio processors applied to a
signal path. The order matters -- EQ before compressor produces different
results than compressor before EQ.

```yaml
InsertChain:
  id: string
  name: string                  # "Main Bus Processing", "Vocal Chain"
  processors: InsertSlot[]      # ordered list of processors
  bypass: bool                  # bypass entire chain

InsertSlot:
  position: uint                # 0-indexed position in chain
  processor: DeviceRef          # audio_processor device
  bypass: bool                  # bypass this slot
  wet_dry: float?               # 0.0 = dry, 1.0 = full wet (for parallel processing)
```

Processors MUST be applied in the order specified by `position`. On
PipeWire, an insert chain MUST be implemented as a sequence of
`pw-filter-chain` nodes linked in series. Each slot MUST be a separate
filter-chain node, allowing independent bypass and parameter control.

When `bypass` is `true` on an `InsertSlot`, the signal MUST pass through
unprocessed. When `bypass` is `true` on the `InsertChain`, the entire chain
MUST be bypassed.

### 7. Automatic Latency Compensation

When multiple audio paths have different processing chain lengths, their
latencies diverge. A source going through 3 processors arrives later than
one going through none. For time-aligned mixing, the shorter paths need
delay inserted to match the longest path.

```yaml
LatencyCompensation:
  mode: string                  # "auto", "manual", "disabled"
  reference_path: string?       # which pipeline is the reference (others compensate to match)
  per_path_delay: PathDelay[]   # computed or manual per-path delays

PathDelay:
  pipeline_id: string
  processing_latency_ms: float  # sum of processor latencies in this path
  compensation_delay_ms: float  # delay added to align with reference
  total_latency_ms: float       # processing + compensation = same for all paths
```

In `auto` mode, the router MUST compute the longest processing chain
latency across all paths feeding a mix bus and MUST insert delay on shorter
paths to match. On PipeWire, delay insertion SHOULD use `pw-loopback
--delay`.

In `manual` mode, the operator MAY specify per-path delays explicitly.
In `disabled` mode, no latency compensation SHALL be applied.

### 8. Metering

Metering is observation of audio levels at any point in the graph.
Implementations MUST support at least `peak` and `lufs_momentary` metering
types.

```yaml
MeteringPoint:
  port: PortRef                 # where in the graph to meter
  type: MeteringType            # what measurement algorithm
  channels: uint                # number of channels metered
  values: MeterValue[]          # current readings (per channel)
  update_rate_hz: float         # how often readings update

MeteringType: enum
  peak                          # instantaneous sample peak (dBFS)
  true_peak                     # inter-sample true peak per ITU-R BS.1770 (dBTP)
  rms                           # root mean square level (dBFS)
  vu                            # VU meter (300ms integration, +4 dBu = 0 VU)
  ppm                           # peak programme meter (5ms attack, configurable release)
  lufs_momentary                # ITU-R BS.1770 momentary loudness (400ms window)
  lufs_short_term               # ITU-R BS.1770 short-term (3s window)
  lufs_integrated               # ITU-R BS.1770 integrated loudness (programme duration)

MeterValue:
  channel: string               # channel label ("FL", "FR", "LFE", etc.)
  level: float                  # current level in the metering type's native unit
  peak_hold: float?             # peak hold value (highest level in decay window)
  clip: bool                    # true if signal has clipped (reached 0 dBFS)
```

Implementations MUST support all `MeteringType` values listed above.
`true_peak` metering MUST conform to ITU-R BS.1770. `lufs_momentary`,
`lufs_short_term`, and `lufs_integrated` MUST conform to ITU-R BS.1770.

Metering data MUST be available via WebSocket events (`audio.meters`) and
REST API. On PipeWire, metering SHOULD read from `pw-dump` volume/peak data
or from a dedicated analysis filter-chain.

### 9. Gain Staging and Headroom

Every audio hop in a pipeline has a gain contribution. The total gain
through the pipeline determines whether the signal clips or is too quiet.

```yaml
GainStage:
  hop: PipelineHopRef           # which hop in the pipeline
  input_level_dbfs: float       # signal level entering this stage
  gain_db: float                # gain applied at this stage
  output_level_dbfs: float      # signal level leaving this stage
  headroom_db: float            # distance from 0 dBFS (clipping)
  clip_risk: bool               # true if headroom < 3 dB
```

The router MUST track gain staging across the entire pipeline and MUST
warn when headroom is insufficient. The `clip_risk` flag MUST be set to
`true` when headroom falls below 3 dB. This MUST feed into the monitoring
system as `audio.gain_stage.clip_risk` events.

### 10. Dither

When audio passes through a bit-depth conversion (e.g., 32-bit float
processing to 24-bit output), truncation introduces quantisation distortion.
Dither MUST be applied on bit-depth reduction unless explicitly disabled.

```yaml
DitherConfig:
  enabled: bool                 # default: true when bit depth decreases
  type: string                  # "tpdf" (triangular PDF, default), "rpdf" (rectangular),
                                # "hp_tpdf" (high-pass TPDF), "noise_shaped"
  auto: bool                    # automatically apply when format negotiation
                                # selects a lower bit depth at any hop
```

When `auto` is `true`, dither MUST be applied automatically whenever the
format negotiation engine selects a format that reduces bit depth at any
hop. The default dither type MUST be `tpdf` (triangular probability density
function). Dither MUST NOT be applied when bit depth remains the same or
increases.

Dither is modelled as a property of format conversion, not a separate
device. The converter plugin handles this.

### 11. Professional Audio Transports

Additional transport types for pro audio installations:

| Transport | Channels | Latency | Use case |
|-----------|----------|---------|----------|
| AES67 | 8-64 | <1ms (LAN) | Studio networked audio (Dante-compatible) |
| Dante | 2-512 | <1ms (LAN) | Broadcast, live sound, installed AV |
| MADI | 64 | <0.5ms | Studio multitrack (BNC/fibre) |
| ADAT | 8 | <0.5ms | Studio interface interconnect (TOSLINK) |
| AES3 (AES/EBU) | 2 | <0.1ms | Studio master clock + digital audio (XLR) |
| S/PDIF | 2 | <0.1ms | Consumer digital audio (coax/TOSLINK) |

These are transport plugins. AES67 and Dante are network transports
(UDP/RTP with PTP clock). MADI, ADAT, AES3, and S/PDIF are physical
transports that require specific hardware interfaces -- they MUST appear
in the graph as device ports with fixed capabilities.

Implementations SHOULD support AES67 and Dante as network transport plugins.
Implementations MAY support MADI, ADAT, AES3, and S/PDIF where the
corresponding hardware interfaces are present.

### 12. Active Redundancy

Professional installations require redundant audio paths -- two independent
network paths carrying the same audio, with automatic failover.

```yaml
RedundantPipeline:
  primary: Pipeline             # primary audio path
  secondary: Pipeline           # secondary audio path (different physical route)
  mode: string                  # "active_active" (both running, receiver selects),
                                # "active_standby" (secondary warm, activates on failure)
  switchover_ms: float          # maximum time to switch from primary to secondary
  monitoring: bool              # continuously monitor both paths for differential errors
```

Both pipelines MUST carry the same audio data. The receiver MUST compare
packet sequence numbers and MUST select from whichever path is delivering.
In `active_active` mode, switchover MUST be seamless -- packets are already
arriving on the secondary path. In `active_standby` mode, switchover
MUST complete within `switchover_ms`.

Implementations SHOULD continuously monitor both paths for differential
errors when `monitoring` is `true`.

Dante natively supports primary/secondary redundancy. AES67 achieves it
via IGMP multicast on two independent network paths. The routing graph
MUST model both pipelines and their independent link health.

### 13. Sample-Accurate Synchronisation

When multiple audio sources must be time-aligned (multi-machine recording,
distributed DAW), they need a shared sample clock.

```yaml
SampleClockSync:
  mode: string                  # "ptp" (IEEE 1588), "word_clock" (BNC),
                                # "aes_clock" (AES11), "internal" (free-run)
  master: DeviceRef?            # which device is the clock master
  rate: uint                    # sample rate (all devices must agree)
  lock_status: string           # "locked", "locking", "unlocked", "freewheel"
  offset_samples: int?          # measured offset from master (for alignment)
```

All synchronised devices MUST agree on sample rate. PTP provides
sub-microsecond sync over Ethernet (sufficient for sample accuracy at
192kHz -- one sample = 5.2us). Word clock and AES11 are hardware clock
distribution standards used in studios.

The routing graph MUST track lock status and MUST raise events when sync
is lost. Implementations MUST support `ptp` and `internal` modes.
Implementations SHOULD support `word_clock` and `aes_clock` where the
corresponding hardware is present.

On PipeWire, the clock master MUST map to a PipeWire driver node -- the
device whose hardware clock drives the graph scheduling.

### 14. Spatial Audio

The routing graph knows the physical position and orientation of every
speaker (`PhysicalLocation` -- `pos` + `rot`), the speaker's directional
characteristics (`SpeakerSpatialSpec` -- dispersion angles, directivity),
and the listener's position (inferred from zone or explicitly set). This
is sufficient to build spatial audio.

#### 14.1 Speaker Arrangement

```yaml
SpeakerArrangement:
  id: string
  name: string                  # "Studio Monitors", "Living Room 5.1", "Desk Stereo"
  speakers: SpeakerPlacement[]
  listener: ListenerPosition
  room: RoomAcoustics?          # optional room model for correction
  channel_assignment: ChannelAssignment  # how channels map to speakers

SpeakerPlacement:
  device_id: string             # speaker device in the graph
  location: PhysicalLocation    # position + orientation
  role: string                  # "front_left", "front_right", "centre", "sub",
                                # "surround_left", "surround_right", "rear_left",
                                # "rear_right", "height_front_left", etc.
  distance_m: float?            # measured distance from listener (overrides computed)
  angle_deg: float?             # measured angle from listener (overrides computed)
  elevation_deg: float?         # measured elevation from listener (overrides computed)
  level_trim_db: float?         # per-speaker level trim

ListenerPosition:
  pos: Position3d               # listener's head position in world space (mm)
  facing: float                 # yaw angle the listener faces (degrees, 0 = +X)
  ear_height_mm: float?         # ear height above floor (default: seated ~1100mm)
  source: string                # "manual", "furniture_derived", "zone_centre"
  # If source is "furniture_derived": computed from the chair/desk position
  # If source is "zone_centre": computed from the zone bounds centre point

ChannelAssignment:
  mode: string                  # "itu" (standard angles), "measured" (actual angles),
                                # "phantom" (virtual speakers from fewer physical speakers)
  assignments: ChannelSpeakerMap[]

ChannelSpeakerMap:
  channel: string               # "FL", "FR", "FC", "LFE", "SL", "SR", etc.
  speaker_id: string            # which physical speaker
  gain_db: float?               # trim for this channel->speaker mapping
  delay_ms: float?              # per-channel delay (for distance compensation)
```

Each speaker in the arrangement MUST reference a valid device in the routing
graph via `device_id`. When `distance_m`, `angle_deg`, or `elevation_deg`
are explicitly provided, they MUST override values computed from
`PhysicalLocation`.

#### 14.2 What Location Data Provides

1. **Distance compensation**: Each speaker's distance from the listener is
   known from `pos` coordinates. The closer speaker needs delay added so
   sound from all speakers arrives simultaneously. Implementations MUST
   compute distance compensation automatically:
   ```
   delay_ms = (max_distance - this_distance) / speed_of_sound_mm_per_ms
   ```
   Speed of sound = 343,000 mm/s = 343 mm/ms.

2. **Level compensation**: Inverse square law -- a speaker 2x farther away
   is 6 dB quieter. The system SHOULD auto-trim levels based on distance.

3. **Angle verification**: ITU-R BS.775 recommends specific speaker angles
   (+/-30 deg for stereo, +/-110 deg for surrounds). The system SHOULD warn
   when placement deviates from standards, or adapt processing to compensate.

4. **Dispersion coverage**: The speaker's dispersion spec (horizontal/vertical
   degrees from device database) combined with its orientation (`rot`) and
   the listener position tells you whether the listener is within the
   speaker's coverage angle. The system SHOULD warn when the listener is
   outside the coverage angle.

5. **Subwoofer integration**: Subwoofer position relative to walls and
   corners affects bass response. The system knows room dimensions and sub
   position, enabling room mode estimation and optimal crossover selection.

6. **Headphone virtualisation**: When the output switches from speakers to
   headphones (e.g., monitor controller speaker set change), the speaker
   arrangement data enables binaural rendering -- virtualising the physical
   speaker positions in the headphone soundstage using HRTF.

#### 14.3 Room Acoustics

Room acoustics is OPTIONAL and feeds into room correction.

```yaml
RoomAcoustics:
  dimensions_mm: Dimensions     # room width, depth, height
  rt60_ms: float?               # measured or estimated reverberation time
  treatment: string[]?          # ["absorption_panels", "diffusers", "bass_traps"]
  floor: string?                # "carpet", "hardwood", "concrete", "tile"
  walls: string?                # "drywall", "concrete", "glass", "curtains"
  ceiling: string?              # "drywall", "acoustic_tile", "exposed"
  noise_floor_dba: float?       # measured ambient noise level
```

Room acoustics combined with speaker positions feeds the room correction
system (`audio_processor` with `processor_type: "room_correction"`). The
sweep to FFT to parametric EQ pipeline uses speaker position and room
dimensions to optimise the correction curve per speaker.

When `RoomAcoustics` is provided, implementations SHOULD use it to inform
room correction processing and subwoofer crossover selection.
