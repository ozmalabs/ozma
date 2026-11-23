const image = document.querySelector("#still");
const video = document.querySelector("#motion");
const stage = document.querySelector("#stage");
const log = document.querySelector("#events");
const start = document.querySelector("#start");

let resourceSent = false;
let starting = false;
let revision = 0;
let lastPlacement = "";
let videoFrame = 0;
const resourceId = 7;

function emit(type, detail, summary = detail) {
  const event = { type, ...detail };
  window.dispatchEvent(new CustomEvent("ozma-scene", { detail: event }));
  if (type !== "video_frame" || detail.frame % 15 === 1) {
    log.textContent = (JSON.stringify({ type, ...summary }) + "\n" + log.textContent).slice(0, 4096);
  }
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
