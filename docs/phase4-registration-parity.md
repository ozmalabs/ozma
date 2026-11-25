# Phase 4 registration, heartbeat, and mDNS parity

Status: P0 blocker 5 (registration parity) closed for the fields on which the
Rust node has a real backend. Fields the Rust node does not implement are
omitted and reported as typed "not available" entries in
`GET /api/v1/connection.registration`; no fake endpoint or capability is
advertised.

Scope: `ozma-node` registration/mDNS, one discovery consumer change
(`controller/discovery.py`), and the focused tests listed at the end. This
document does not change `docs/parity-matrix.md` or
`docs/plan-execution-status.md`.

## 1. Sources of truth

| Provider | mDNS TXT | Direct registration |
|---|---|---|
| Python VM soft node | `softnode/soft_node.py:846-865` (`_announce`) | `softnode/soft_node.py:904-933` (`_direct_register`) |
| Python desktop soft node | `softnode/desktop_node.py:448-462` | `softnode/desktop_node.py:534-548` |
| Python desktop agent | — | `agent/ozma_desktop_agent.py:1313-1327` |
| Python multiseat seat | — | `agent/multiseat/seat.py:432-448` |
| Python hardware node | `node/node.py:484-511` | `node/node.py:585-590` |
| Rust node | `ozma-node/src/main.rs::build_txt` → `mdns::advertise_on` | same TXT map → `registration::registration_payload` |

The Rust node deliberately has **one** field map. What mDNS advertises and what
direct registration POSTs cannot drift from each other (except the resolved
`host`, which is always the node's registration address, never a TXT override).

## 2. Field-by-field parity

Legend: **Emitted** = present only when backed by real configuration;
**Alias** = compatibility key emitted alongside the canonical key;
**Omitted (typed)** = absent, and named in
`/api/v1/connection.registration.unsupported_controller_fields`.

| Python key | Rust key / status | Controller consumer | Notes |
|---|---|---|---|
| `id` (derived) | `id` — Emitted | `controller/api.py:213,1033-1080`; `controller/state.py:26-97` | Node id remains `<name>._ozma._udp.local.`. |
| `host` | `host` — Emitted (resolved address) | `controller/api.py:214`; `controller/discovery.py:110` | TXT `host` is ignored by registration so `--register-host` cannot override the resolved address. |
| `port` | `port` — Emitted (`--hid-udp-port`) | `controller/api.py:215`; `controller/discovery.py:111` | 7331 default. |
| `proto` | `proto` — Emitted (`"1"`) | `controller/discovery.py:114-120` | Non-1 nodes are ignored by discovery. |
| `role` | `role` — Emitted | `controller/api.py:217`; `controller/discovery.py:164` | `compute` default. |
| `hw` | `hw` — Emitted | `controller/api.py:218`; `controller/discovery.py:165` | Rust hardware/desktop/vm string. |
| `fw` | `fw` — Emitted | `controller/api.py:219`; `controller/discovery.py:166` | |
| `cap` | `cap` — Emitted | `controller/api.py:220,1041`; `controller/discovery.py:122-123` | Real capability list only. |
| `api_port` | `api_port` — Emitted | `controller/api.py:225`; `controller/discovery.py:133-134` | |
| `quic_port` | `quic_port` — Emitted | No current controller reader | Extra field ignored by Pydantic; harmless. |
| `stream_port`, `stream_path` | Both — Emitted when `video` capability | `controller/api.py:221-222`; `controller/discovery.py:129-131`; `controller/state.py:100-104` | Rust HLS manifest path. |
| `capture_device` | `capture_device` — Emitted, may be `""` | `controller/api.py:233`; `controller/discovery.py:142` | Mirrors current Python behavior; an empty device is not a claimed source. |
| `machine_class` | `machine_class` — Emitted | `controller/api.py:234,1066`; `controller/discovery.py:149` | Validated to workstation/server/kiosk/camera. |
| `display_outputs` | `display_outputs` (JSON string) — Emitted | `controller/api.py:235,1067`; `controller/discovery.py` does not read it | Direct registration only; matches Rust display topology. |
| `pci_devices` | `pci_devices` (JSON string) — Emitted | `controller/api.py:241,1073` | Empty JSON array when none. |
| `vnc_host`, `vnc_port` | **Emitted only when both are configured** (`--vnc-host`/`--vnc-port`) | `controller/api.py:223-224,1042,1055-1056`; `controller/discovery.py:125-127`; VNC readers at `controller/api.py:3107-3159`, `controller/stream.py:535-584`, `controller/agent_engine.py:382-408`, `controller/testbench.py:364-366` | The endpoint is the external QEMU VNC server used for capture, never a VNC server started by the Rust node. A missing/partial pair is omitted. |
| `seat_id` | **Emitted from `OZMA_SEAT_ID` when non-empty** | `controller/api.py:244,1074`; `controller/state.py:87`; `controller/seats.py` grouping; `controller/discovery.py` (updated) | Multiseat's planner sets `OZMA_SEAT_ID` (`ozma-multiseat/src/planner.rs:658`). Never invented: an unset/empty value is omitted and name-convention grouping still works. |
| `vm_ip` (mDNS) | **Alias — emitted with `vm_guest_ip`** when a guest IP is known | `controller/discovery.py:147` (now `vm_ip` → `vm_guest_ip` fallback) | Keeps mDNS-discovered Rust VM nodes from losing the guest IP. |
| `vm_guest_ip` (direct) | **Canonical — emitted with `vm_ip`** when known | `controller/api.py:240,1072`; `controller/state.py:94,160-164` | Both keys carry the same value; direct and mDNS consumers each find theirs. |
| `audio_type`, `audio_sink` | Emitted when the `audio` capability and sink are configured | `controller/api.py:229-230,1061-1062`; `controller/discovery.py:136-137` | PipeWire only, matching the implemented backend. |
| `audio_vban_port`, `mic_vban_port` | **Omitted (typed)** | `controller/api.py:231-232,1063-1064`; `controller/discovery.py:138-141`; `controller/api.py:2917-2961`; `controller/routing/builder.py`; `controller/gql/types/vban.py` | No VBAN backend in `ozma-node`; VBAN features remain unavailable. |
| `camera_streams` | **Omitted (typed)** | `controller/api.py:237,1068`; `controller/discovery.py:153-158`; camera recording/GraphQL readers | No camera-node negotiation in `ozma-node`. |
| `frigate_host`, `frigate_port` | **Omitted (typed)** | `controller/api.py:238-239,1069-1070`; `controller/discovery.py:150-152` | No Frigate integration in `ozma-node`. |
| `sunshine_port` | **Omitted (typed)** | `controller/discovery.py:144-145`; `controller/state.py:88-90` | No Sunshine manager in `ozma-node`. |
| `api_token` | **Never emitted** | `controller/api.py:226-228,1060`; token store takes precedence | A registration token is optional in the controller. The Rust node does not put tokens in TXT, registration JSON, or logs; the controller hydrates its token store instead. |
| `seat_index`, `display_name` | **Not emitted** (Python multiseat agent legacy extras) | Unknown to `DirectRegisterRequest`; ignored by Pydantic | Replaced by the explicit `seat_id` when `OZMA_SEAT_ID` is available. |

## 3. Heartbeat parity

| Behavior | Python | Rust |
|---|---|---|
| Keep-alive | `softnode/soft_node.py:961-979` re-POSTs the full registration body every 30 s; desktop node every 60 s | `registration::heartbeat_loop` POSTs `{"node_id": ...}` every `heartbeat_interval_secs` |
| Controller handling | `controller/api.py:1082-1093` only updates `last_seen` | Same endpoint |
| Field refresh | Full body refresh doubles as controller-restart recovery | Fields are static after startup |

Remaining gap: if the controller restarts while a Rust node keeps running, the
heartbeat returns `404 Node not found` and the node does not re-register. The
Python providers recover automatically because their heartbeat carries the full
body. This is a lifecycle gap, not a field-parity gap; a future change can make
the Rust heartbeat re-POST the registration payload on 404.

## 4. Secret handling

* `registration::is_secret_field` recognizes credential-shaped keys
  (`token`, `secret`, `password`, `passwd`, `credential`, `api_key`, `apikey`,
  `private_key`).
* `registration_payload` drops those keys even if a caller puts them in the TXT
  map.
* `mdns::advertise_on` rejects the whole advertisement before formatting or
  logging if a secret-shaped key is present; the error names the key, never its
  value. This keeps the existing debug TXT dump from leaking a future secret.
* The Rust node has no flag that places an API token, password, or TURN
  credential in the registration map. WebRTC TURN credentials stay in the
  WebRTC helper configuration and are not part of registration.

## 5. Controller change

`controller/discovery.py` now:

1. parses `seat_id` (trimmed, optional) into `NodeInfo.seat_id`, and
2. accepts `vm_ip` with `vm_guest_ip` as a fallback, so Rust and Python nodes
   both populate `NodeInfo.vm_guest_ip`.

Both changes are backward compatible: old nodes omit `seat_id` and still group
by the `<machine>-seat-<n>` name convention; Python's `vm_ip` remains the first
choice.

No controller change was needed for VNC or the omitted fields. A controller
that receives a Rust node without `vnc_host`/`vnc_port` keeps its existing
"VNC unavailable" path instead of connecting to an empty endpoint.

## 6. Tests and evidence

Rust (`cargo test -p ozma-node --lib`):

* `registration::tests::desktop_payload_is_exact_and_omits_unavailable_endpoints`
* `registration::tests::vm_payload_carries_vm_ip_alias_and_configured_vnc_endpoint`
* `registration::tests::multiseat_payload_carries_the_explicit_seat_id`
* `registration::tests::empty_or_partial_values_are_omitted_not_sent_as_empty_strings`
* `registration::tests::registration_status_is_typed_for_controller_consumers`
* `registration::tests::secrets_never_reach_mdns_txt_or_registration_payloads`
* `mdns::tests::credential_shaped_txt_keys_are_rejected_before_advertising`
* Main binary: `txt_carries_registration_parity_fields`,
  `txt_vm_case_emits_vnc_endpoint_and_both_guest_ip_keys`,
  `txt_multiseat_case_emits_only_the_explicit_seat_id`

Controller (`pytest tests/unit/controller/test_discovery_fields.py
tests/unit/controller/test_seats.py -q`):

* Rust TXT with `seat_id` + both guest-IP keys registers the explicit seat.
* Python TXT with only `vm_ip` still populates `vm_guest_ip` and keeps an empty
  `seat_id`.
* Wrong `proto` is still ignored.
* Existing optional-`seat_id` direct-registration compatibility test remains
  green.

Commands run:

```bash
cargo test -p ozma-node --lib
cargo test -p ozma-node --bin ozma-node txt_
cargo check -p ozma-node --all-targets
rustfmt --edition 2021 --check ozma-node/src/{registration,mdns,main,controller_compat}.rs
python -m pytest tests/unit/controller/test_discovery_fields.py tests/unit/controller/test_seats.py -q
```

## 7. Remaining gaps

| Gap | Impact | Decision |
|---|---|---|
| No Rust VNC server | `vnc_host`/`vnc_port` are only registered when an external QEMU VNC endpoint is configured. Nodes without one cannot serve controller VNC screen-read/OCR/testbench fallbacks. | Accepted: snapshots/HLS remain the Rust path; registering a fake server is forbidden. |
| No VBAN | `audio_vban_port`/`mic_vban_port` omitted. Controller VBAN config/routing cannot target Rust nodes. | Capability gap; typed in `registration.unsupported_controller_fields`. |
| No camera/Frigate | `camera_streams`/`frigate_*` omitted. Camera-node discovery/recording cannot target Rust nodes. | Capability gap; typed. |
| No Sunshine | `sunshine_port` omitted. Controller game-stream discovery cannot target Rust nodes. | Capability gap; typed. |
| No full-body heartbeat refresh | Controller restart leaves a running Rust node unregistered until it restarts. | Lifecycle follow-up; not a field-parity regression. |
| Python multiseat `seat_index`/`display_name` | Rust does not emit the Python-only extras. | Intentionally replaced by `seat_id`; controller ignored the extras. |
