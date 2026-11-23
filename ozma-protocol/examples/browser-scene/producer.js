const image = document.querySelector("#still");
const video = document.querySelector("#motion");
const stage = document.querySelector("#stage");
const log = document.querySelector("#events");
const start = document.querySelector("#start");
const exportButton = document.querySelector("#export");

// Optional live connection. Open the fixture with
//   ?scene-ws=ws://127.0.0.1:7382/api/v1/browser/scene/ws
// to forward the same events to the node WebSocket. The Node live.mjs bridge
// is the alternative when the node requires a bearer token.
const liveUrl = new URLSearchParams(location.search).get("scene-ws");
let liveSocket;
const liveQueue = [];

const replay = [];
const replayLimit = 4 * 1024 * 1024;
let replayBytes = 0;

let resourceSent = false;
let starting = false;
let revision = 0;
let lastPlacement = "";
let videoFrame = 0;
const resourceId = 7;

function emit(type, detail, summary = detail) {
  const event = { type, ...detail };
  if (detail.bytes instanceof Uint8Array) {
    let binary = "";
    for (const byte of detail.bytes) binary += String.fromCharCode(byte);
    event.payload_base64 = btoa(binary);
  }
  if (type === "resource" || type === "scene_commit") {
    const { bytes, ...record } = event;
    const line = JSON.stringify(record) + "\n";
    if (replayBytes + line.length <= replayLimit) {
      replay.push(line);
      replayBytes += line.length;
      exportButton.disabled = false;
    }
  }
  window.dispatchEvent(new CustomEvent("ozma-scene", { detail: event }));
  sendLive(event);
  if (type !== "video_frame" || detail.frame % 15 === 1) {
    log.textContent = (JSON.stringify({ type, ...summary }) + "\n" + log.textContent).slice(0, 4096);
  }
}

function liveRecord(event) {
  if (event.type === "resource" && event.payload_base64) {
    return { type: "resource", id: event.id, payload_base64: event.payload_base64 };
  }
  if (event.type === "scene_commit") {
    return { type: "scene_commit", commit: event.commit };
  }
  if (event.type === "video_frame") {
    return { type: "video_frame", surface_id: event.surface_id, frame: event.frame };
  }
  return undefined;
}

function connectLive() {
  if (!liveUrl || liveSocket) return;
  liveSocket = new WebSocket(liveUrl);
  liveSocket.addEventListener("open", () => {
    log.textContent = (`live scene connected to ${liveUrl}\n` + log.textContent).slice(0, 4096);
    for (const text of liveQueue.splice(0)) liveSocket.send(text);
  });
  liveSocket.addEventListener("message", message => {
    log.textContent = (`${message.data}\n` + log.textContent).slice(0, 4096);
  });
  liveSocket.addEventListener("close", () => {
    liveSocket = undefined;
  });
  liveSocket.addEventListener("error", () => {
    log.textContent = ("live scene socket failed\n" + log.textContent).slice(0, 4096);
  });
}

function sendLive(event) {
  if (!liveUrl) return;
  const record = liveRecord(event);
  if (!record) return;
  if (liveSocket && liveSocket.readyState === WebSocket.CLOSED) liveSocket = undefined;
  if (!liveSocket) connectLive();
  const text = JSON.stringify(record);
  if (liveSocket.readyState === WebSocket.OPEN) liveSocket.send(text);
  else liveQueue.push(text);
}

function placement() {
  const rect = image.getBoundingClientRect();
  const bounds = stage.getBoundingClientRect();
  return {
    node_id: 1,
    resource_id: resourceId,
    x: Math.round(rect.left - bounds.left),
    y: Math.round(rect.top - bounds.top),
    width: Math.round(rect.width),
    height: Math.round(rect.height),
  };
}

start.addEventListener("click", async () => {
  if (starting) return;
  starting = true;
  start.disabled = true;
  const canvas = document.createElement("canvas");
  canvas.width = 320;
  canvas.height = 180;
  const context = canvas.getContext("2d");
  context.fillStyle = "#155e75";
  context.fillRect(0, 0, 320, 180);
  context.fillStyle = "#ecfeff";
  context.font = "bold 28px system-ui";
  context.fillText("Reusable image asset", 18, 96);
  const blob = await new Promise(resolve => canvas.toBlob(resolve, "image/png"));
  if (!blob) {
    log.textContent = "PNG encoding failed";
    start.disabled = false;
    starting = false;
    return;
  }
  const bytes = new Uint8Array(8 + blob.size);
  new DataView(bytes.buffer).setBigUint64(0, BigInt(resourceId));
  bytes.set(new Uint8Array(await blob.arrayBuffer()), 8);
  image.src = URL.createObjectURL(blob);
  emit("resource", { id: resourceId, mime: "image/png", bytes }, { id: resourceId, size: bytes.length });
  resourceSent = true;
  commitPlacement();

  let phase = 0;
  const videoCanvas = document.createElement("canvas");
  videoCanvas.width = 320;
  videoCanvas.height = 180;
  const videoContext = videoCanvas.getContext("2d");
  video.srcObject = videoCanvas.captureStream(15);
  let lastFrameTime = -Infinity;
  const draw = time => {
    requestAnimationFrame(draw);
    if (time - lastFrameTime < 1000 / 15) return;
    lastFrameTime = time;
    phase += 3;
    videoContext.fillStyle = "#1e293b";
    videoContext.fillRect(0, 0, 320, 180);
    videoContext.fillStyle = "#38bdf8";
    videoContext.beginPath();
    videoContext.arc(40 + phase % 240, 90, 24, 0, Math.PI * 2);
    videoContext.fill();
    videoFrame += 1;
    emit("video_frame", { surface_id: 1, frame: videoFrame });
  };
  requestAnimationFrame(draw);
});

function commitPlacement() {
  if (!resourceSent) return;
  const imagePlacement = placement();
  const next = JSON.stringify(imagePlacement);
  if (next === lastPlacement) return;
  lastPlacement = next;
  emit("scene_commit", { commit: { revision: ++revision, images: [imagePlacement] } });
}

stage.addEventListener("scroll", commitPlacement, { passive: true });

exportButton.addEventListener("click", () => {
  const href = URL.createObjectURL(new Blob(replay, { type: "application/x-ndjson" }));
  const link = document.createElement("a");
  link.href = href;
  link.download = "ozma-scene.jsonl";
  link.click();
  setTimeout(() => URL.revokeObjectURL(href), 1000);
});
