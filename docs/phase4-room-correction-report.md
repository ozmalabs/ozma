# Phase 4 — calibrated octave-band analysis and profile persistence

Status: **implemented and live-validated** on the host's existing
PipeWire/PulseAudio server (2026-11-26). `POST /audio/sweep` now returns a real
10-band octave response with per-band confidence and a target-curve comparison.
When the operator opts in with `--audio-profile-dir`, the last measured profile
is stored atomically and served by `GET`/`POST`/`DELETE
/audio/room-correction/profile`.

This report covers measurement analysis and persistence only. It does **not**
insert parametric EQ, a PipeWire filter-chain, or convolution into the graph;
`active` is always false and an unsupported filter-application surface stays
typed 501.

## Band analysis

### Transform

Bands are computed from the captured PCM, never from request parameters alone:

1. The capture is downmixed to mono exactly as the peak/RMS path does.
2. Ten octave centres are considered: 31.5, 63, 125, 250, 500, 1 k, 2 k, 4 k,
   8 k, and 16 k Hz (`ANALYSIS_BAND_CENTRES_HZ`).
3. A centre outside the requested sweep range or at/above Nyquist is counted
   in `bands_skipped`; it is never extrapolated.
4. Each usable centre gets a constant-Q (Q = 1.4) RBJ band-pass over the whole
   capture, and the output energy is accumulated
   (`sweep::bandpass_energy`).
5. A log sweep spends the same time in every octave, so equal-Q band energies
   are directly comparable. Each band is normalised to the **median** measured
   band (`normalisation_db`), which removes overall gain and yields a relative
   room response.
6. A per-band SNR estimate uses the quietest 20 ms window, scaled to the full
   capture length, with a broadband-noise bound as a floor.
7. Bands whose signal is more than 20 dB below the noise estimate are skipped.
   A skipped band is absent, not filled with 0 dB.
8. Suggested gain is `target - measured`, clamped to ±12 dB. A suggestion is
   emitted only when confidence is at least 0.2 and the correction is at least
   0.5 dB.

The implementation has no FFT/Goertzel crate dependency and no random or
time-dependent math. It is deterministic for a given capture and request.

### Choosing an approach

A full-window chirp DFT ratio was prototyped and rejected: the chirp's phase
evolution made the ratio numerically unstable (a null-sink loopback produced
spurious ±40 dB bands). Octave-band energy is insensitive to the constant
playback/capture latency and produced a flat null-sink response within about
±2 dB, so it is the shipped transform.

### Target curves

The controller's three documented curves are built in:

| Name | Shape |
|---|---|
| `flat` | 0 dB at every band |
| `harman` | +4 dB bass shelf, flat mids, gentle treble roll-off to −7 dB |
| `bbc` | +2 dB low bass, small presence dip, −5 dB at 20 kHz |

Unknown names fall back to `harman`, and both `target_curve_requested` and
`target_curve_used` are reported so a caller can see the fallback. Target
levels are normalised by their own median across the measured bands before
comparison.

### Response shape

`SweepProfile.bands` remains the controller-compatible EQ list:

```json
{"freq": 1000.0, "gain": -3.2, "q": 1.4, "type": "peaking"}
```

`SweepProfile.analysis` carries the full measurement:

```json
{
  "method": "bandpass-octave",
  "reference": "captured sweep band energy",
  "normalisation": "median of measured bands",
  "normalisation_db": -7.6,
  "target_curve_requested": "harman",
  "target_curve_used": "harman",
  "gain_limit_db": 12.0,
  "bands_measured": 7,
  "bands_skipped": 3,
  "mean_confidence": 0.73,
  "bands": [
    {
      "freq": 1000.0,
      "measured_db": -1.2,
      "target_db": 0.0,
      "deviation_db": -1.2,
      "suggested_gain_db": 1.2,
      "confidence": 0.82,
      "snr_db": 26.4,
      "suggested": true
    }
  ]
}
```

`bands_measured + bands_skipped = 10` for a full-range request. Bands are
relative to the capture's own median; there is no calibrated reference
microphone, so absolute SPL is not claimed.

### Safety limits

- At most 10 bands (`MAX_SWEEP_BANDS`); suggested gains are clamped to ±12 dB.
- All measured/target/deviation levels are clamped to ±40 dB and finite.
- Confidence is clamped to 0..1.
- A silent capture (peak below −60 dBFS) or an empty capture is a typed
  `audio.capture_failed`; no profile, bands, or analysis is returned.
- A capture too short or too noisy for a band simply produces fewer bands.

## Profile persistence

### Opt-in configuration

Storage is disabled by default. The operator enables it with:

```text
--audio-router pipewire|pulseaudio \
--audio-profile-dir /var/lib/ozma/room-profiles
```

or `OZMA_AUDIO_PROFILE_DIR`. Without `--audio-profile-dir`, all three profile
routes answer a typed 501 `audio-profile-store-unsupported`; nothing is written.

### Routes

| Method | Route | Behaviour |
|---|---|---|
| `GET` | `/audio/room-correction/profile` | `{ok:true, stored:true, profile:{...}}`, or `stored:false, profile:null` when empty |
| `POST` | `/audio/room-correction/profile` | Accepts the profile directly or `{"profile":{...}}`; validates and atomically replaces |
| `DELETE` | `/audio/room-correction/profile` | Removes the file; `{ok:true, cleared:true|false}` |
| `POST` | `/audio/room-correction/apply` | Typed 501 `audio-eq-application-unsupported`; no filter node is inserted |

`POST /audio/sweep` also stores the measured profile automatically when a
directory is configured. Storage failure does not fail the measurement: the
sweep response carries `storage_enabled`, `stored`, and a typed
`storage_error` when applicable.

The controller proxies these as:

- `GET /api/v1/audio/room-correction/profile?node_id=<id>`
- `POST /api/v1/audio/room-correction/profile` (body contains `node_id`)
- `DELETE /api/v1/audio/room-correction/profile?node_id=<id>`

The proxy forwards the node bearer token, removes `node_id` from the POST body,
and passes typed node statuses/bodies through unchanged. A node disconnect is a
typed 502 `audio.proxy_failed`.

### File and validation properties

- Fixed file name `room-correction-profile.json` inside the configured
  directory. A request cannot choose a path.
- Atomic replacement: a same-directory temporary file with `0600` permissions
  (Unix) is written, flushed, `fsync`ed, and renamed over the target.
- Reads are capped at 256 KiB (`MAX_ROOM_PROFILE_BYTES`); symlinks and
  non-regular files are rejected.
- Validation requires:
  - finite, non-negative `created_at`;
  - `active == false` (a stored measurement cannot claim EQ was applied);
  - `band_count == bands.len()` and at most 10 bands;
  - finite band frequency inside 10 Hz–22 kHz, gain within ±12 dB, Q inside
    0.1–10, and `peaking`/`low_shelf`/`high_shelf` type;
  - `captured_frames > 0`, positive peak, finite measurement fields, and a
    valid generated-sweep descriptor;
  - analysis fields finite, confidence 0..1, at most 10 response bands, and
    `bands_measured == analysis.bands.len()`.
- No token, secret, path, or arbitrary property bag is stored.

### Typed errors

| Case | Result |
|---|---|
| No store configured | 501 `audio-profile-store-unsupported` |
| Invalid profile (missing fields, active, bad gain, no capture) | 400 `audio.invalid_spec` |
| Corrupt or oversized stored file | 502 `audio.output_parse` |
| File I/O failure | 503 `audio.process_failed` |
| No stored profile | 200 with `stored:false`, `profile:null` |

Every error body has `ok:false`, `available:false`, `stored:false`, and
`profile:null`.

## Tests and live evidence

### Synthetic bands

`ozma-audio/src/sweep.rs` renders a log sweep with a known +12 dB bell at 1 kHz
and asserts:

- all 10 bands are measured for a 20 Hz–20 kHz request;
- the 1 kHz band is more than 6 dB above its 500 Hz neighbour;
- the suggested gain at 1 kHz is a cut below −4 dB;
- every gain is finite and within ±12 dB;
- target comparison and confidence metadata are populated.

A separate test feeds silence through the analyser and asserts zero bands, zero
measured bands, and all 10 centres skipped (no invented bands). Target-curve
tests check flat/Harman/BBC values and the unknown-name fallback.

### Persistence tests

`ozma-audio/src/room.rs` covers:

- store → load → exact round trip → clear;
- missing file returns `None` and `clear` returns `false`;
- corrupt JSON is `audio.output_parse`; an oversized file is
  `audio.invalid_spec`;
- profiles without captured audio, with `active:true`, with a band-count
  mismatch, or with a gain beyond ±12 dB are rejected;
- the stored file is `0600` on Unix.

`ozma-node/src/controller_compat.rs` covers the routes end to end with a fake
router and a temporary directory:

- typed 501 without an opt-in directory;
- sweep auto-store, GET round trip, POST wrapper, DELETE, and `stored:false`
  afterwards;
- invalid POST is 400 and corrupt-file GET is 502.

### Live run

```text
OZMA_AUDIO_LIVE=1 cargo test -p ozma-audio --test router_live -- --ignored --nocapture
running 3 tests
test ensure_sink_reuses_then_remove_unloads_live ... ok
test graph_lists_the_live_server_read_only ... ok
live sweep: 74400 frames, peak -20.9 dBFS, rms -29.7 dBFS, noise -43.6 dBFS,
  rt60 None, bands 7/10 (skipped 3), mean confidence 0.00
test sweep_measures_a_disposable_null_sink_monitor_live ... ok
```

The disposable null-sink loopback measured flat within about ±2 dB across the
seven bands inside the requested 100 Hz–8 kHz sweep; the three outside centres
(31.5, 63, and 16 k Hz) were reported as skipped. The low mean confidence is
honest: a null sink has no room and little signal above the quiet-window noise
estimate, so no gain was flagged as a confident suggestion. The same test
stores, loads, and clears the measured profile through `RoomProfileStore`. No
microphone was opened and the default sink/source were untouched.

## Remaining gaps

1. **No EQ or filter-chain application.** Suggested bands are advisory. There
   is no PipeWire filter-chain insertion, no convolution, and no automatic
   application; `active` stays false.
2. **Relative levels only.** No calibrated microphone or absolute SPL is
   claimed. Phone/USB mic compensation remains controller-side.
3. **Octave resolution.** The transform resolves octave trends, not narrow
   room modes or parametric-Q fitting. A future deconvolution/impulse-response
   fit would be a separate implementation.
4. **One profile file.** There is no measurement history, multi-room index, or
   cross-node sync; a new measurement replaces the previous one.
5. **RT60 remains basic.** It is the existing windowed-envelope fit, not a
   calibrated Schroeder integration.
6. **Live validation used a null sink only.** A real microphone and a real
   room were not exercised in this pass.
7. **Same-host, same-user validation.** Remote PulseAudio servers,
   authentication, and non-Linux hosts were not exercised.
