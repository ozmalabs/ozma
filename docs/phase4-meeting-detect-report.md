# Phase 4 meeting detection report — supervisor status and controller report path

Date: 2026-11-25
Scope: close the "meeting detection has no controller report path" gap from
[`phase4-cutover-audit.md`](phase4-cutover-audit.md) §16.3 item 5, without
touching `ozma-node`, `ozma-usb`, `ozma-multiseat`, or `ozma-mesh`.

## Problem

`ozma-meeting-detect` already had a working Unix-socket IPC server, but it was
an island:

- the supervisor did not start it, supervise it, or report it;
- `GET /status` had no meeting field, so an operator could not tell "disabled"
  from "detector broken" from "no call";
- the controller had no endpoint and no typed shape to pass through;
- a newly connected IPC client did not reliably receive a snapshot because the
  `ForceScan`/`GetSnapshot` notify was never consumed.

The result was an honest detector that nothing could observe. There was no path
that could both expose a real detection and refuse to fabricate one.

## Architecture

```
ozma-meeting-detect                     ozma-virtual-node                  controller
────────────────────                    ────────────────                   ──────────
process scan + ICS                      --meeting-detect (opt-in)          GET /api/v1/meeting/status
  │                                     bridge task                        │
  │  Unix socket                       ├─ spawn child / attach socket      │  GET <supervisor>/status
  └──────── snapshot ──────────────────►├─ poll snapshot (bounded)          │  (OZMA_SUPERVISOR_STATUS_URL)
        ($XDG_RUNTIME_DIR/               ├─ restart with backoff            ◄──┘  typed + bounded projection
         ozma-meeting-detect.sock)       └─ /status.meeting
```

The supervisor owns lifecycle and normalization. The controller only reads the
already-aggregated HTTP document; it never opens the detector socket, scans
processes, or reads calendars.

### Detector IPC changes

- `ozma-meeting-detect --socket <path>` selects the socket explicitly. The
  default is unchanged (`$XDG_RUNTIME_DIR/ozma-meeting-detect.sock`, fallback
  `/tmp/ozma-meeting-detect.sock`).
- The detector now consumes the existing `ForceScan`/`GetSnapshot` notify and
  publishes a snapshot immediately, so a newly connected client always receives
  current state instead of waiting for a change.

### Supervisor `GET /status` field

`meeting` is always present and defaults to `disabled`, so a controller can read
old and new supervisors with the same code.

```json
{
  "meeting": {
    "enabled": true,
    "state": "running",
    "meeting_state": "active",
    "connected": true,
    "manage_child": false,
    "detector_interval_secs": 5,
    "poll_interval_ms": 2000,
    "restart_count": 0,
    "last_update_unix_ms": 1700000000000,
    "last_update_age_ms": 120,
    "last_detection_unix_ms": 1700000000000,
    "last_detection": { "...": "most recent real meeting, retained after it ends" },
    "meetings": [ { "...": "currently active meetings, capped at 8" } ],
    "error": null
  }
}
```

Meeting entry shape (whitelisted; no attendee addresses or calendar UIDs):

```json
{
  "id": "zoom-1234567",
  "platform": "zoom",
  "active": true,
  "started_at_unix_ms": 1700000000000,
  "duration_s": null,
  "title": "Daily standup",
  "organizer": "host@example.test",
  "attendee_count": 4,
  "has_calendar_metadata": true
}
```

`state` is lifecycle: `disabled`, `starting`, `running`, `unavailable`,
`stopped`. `meeting_state` is call state: `idle` (connected, no call), `active`
(at least one real entry), `unknown` (not connected or inconsistent).

A meeting is `active` **only** when an IPC snapshot contains a real entry with
`active: true`. If the detector says `active` but sends no entry, the report is
`unknown` with error `inconsistent_snapshot`; it never claims a call. When the
bridge stops, current meetings are cleared and `meeting_state` becomes `unknown`
while `last_detection` is retained.

Bounded fields: 8 meetings, 64-char id, 200-char title, 128-char organizer, and
at most 1024 attendees counted (addresses are never copied). Socket paths,
binary paths, tokens, CALDAV/ICS paths, attendee lists and calendar UIDs are
absent by construction.

### Typed errors

| Code | Meaning |
|---|---|
| `binary_missing` | Configured detector binary does not exist. |
| `socket_missing` | Socket was absent before the startup timeout. |
| `socket_unavailable` | Socket exists but refuses/resets connections. |
| `timeout` | Connect/write/read exceeded `--meeting-detect-timeout-ms`. |
| `protocol` | Peer sent a frame that is not a snapshot. |
| `spawn_failed` | Child could not be started for a non-`NotFound` reason. |
| `exited` | Child exited while the bridge expected it to run. |
| `inconsistent_snapshot` | `active` summary arrived with no active meeting entry. |
| `io` | Generic local I/O failure. |

Messages are fixed text or bounded I/O kind descriptions; they never include a
socket or binary path.

## Lifecycle

All flags are opt-in; without them the document stays `disabled` and the
historical behavior is preserved.

| Flag | Default | Effect |
|---|---|---|
| `--meeting-detect` | off | Enable the bridge and report `meeting`. |
| `--meeting-detect-attach` | off | Attach to an existing socket instead of spawning. |
| `--meeting-detect-bin` | `ozma-meeting-detect` | Child binary path. |
| `--meeting-detect-socket` | XDG default | Explicit detector socket. |
| `--meeting-detect-poll-interval-ms` | `2000` | Snapshot poll cadence (minimum 100 ms). |
| `--meeting-detect-timeout-ms` | `3000` | Startup/snapshot timeout (minimum 100 ms). |
| `--meeting-detect-interval-secs` | `5` | Detector process scan cadence (`--interval`). |
| `--meeting-detect-ics-path` | none | Repeatable `--ics-path` for enrichment. |

Policy:

- **Spawn**: `--meeting-detect` starts the child with an explicit `--socket`, so
  the supervisor owns the socket namespace. Attach mode never spawns.
- **Timeout**: the first snapshot must arrive within the configured timeout,
  otherwise the child is killed and the report becomes `unavailable`.
- **Restart**: failures restart with capped exponential backoff (1 s doubling
  to 30 s). `restart_count` is exposed; a successful snapshot resets the delay.
- **Shutdown**: a bridge receiving the supervisor's shutdown watch stops
  polling, sends SIGKILL through `start_kill`, waits up to 2 s for the child to
  be reaped, clears current meetings, and publishes `stopped`. `main` gives the
  bridge a 5 s window after `mgr.run()` returns.

## Controller report path

`GET /api/v1/meeting/status` (read scope):

- Reads only `OZMA_SUPERVISOR_STATUS_URL` (and optional
  `OZMA_SUPERVISOR_STATUS_TOKEN`, sent upstream and never returned).
- Returns `503` with a clear message when the URL is not configured.
- Returns `200` with `available: false` and `state: "disabled"` when the
  supervisor predates the report. This preserves old deployments.
- Returns a whitelisted, bounded projection from `controller/meeting_status.py`.
  Unknown upstream keys are dropped. Unknown states become `unavailable`;
  unknown error codes become `unknown`; error text has absolute paths and
  bearer tokens redacted. `available` means the supervisor reported the field,
  not that detection is enabled: an explicitly disabled bridge is
  `available: true, enabled: false, state: "disabled"`.
- Never fetches anything from `/api/v1/status` (the hot local snapshot). That
  document only advertises `meeting_detect.{configured,status_url}`.

Automation/transcription consumers can poll this endpoint. This pass does not
auto-start transcription: that remains a product decision rather than an
implicit side effect of detection.

## Tests

Commands ran in `/home/matt/ozma/ozma` with no sudo, no network installs, and no
real call.

```bash
cargo test -p ozma-softnode
# 139 passed; 0 failed

cargo check -p ozma-softnode --all-targets
# Finished successfully; only the pre-existing VMInfo dead-code warnings

rustfmt --edition 2021 --check \
    ozma-softnode/src/meeting.rs \
    ozma-softnode/src/meeting_main.rs \
    ozma-softnode/src/meeting_detect.rs \
    ozma-softnode/src/ipc.rs \
    ozma-softnode/src/main.rs \
    ozma-softnode/src/status.rs
# exit 0

python -m pytest \
    tests/unit/controller/test_meeting_status_proxy.py \
    tests/unit/controller/test_supervisor_status_proxy.py -q
# 17 passed
```

Coverage added:

- fake detector snapshot → `running`/`idle`; a real active meeting → `active`;
- `active` with no entry → `unknown`, no fabricated meeting;
- missing socket and missing binary → typed codes, messages without paths;
- bounded meeting count/title/id and attendee counts (no addresses);
- bridge start → poll → shutdown publishes `stopped` and clears meetings;
- managed child spawn is killed and reaped on shutdown;
- controller typed pass-through, absent field (200/disabled), unconfigured
  (503), inconsistent upstream (not active), path/token redaction, and field
  bounds.

## Remaining gaps

- Detection is still process-name based (`sysinfo`); Google Meet can over-match
  a browser process when no calendar event confirms it. The status is honest
  about what the detector reported, not proof of call audio.
- No dashboard panel or notification was added. Consumers must call
  `/api/v1/meeting/status`.
- No systemd unit. The detector is still a session/on-demand binary because it
  needs the user's calendar files and PipeWire session. Attach mode supports a
  session unit that owns those resources.
- Polling is periodic (default 2 s), not a persistent push subscription. Real
  detections are bounded by that latency.
- No live Zoom/Teams/Meet run was possible here; evidence is fake-IPC and
  contract-level.
- Packaging is unchanged: `ozma-meeting-detect` is installed as an optional
  on-demand binary, not a unit.

## Files changed

- `ozma-softnode/src/meeting.rs` — new bridge: child/attach lifecycle, bounded
  IPC snapshot polling, typed report/errors, tests.
- `ozma-softnode/src/meeting_main.rs` — `--socket` and immediate-snapshot
  notification.
- `ozma-softnode/src/meeting_detect.rs` — consume the force-scan notify.
- `ozma-softnode/src/ipc.rs` — shared decode types for both binaries.
- `ozma-softnode/src/main.rs` — opt-in CLI flags and bridge wiring.
- `ozma-softnode/src/status.rs` — `meeting` field on `/status`, route tests.
- `controller/meeting_status.py` — new typed, bounded projection.
- `controller/api.py` — `GET /api/v1/meeting/status`, shared supervisor fetch,
  status hint.
- `tests/unit/controller/test_meeting_status_proxy.py` — controller tests.
- `docs/phase4-packaging-report.md` — on-demand note updated for the new
  supervisor/controller wiring.
- `docs/phase4-meeting-detect-report.md` — this report.

`docs/parity-matrix.md` and `docs/plan-execution-status.md` were not edited.
