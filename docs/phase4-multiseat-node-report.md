# Phase 4 — read-only multiseat node report

Status: **implemented (2026-11-25).** `ozma-node` can expose the persisted
multiseat plan/status at public `GET /api/v1/seats`, and the controller consumes
that report when present. Seat lifecycle remains with the standalone
`ozma-multiseat` supervisor.

## Why this is read-only

The standalone CLI already owns the only safe lifecycle path:

* it enumerates displays and input topology;
* it plans one `ozma-node --profile desktop` child per seat;
* it applies optional ownership/routing/audio/input-isolation resources;
* it waits for readiness, restarts failures with backoff, and stops/reaps
  children on signal.

Moving start/stop into `ozma-node` would create two supervisors for the same
children, require the node to know every other seat and display, and put
process-group teardown, port ownership, and resource rollback in the wrong
process. It would also let an HTTP request on one seat start or kill another
seat without a controller-side lifecycle model.

Decision: **the node reports, the supervisor mutates.** There is no
`POST /api/v1/seats`, no start/stop/restart route, no child spawn on this path,
and no runtime pid/restart state in the node report. The response states this
explicitly with `lifecycle:"external"` and `start_stop:false`.

## Node surface

Flags (off by default):

| Flag | Meaning |
|---|---|
| `--multiseat` (visible alias `--multiseat-plan`) | Opt in to the read-only seat report. Requires `--seats-file`. |
| `--seats-file PATH` | Versioned `seats.json` store shared with the supervisor. Loaded once at startup. |

Without `--multiseat`, the route is still mounted and returns
`configured:false`, so the controller can distinguish "not a multiseat node"
from a transport failure. `--seats-file` without `--multiseat` is ignored with
a warning.

`GET /api/v1/seats` returns:

```json
{
  "ok": true,
  "configured": true,
  "mode": "read-only",
  "lifecycle": "external",
  "start_stop": false,
  "source": "/etc/ozma/seats.json",
  "seat_id": "host-seat-0",
  "plan": {"version": 1, "seats": [{"id": "host-seat-0", "...": "..."}]},
  "status": {
    "version": 1,
    "seat_count": 1,
    "node_count": 1,
    "seats": [
      {
        "seat_id": "host-seat-0",
        "display_output": "HDMI-1",
        "display_index": 0,
        "display_width": 1920,
        "display_height": 1080,
        "input_hub": "1-1",
        "input_devices": ["/dev/input/event0"],
        "audio_sink": "ozma-seat-0",
        "udp_port": 7331,
        "http_port": 7382,
        "profile": "desktop",
        "state": "planned",
        "available": true
      }
    ],
    "diagnostics": []
  },
  "node_seat": {"seat_id": "host-seat-0", "state": "planned", "available": true}
}
```

Rules:

* `seat_id` is the explicit `OZMA_SEAT_ID` environment value only. The node
  never derives an id from `--name` or a hostname. `node_seat` appears only when
  that id names a persisted seat.
* The plan is the persisted store verbatim; the node does not re-enumerate
  displays or re-plan. Capture transport/ownership/routing intent is not
  invented because the store does not record it.
* The only observed inputs for missing-display detection are configured
  `--display-output` names and the explicit `OZMA_X11_OUTPUT` environment value.
  An empty observation set means "not discovered", not "all displays missing".
* A persisted seat whose display was not observed stays in every list with
  `available:false`, `state:"planned"`, and a
  `persisted_seat_missing_display` diagnostic. It is never reported running.
* Store failures are typed HTTP 503 documents with no seats:
  `seat_store_read`, `seat_store_parse`, `seat_store_version`,
  `seat_store_invalid`, or `seat_store_conflict`. The message may name the
  local path; the code never does.
* The route is mounted next to the other public read-only probes
  (`/display/info`, `/node/status`, `/agent/metrics`), so the controller can
  read it with or without a node bearer token. It exposes no control surface.

## Controller consumption

`controller/seats.py` now fetches each node's `/api/v1/seats` with a bounded
timeout (1.5 s) and node bearer headers:

* 404, no HTTP API, or `configured:false` — preserve the historical
  `seat_id`/`parent_node_id`/`<machine>-seat-<n>` grouping exactly.
* 200 with `configured:true` — use the explicit persisted `seat_id` as
  authoritative membership (so a renamed node still joins its machine group)
  and add a bounded `multiseat` entry to the seat row:
  `mode`, `plan_seat_count`, `state`, `available`.
* Typed node failure — preserve `{node_id, status, error:{code,message}}` in a
  top-level `node_errors` list. The node still appears through the fallback
  grouping; no node is dropped or invented.
* Malformed 200 — `seat_report_invalid` in `node_errors`, fallback grouping.
* `node_errors` is omitted when every node is absent/healthy, so the response
  shape is unchanged for a purely Python/older fleet.
* The node's local `source` field, `plan`, argv, environment, and token are
  never copied into the controller response. A typed node error message is
  passed through verbatim, so it may contain the node's own local store path;
  only the stable `code` is guaranteed path-free.

## Tests

Rust (`cargo test -p ozma-multiseat -p ozma-node --lib`):

* persistence status: no invented missing display without discovery; missing
  display retained as `planned` with a typed diagnostic.
* node route via `tower::oneshot`: not-configured (`configured:false`), persisted
  plan with explicit seat id, missing display, read-only lifecycle
  (`lifecycle:"external"`, `start_stop:false`, no pid), parse error as typed
  503, newer-version error, no invented seat id.
* CLI parsing: off by default, `--multiseat-plan` alias, `--seats-file`
  requirement.

Python (`python3 -m pytest tests/unit/controller/test_seats.py`):

* existing grouping/API/token/dashboard tests preserved;
* node report consumed into the explicit seat id and bounded `multiseat` data;
* typed node error passed through while fallback grouping remains;
* absent report preserves the historical response shape;
* default fixture answers 404, so no unit test opens a socket.

No test starts a seat process, uses sudo, or requires a real display.

## Remaining gaps

1. **No node/controller lifecycle control.** Start, stop, restart, and runtime
   pid/restart state remain CLI-only and are not surfaced to the controller.
2. **No live display re-enumeration in the node.** Missing-display detection
   trusts configured outputs and `OZMA_X11_OUTPUT`; the node does not run
   `xrandr` or watch DRM hotplug.
3. **No runtime overlay on the persisted plan.** The report is `planned` plus
   availability, not the supervisor's actual state. The supervisor's own
   `/status` on `--status-port` still holds the live pid/restart/resource data.
4. **No store reload endpoint.** The store is read once at startup; an operator
   replan requires a node restart (or a future explicit reload route).
5. **No per-seat capture/ownership fields.** The version-1 store does not record
   capture transport, ownership, or routing intent, so the node report omits
   them rather than inventing X11/PipeWire details.
6. **`ozma-softnode` is not integrated.**
7. **Process isolation is unchanged.** cgroups, namespaces, subreapers, log
   rotation, and restart limits remain future supervisor work.
