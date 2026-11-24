# Gate 2 controller isolated-venv compatibility report

Read-only compatibility audit of the Python controller against a disposable
Rust `ozma-node`, followed by a minimal startup repair (§3a), using an isolated
`/tmp` virtualenv, a temporary copy of `controller/`, and temporary ports.
**Gate 2 has not been run and is not claimed.** No global Python package was
installed, no `sudo` was used, and no commit was made. The repair touched only
`controller/main.py`; no Rust source or `docs/parity-matrix.md` was changed.

> **Main result (after repair):** the startup regression is fixed. In a clean
> venv built from `controller/requirements.txt`, `import main` and
> `main.py --help` both succeed, and the controller daemon starts on a free
> loopback port with `GET /health` returning `200 {"status":"ok"}`. The original
> blocker was `controller/main.py:111` importing `HybridStreamingManager` from
> `gaming.scenario_app_mapping` after that class was removed by `f072c7a`; two
> stale Moonlight constructor call sites and a duplicate protocol start were
> also repaired. The Rust node itself starts and its registration/heartbeat and
> power-proxy contracts work; that was verified separately with an API-only
> harness (§5), not through the repaired daemon.

## 1. Run metadata

| Item | Value |
|---|---|
| Audit window | 2026-11-24 13:14–13:36 AEDT (02:14–02:36 UTC) |
| Repair window | 2026-11-24 13:39–13:42 AEDT (02:39–02:42 UTC) |
| Repo | `/home/matt/ozma/ozma` |
| HEAD observed at start | `38e7dfa` (`feat(gate2): close hardware, WebRTC audio, snapshot, and audit gaps`) |
| HEAD at report time / repair | `1e48641` (`docs: make intent a first-class graph request`) |
| Working tree | **moving during the audit and repair**: another process modified `ozma-node/src/*`, `ozma-capture/src/*`, `ozma-qmp/src/*`, `Cargo.lock`, and docs. The repair touched only `controller/main.py`. |
| Node tested | `target/debug/ozma-node`, 277,583,960 bytes, mtime 2026-11-24 13:29:42 AEDT |
| Python | 3.14.10, clean venv at `/tmp/ozma-controller-venv` (recreated for the repair, then removed) |
| Host | `SFFPC`, CachyOS, kernel `7.3.5-2-cachyos`, x86_64 |

Because the tree moved, the results below describe the tree and binary present at
the timestamps above, not a frozen commit. The pre-repair import failure was
re-checked at the end of the audit and was still present; it was then repaired
in §3a.

## 2. Isolated venv and dependencies

Commands actually run:

```console
$ python3 -m venv /tmp/ozma-controller-venv
$ /tmp/ozma-controller-venv/bin/python -m pip install -r controller/requirements.txt
...
Successfully installed PyJWT-2.15.1 aiohttp-3.14.5 ... fastapi-0.165.0 ...
pynacl-1.6.2 ... strawberry-graphql-0.341.3 ... toml-0.10.2 ...
$ /tmp/ozma-controller-venv/bin/python -m pip install pytest pytest-asyncio pytest-timeout
...
Successfully installed pytest-9.2.0 pytest-asyncio-1.5.0 pytest-timeout-2.4.0
```

Network was available. The full requirements file installed successfully; no
build failure occurred (`evdev` built a `cp314` wheel). Key resolved versions:

| Package | Version |
|---|---|
| fastapi | 0.165.0 |
| uvicorn | 0.54.0 |
| pydantic | 2.14.4 |
| strawberry-graphql | 0.341.3 |
| graphql-core | 3.3.2 |
| PyNaCl | 1.6.2 |
| toml | 0.10.2 |
| mido | 1.3.3 |
| evdev | 2.0.0 |
| pytest | 9.2.0 |
| pytest-asyncio | 1.5.0 |

The three previously missing controller deps (`nacl`, `strawberry`, `toml`) are
now importable in the venv. `argon2` (argon2-cffi) and `mnemonic` remain absent;
they are optional, are not in `requirements.txt`, and only produce these startup
warnings:

```text
argon2-cffi not installed — password backup method unavailable
mnemonic not installed — BIP39 export will use hex fallback
```

They are **not** the blocker.

## 3. Controller daemon startup: original regression (before repair)

The transcript below records the **pre-repair** failure. It is kept so the
regression and its later fix (§3a) can be compared.

```console
$ cd /home/matt/ozma/ozma/controller
$ PYTHONDONTWRITEBYTECODE=1 /tmp/ozma-controller-venv/bin/python main.py --help
argon2-cffi not installed — password backup method unavailable
mnemonic not installed — BIP39 export will use hex fallback
Traceback (most recent call last):
  File "/home/matt/ozma/ozma/controller/main.py", line 111, in <module>
    from gaming.scenario_app_mapping import ScenarioAppMapper, HybridStreamingManager
ImportError: cannot import name 'HybridStreamingManager' from 'gaming.scenario_app_mapping'
  (/home/matt/ozma/ozma/controller/gaming/scenario_app_mapping.py)
$ echo $?
1
```

The same failure occurs for the documented dev invocation, so no controller port
ever opened:

```console
$ /tmp/ozma-controller-venv/bin/python main.py --host 127.0.0.1 --port 37435
... ImportError: cannot import name 'HybridStreamingManager' ...
controller_exit=1
```

### Root cause

- `controller/main.py:111` imports `HybridStreamingManager`; `main.py:696`
  instantiates it.
- `controller/gaming/scenario_app_mapping.py` no longer defines that class.
- `git blame` puts the import at `8aaa04d`; commit `f072c7a` (“Task #85: V1.2
  Moonlight: Hybrid streaming…”) **removed** `HybridStreamingManager` and did
  not update `main.py`.
- A module-level import audit of every `from … import …` in `main.py` found
  **only this one missing name**. That means dependency installation is
  complete; the next failures are stale Moonlight wiring, not environment.

Two more stale call sites are visible statically after the import line (not
executed, because import fails first):

| Location | Current callee signature | Call in `main.py` |
|---|---|---|
| `main.py:687` | `ScenarioAppMapper(capture_manager=None, poster_base_url="/static/posters")` | `ScenarioAppMapper(scenarios, moonlight_protocol)` — first arg becomes `capture_manager`, second is passed to `.rstrip()` |
| `main.py:699` | `MoonlightServer(scenario_manager, moonlight_protocol, data_dir=None)` | `MoonlightServer(scenarios, moonlight_protocol, app_mapper, hybrid_streaming, moonlight_data)` — five positional args |

So fixing only the import is unlikely to make the daemon start; the Moonlight
wiring in `main.py` also lags the refactored managers. This is a controller-code
regression, **not** a Rust-compat or host-package issue.

### Consequence for live registration (before repair)

With no controller process, the real controller endpoints were unreachable:

```console
GET controller /health                                  -> 000  curl: (7) Could not connect
GET controller /api/v1/nodes                            -> 000
GET controller /api/v1/nodes/ozma-ctrl-audit...         -> 000
GET controller /api/v1/nodes/ozma-ctrl-audit.../power/state -> 000
```

### 3a. Startup repair (2026-11-24 13:39–13:42 AEDT)

Diagnosis against HEAD `1e48641`:

| Stale item | Actual current API |
|---|---|
| `from gaming.scenario_app_mapping import ScenarioAppMapper, HybridStreamingManager` | `HybridStreamingManager` does not exist anywhere in the tree. It was a small source-adapter registry removed by `f072c7a`; no replacement class was added. |
| `ScenarioAppMapper(scenarios, moonlight_protocol)` | `ScenarioAppMapper(capture_manager: DisplayCaptureManager \| None = None, poster_base_url: str = "/static/posters")`; scenarios are supplied via `refresh(list[dict])`, not the constructor. |
| `MoonlightServer(scenarios, moonlight_protocol, app_mapper, hybrid_streaming, moonlight_data)` | `MoonlightServer(scenario_manager, moonlight_protocol, data_dir: Path \| None = None)`. |
| `await moonlight_protocol.start()` followed by `MoonlightServer.start()` | `MoonlightServer.start()` already calls `self._moonlight.start()`. `RTSPServer.start()` is not idempotent, so the second bind would fail on RTSP port `47996`. |

Two separate app-mapping modules now exist: `gaming.scenario_app_mapping`
(source-aware `ScenarioAppMapper`, used for the app-list/**source resolution**
model) and `gaming.moonlight_app_mapping` (`MoonlightAppMapper`, used by
`/api/v1/moonlight/apps` in `api.py`). The current `MoonlightServer._update_apps()`
builds its list directly from `ScenarioManager.list()` and does not accept an
app mapper. The repair therefore keeps `ScenarioAppMapper` constructed and
refreshed, but does not pretend it is wired into `MoonlightServer`.

Exact edits, all in `controller/main.py`:

```diff
-from gaming.scenario_app_mapping import ScenarioAppMapper, HybridStreamingManager
+from gaming.scenario_app_mapping import ScenarioAppMapper
...
     moonlight_data = Path(__file__).parent / "moonlight_data"
     moonlight_protocol = MoonlightProtocol(moonlight_data)
-    await moonlight_protocol.start()
 
     # Scenario-to-Moonlight app mapper
-    app_mapper = ScenarioAppMapper(scenarios, moonlight_protocol)
+    app_mapper = ScenarioAppMapper(captures)
+    app_mapper.refresh(scenarios.list())
...
-    # Hybrid streaming manager (unified source adapters)
-    hybrid_streaming = HybridStreamingManager(moonlight_data / "hybrid")
+    # The V1.2 refactor removed the HybridStreamingManager source-adapter
+    # registry; ScenarioAppMapper now resolves sources directly.
+    log.warning("HybridStreamingManager removed; hybrid source-adapter registry disabled")
 
     # Full Moonlight server
-    moonlight_server = MoonlightServer(
-        scenarios, moonlight_protocol, app_mapper, hybrid_streaming, moonlight_data
-    )
+    moonlight_server = MoonlightServer(scenarios, moonlight_protocol, moonlight_data)
     await moonlight_server.start()
```

The hybrid source-adapter registry is genuinely gone, so it is disabled with an
explicit warning rather than recreated. All other flags and managers were left
alone.

#### Verification after repair

Import and CLI, from `controller/` with a clean venv:

```console
$ PYTHONDONTWRITEBYTECODE=1 /tmp/ozma-controller-venv/bin/python -c "import main"
argon2-cffi not installed — password backup method unavailable
mnemonic not installed — BIP39 export will use hex fallback
import main OK

$ PYTHONDONTWRITEBYTECODE=1 /tmp/ozma-controller-venv/bin/python main.py --help
usage: main.py [-h] [--debug] [--kbd DEVICE] [--mouse DEVICE] [--host HOST]
               [--port PORT] [--virtual-only]
```

Daemon start was run from a **temporary copy** of `controller/` so all relative
`Path(__file__).parent` data directories were created under `/tmp`, not in the
repo. The copy was byte-identical to the patched tree; the only repo edit under
test was `controller/main.py`.

```console
$ cd /tmp/ozma-controller-run/controller
$ OZMA_AUDIO=0 /tmp/ozma-controller-venv/bin/python main.py \
    --host 127.0.0.1 --port 37435
...
INFO ozma.gaming.scenario_app_mapping: App list refreshed: 0 apps ()
INFO ozma.moonlight.capture: Capture-to-Moonlight manager started
WARNING ozma: HybridStreamingManager removed; hybrid source-adapter registry disabled
INFO ozma.moonlight.protocol: RTSP server listening on port 47996
INFO ozma.moonlight.protocol: Moonlight protocol server started
INFO ozma.moonlight.server: Moonlight server started
...
INFO:     Application startup complete.
INFO:     Uvicorn running on http://127.0.0.1:37435

$ curl -s -w ' %{http_code}\n' http://127.0.0.1:37435/health
{"status":"ok"} 200
```

The process was then stopped; `37435`, `47996`, and MCP `7381` all closed. This
is a **controller health/startup check only** — it is not a Gate 2
controller/dashboard run, and it does not re-run node registration through the
real daemon.

## 4. Disposable Rust node run without a controller

The node was started with a VM profile, a recording input sink, and a minimal
**fake QMP** Unix-socket peer so that power state could be read without QEMU.
The fake QMP only answers the greeting, `qmp_capabilities`, and `query-status`;
it refuses `stop`, `cont`, `system_powerdown`, `system_reset`, and `screendump`.

```console
$ ./target/debug/ozma-node \
    --name ozma-ctrl-audit --profile vm \
    --qmp-socket /tmp/.../qmp.sock \
    --capture-provider qmp --power-provider qmp \
    --input-provider recording \
    --cap hid --machine-class server \
    --http-port 57889 --hid-udp-port 60450 \
    --register-url http://127.0.0.1:37435 \
    --register-host 127.0.0.1 --heartbeat-interval 5
```

Node surface (direct, no controller):

| Request | Result |
|---|---|
| `GET /health` | `200` `{"ok":true,"node_id":"ozma-ctrl-audit._ozma._udp.local.","version":"0.1.0",...}` |
| `GET /api/v1/connection` | `200`, `profile:"vm"`, `sink_kind:"fake-recording"`, `power:{"available":true,"status":"running"}` |
| `GET /power/state` | `200` `{"available":true,"status":"running"}` |
| `GET /display/info` | `200`, `source_id:"qmp0"`, `snapshot_available:true` (metadata only; no snapshot was taken) |
| `GET /node/status` | `200` alias of the connection summary |

Registration and heartbeat correctly failed against the absent controller:

```text
ERROR ozma_node: Direct registration failed: error sending request for url
  (http://127.0.0.1:37435/api/v1/nodes/register)
WARN  ozma_node::registration: Heartbeat request failed
  node_id="ozma-ctrl-audit._ozma._udp.local."
  error=error sending request for url
  (http://127.0.0.1:37435/api/v1/nodes/heartbeat)
```

`scripts/gate2/check-node.sh` without `OZMA_CONTROLLER_URL`:

```text
PASS  /health returned 200 and ok=true
PASS  /api/v1/connection returned 200 for node_id ... (profile 'vm')
PASS  /power/state reports available=true (status 'running')
SKIP  HLS is not configured
SKIP  WebRTC offer POST disabled
SKIP  controller checks disabled
Summary: 3 passed, 0 failed, 0 warnings, 3 skipped   (exit 0)
```

## 5. API-only registration/proxy probe (not a daemon start)

To isolate the HTTP contract from the broken `main.py` import, a small harness in
`/tmp` imported the **unmodified** `state.AppState`, `scenarios.ScenarioManager`,
and `api.build_app`, then served it with uvicorn. It did **not** import
`main.py` and did **not** build the controller’s managers. **This is not a
controller daemon start and must not be counted as one.** It only answers:
“do the registration/heartbeat/list/detail/power-proxy routes in `api.py` match
the Rust node?”

```python
# /tmp/.../api_harness.py  (excerpt)
sys.path.insert(0, "/home/matt/ozma/ozma/controller")
from state import AppState
from scenarios import ScenarioManager
from api import build_app
state = AppState()
app = build_app(state, ScenarioManager(scenario_path, state))
uvicorn.run(app, host="127.0.0.1", port=port)
```

The node was started exactly as in §4, but with
`--register-url http://127.0.0.1:44451`.

| Check | Captured result |
|---|---|
| Node → controller registration | `INFO ozma_node::registration: Registration accepted by controller node_id="ozma-ctrl-audit._ozma._udp.local." url=http://127.0.0.1:44451/api/v1/nodes/register` |
| `GET /health` | `200` `{"status":"ok"}` |
| `GET /api/v1/nodes` | `200`, list contains `id:"ozma-ctrl-audit._ozma._udp.local."`, `host:"127.0.0.1"`, `port:50631`, `capabilities:["hid"]`, `machine_class:"server"`, `api_port:34019` |
| `GET /api/v1/nodes/{id}` | `200`, same fields plus `hid_active:false`, `displays:[]` |
| Heartbeat | two `Heartbeat OK` log lines; `last_seen` rose from `247854.458521515` to `247864.480161353` (`increased=True`) |
| `GET /api/v1/nodes/{id}/power/state` | `200` `{"available":true,"status":"running"}` — controller proxied to the node using registered `host`/`api_port` |
| `GET /api/v1/streams/{id}/snapshot` | `503` `{"detail":"Snapshot unavailable"}` — the controller reached the node snapshot route, but the fake QMP refuses `screendump`; this is an honest failure, not a snapshot pass |

`scripts/gate2/check-node.sh` with the harness as controller:

```text
PASS  /health returned 200 and ok=true
PASS  /api/v1/connection returned 200 ... (profile 'vm')
PASS  /power/state reports available=true (status 'running')
PASS  controller /health reports status=ok
PASS  controller lists node 'ozma-ctrl-audit._ozma._udp.local.'
PASS  controller node detail ... returned 200
PASS  controller power proxy reports available=true
SKIP  HLS is not configured
SKIP  WebRTC offer POST disabled
SKIP  controller HLS proxy not checked
SKIP  controller input proxy not checked
Summary: 7 passed, 0 failed, 0 warnings, 4 skipped   (exit 0)
```

Interpretation: the registration payload, heartbeat, node list/detail, and
read-only power proxy contracts all work between the tested Rust node and the
unmodified `controller/api.py`. At the time of that probe the only observed
controller-side blocker was daemon construction/startup, which is now repaired
(§3a); the probe itself was not re-run through the daemon. No destructive power
or input action was performed.

## 6. Controller test suite in the venv

Post-repair re-run, same command and same result:

```console
$ cd /home/matt/ozma/ozma
$ PYTHONDONTWRITEBYTECODE=1 /tmp/ozma-controller-venv/bin/python -m pytest \
    tests/unit/controller -q --continue-on-collection-errors \
    --timeout=15 --timeout-method=signal -p no:cacheprovider
10 failed, 2347 passed, 4 skipped, 1 warning, 1 error in 67.73s
```

The `main.py` repair does not import into these tests, so this is the same
pre-existing drift documented below, not a new regression from the fix.

Targeted checks:

```console
$ ... -m pytest tests/unit/controller/gaming/test_scenario_app_mapping.py \
    tests/test_node_reconciler.py -q --timeout=30 --timeout-method=signal
56 passed in 0.08s

$ ... -m pytest tests/unit/controller/test_capture_to_moonlight.py -q \
    --timeout=30 --timeout-method=signal
2 failed, 28 passed in 0.12s
```

The two capture failures are the real `/dev/video0` host assumption, listed
below.

An earlier run without a per-test timeout hung for more than 600 s at
`test_graphql_subscriptions.py::TestNodeStateChanged::test_node_state_subscription_yields_new_node`;
it was killed. `--timeout-method=signal` was required to interrupt the async
wait.

### Failure classification

None of these failures exercises the Rust node, and **none is a Rust-compat
contract mismatch**.

| Test | Outcome | Root cause | Class |
|---|---|---|---|
| `test_capture_to_moonlight.py::TestListMoonlightApps::test_excludes_missing_device` | FAIL | test uses `/dev/video0` and assumes it is absent; this host has real `/dev/video0` and `/dev/video1` | host environment |
| `test_capture_to_moonlight.py::TestStartCaptureSession::test_start_with_missing_device` | FAIL | same real `/dev/video0`; code then compares `MagicMock.max_width < 160` | host environment |
| `test_graphql_subscriptions.py::TestEventRouter::test_event_routing` | FAIL | `NameError: name 'start_event_router' is not defined` — test fails to import it; its `finally` cleanup never runs | stale/buggy test |
| `test_graphql_subscriptions.py::TestNodeStateChanged::test_node_state_subscription_yields_new_node` | FAIL (timeout) | cascades from the previous test leaving the global `_event_router_task` set, so `start_event_router()` is a no-op in later tests | test isolation bug |
| `test_graphql_subscriptions.py::TestScenarioActivated::test_scenario_activated_subscription_yields_new_activation` | FAIL (timeout) | same stale global event-router task | test isolation bug |
| `test_graphql_subscriptions.py::TestAudioLevelUpdate::test_audio_level_subscription_yields_levels` | FAIL | fixture `audio_router` called directly; pytest 9 rejects direct fixture calls | pytest-version drift |
| `test_graphql_subscriptions.py::TestAlertFired::test_alert_fired_subscription_yields_new_alert` | FAIL (timeout) | same stale global event-router task | test isolation bug |
| `test_graphql_subscriptions.py::TestAlertFired::test_alert_fired_subscription_yields_updated_alert` | FAIL (timeout) | same stale global event-router task | test isolation bug |
| `test_graphql_subscriptions.py::TestGraphQLSchemaIntegration::test_schema_has_all_subscription_fields` | FAIL | `KeyError: 'data'` — `schema.introspect()` return shape differs in the resolved strawberry/graphql-core versions | dependency-version drift |
| `test_moonlight_pairing.py::TestMoonlightPairingServer::test_serverinfo_endpoint_unpaired` | FAIL | `OSError: [Errno 98] address already in use` on `0.0.0.0:47984`; a host Sunshine/Moonlight listener owns that port | host environment |
| `test_graphql_mutations.py` | ERROR (collection) | imports `Mutation`, `GraphQLError`, `Node`, `_require_write_scope`, etc. from `controller.gql`; that package exports only `schema` | stale test / code-test API drift |

The 4 skips are existing optional/host-dependent tests. The single warning is
Starlette’s `httpx` TestClient deprecation.

## 7. Remaining blockers, drift, and next actions

1. **Controller daemon startup is repaired** for the stale Moonlight wiring. The
   daemon now imports, starts, and answers `/health` on loopback (§3a).
2. **Gate 2 is still not run or claimed.** The repair verified startup and
   `/health`, not dashboard rendering, WebRTC/HLS streams, snapshots, input, or
   registration through the real daemon.
3. **Repeat the node contract through the real daemon.** The
   registration/heartbeat/list/detail/power-proxy contract was exercised through
   the API harness (§5). Start `main.py` on a free port, point a disposable
   `ozma-node` at it, and repeat those checks. Add an HLS/snapshot run with a
   real QMP source if a snapshot is required.
4. **Moonlight has duplicated app-mapping paths.** `app_mapper`
   (`scenario_app_mapping.ScenarioAppMapper`) is now constructed and refreshed,
   but current `MoonlightServer` builds its own list from `ScenarioManager` and
   `/api/v1/moonlight/apps` uses `gaming.moonlight_app_mapping.MoonlightAppMapper`.
   Unifying these is an owner/design decision and was deliberately not done here.
5. **Hybrid source adapters are gone.** `HybridStreamingManager` was removed in
   `f072c7a`; startup now logs a warning and disables that registry. Do not
   reintroduce it without a replacement design.
6. **Environment/version test drift remains unchanged** (`10 failed, 2347
   passed, 4 skipped, 1 error`) — **superseded by §9**, which records the
   2026-11-24 cleanup (`0 failed, 0 errors`):
   - GraphQL/strawberry drift: `start_event_router` import bug and cascading
     global event-router leakage; `schema.introspect()` shape; direct fixture
     call under pytest 9; `test_graphql_mutations.py` imports names not exported
     by `controller.gql`.
   - Host port: this host's Sunshine/Moonlight listener owns `0.0.0.0:47984`, so
     `test_moonlight_pairing.py::test_serverinfo_endpoint_unpaired` fails. The
     controller's own RTSP listener is `47996`, which was free and bound
     successfully during the repair.
   - Host devices: real `/dev/video0`/`/dev/video1` invalidate two
     `test_capture_to_moonlight.py` "missing device" assumptions.
   - Optional packages: `argon2-cffi` and `mnemonic` remain absent from
     `requirements.txt` and only emit startup warnings.
7. **Runtime data is still written beside the controller source.** `main.py`
   uses `Path(__file__).parent / ...` for many data directories. The repair ran
   from a temporary copy to avoid polluting the repo; production/deployment
   should provide a writable data directory instead. This was out of scope.

## 8. Cleanup confirmation

- All audit and repair processes (`ozma-node`, fake QMP, API harness, controller
  attempts, repaired controller) were stopped; `ps` showed no remaining
  `main.py`, `ozma-node`, `api_harness.py`, or `fake_qmp.py`.
- All temporary ports used by the runs (`60450`, `57889`, `37435`, `50631`,
  `34019`, `44451`, `47996`, `7381`) are closed; `ss` showed none listening.
- `/tmp/ozma-controller-venv`, `/tmp/ozma-controller-audit`, and the repair's
  `/tmp/ozma-controller-run` copy (with its logs/data) were removed.
- The API harness previously wrote `controller/mesh_network.json` into the repo;
  it was deleted. The repair ran from `/tmp`, so it wrote no controller data
  directories into the repo. The `__pycache__/main.cpython-314.pyc` created by
  `py_compile` was deleted; `.pytest_cache` was suppressed with
  `-p no:cacheprovider`.
- No global package was installed or removed. Pip's download cache may retain
  ordinary wheels from these temporary installs; no repository file is affected.
- The only source change from the repair is `controller/main.py`. The repo still
  shows unrelated concurrent modifications from another process
  (`ozma-node/src/*`, `ozma-capture/src/*`, `ozma-qmp/src/*`, docs,
  `Cargo.lock`, plus untracked reports). Those are not this repair's changes and
  were left untouched.

## 9. Controller test-suite cleanup (2026-11-24)

Follow-up drift cleanup for Phase 3 / clean-host readiness. This section
supersedes §6 and §7 item 6. Only controller tests, their fixtures, and docs
changed; no Rust source and no product behaviour was modified.

### 9.1 Environment

| Item | Value |
|---|---|
| Audit window | 2026-11-24 20:45–21:10 AEDT |
| Repo | `/home/matt/ozma/ozma`, HEAD `4bd7539` (dirty tree: unrelated Rust/docs edits from another process) |
| Python | 3.14.10, disposable `/tmp/ozma-ctrl-venv` (removed after the run) |
| Dependencies | `controller/requirements.txt` + `pytest`, `pytest-asyncio`, `pytest-timeout` |
| Container check | rootless `podman`, official `python:3.14` image, repo mounted read-only, venv inside the container |

### 9.2 Before/after counts

Command (identical in the host venv and the container):

```console
$ python -m pytest tests/unit/controller -q --continue-on-collection-errors \
    --timeout=20 --timeout-method=signal -p no:cacheprovider
```

| Run | Result |
|---|---|
| Before (host `/tmp` venv) | **9 failed, 2463 passed, 6 skipped, 1 error** (`test_graphql_mutations.py` collection) in 138.06 s |
| After (host `/tmp` venv) | **2481 passed, 6 skipped, 1 xfailed, 0 failed, 0 errors** in 60.48 s |
| After (clean `python:3.14` container) | **2481 passed, 6 skipped, 1 xfailed, 0 failed, 0 errors** in 59.00 s |
| Focused: `tests/test_node_reconciler.py` | **10 passed** (before and after) |

The pass-count increase is the rewritten mutation module (10 current tests that
previously did not collect: 9 pass and 1 strict xfail) plus the seven
subscription tests and two capture tests that were failing. The single
`xfailed` test is an explicit, strict marker for the product drift in §9.4.

### 9.3 Drift fixed (tests only)

| Test / file | Root cause | Fix |
|---|---|---|
| `test_graphql_mutations.py` (collection error) | Imported `Mutation`, `GraphQLError`, `Node`, `ScenarioInput`, `_require_write_scope`, … from `controller.gql`, but commit d939c61 moved the module to `controller/gql_router.py`; Task #51 had already removed those classes. `controller.gql` exports only `schema`. | Retargeted to `controller.gql_router` and the API that actually exists: schema contains `activateNode`, `NodeInfo` fields, `RequireWritePermission` (write/read/unauthenticated), `Mutation.activate_node` success, and `Query.active_node`/`nodes`. 27 obsolete tests for removed mutations were replaced by 10 current tests (9 pass + 1 strict xfail for the product error-path drift below). |
| `test_graphql_subscriptions.py::test_event_routing` | `start_event_router` was not imported. The `NameError` fired before the `finally`, leaving the module-global router task and registry behind and hanging later tests. | Import `start_event_router`/`stop_event_router`; added an autouse fixture that cancels/clears the global router task and subscription registry before and after every test. |
| `test_graphql_subscriptions.py` new-node/new-scenario/new-alert/updated-alert | Stale expectations assumed an initial snapshot was always yielded. Empty mock state produced 0 initial values, so the generators waited forever for 2–3 results. | Seed the initial state where the original intent was "initial + event"; otherwise wait for the one event actually produced. The scenario test now also asserts both the snapshot and the activation. |
| `test_graphql_subscriptions.py::test_audio_level...` | Called the `audio_router` fixture directly (rejected by pytest 9). | Injected the fixture as a parameter. |
| `test_graphql_subscriptions.py::test_schema_has_all_subscription_fields` | `schema.introspect()` returns `{"__schema": ...}` in the installed graphql-core/strawberry versions, not `{"data": {...}}`. | Accept both shapes; use `controller.gql.schema` instead of the shadow `gql.schema` path. |
| `test_capture_to_moonlight.py` missing-device tests (2) | Assumed `/dev/video0` did not exist; this host has real capture devices. | Use a guaranteed-missing path under pytest's `tmp_path`. The product's `Path(card.path).exists()` check is still exercised. |
| `test_moonlight_pairing.py::test_serverinfo_endpoint_unpaired` | Already fixed in the working tree: the handler is called directly and no longer binds the fixed `0.0.0.0:47984` port. The `47984` assertion is response data only. | Verified; no test binds 47984/47996. |

No test was skipped to make it pass. No coverage of existing product behaviour
was removed; the only deleted tests covered mutation classes that no longer
exist.

### 9.4 Remaining drift (not test-only)

- **Product GraphQL error paths:** `controller/gql_router.py` and
  `controller/graphql_api.py` still call `strawberry.ExceptionField`, which the
  installed Strawberry version does not export. Missing-node activation
  therefore raises `AttributeError` instead of a typed `NOT_FOUND`. This is
  marked by the strict xfail in `test_graphql_mutations.py`; fixing it is a
  product decision, outside this test-only cleanup.
- **Duplicate/unmounted GraphQL modules:** `main.py` mounts `graphql_api.py`
  through `api.py`, while `gql_router.py` is the module exercised by
  `test_graphql.py` and now `test_graphql_mutations.py`. `graphql_api.py`'s
  `activate_node` uses `app_state.activate_node` (the real `AppState` exposes
  `set_active_node`) and runs the coroutine with
  `run_coroutine_threadsafe(...).result()` from the calling loop. These are
  product drift, not test drift.
- Optional packages `argon2-cffi` and `mnemonic` are still absent from
  `requirements.txt`; `main.py --help` starts with a "mnemonic not installed"
  warning only. Not a blocker.
- Host-dependent tests that remain skipped are unchanged (6 skips, all with
  their existing reasons).

### 9.5 Clean-host install

The controller installs and tests cleanly without sudo:

```console
$ python3 -m venv /tmp/ozma-ctrl-venv
$ /tmp/ozma-ctrl-venv/bin/pip install -r controller/requirements.txt \
    pytest pytest-asyncio pytest-timeout
$ cd controller && /tmp/ozma-ctrl-venv/bin/python main.py --help   # exit 0
$ cd .. && /tmp/ozma-ctrl-venv/bin/python -m pytest tests/unit/controller -q \
    --continue-on-collection-errors --timeout=20 --timeout-method=signal
2481 passed, 6 skipped, 1 xfailed
```

The same commands were run in a read-only rootless container
(`python:3.14`, repo mounted at `/src:ro`, venv at `/tmp/v`), with identical
results. No repository data directory was needed to collect/run the suite.
`controller/mesh_network.json` is written into the repo by the suite and was
removed after the run; a clean host should still treat that as a test artifact.

### 9.6 Cleanup

`/tmp/ozma-ctrl-venv`, the container (removed with `--rm`), and the test-written
`controller/mesh_network.json` were removed. No global package was installed, no
sudo was used, and no commit was made.

