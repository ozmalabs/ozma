# Phase 4 — opt-in application control

Status: **opt-in, exact-pid process control implemented behind
`--app-control`; policy enforcement, timers, and kiosk management remain out of
scope. The flag is off by default.**

`ozma-node` still exports the read-only `/apps` monitor described in
[`phase4-storage-apps-report.md`](phase4-storage-apps-report.md). This report
covers the separate, explicit control surface layered beside it. Nothing here
runs unless the operator starts the node with `--app-control`.

## Enablement

| Flag | Default | Meaning |
|---|---|---|
| `--app-control` (`OZMA_APP_CONTROL`) | off | Mount useful control handlers; without it every control route is a typed `503 apps.control-disabled`. |
| `--app-control-kill-after-seconds` (`OZMA_APP_CONTROL_KILL_AFTER_SECONDS`) | `2` | Wait between `SIGTERM` and the bounded `SIGKILL` escalation. Clamped to 1–30 seconds. |
| `--app-control-allow-other-users` (`OZMA_APP_CONTROL_ALLOW_OTHER_USERS`) | off | Deliberately widen the ownership policy. All exact-pid, start-time, process-tree, kernel-thread, and confirmation checks still apply. |

The routes are mounted in `controller_compat::compat_router`, so they use the
existing node bearer-token guard. Without `--app-control` the routes exist but
are inert: they return `503 apps.control-disabled` and never call `kill(2)`.

## Endpoints

All control routes accept an optional JSON body:

```json
{
  "start_time": 123456,
  "confirm": false
}
```

- `start_time` is the `/proc/<pid>/stat` start time (field 22). When supplied it
  must match. Even when omitted, the node reads the start time at request time
  and re-reads it immediately before **every** signal, so a reused pid is never
  signalled. `/apps` deliberately does not expose `start_time`, so callers that
  only use the process list normally omit it; supplying it adds an explicit
  caller-side check.
- `confirm: true` is required for an explicit `SIGKILL` through `/signal`. The
  bounded escalation inside `/terminate` is authorized by the terminate action
  itself and is reported as `escalated: true`.

| Route (and `/api/v1` alias) | Action |
|---|---|
| `POST /apps/{pid}/terminate` | `SIGTERM`, wait `kill_after`, then `SIGKILL` only if the same pid/start-time is still present and is not a zombie. |
| `POST /apps/{pid}/signal` | Exactly one of `TERM`, `KILL`, `STOP`, `CONT` (an optional `SIG` prefix is accepted). No escalation. |
| `POST /apps/{pid}/suspend` | `SIGSTOP`. |
| `POST /apps/{pid}/resume` | `SIGCONT`. |

The `/signal` body adds the signal name:

```json
{"signal": "TERM", "start_time": 123456}
```

### Success response

```json
{
  "ok": true,
  "pid": 4242,
  "start_time": 123456,
  "action": "terminate",
  "requested": "TERM",
  "signals": ["TERM", "KILL"],
  "escalated": true,
  "terminated": true
}
```

`terminated` is present only for `terminate`:

- `true` — the pid was absent or a zombie after the bounded wait;
- `false` — it was still running at the final observation (the `SIGKILL` was
  delivered, but the node does not promise synchronous reaping);
- omitted — the final state could not be observed.

## Policy

Every signal is preceded by a fresh `/proc/<pid>/{stat,status}` read and the
same checks. Only the exact pid is signalled; names, `comm`, `cmdline`, and
wildcards are never used.

| Condition | Result |
|---|---|
| pid `0` | `403 apps.policy` — pid 0 names a process group. |
| pid `1` | `403 apps.policy` — init is never signalled. |
| This node's pid, an ancestor, or a descendant | `403 apps.own-process`. Descendants include node-spawned helpers such as ffmpeg; ancestors include the supervisor chain. |
| Kernel thread (`PF_KTHREAD`) | `403 apps.policy`. |
| Zombie/dead state (`Z`, `X`, `x`) | `404 apps.not-found`. |
| Process owned by another uid | `403 apps.permission` by default; allowed only with `--app-control-allow-other-users`. |
| Explicit `SIGKILL` without `confirm: true` | `403 apps.policy`. |
| Caller `start_time` mismatch or pid reused before a signal | `403 apps.policy`; no signal is sent. |
| pid absent | `404 apps.not-found`. |
| Backend `EPERM`/unreadable ancestry | `403 apps.permission`. |
| Other procfs failure | `503 apps.proc-unavailable`. |
| Flag disabled | `503 apps.control-disabled`. |
| Signal outside the allowlist or malformed request | `400 apps.invalid-request`. |

Typed errors use the same document shape as the read-only host surfaces:

```json
{
  "ok": false,
  "available": false,
  "code": "apps.own-process",
  "error": "pid 4242 belongs to the ozma-node process tree and is never signalled"
}
```

The policy is deliberately fail-closed: if process ancestry cannot be read, the
node refuses rather than guessing that the target is unrelated.

## Escalation bounds

`terminate` waits at most the configured delay (1–30 seconds, default 2). It
does not signal by name, does not retry forever, and does not send `SIGKILL`
when:

- the pid disappeared;
- the process became a zombie;
- the pid was reused (start time changed);
- policy revalidation failed.

The Python `agent/app_monitor.py` used the same `SIGTERM` then 2-second
`SIGKILL` shape, but matched processes by name. This surface deliberately uses
an exact pid and kernel start time instead.

## Read-only `/apps` is unchanged

`GET /apps` still returns only `pid`, `name`, `cpu_percent`, `cpu_window`,
`rss_bytes`, `user`, and `uid`. It does not gain a `start_time`, `cmdline`, or
control field, and the control routes do not mutate its process list. The
`host_surfaces.apps` connection advertisement continues to describe the
read-only list; app-control enablement is a startup flag, not a per-process
advertisement.

## Controller proxy

The controller exposes minimal pass-throughs for an external consumer:

| Controller route | Node path |
|---|---|
| `POST /api/v1/nodes/{node_id}/apps/{pid}/terminate` | `/apps/{pid}/terminate` |
| `POST /api/v1/nodes/{node_id}/apps/{pid}/signal` | `/apps/{pid}/signal` |
| `POST /api/v1/nodes/{node_id}/apps/{pid}/suspend` | `/apps/{pid}/suspend` |
| `POST /api/v1/nodes/{node_id}/apps/{pid}/resume` | `/apps/{pid}/resume` |

The controller uses `_node_app_control`, which preserves typed `apps.*`
responses (including 403 policy/permission/own-process) and maps only an
untyped 401/403 to a node-auth error. It forwards the node token only in the
request header, and the controller never resolves a name or enumerates `/proc`
on the node's behalf.

No controller UI or parental-control consumer calls these routes yet. The
Python agent enforced pushed whitelist/blacklist/kiosk/timer policy locally;
that policy engine is not ported. The proxies are therefore an operator/API
primitive, not an enforcement feature.

## Tests

Rust (`cargo test -p ozma-node --lib app_control::`):

- fake proc/signal backend policy matrix: pid 0, pid 1, this process, an
  ancestor, a descendant, another uid, a kernel thread, a zombie, a missing pid,
  and a stale caller start time (reused pid); asserts no signal was delivered;
- `SIGTERM → SIGKILL` escalation with a bounded wait, and no escalation when the
  process exits after `SIGTERM`;
- explicit `SIGKILL` requires `confirm: true`, while `TERM`/`STOP`/`CONT` do not;
- suspend/resume map to `STOP`/`CONT`;
- the other-user policy can be widened only by explicit configuration;
- production kill delay is clamped to 1–30 seconds;
- allowlist parsing rejects `HUP` and numeric signal values;
- procfs parsing covers start time, ppid, `PF_KTHREAD`, uid, and a comm
  containing spaces and `)`.

Live Linux evidence (`#[cfg(target_os = "linux")]` test):

- spawns a scratch `sleep` through a short-lived shell so it is reparented to
  init and is not in the node's process tree;
- reads its real start time and terminates it through `AppControl`;
- waits for the pid to disappear;
- refuses this process's own pid with `apps.own-process`;
- refuses pid 1 with `apps.policy`.

Controller (`python3 -m pytest tests/unit/controller/test_node_api_token_proxy.py`):

- the bearer-token proxy matrix covers terminate/signal/suspend/resume;
- the request body and exact pid reach the node;
- a typed `403 apps.own-process` body keeps its status, code, and error, and the
  token never leaks into the response.

## Remaining gaps

- **No pushed policy engine.** Whitelist/blacklist/kiosk modes, schedules,
  timers, breaks, screen locking, kiosk relaunch, and usage reporting from
  `agent/app_monitor.py` are not ported. The node offers exact-pid primitives,
  not parental-control enforcement.
- **No process discovery by application name.** This is intentional: the node
  never matches on `comm`/`cmdline`, so a controller that wants to block an
  application must resolve its pids from `/apps` and pass the exact pid and
  start time.
- **No audit stream.** Successful control responses are not persisted; the
  controller must log them. There is no per-node control event history.
- **No synchronous reaping.** `terminate` reports the state it observes; a node
  is not the parent of the target and does not reap unrelated processes.
- **No Windows/macOS backend.** The procfs/signal backend is Linux-specific;
  other platforms would need a separate implementation.
