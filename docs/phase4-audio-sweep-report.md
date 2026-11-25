# Phase 4 — audio graph enumeration and generated sweep

Status: **live-validated** on the host's existing PipeWire/PulseAudio server
(2026-11-26). `GET /audio/nodes` now returns a real bounded graph snapshot, and
`POST /audio/sweep` generates and plays a log sweep, captures a named source,
and returns a measured profile only when audio was captured.

This report covers the two routes that were typed 501 in
[`phase4-audio-live-report.md`](phase4-audio-live-report.md). Routing control
(`/audio/apply`, `/audio/remove`, `/audio/play`, `/audio/stop`) is unchanged.
The measurement path is deliberately basic: it does **not** fit equalisation
bands, apply convolution, or change the graph.

## Environment

- Host: `SFFPC`, Linux, same-user session.
- `pactl 17.0-134-g77d25`, `pipewire 1.6.11` / `libpipewire 1.6.11`,
  server string `PulseAudio (on PipeWire 1.6.11)`.
- Tools present: `pactl`, `pw-play`, `paplay`, `parec`, `pw-record`.
- Live test used a uniquely named disposable `module-null-sink`. It never
  opened a microphone and never changed the default sink/source.

## `GET /audio/nodes` — read-only graph snapshot

`AudioRouter::graph` runs only these read-only commands through the existing
argv-based `CommandRunner` (no shell):

| Purpose | argv |
|---|---|
| defaults | `pactl -f json info` |
| sinks | `pactl -f json list sinks` |
| sources | `pactl -f json list sources` |
| playback streams | `pactl -f json list sink-inputs` |
| capture streams | `pactl -f json list source-outputs` |

Response shape (the soft-node/controller shape plus bounded extras):

```json
{
  "ok": true,
  "available": true,
  "backend": "pipewire",
  "nodes": [
    {
      "id": 42437,
      "name": "ozma-vm1",
      "description": "Ozma VM 1",
      "media_class": "Audio/Sink",
      "default": false,
      "state": "RUNNING",
      "mute": false,
      "volume_percent": 100.0,
      "monitor_source": "ozma-vm1.monitor",
      "monitor_of_sink": null
    }
  ],
  "streams": [
    {"index": 42572, "kind": "sink-input", "application": "Browser",
     "target": "ozma-vm1", "mute": false, "corked": false}
  ],
  "links": [],
  "default_sink": "alsa_output...",
  "default_source": "alsa_input...",
  "truncated": false,
  "links_available": false
}
```

Bounds and privacy:

- At most 256 nodes and 512 streams (`MAX_GRAPH_NODES`, `MAX_GRAPH_STREAMS`);
  `truncated` is true when a list was shortened.
- Only index, name, description, media class, state, mute, average volume,
  monitor metadata, `application.name` (truncated to 80 chars), and the
  resolved stream target are copied. The raw PulseAudio property bag is never
  serialized because it can contain file names, application data, and user
  context.
- `pactl` has no graph-link listing. `links` is therefore always `[]` and
  `links_available` is `false`; this is absence, not a fabricated empty graph.
- Each command is bounded by a 5 s timeout. A missing `pactl`, unreachable
  server, non-zero exit, or unparseable JSON is a typed error. The node maps
  tool/server failures to HTTP 503 (`audio.missing_command`,
  `audio.unavailable`, `audio.process_failed`, `audio.command_failed`) and
  malformed server output to 502 (`audio.output_parse`). No error body contains
  a fabricated node list.

## `POST /audio/sweep` — generated sweep and basic measurement

Request (all optional except `source` and `sink`; controller defaults in
parentheses):

| Field | Bound / default |
|---|---|
| `source` | existing source or `<sink>.monitor` (required) |
| `sink` | existing sink (required) |
| `start_hz` / `end_hz` | 10 Hz–22 kHz, end > start (20 / 20000) |
| `duration_secs` (alias `duration`) | 0.5–20 s (5) |
| `sample_rate` | 8–192 kHz (48 kHz) |
| `channels` | 1–2 (1) |
| `amplitude` | 0.05–0.95 full scale (0.5) |
| `start_delay_ms` | 0–2000 (250) |
| `tail_secs` | 0.1–5 s (0.5) |
| `phone_model`, `target_curve`, `room_name`, `node_id`, `mic_type`, `mic_model` | ≤128 chars, no control characters; echoed into the profile |

Sequence:

1. Validate every bound and both graph names; invalid input is a 400 before
   any process starts.
2. Generate a mono S16 WAV log sweep (20 Hz–20 kHz by default, 50 ms fades) in
   a private temporary directory.
3. Verify the sink and source exist through `pactl list short`.
4. Start capture (`pw-record --target <source> ... --raw -` or
   `parec --device=<source> ... --raw`) with stdout piped.
5. Wait `start_delay_ms`, play the WAV (`pw-play --target <sink>` or
   `paplay --device=<sink>`), wait for playback to exit, then collect the
   configured tail.
6. Stop and reap capture, delete the temporary directory, and analyse the
   captured PCM.

Measurement (computed in pure Rust over captured S16):

- frames and duration, peak/RMS in dBFS;
- noise floor from the quietest 10% of 20 ms windows;
- impulse peak/index/time (maximum absolute sample);
- a best-effort RT60 and dB/s decay slope from the post-peak window envelope,
  reported as `null` unless the envelope falls at least 35 dB after a 5 dB
  drop.

Profile returned inside `{"ok": true, "profile": {...}}`:

```json
{
  "id": "sweep-...",
  "name": "Office",
  "created_at": 1790000000.0,
  "bands": [],
  "band_count": 0,
  "mic_type": "phone",
  "phone_model": "generic",
  "mic_model": "",
  "target_curve": "harman",
  "room_name": "Office",
  "node_id": "",
  "active": false,
  "measurement": {
    "sample_rate": 48000,
    "channels": 1,
    "captured_frames": 74400,
    "captured_secs": 1.55,
    "peak": 0.101,
    "peak_dbfs": -19.9,
    "rms_dbfs": -31.4,
    "noise_floor_dbfs": -44.2,
    "impulse_peak": 0.118,
    "impulse_index": 18000,
    "impulse_index_secs": 0.375,
    "rt60_secs": null,
    "decay_db_per_sec": null,
    "sweep": {"start_hz": 100.0, "end_hz": 8000.0, "duration_secs": 1.0}
  }
}
```

`bands` is always empty and `active` is always false. This engine measures; it
does not fit or apply EQ. The controller's existing
`CorrectionProfile.to_dict()` fields are a documented subset (no
`raw_response_db`/`corrected_response_db`), so the dashboard can accept the
profile without claiming corrections exist.

### Live measurement evidence

The opt-in test creates a disposable null sink, plays a generated sweep to it,
and captures the sink monitor:

```text
OZMA_AUDIO_LIVE=1 cargo test -p ozma-audio --test router_live -- --ignored --nocapture
running 3 tests
live graph: 11 nodes, 0 streams,
  default sink Some("alsa_output.usb-Roland_Rubix22-00.analog-stereo.99"),
  default source Some("alsa_input.pci-0000_13_00.6.analog-stereo.99")
test graph_lists_the_live_server_read_only ... ok
test ensure_sink_reuses_then_remove_unloads_live ... ok
live sweep: 74400 frames, peak -19.9 dBFS, rms -31.4 dBFS, noise -44.2 dBFS, rt60 None
test sweep_measures_a_disposable_null_sink_monitor_live ... ok
```

The graph snapshot listed 11 real nodes (the host's hardware sinks/sources
plus the pre-existing Ozma null sinks) and no active streams at that moment.
No command other than `list`/`info` ran, and the defaults above were unchanged.

The capture length matches `250 ms + 1 s + 300 ms` at 48 kHz. The captured peak
is well above the silence threshold, so a profile is returned; RT60 is `null`
because the null-sink tail has no 35 dB decay range. No microphone was opened
and the default sink/source were untouched. Cleanup unloads the disposable
module and removes the temporary WAV directory.

### Typed errors

| Case | Result |
|---|---|
| No router / tool missing | 503 `audio.unavailable` / `audio.missing_command` |
| Sink or source absent | 400 `audio.refused` before any process |
| Invalid frequency, duration, rate, channels, amplitude, tail, name | 400 `audio.invalid_spec`; no process |
| Capture process fails or is too short | 502 `audio.capture_failed` |
| Playback exits non-zero | 502 `audio.command_failed` |
| Playback spawn fails or times out | 503 `audio.process_failed` / `audio.missing_command` |
| Captured peak below −60 dBFS | 502 `audio.capture_failed`; no profile |
| No capture bytes | 502 `audio.capture_failed`; no profile |

Every error body has `ok:false`, `available:false`, and `profile:null`.
Children are killed on success, on error, and by `kill_on_drop` on shutdown;
temporary WAV directories are removed on every path.

## Controller proxy pass-through

- `GET /api/v1/audio/nodes?node_id=<id>` proxies the node's real
  `GET /audio/nodes` body and typed status with the per-node bearer token.
  Without `node_id` it keeps the controller's local watcher snapshot.
- `GET /api/v1/audio/room-correction/node-audio` now uses the same proxy and
  passes typed errors instead of inventing `{"nodes": []}`.
- `POST /api/v1/audio/room-correction/sweep` forwards `source`, `sink`, the
  bounded frequency/duration fields, and profile metadata, and returns the
  node's measured JSON or exact typed error body/status.
- A node disconnect is a typed 502 `audio.proxy_failed`; a node 401/403 is
  passed through without leaking the token.

## Tests

```text
cargo test -p ozma-audio -p ozma-node --lib
# ozma-audio: 99 passed, 1 ignored
# ozma-node: 635 passed, 4 ignored

cargo check -p ozma-audio -p ozma-node --all-targets
# Finished

OZMA_AUDIO_LIVE=1 cargo test -p ozma-audio --test router_live -- --ignored --nocapture
# 3 passed

python3 -m pytest tests/unit/controller/test_node_api_token_proxy.py -q
# 87 passed
```

Fake-runner/unit coverage includes graph classification, defaults, truncation,
secret-free serialization, missing `pactl`, malformed JSON, bounded request
validation, generated WAV header, measured profile, silent/empty capture
rejection, and playback-failure cleanup.

## Remaining gaps

1. **No EQ fitting or application.** The profile exposes raw decay/impulse
   measurements and an empty `bands` list. Parametric EQ fitting, target-curve
   compensation (Harman/BBC/flat), PipeWire filter-chain insertion, and profile
   persistence remain controller-side or future work.
2. **No graph links.** `pactl` does not list PipeWire links; `links` is empty
   with `links_available:false`. A `pw-dump` link reader would be a separate
   parser and is not claimed here.
3. **RT60 is basic.** It is a windowed-envelope fit, not a calibrated
   Schroeder integration, and is `null` when the captured decay is shorter than
   the 35 dB fit range.
4. **No microphone validation.** The live test used only a null-sink monitor.
   The route accepts a named source, including a real microphone, but no
   microphone calibration or compensation was exercised.
5. **No resampling/room DSP.** Capture uses the requested rate; there is no
   channel mapping, latency compensation, or convolution.
6. **Single-host, same-user validation.** Remote PulseAudio servers,
   authentication, and non-Linux hosts were not exercised.
