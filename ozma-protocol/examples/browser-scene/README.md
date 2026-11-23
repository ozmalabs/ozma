# Browser scene fixture

This directory contains the deterministic browser scene fixture, its JSONL
replay bridge, and the live WebSocket bridge to a node.

## Fixture

Serve this directory on localhost:

```sh
cd ozma-protocol/examples/browser-scene
python3 -m http.server 8080
```

Open `http://127.0.0.1:8080/`, press **Start producer**, scroll the stage, and
optionally download `ozma-scene.jsonl`. The still image is uploaded once and
only its placement changes as the page scrolls. The video canvas advances
independently and emits metadata-only `video_frame` records.

## JSONL replay

The existing fixture path is unchanged:

```sh
node bridge.mjs < ozma-scene.jsonl
cargo run -p ozma-node -- --scene-replay ozma-scene.jsonl
```

`bridge.mjs` validates the resource cache and placement revisions and prints
`presented`, `video_frame`, and `error` records. A node started with
`--scene-replay` replays that trusted file to scene-capable sessions.

## Live node bridge

Start a development node **without** `--scene-replay` so the live browser hub is
its scene producer, then forward the fixture records:

```sh
node live.mjs --node ws://127.0.0.1:7382 --file ozma-scene.jsonl
# or
node live.mjs --node ws://127.0.0.1:7382 < ozma-scene.jsonl
```

The node endpoint is `GET /api/v1/browser/scene/ws`. It is covered by the node's
optional bearer-token guard; pass `--token "$OZMA_API_TOKEN"` to `live.mjs`
when the node was started with `--api-token`.

The fixture can also connect directly when the node has no API token. Open:

```text
http://127.0.0.1:8080/?scene-ws=ws://127.0.0.1:7382/api/v1/browser/scene/ws
```

Token-protected nodes need the `live.mjs` bridge or a local proxy because the
browser WebSocket API cannot set an `Authorization` header.

### WebSocket messages

Send JSON text frames. The node replies with `ack` or `error` records:

```json
{"type":"resource","id":7,"payload_base64":"<base64 of u64be id || PNG>"}
{"type":"scene_commit","commit":{"revision":1,"images":[{"node_id":1,"resource_id":7,"x":0,"y":0,"width":320,"height":180}]}}
{"type":"video_frame","surface_id":1,"frame":3}
```

```json
{"type":"ack","kind":"resource","id":7}
{"type":"ack","kind":"scene_commit","revision":1,"images":1}
{"type":"ack","kind":"video_frame","surface_id":1,"frame":3}
{"type":"error","code":"stale_revision","message":"scene revision 1 is not newer than 1"}
```

An invalid single message returns `error` and leaves the socket open. Binary
frames, messages over 4 MiB, or 32 consecutive malformed messages close the
socket. The hub also enforces:

- a 4 MiB default PNG cache budget and the node's bounded commit/video history;
- conflicting resource ids and non-PNG framed payloads;
- monotonically increasing commit revisions and valid, unique placements;
- resources that arrive after a commit are delivered before the waiting revision.

Video records are accepted as bounded metadata only. This adapter does not
ingest WebCodecs frames or forward browser video into the native video channel.
The node's normal capture pipeline remains the video source.
