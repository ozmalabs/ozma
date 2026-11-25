# Phase 4 — audio graph enumeration and generated sweep

Status: **live-validated** on the host's existing PipeWire/PulseAudio server
(2026-11-26). `GET /audio/nodes` now returns a real bounded graph snapshot,
`POST /audio/sweep` generates and plays a log sweep, captures a named source,
and returns a measured profile only when audio was captured, and
`GET`/`POST`/`DELETE /audio/room-correction/profile` persist the last measured
profile when the operator opts in with `--audio-profile-dir`.

This report covers the two routes that were typed 501 in
[`phase4-audio-live-report.md`](phase4-audio-live-report.md). Routing control
(`/audio/apply`, `/audio/remove`, `/audio/play`, `/audio/stop`) is unchanged.
The measurement path now returns a deterministic 10-band octave response and
suggested gains, but it still does **not** insert parametric EQ, apply
convolution, or change the graph.

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
  drop;
- a 10-band octave response at 31.5, 63, 125, 250, 500, 1 k, 2 k, 4 k, 8 k,
  and 16 k Hz. Each band is a constant-Q band-pass over the whole capture, then
  normalised to the median measured band. Because a log sweep dwells for equal
  time in every octave, the band energies are directly comparable without an
  inverse-filter reference. Centres outside the requested sweep range are
  counted in `bands_skipped`, never extrapolated.

`sweep::analyse_bands` is dependency-free (no FFT crate) and deterministic.
Per-band confidence combines the band's signal-to-noise estimate from the
quietest 20 ms window with a broadband noise-floor bound; a gain is suggested
only when confidence is at least 0.2 and the deviation is at least 0.5 dB.
Every suggested gain is clamped to ±12 dB, and the band list is always derived
from captured PCM. Bands with no usable signal are skipped rather than filled
with a default.

Profile returned inside `{"ok": true, "profile": {...}}`:

```json
{
  "id": "sweep-...",
  "name": "Office",
  "created_at": 1790000000.0,
  "bands": [
    {"freq": 1000.0, "gain": -3.2, "q": 1.4, "type": "peaking"}
  ],
  "band_count": 1,
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
  },
  "analysis": {
    "method": "bandpass-octave",
    "reference": "captured sweep band energy",
    "normalisation": "median of measured bands",
    "normalisation_db": 0.4,
    "target_curve_requested": "harman",
    "target_curve_used": "harman",
    "gain_limit_db": 12.0,
    "bands_measured": 7,
    "bands_skipped": 3,
    "mean_confidence": 0.94,
    "bands": [
      {
        "freq": 1000.0,
        "measured_db": 3.2,
        "target_db": 0.0,
        "deviation_db": 3.2,
        "suggested_gain_db": -3.2,
        "confidence": 0.96,
        "snr_db": 41.2,
        "suggested": true
      }
    ]
  }
}
```

`bands` contains only confident suggestions; `analysis.bands` keeps every
measured band (including low-confidence ones) plus the target comparison.
`active` is always false: nothing is applied to the graph. The controller's
`CorrectionProfile.to_dict()` fields remain a documented subset (no
`raw_response_db`/`corrected_response_db`), so the dashboard can accept the
profile without claiming corrections exist.

### Profile persistence

Storage is opt-in and off by default:

- `--audio-profile-dir <path>` (or `OZMA_AUDIO_PROFILE_DIR`) enables it;
  without the flag all three profile routes answer a typed 501 and nothing is
  written.
- `GET /audio/room-correction/profile` returns
  `{"ok":true,"stored":true,"profile":{...}}`, or `stored:false`/`profile:null`
  when no profile exists. A corrupt or oversized file is a typed 502.
- `POST /audio/room-correction/profile` accepts the profile directly or as
  `{"profile": {...}}`, validates it, and atomically replaces the stored copy.
- `DELETE /audio/room-correction/profile` removes the file and reports
  `cleared:true|false`.
- `POST /audio/sweep` stores the measured profile automatically when the
  directory is configured. Storage failure does not invalidate the
  measurement; the response carries `storage_enabled`, `stored`, and a typed
  `storage_error`.

The fixed file name is `room-correction-profile.json`; the writer uses a
same-directory temporary file with `0600` permissions and a rename, and reads
are bounded to 256 KiB and reject symlinks. Validation requires captured
audio (`captured_frames > 0`), at most 10 bands, finite values, `active:false`,
and gains within ±12 dB. No token or other secret is stored.

### Live measurement evidence

The opt-in test creates a disposable null sink, plays a generated sweep to it,
and captures the sink monitor:

```text
OZMA_AUDIO_LIVE=1 cargo test -p ozma-audio --test router_live -- --ignored --nocapture
running 3 tests
live graph: 15 nodes, 0 streams,
  default sink Some("alsa_output.usb-Roland_Rubix22-00.analog-stereo.99"),
  default source Some("alsa_input.pci-0000_13_00.6.analog-stereo.99")
test graph_lists_the_live_server_read_only ... ok
test ensure_sink_reuses_then_remove_unloads_live ... ok
live sweep: 74400 frames, peak -20.9 dBFS, rms -29.7 dBFS, noise -43.6 dBFS,
  rt60 None, bands 7/10 (skipped 3), mean confidence 0.00
    125.0 Hz: measured   +2.0 dB, target  +1.9 dB, suggested  -0.0 dB, confidence 0.00
    250.0 Hz: measured   -0.4 dB, target  +0.4 dB, suggested  +0.8 dB, confidence 0.00
    500.0 Hz: measured   -1.2 dB, target  +0.0 dB, suggested  +1.2 dB, confidence 0.00
   1000.0 Hz: measured   -0.9 dB, target  +0.0 dB, suggested  +0.9 dB, confidence 0.00
   2000.0 Hz: measured   +0.0 dB, target  +0.0 dB, suggested  -0.0 dB, confidence 0.00
   4000.0 Hz: measured   +1.2 dB, target  -0.5 dB, suggested  -1.7 dB, confidence 0.00
   8000.0 Hz: measured   +2.0 dB, target  -1.5 dB, suggested  -3.5 dB, confidence 0.00
test sweep_measures_a_disposable_null_sink_monitor_live ... ok
```

The graph snapshot listed real nodes (the host's hardware sinks/sources plus
the pre-existing Ozma null sinks) and no active streams at that moment. No
command other than `list`/`info` ran, and the defaults above were unchanged.

The capture length matches `250 ms + 1 s + 300 ms` at 48 kHz. The null-sink
loopback is flat within about ±2 dB across the seven measurable bands. Mean
confidence is intentionally low: the null-sink monitor has no room and little
signal above the quiet-window noise estimate, so no gain is claimed as
"confident" (all `suggested` flags are false). RT60 is `null` because the tail
has no 35 dB decay range. The same run stores, loads, and clears the profile
through `RoomProfileStore` in a private temporary directory. No microphone was
opened and the default sink/source were untouched. Cleanup unloads the
disposable module and removes the temporary WAV and profile directories.

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
| Profile store not configured | 501 `audio-profile-store-unsupported`; nothing written |
| Invalid posted profile | 400 `audio.invalid_spec`; nothing replaced |
| Corrupt/oversized stored profile | 502 `audio.output_parse` |
| Profile file I/O failure | 503 `audio.process_failed` |
| `POST /audio/room-correction/apply` | 501 `audio-eq-application-unsupported`; no graph change |

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
- `GET`, `POST`, and `DELETE /api/v1/audio/room-correction/profile` proxy the
  node's stored-profile routes with the same bearer token. `node_id` is a
  query parameter for GET/DELETE and is removed from the POST body before
  forwarding. Typed node bodies and statuses are passed through unchanged.
- A node disconnect is a typed 502 `audio.proxy_failed`; a node 401/403 is
  passed through without leaking the token.

## Tests

```text
cargo test -p ozma-audio -p ozma-node --lib
# ozma-audio: 106 passed, 1 ignored
# ozma-node: 689 passed, 5 ignored

cargo check -p ozma-audio -p ozma-node --all-targets
# Finished

OZMA_AUDIO_LIVE=1 cargo test -p ozma-audio --test router_live -- --ignored --nocapture
# 3 passed

python3 -m pytest tests/unit/controller/test_node_api_token_proxy.py -q
# 111 passed
```

Fake-runner/unit coverage includes graph classification, defaults, truncation,
secret-free serialization, missing `pactl`, malformed JSON, bounded request
validation, generated WAV header, measured octave bands with a known synthetic
boost, target-curve normalisation, silent/empty capture rejection,
playback-failure cleanup, and profile store round-trip/corruption/validation.

## Remaining gaps

1. **No EQ or filter-chain application.** The profile returns suggested
   parametric bands and a median-normalised target comparison, but nothing is
   inserted into PipeWire and `active` stays false. Unsupported filter-chain
   application stays typed 501; no automatic EQ is applied.
2. **Relative, not calibrated, levels.** Bands are normalised to their own
   median because there is no calibrated reference microphone. Absolute SPL
   and microphone calibration are not claimed.
3. **No graph links.** `pactl` does not list PipeWire links; `links` is empty
   with `links_available:false`. A `pw-dump` link reader would be a separate
   parser and is not claimed here.
4. **RT60 is basic.** It is a windowed-envelope fit, not a calibrated
   Schroeder integration, and is `null` when the captured decay is shorter than
   the 35 dB fit range.
5. **Band analysis is an octave-energy estimate.** The constant-Q band-pass
   assumes the generated sweep and a linear, roughly time-invariant path. It is
   latency-insensitive at the octave level, but it is not a full
   deconvolution/impulse-response fit and does not resolve narrow room modes.
6. **No microphone validation.** The live test used only a null-sink monitor.
   The route accepts a named source, including a real microphone, but no
   microphone calibration or compensation was exercised.
7. **Persistence is one local file.** There is no history, multi-room index, or
   cross-node sync; a second measurement replaces the first.
8. **No resampling/room DSP.** Capture uses the requested rate; there is no
   channel mapping, latency compensation, or convolution.
9. **Single-host, same-user validation.** Remote PulseAudio servers,
   authentication, and non-Linux hosts were not exercised.
