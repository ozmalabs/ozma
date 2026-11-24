# Gate 2 live WebRTC browser/NAT matrix report

**Scope:** the browser half of the Gate 2 WebRTC matrix — the controller
dashboard's real `RTCPeerConnection` path against the disposable
QEMU → `ozma-node` → GStreamer/Python helper stack — plus an audit of local
STUN/TURN tooling and trickle-ICE support.

**Result: PASS for Firefox and Chromium mobile emulation on the dashboard, with
the remaining matrix gaps named precisely.** Playwright Firefox 159.0 (bundled,
with the existing OpenH264 2.6.0 plugin) connected to the helper through the
controller, decoded **65 H.264 frames at 720x400 / 15 fps**, and received
**225 Opus packets** with zero page, console, or request errors. Playwright
Chromium 159.0.8154.6 under an iPhone 13 emulation connected and decoded
**63 H.264 frames at 720x400 / 15 fps** plus **217 Opus packets**. A real
GStreamer peer remained the sanity check (**123 frames / 420 Opus buffers**).

**Not validated:** Apple Safari/WebKit (WebKit 27.2 was not installed and Safari
cannot run on this Linux host), TURN relay, a distinct server-reflexive
candidate, and trickle ICE. The helper and node have **no STUN/TURN
configuration at all**, the dashboard hard-codes `iceServers: []`, and there is
no signaling message for incremental ICE candidates. Those are reported as gaps,
not passes. See [Remaining gaps](#remaining-gaps).

> **Follow-up (2026-11-24, later):** STUN/TURN configuration, browser
> `iceServers` configuration, and one-way (browser→helper) trickle candidate
> signaling were added after this matrix run. The matrix above remains an
> honest record of the **no-ICE-configuration** run. See
> [§11](#11-follow-up-stunturn-configuration-and-trickle-candidates) for the
> new configuration surface, live candidate evidence, and the narrower
> remaining gaps.

> No `sudo`, no global package install, no commit, and no external network
> service was used at runtime. Controller dependencies were installed only into
> a disposable `/tmp` venv. Every QEMU, node, helper, controller, browser, STUN
> server, null sink, port, venv, and temporary file was removed; see
> [Cleanup](#cleanup).

## 1. Run metadata

| Item | Value |
|---|---|
| Date | 2026-11-24, ~18:30–18:45 AEDT (07:30–07:45Z) |
| Repo | `/home/matt/ozma/ozma` |
| HEAD | `f12add9` |
| Working tree | dirty with unrelated concurrent edits; nothing committed |
| QEMU | 11.1.3, TCG, 720x400 VGA, no guest/disk/network |
| Encoder | x264 helper fallback (`x264enc`, no hardware encoder) |
| Node | `target/debug/ozma-node` (`ozma-node 0.1.0`) |
| Controller | temporary copy under `/tmp`, isolated venv (`pynacl`, `strawberry-graphql`, `toml`) |
| Ports | controller 43127, node HTTP 53149, node HID/UDP 55140, node QUIC 53151, STUN 3478 |
| Temp root | `/tmp/opencode/gate2-matrix` (removed) |
| Audio | disposable null sink `ozma-gate2-matrix-r2.monitor`, 440 Hz tone |

## 2. Client and tooling inventory

Checked without installing a global package:

| Client/tool | Availability | Use |
|---|---|---|
| Playwright Python | **1.65.2 in the `/tmp` venv**; system Python had 1.58.0 | browser automation |
| Playwright Firefox | **159.0** (`firefox-1572`) | **used** |
| Playwright Chromium | **159.0.8154.6** (`chromium-1255`) | **used** for mobile emulation |
| Playwright WebKit | **not installed** (`webkit-2388` absent) | not used |
| System Firefox | **160.0.1** installed | inventory only; Playwright's Juggler build is the automated one |
| System Chrome | **157.0.8103.50** installed | inventory only |
| OpenH264 GMP plugin | **2.6.0** already in the user Firefox profile | copied into the temporary Firefox profile |
| `coturn` / `turnserver` / `eturnal` / `restund` | **absent** | no TURN |
| `stund` (libnice 0.1.24-1.1) | **present** | local STUN responder test |
| `stunbdc` (libnice) | **present** | STUN Binding Discovery client |
| `socat` | present | not needed |
| QEMU / GStreamer | 11.1.3 / 1.28.9, `webrtcbin` + `x264enc` | live source and helper |

Safari is **not available**: Apple Safari is macOS/iOS-only, and the Playwright
WebKit browser revision was not installed. No Safari result is claimed.

## 3. Disposable stack

The stack mirrored the earlier controller report with new ports and paths:

```bash
BASE=/tmp/opencode/gate2-matrix
qemu-system-x86_64 -name ozma-gate2-matrix -machine pc,accel=tcg -m 256 \
  -nodefaults -device VGA -device qemu-xhci,id=xhci \
  -device usb-kbd,bus=xhci.0 -display none \
  -qmp "unix:$BASE/qemu/qmp.sock,server=on,wait=off" \
  -pidfile "$BASE/qemu/qemu.pid" -daemonize

pactl load-module module-null-sink sink_name=ozma-gate2-matrix-r2
gst-launch-1.0 -q audiotestsrc is-live=true wave=sine freq=440 \
  ! audioconvert ! audioresample \
  ! audio/x-raw,format=F32LE,rate=48000,channels=2 \
  ! pulsesink device=ozma-gate2-matrix-r2

./target/debug/ozma-node --name ozma-gate2-matrix --profile vm \
  --qmp-socket "$BASE/qemu/qmp.sock" \
  --input-provider qmp --power-provider qmp \
  --http-port 53149 --hid-udp-port 55140 \
  --cap hid,video,audio,power --machine-class server \
  --register-url http://127.0.0.1:43127 --register-host 127.0.0.1 \
  --heartbeat-interval 5 --hls --hls-dir "$BASE/node/hls" \
  --hls-fps 5 --hls-bitrate-kbps 1000 --ffmpeg-bin /usr/bin/ffmpeg \
  --webrtc-helper /usr/bin/python3 \
  --webrtc-helper-script "$PWD/softnode/gst_webrtc_worker.py" \
  --webrtc-width 720 --webrtc-height 400 --webrtc-fps 15 \
  --webrtc-audio-source ozma-gate2-matrix-r2.monitor \
  --webrtc-audio-backend pulse
```

QMP `qmp_capabilities` and `query-status` (`running`) succeeded. The node
reported HLS `running` and served WebRTC frames, which exercises its QMP
`screendump` capture source; the controller served `dashboard.html` on
`127.0.0.1:43127`, and `/api/v1/nodes` listed
`ozma-gate2-matrix._ozma._udp.local.` with `api_port: 53149`.

The controller's fixed Moonlight RTSP port 47996 briefly failed startup while a
previous socket was in `TIME_WAIT`; after it cleared, the controller started and
stayed up. This is a startup-environment detail, not a WebRTC result.

## 4. Browser matrix

Each browser loaded the real `controller/static/dashboard.html`, set
`localStorage['ozma-ui-mode']='full'`, activated the node, and opened the KVM
modal through the page's own `openStream(nodeId)` path. The page creates
`RTCPeerConnection({iceServers: []})`, prefers H.264 via
`setCodecPreferences`, waits up to 2 s for gathering, POSTs the full SDP to
`/api/v1/streams/<node>/webrtc`, and applies the answer.

| Browser | Engine/version | Offer/answer | ICE/DTLS | Video | Audio | Result |
|---|---|---|---|---|---|---|
| Firefox | Firefox 159.0 (Playwright `firefox-1572`) + OpenH264 2.6.0 | offer 2 m-lines; H.264 PT 126, Opus PT 109; answer `sendonly` PT 126/109 | `connected` / ICE `connected`, gathering `complete`, signaling `stable` | **65 decoded frames**, 15 fps, 720x400, 513 packets, 77,625 bytes, 0 lost | **225 Opus packets**, jitter 0.002, 0 lost | **PASS**; no console/page/request errors |
| Chromium mobile emulation | Chrome for Testing 159.0.8154.6, iPhone 13 descriptor | offer includes H.264 PT 102, Opus PT 111; answer `sendonly` PT 102/111 | `connected` / ICE `connected`, gathering `complete`, signaling `stable` | **63 decoded frames**, 15 fps, 720x400, 499 packets, 77,476 bytes, 0 lost | **217 Opus packets**, jitter 0.002, 0 lost | **PASS for media**; one unrelated dashboard JS error (below) |
| Chromium desktop (prior run) | Chrome for Testing 145.0.7632.6 | H.264 103 / Opus 111 | connected / ICE connected | 86 frames, 15 fps, 720x400 | 295 packets | reference from `gate2-live-controller-webrtc-report.md` |
| Safari / WebKit | — | — | — | — | — | **unavailable; not tested** |

The Firefox dashboard status line read `WebRTC 61f 14fps 75KB`; the mobile run
read `WebRTC 61f 15fps 75KB`. In both cases `videoWidth=720`,
`videoHeight=400`, `readyState=4`, `paused=false`, and `srcObject` was present.
The video element screenshots were non-uniform (Firefox 728x405, Chromium mobile
1110x618), not a black placeholder.

### 4.1 Firefox H.264 prerequisite

The first Firefox run **failed to receive video**, and that failure is recorded:

- The bundled Playwright Firefox offered only VP8, VP9, and AV1 in
  `RTCRtpReceiver.getCapabilities('video')`; there was no H.264.
- The helper correctly answered the video m-line `a=inactive` because it has no
  VP8/VP9 branch. ICE/DTLS and Opus still connected, but the video element stayed
  `0x0` and `getStats()` had no video `inbound-rtp`.

This was not a payload-type bug: the helper only emits H.264, and Firefox did not
offer it. Copying the pre-existing user-profile GMP plugin
(`~/.mozilla/firefox/<profile>/gmp-gmpopenh264/2.6.0`) into a temporary
Playwright profile and enabling `media.gmp-gmpopenh264.enabled` made Firefox
offer H.264 PT 126. The same helper then answered `sendonly` with PT 126 and
video decoded. The test did not download or install a plugin; it reused the
existing one.

**Codec gap:** a Firefox build without OpenH264 (GMP disabled, plugin missing)
cannot play this helper's H.264-only stream. `vp8enc` is present on the host, but
there is no VP8 fallback in the worker or `webrtc.rs`, so this is not silently
handled. Chrome advertises H.264; Safari was not tested.

### 4.2 Chromium mobile emulation

`playwright.devices["iPhone 13"]` produced a 604x1029 CSS viewport at DPR 3,
touch enabled, and an iPhone Safari user agent over Blink. It is **Chromium
mobile emulation, not iOS Safari and not a real phone**. Media connected and
decoded as in the table.

One page error was captured:

```text
TypeError: Cannot read properties of null (reading 'style')
    at initRoomCorrection (dashboard.html:3208:46)
```

The iPhone user agent makes `RC_IS_IOS` true, and `initRoomCorrection()` then
dereferences `#rc-ios-warning`, which is absent from the mobile/full-mode DOM.
It does not affect ICE, DTLS, or media, and it is not a helper/`webrtc.rs` bug;
it was reported rather than fixed to keep this run's changes scoped to WebRTC.

### 4.3 Evidence limits

Screenshots and the full `getStats()` reports were captured in the temporary
evidence directory. `about:webrtc` timed out under headless Playwright Firefox,
and `chrome://webrtc-internals/` was refused by Playwright Chromium
(`net::ERR_FAILED`). The retained evidence is therefore the dashboard
screenshot, a cropped video-element screenshot, and a JSON dump of
`RTCPeerConnection.getStats()` (inbound RTP, candidate pair, selected pair) —
not a webrtc-internals page.

## 5. STUN

### 5.1 Local STUN responder works

`stund` from libnice was started on UDP 3478 (no external STUN/TURN service was
used). A direct Binding Discovery request to loopback succeeded:

```text
$ stunbdc -4 127.0.0.1 3478
Server address: 127.0.0.1 port 3478
Mapped address: 127.0.0.1 port 51140
```

### 5.2 The helper/node do not use STUN

`ozma-node/src/webrtc.rs::helper_argv()` passes only `--sock`, `--shm`,
`--width`, `--height`, `--fps`, and optional audio flags. `WebRtcConfig` has no
STUN/TURN fields. `softnode/gst_webrtc_worker.py` never sets `stun-server` or
`turn-server` on `webrtcbin`. `controller/static/dashboard.html` and
`console.html` hard-code `iceServers: []`.

A standalone `webrtcbin` probe with `stun-server=stun://192.168.40.19:3478`
gathered to `COMPLETE` with **9 candidates, all `typ host`; no `srflx`**. On a
same-host path the probe cannot distinguish a missing STUN request from a
reflexive mapping that libnice discarded as redundant; without a NAT namespace
there is no distinct mapped address to observe. Either way, the production
helper never sets `stun-server`, so the result is expected. This probe does not
prove a real NAT would work; it shows the product has no STUN configuration.

**Exact gap:** there is no node CLI/env field, no helper argument, and no
`webrtcbin` property wiring for STUN. A node behind NAT has only host candidates
(and Chrome's mDNS `.local` host candidates). The live stack was same-host, so
this did not fail the browser tests.

## 6. TURN

No `coturn`, `turnserver`, `eturnal`, or `restund` binary is installed, and no
TURN server was started. There is also no node/helper/dashboard TURN
configuration: `helper_argv()` has no TURN flag, the worker has no
`turn-server`/`add-turn-server` call, and the dashboard uses `iceServers: []`.
The relay path is therefore **not implemented and not validated**. No relay
candidate is claimed.

## 7. Trickle ICE

**The signaling protocol is candidate-embedded only.** Evidence:

- `webrtc.rs` accepts `{"type":"offer","sdp":...}` and
  `{"type":"bitrate","bitrate":...}`; the answer is one JSON object. There is no
  `add-ice-candidate`, `candidate`, or `end-of-candidates` message.
- `gst_webrtc_worker.py` parses `a=candidate` lines out of the offer
  (`remote_ice_candidates`), waits up to 5 s for gathering, and appends every
  local candidate to the answer SDP (`append_ice_candidates`). It never emits
  candidates incrementally, and `webrtc.rs` never forwards any.
- The dashboard waits for `iceGatheringState == complete` (or 2 s) and POSTs
  the local description once.

**Empirical probe:** a Chromium offer was rewritten to remove its two
`a=candidate` lines and `a=end-of-candidates`, so the helper received **0 remote
candidates**. The answer still carried 9 candidates. The peer nevertheless
reached `connected` and decoded 47 video frames / 167 audio packets on the same
host, because ICE learned a peer-reflexive candidate from the browser's checks.
That is a same-host coincidence, not trickle support.

**Exact gap:** a peer that gathers candidates after the answer, or a NAT peer
whose only usable candidate is late, has no channel to deliver it. A
candidate-less same-host peer can still connect through peer-reflexive checks,
so this run cannot claim a hard LAN failure; it also cannot claim NAT/trickle.

## 8. Sanity check: real GStreamer peer

The checked-in `scripts/gate2/webrtc-client.py` was run through the controller
proxy before the browser runs:

```json
{
  "ok": true,
  "offer_candidates": 9,
  "answer_candidates": 9,
  "connection_state": "connected",
  "ice_connection_state": "completed",
  "ice_gathering_state": "complete",
  "media": {
    "video": {"buffers": 123, "caps": "I420 720x400 15/1"},
    "audio": {"buffers": 420, "caps": "S16LE 48000 Hz stereo"}
  },
  "bitrate": {"ok": true, "body": {"bitrate": 4000000, "ok": true}}
}
```

This confirms the helper and controller proxy were healthy before the browser
differences were attributed to the browsers.

## 9. Fixes

**No product code change was required.** The helper already selected the
offered H.264/Opus payload types correctly for Firefox (126/109) and Chromium
mobile (102/111); both answers were `sendonly` and both decoded media. The
Firefox failure was an H.264/OpenH264 capability gap in the test build, resolved
by enabling the pre-existing plugin in a temporary profile. The dashboard
mobile `#rc-ios-warning` null dereference was left unchanged because it is
outside the helper/`webrtc.rs` scope of this run.

The checked-in worker tests still pass:

```text
python3 softnode/gst_webrtc_worker_test.py
  -> Ran 26 tests ... OK
```

## 10. Tests / commands run

```text
scripts/gate2/webrtc-client.py (controller offer URL)
  -> connected / ICE completed / 123 video + 420 audio buffers / bitrate ack

Playwright Firefox 159.0 + OpenH264 2.6.0, dashboard
  -> connected / 65 video frames / 15 fps / 720x400 / 225 Opus packets
  -> no console messages, page errors, or failed requests

Playwright Chromium 159.0.8154.6, iPhone 13 emulation, dashboard
  -> connected / 63 video frames / 15 fps / 720x400 / 217 Opus packets
  -> one unrelated initRoomCorrection page error; no failed requests

Chromium candidate-less offer probe
  -> 0 offer candidates / 9 answer candidates / connected / 47 video frames

stunbdc -4 127.0.0.1 3478
  -> Mapped address: 127.0.0.1 port 51140

webrtcbin stun-server probe
  -> gathering COMPLETE / 9 candidates / all typ host / no srflx
```

## 11. Follow-up: STUN/TURN configuration and trickle candidates

This follow-up adds a product configuration surface and validates it on the
same host. It does **not** turn the earlier same-host result into a cross-NAT
pass.

### 11.1 Configuration surface

| Layer | Configuration |
|---|---|
| Node CLI/env | `--webrtc-stun` / `OZMA_WEBRTC_STUN`; `--webrtc-turn` / `OZMA_WEBRTC_TURN`; `--webrtc-turn-user` / `OZMA_WEBRTC_TURN_USER`; `--webrtc-turn-password` / `OZMA_WEBRTC_TURN_PASSWORD` |
| `WebRtcConfig` | `stun_server`, `turn_server`, `turn_user`, `turn_password`; manual `Debug` redacts both TURN username and password |
| Helper argv | `--stun-server <addr>`, `--turn-server <addr>`, `--turn-user <user>`, `--turn-password <secret>` (appended only when configured) |
| `webrtcbin` | `stun-server=stun://host:port`; `turn-server=turn://user:password@host:port` with percent-encoded credentials |
| Controller/browser | `OZMA_WEBRTC_ICE_SERVERS` JSON array, served at `GET /api/v1/webrtc/ice` and consumed by `dashboard.html`/`console.html` |
| Node signaling | `POST /webrtc/offer` returns optional `session`; `POST /webrtc/candidate` accepts `{"session", "candidate", "sdpMLineIndex"}` |

Validation is deliberately strict: empty or whitespace addresses, embedded
`user:pass@` credentials, an unsupported scheme, TURN without both credentials,
or credentials without a TURN server are startup errors. The node log reports
only the server addresses and `turn_auth=true`; the password never appears.
With no STUN/TURN values the helper argv and the historical host-candidate-only
pipeline are byte-for-byte unchanged. Browser `iceServers` default to `[]` and
keep the old bundled-offer behavior when the controller has no configured
servers.

The TURN password is passed to the helper as a separate argv value because the
worker CLI has no secret-stdin/environment channel. It is redacted from
`Debug`, logs, and status output, but a same-user process listing can still see
the argv. That residual exposure is a known limitation, not a claim of secret
isolation.

### 11.2 Live candidate evidence

The stack used the debug `ozma-node` with a synthetic RGB24 capture source
(`--capture-provider fake`, 320x240@15), the real Python/GStreamer worker, a
local `stund` on UDP 3478, and coturn extracted under `/tmp` on UDP 34780 with
relay ports 50000–50020. No `sudo`, package install, or system service was used.

A real GStreamer client (`scripts/gate2/webrtc-client.py`) received this
helper answer and decoded **93 H.264 buffers** at 320x240/15 fps:

```text
host  UDP  192.168.40.19:53228
host  TCP  active/passive
host  UDP  IPv6
srflx TCP  127.0.0.1:38987  raddr 192.168.40.19 rport 38987  tcptype passive
srflx TCP  127.0.0.1:9      raddr 192.168.40.19 rport 9      tcptype active
relay UDP  127.0.0.1:50007  raddr 192.168.40.19 rport 9
```

The `relay` UDP port is inside coturn's configured relay range, so it is a real
allocation from the local TURN server, not a fabricated candidate. No UDP
`srflx` appeared on this same-host path; libnice can discard a reflexive mapping
that duplicates the host address. The TCP `srflx` entries are real webrtcbin
output, but they are not evidence of a distinct NAT mapping.

Playwright Chromium (Chrome for Testing 159.0.8154.6) with the configured
STUN/TURN servers produced:

```text
browser candidate types: {"host": 1, "srflx": 1, "relay": 1}
trickle acks: [(200, true), (200, true), (200, true)]
connectionState: connected
selected pair: host ↔ host (UDP, 192.168.40.19)  # same host
inbound video: 140 frames decoded, 15 fps, 320x240, 774,525 bytes
```

The browser posted the offer immediately, then delivered its later candidates
through `/webrtc/candidate`; every candidate received an explicit `{"ok":true}`
ack from the helper. This exercises the new browser→helper trickle path with
real candidate types. The selected pair was still host↔host, so the run does
not prove relay media or connectivity across a NAT/firewall.

### 11.3 Trickle status

- **Browser→helper: implemented and exercised.** The offer response carries a
  helper session id, the dashboard/console buffer candidates until the answer
  arrives, and the node forwards `add-ice-candidate` messages to that session.
  Unknown sessions, malformed candidates, and candidates without a recognised
  `typ` token are rejected without reaching webrtcbin.
- **Helper→browser: still embedded, not trickled.** The worker waits for its
  bounded gather and appends every local candidate to the answer. A peer that
  needs a helper candidate discovered after the answer has no incremental
  channel; this is the remaining trickle limitation.
- Old helpers that do not return a `session` stay compatible: the browser keeps
  candidates embedded in the offer and never calls the candidate endpoint.

### 11.4 Follow-up cleanup

The follow-up node and its helper were stopped with `SIGINT`; `stund` and the
`/tmp` coturn process were killed; `/tmp/opencode/nat-ice`, the extracted
coturn tree, browser profiles, sockets, shared-memory files, and relay ports
50000–50020 were removed. No commit was made.


- Node received `SIGINT`; it reaped the Python WebRTC helper and its Unix
  socket/shared-memory file.
- Controller received `SIGINT`/`SIGTERM` and exited; its temporary capture and
  browser children were terminated.
- QEMU was stopped via its pidfile; the QMP socket was removed with the temp
  root.
- The tone `gst-launch` was killed; the null sink was unloaded with
  `pactl unload-module 536870916`.
- `stund` was stopped; no process remains on 3478.
- Playwright browsers and the temporary Firefox profile were closed.
- The `/tmp` venv, controller copy, evidence, and `/tmp/opencode/gate2-matrix`
  were removed. No commit was made.

## Remaining gaps

- **Safari/WebKit is untested.** Apple Safari is unavailable on Linux and the
  Playwright WebKit browser revision was not installed.
- **No real mobile browser or OS.** The mobile result is Chromium Blink under an
  iPhone descriptor, not iOS Safari/WebKit.
- **No STUN/TURN wiring in this run.** Node/helper/dashboard expose a STUN/TURN
  setting after the follow-up (§11), and live local allocations produced real
  `srflx`/`relay` candidates, but this matrix run itself configured none.
- **No trickle ICE in this run.** Candidates were gathered into one
  offer/answer exchange. The follow-up adds browser→helper candidate
  signaling; helper→browser candidates remain embedded (§11.3).
- **No cross-NAT or relay-media proof.** STUN/TURN configuration and real
  `srflx`/`relay` candidates exist, but every validated selected pair was
  host↔host on one machine. No NAT namespace, firewall, or remote peer was
  exercised, and relay media was not selected.
- **No end-to-end controller dashboard ICE run.** The new
  `OZMA_WEBRTC_ICE_SERVERS` parser and `/api/v1/webrtc/ice` endpoint are unit
  tested, and Chromium used the same ICE configuration directly; the full
  controller+dashboard page was not re-run with the new endpoint.
- **H.264-only helper.** Firefox without OpenH264 offers no H.264 and gets an
  `inactive` video m-line. VP8 encoding exists on the host but is not wired.
- **No webrtc-internals artifact.** `about:webrtc` and
  `chrome://webrtc-internals/` were not capturable in headless Playwright; the
  evidence is `getStats()` JSON plus screenshots.
- **No multi-peer, reconnect, or session-teardown matrix.** One peer per run.
- **Same host only.** The node and browsers share the loopback/LAN host; no NAT,
  firewall, or cross-network path was exercised.
- **Hardware decode and audible quality** were not asserted; audio used a
  synthetic tone on a disposable null-sink monitor.
