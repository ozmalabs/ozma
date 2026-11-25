# Phase 4 — UAC2 bridge audio adapters and node status

Status: library and node wiring complete; real UDC/gadget-card validation still
required on hardware.

## What changed

### `ozma-audio`

New `ozma-audio/src/uac2.rs` implements the two traits exported by
`ozma-usb`'s transport-independent bridge:

| Adapter | Trait | Production device | Test seam |
|---|---|---|---|
| `AlsaUac2Sink` | `Uac2AudioSink` | non-blocking ALSA playback PCM | `Uac2PcmWriterFactory` / `Uac2PcmWriter` |
| `AlsaUac2Source` | `Uac2AudioSource` | non-blocking ALSA capture PCM | `Uac2PcmReaderFactory` / `Uac2PcmReader` |
| `PipeWireUac2Sink` | `Uac2AudioSink` | existing `PipeWireAudioSink` (S16) | `from_sink` + native PipeWire stats |
| `PipeWireUac2Source` | `Uac2AudioSource` | existing `PipeWireAudioSource` (S16) | `from_source` + native PipeWire stats |

Design points:

- The bridge's `Uac2PcmFormat` is authoritative. ALSA is configured at the
  exact rate/channel count/packed sample width; the negotiated values are read
  back and any mismatch is a typed `UsbError::Uac2Format`. No resampler is
  inserted and `ValueOr::Nearest` is only used for the rate request before the
  exact-value check.
- S16 maps to ALSA `S16_LE`, S24 to packed `S24_3LE`, S32 to `S32_LE`.
- PCM handles are opened non-blocking. `EAGAIN` becomes `Ok(0)`; the bridge
  already counts that as sink backpressure or capture underrun. It is never
  reported as consumed bytes.
- Short writes/reads are returned as-is, validated for whole-frame alignment,
  and counted. A writer that claims more than the offered buffer or a ragged
  frame count is a typed error.
- `Uac2SinkStats`/`Uac2SourceStats` are available through cloneable handles:
  write/read calls, bytes/frames, short transfers, backpressure, underruns,
  device errors, and flushes.
- PipeWire streams deliver interleaved `i16` even when the graph negotiates
  F32. The PipeWire adapters therefore accept only S16, check the negotiated
  rate/channels, bound writes by `queue_capacity - queued_samples`, and retain
  partial capture frames in a pending byte buffer. A `None` from
  `try_next_frame` is an underrun, not an error.

### `ozma-node`

New `ozma-node/src/usb_uac2.rs` provides:

- `Uac2BridgeDeviceFactory` and `Uac2BridgeDeviceConfig`; the production
  `AlsaUac2BridgeFactory` turns the kernel UAC2 gadget card into an
  `Uac2EndpointIo`:
  - gadget capture `hw:N,1` (host→device) → playback OUT packet reads,
  - gadget playback `hw:N,0` (device→host) → capture IN packet writes.
- Host audio comes from `AlsaUac2Sink`/`AlsaUac2Source` on
  `--usb-uac2-sink-device`/`--usb-uac2-source-device` (default `default`).
- `Uac2BridgeRuntime::start_with` opens every device inside the pump thread,
  builds `Uac2Bridge`, starts it, and only then signals readiness. A missing
  device is a typed error and the shared report stays `unsupported`.
- The pump thread updates `Uac2BridgeShared` after every `pump()`. Last errors
  are retained as `Uac2BridgeError { kind, message }` using
  `UsbError::kind()`. `Uac2BridgeShared::from_report` also downgrades a
  `running` report with zero PCM counters to `configured`, so an injected
  snapshot cannot claim live audio.
- Shutdown sets the stop flag, joins the pump, calls `bridge.stop()` (which
  flushes both audio paths), and leaves the final report at `configured` or
  `stopped`; it never leaves a stale `running`.

CLI/config (opt-in, validated):

```text
--usb-uac2-bridge          OZMA_USB_UAC2_BRIDGE
--usb-uac2-sink-device     OZMA_USB_UAC2_SINK_DEVICE     (default `default`)
--usb-uac2-source-device   OZMA_USB_UAC2_SOURCE_DEVICE   (default `default`)
```

`--usb-uac2-bridge` requires `--usb-uac2`. If the gadget/controller is
unavailable, the flag is ignored for startup and `/usb` receives a typed
`unsupported` reason. When the flag is set, the legacy `usb_audio` ALSA loop is
not started, so two bridges never claim the same gadget card.

`GET /usb` overlay:

- `gadget.functions[].uac2.bridge` is
  `unsupported|configured|running|stopped`.
- `gadget.functions[].uac2.bridge_report` carries:
  - `status`, per-direction `playback`/`capture` (`endpoint`, `direction`,
    `format`, `packet_interval`, `max_packet_size`, `maximum_packet_bytes`),
  - cumulative `Uac2BridgeCounters` (packets/bytes/frames, drops, overflow,
    backpressure, short/underrun/padded, errors),
  - `missing_paths` and an unsupported `reason` when applicable.
- `uac2_bridge_error` is present only after the pump observes a typed error:
  `{ "kind": "uac2-endpoint", "message": "..." }`.
- `running` is possible only when `Uac2BridgeReport::is_live()` is true, i.e.
  PCM counters moved. A started-but-idle bridge serialises as `configured`.

## Evidence

Default suites (no real UDC, ALSA device, or PipeWire session):

```text
cargo test -p ozma-usb -p ozma-audio -p ozma-node --lib
  ozma-node: 424 passed, 0 failed, 3 ignored
  ozma-usb:   10 passed, 0 failed
  ozma-audio:  87 passed, 1 ignored (including 8 adapter unit tests)

cargo test -p ozma-audio --test uac2_adapters
  7 passed, 0 failed
    playback round trip through the bridge ring
    playback backpressure / zero sink progress
    typed playback device error
    capture round trip and packetisation
    partial capture reads padded/counted
    open failure (missing fake device) typed
    configured bridge never claims running

cargo check -p ozma-node --all-targets
  ok

rustfmt --edition 2021 --check <touched files>
  no diff
```

The node route tests cover:

- descriptor-only UAC2 without a runtime bridge (`unsupported`),
- a configured bridge with format/endpoint/packet size and zero counters
  (`configured`, explicitly not `running`),
- a running bridge with non-zero sink/capture counters,
- a stopped bridge plus `uac2_bridge_error.kind == "uac2-endpoint"`.

The node runtime tests cover configured → running → stopped with fake
endpoints/audio, typed missing-device startup failure, an endpoint error that
does not claim `running`, and the missing ConfigFS function probe.

Real-device test remains opt-in:

```text
OZMA_AUDIO_TEST_DEVICE=default \
  cargo test -p ozma-audio --test uac2_bridge_real -- --ignored
```

## Remaining real-hardware gaps

1. **Real UDC + host enumeration.** No hardware run has exercised a host
   enumerating the UAC2 function while the node pumps ALSA gadget PCM. The
   endpoint adapter assumes the kernel `usb_f_uac2` card appears as
   `hw:N,0`/`hw:N,1` and that the naming/`/proc/asound/cards` discovery used by
   `usb_audio.rs` identifies it. A custom card label or a different kernel
   layout needs explicit device flags.
2. **ALSA gadget PCM semantics.** USB isochronous packet boundaries are not
   preserved by ALSA. The bridge treats one ALSA read as one OUT packet (capped
   at the endpoint max packet) and writes each IN packet as one ALSA write.
   Sample-accurate packet sizes still hold on the USB side, but real xrun and
   short-write behaviour under a host stall is untested.
3. **Format coverage.** The node path currently fixes the UAC2 model at
   2 playback / 1 capture channels, 48 kHz, S16. The adapters support
   S16/S24_3/S32 and 1–8 channels, but no node CLI selects them yet. PipeWire
   adapters intentionally accept S16 only.
4. **PipeWire live validation.** The wrappers compile and are covered by
   conversion/bounds logic, but no live PipeWire graph (S16/F32 conversion,
   monitor capture, underrun behaviour) was run in this task.
5. **Hotplug.** The legacy `usb_audio` watcher reopens on card
   disappearance/reappearance. The new runtime probes once at startup and
   reports a typed unsupported/error state; it does not recreate a bridge after
   a UDC reset or a card replug without a node restart.
6. **Capability advertisement.** Starting the bridge does not add a separate
   capability; `/usb` still reports the gadget capability and the UAC2 bridge
   detail per function. The controller has no dedicated “audio bridge live”
   registration field.
7. **PipeWire source construction.** `PipeWireAudioSource::new` is async and
   must be called by the node’s async context before handing it to
   `PipeWireUac2Source::from_source`. The current node factory uses ALSA for
   both host audio and gadget endpoint PCM.
