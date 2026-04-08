# Intents

**Status**: Draft

## Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

An intent declares what the user (or system) wants to achieve with a pipeline.
Intents drive every routing decision -- they are not metadata, they are the
primary input to the route calculator. This document specifies the intent
structure, the built-in intents that MUST be available in every implementation,
the rules for composing intents, degradation policies, and the extension
mechanism for custom intents.

## Specification

### 3.1 Intent structure

```yaml
Intent:
  name: string                  # identifier (built-in or user-defined)
  description: string           # human-readable purpose
  streams: StreamIntent[]       # per-media-type requirements
  priority: uint                # relative priority when resources are contended
  degradation: DegradationPolicy # what to do when constraints can't be met
```

**StreamIntent**:

```yaml
StreamIntent:
  media_type: video | audio | hid | screen | rgb | control | data
  required: bool                # MUST this stream exist for the intent to be satisfied?
  constraints: Constraints      # hard limits -- pipeline MUST be rejected if these can't be met
  preferences: Preferences      # soft targets -- optimise toward these, accept less
```

**Constraints** (hard -- the router MUST reject a pipeline if any constraint is violated):

```yaml
Constraints:
  max_latency_ms: float?        # end-to-end latency ceiling (steady-state)
  max_activation_time_ms: float? # maximum time to go from current state to active (see S2.6)
  min_bandwidth_bps: uint64?    # minimum available bandwidth required
  max_loss: float?              # maximum acceptable loss rate
  max_jitter_ms: float?         # maximum acceptable jitter
  max_hops: uint?               # maximum number of links traversed
  max_conversions: uint?        # maximum format changes allowed
  required_formats: Format[]?   # MUST support at least one of these formats
  forbidden_formats: Format[]?  # MUST NOT use these formats
  encryption: required | preferred | none  # data-in-transit encryption requirement
```

**Preferences** (soft -- used for ranking candidate pipelines):

```yaml
Preferences:
  target_latency_ms: float?     # ideal latency
  target_resolution: Resolution? # ideal video resolution
  target_framerate: float?      # ideal framerate
  target_sample_rate: uint?     # ideal audio sample rate
  target_channels: uint?        # ideal audio channel count
  target_bit_depth: uint?       # ideal audio bit depth
  prefer_lossless: bool?        # prefer lossless codecs when available
  prefer_hardware_codec: bool?  # prefer hardware encode/decode
  prefer_fewer_hops: bool?      # weight hop count more heavily
  prefer_lower_latency: bool?   # weight latency more heavily
  prefer_higher_quality: bool?  # weight quality more heavily
```

### 3.2 Built-in intents

The following standard intents MUST be available in every Ozma implementation.
Users MAY modify their parameters or define entirely new intents.

#### `control`

Headless operation. Keyboard and mouse only, no video or audio needed.

```yaml
name: control
description: Keyboard and mouse input to a headless or blind target
priority: 90
streams:
  - media_type: hid
    required: true
    constraints:
      max_latency_ms: 5
      max_loss: 0.001
    preferences:
      prefer_lower_latency: true
  - media_type: video
    required: false
  - media_type: audio
    required: false
```

#### `preview`

Low-resolution, high-latency video for thumbnails and machine pickers. Cheap to
produce and transport.

```yaml
name: preview
description: Low-res thumbnail for dashboards, machine picker, monitoring grid
priority: 10
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 2000
    preferences:
      target_resolution: { w: 640, h: 360 }
      target_framerate: 2
      prefer_lower_latency: false
      prefer_higher_quality: false
  - media_type: hid
    required: false
  - media_type: audio
    required: false
```

#### `observe`

High-resolution monitoring where latency is secondary. Security cameras, kiosk
checks, passive monitoring.

```yaml
name: observe
description: High-quality passive monitoring -- latency is acceptable
priority: 20
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 1000
    preferences:
      target_resolution: { w: 1920, h: 1080 }
      target_framerate: 15
      prefer_higher_quality: true
  - media_type: audio
    required: false
    constraints:
      max_latency_ms: 1000
    preferences:
      target_channels: 1
      prefer_lossless: false
  - media_type: hid
    required: false
```

#### `desktop`

General productivity. Good video, reasonable latency, stereo audio. The default
for most KVM usage.

```yaml
name: desktop
description: General productivity -- good video, reasonable latency, stereo audio
priority: 50
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 100
    preferences:
      target_resolution: { w: 1920, h: 1080 }
      target_framerate: 30
      prefer_lower_latency: true
  - media_type: audio
    required: true
    constraints:
      max_latency_ms: 50
    preferences:
      target_channels: 2
      target_sample_rate: 48000
      target_bit_depth: 16
  - media_type: hid
    required: true
    constraints:
      max_latency_ms: 10
      max_loss: 0.0001
```

#### `creative`

Video/audio production, CAD, music. High resolution, low latency, multichannel
lossless audio.

```yaml
name: creative
description: Content creation -- high resolution, low latency, lossless audio
priority: 70
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 50
    preferences:
      target_resolution: { w: 3840, h: 2160 }
      target_framerate: 60
      prefer_higher_quality: true
      prefer_hardware_codec: true
  - media_type: audio
    required: true
    constraints:
      max_latency_ms: 20
      forbidden_formats:
        - { codec: opus }
        - { codec: aac }
        - { codec: mp3 }
    preferences:
      target_channels: 8
      target_sample_rate: 96000
      target_bit_depth: 24
      prefer_lossless: true
  - media_type: hid
    required: true
    constraints:
      max_latency_ms: 5
```

#### `gaming`

Interactive gaming. Native resolution, minimum latency for both video and input.
Gamepad support.

```yaml
name: gaming
description: Interactive gaming -- minimum latency, native resolution, gamepad
priority: 80
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 16
    preferences:
      target_framerate: 60
      prefer_lower_latency: true
      prefer_hardware_codec: true
  - media_type: audio
    required: true
    constraints:
      max_latency_ms: 20
    preferences:
      target_channels: 2
      target_sample_rate: 48000
      prefer_lower_latency: true
  - media_type: hid
    required: true
    constraints:
      max_latency_ms: 5
      max_loss: 0.0001
    preferences:
      prefer_lower_latency: true
degradation:
  video: reduce_framerate_first
  audio: allow_lossy_compression
  hid: never_degrade
```

#### `broadcast`

One-to-many distribution. Source quality, latency is irrelevant. Teacher mode,
screen share, recording source.

```yaml
name: broadcast
description: One-to-many distribution -- source quality, latency irrelevant
priority: 30
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 5000
      max_conversions: 1
    preferences:
      prefer_higher_quality: true
      prefer_fewer_hops: true
  - media_type: audio
    required: false
    constraints:
      max_latency_ms: 5000
    preferences:
      target_channels: 2
      prefer_higher_quality: true
  - media_type: hid
    required: false
```

#### `fidelity_audio`

Audio is the primary concern. No compression, ever. Video and HID are
irrelevant. Music listening, mixing, mastering, room correction measurement.

```yaml
name: fidelity_audio
description: Uncompromised audio -- never compressed, multichannel, high sample rate
priority: 60
streams:
  - media_type: audio
    required: true
    constraints:
      max_latency_ms: 10
      forbidden_formats:
        - { codec: opus }
        - { codec: aac }
        - { codec: mp3 }
        - { codec: vorbis }
        - { codec: wma }
        - { lossy: true }
    preferences:
      target_channels: 8
      target_sample_rate: 96000
      target_bit_depth: 32
      prefer_lossless: true
  - media_type: video
    required: false
  - media_type: hid
    required: false
```

### 3.3 Intent composition

Intents are composable. A user MAY want `control` + `fidelity_audio` (typing on
a headless audio workstation). When intents are composed:

1. Each intent's stream requirements MUST be merged per media type.
2. Constraints MUST be intersected -- the strictest constraint wins.
3. Preferences MUST be merged; conflicts MUST be resolved by the higher-priority intent.
4. Priority MUST be the maximum of the composed intents.

Composition produces a new synthetic intent that the router MUST treat
identically to a named intent.

### 3.4 Degradation policy

When the graph cannot satisfy an intent's constraints, the degradation policy
SHOULD be applied to determine what happens:

```yaml
DegradationPolicy:
  # per-media-type degradation strategy
  video: DegradationStrategy
  audio: DegradationStrategy
  hid: DegradationStrategy

DegradationStrategy: enum
  never_degrade       # fail the pipeline rather than degrade this stream
  reduce_framerate_first   # video: drop framerate before resolution
  reduce_resolution_first  # video: drop resolution before framerate
  allow_lossy_compression  # audio: permit lossy codec as last resort
  reduce_sample_rate       # audio: lower sample rate before allowing lossy
  increase_latency         # accept higher latency to maintain quality
  drop_stream              # remove this stream entirely rather than degrade others
```

The router MUST apply degradation strategies in order of the intent's stream
priorities. HID SHOULD almost never be degraded -- it is tiny bandwidth and
critical for usability. Audio next. Video last (it has the most room to degrade
gracefully).

### 3.5 Custom intents

Users MAY define custom intents in the same YAML schema. Custom intents MAY
extend or override built-in intents:

```yaml
name: my_streaming_setup
extends: broadcast
description: My Twitch streaming with specific audio requirements
streams:
  - media_type: audio
    required: true
    constraints:
      max_latency_ms: 100
    preferences:
      target_channels: 2
      target_sample_rate: 48000
      target_bit_depth: 24
      prefer_lossless: true
```

Fields in the child override the parent. Streams MUST be merged by `media_type` --
a child stream definition replaces the parent's definition for that media type.
