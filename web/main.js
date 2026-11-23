// SPDX-License-Identifier: AGPL-3.0-only
// Dependency-free demo for the Rust/WebAssembly Ozma client.
//
// The protocol work (WebTransport, channel framing, video reassembly,
// WebCodecs decode) happens in `ozma-web` -> `ozma-client-core`/
// `ozma-transport-web`. This file only does DOM plumbing:
//
//   1. create + start a session over the node's JSON API,
//   2. choose the WebTransport certificate (additive /api/v1/webtransport
//      endpoint, falling back to /api/v1/transport),
//   3. connect `OzmaClient`,
//   4. translate DOM events into `send_pointer`/`send_button`/`send_wheel`/
//      `send_key`, and poll `stats()` for the status panel.
//
// Build the wasm package first: ./build.sh (from web/).

import init, { OzmaClient, jsCodeToHid } from './pkg/ozma_web.js';

const canvas = document.getElementById('screen');
const stage = document.getElementById('stage');
const nodeInput = document.getElementById('node');
const connectButton = document.getElementById('connect');
const disconnectButton = document.getElementById('disconnect');
const statusLine = document.getElementById('status');
const statsLine = document.getElementById('stats');
const shareCamera = document.getElementById('share-camera');
const shareMic = document.getElementById('share-mic');
const startCaptureButton = document.getElementById('start-capture');
const stopCaptureButton = document.getElementById('stop-capture');
const previewToggle = document.getElementById('preview-camera');
const previewVideo = document.getElementById('camera-preview');
const captureStatusLine = document.getElementById('capture-status');

let client = null;
let statsTimer = null;
let lastSample = null;
let lastCaptureSample = null;
let connecting = false;
let capturing = false;

const pressedKeys = new Set();
const pressedButtons = new Set();
let pendingPointer = null;
let pointerFrame = 0;

await init();

connectButton.addEventListener('click', () => {
  connect().catch((error) => setStatus(describeError(error), 'error'));
});
disconnectButton.addEventListener('click', () => disconnect('user requested'));
startCaptureButton.addEventListener('click', () => {
  startCapture().catch((error) => setCaptureStatus(describeError(error), 'error'));
});
stopCaptureButton.addEventListener('click', () => stopCapture());
previewToggle.addEventListener('change', () => {
  // The Rust camera pipeline owns the stream and keeps this element muted.
  previewVideo.hidden = !previewToggle.checked;
});

canvas.addEventListener('pointerdown', async (event) => {
  if (!client) return;
  event.preventDefault();
  canvas.focus();
  // Browsers start an AudioContext suspended until a user gesture.
  try {
    client.resume_audio();
  } catch (error) {
    setStatus(`audio unlock failed: ${describeError(error)}`, 'error');
  }
  // Capture the pointer so a release outside the canvas still arrives.
  try {
    canvas.setPointerCapture(event.pointerId);
  } catch {
    // Pointer capture is best-effort.
  }
  pressedButtons.add(event.button);
  send(() => client.send_button(event.button, true));
});

canvas.addEventListener('pointerup', (event) => {
  if (!client) return;
  event.preventDefault();
  pressedButtons.delete(event.button);
  send(() => client.send_button(event.button, false));
  try {
    canvas.releasePointerCapture(event.pointerId);
  } catch {
    // Released already.
  }
});

canvas.addEventListener('pointercancel', (event) => {
  if (!client) return;
  pressedButtons.delete(event.button);
  send(() => client.send_button(event.button, false));
});

canvas.addEventListener('pointermove', (event) => {
  if (!client) return;
  const { x, y } = canvasPoint(event);
  pendingPointer = { x, y };
  if (pointerFrame === 0) {
    pointerFrame = requestAnimationFrame(() => {
      pointerFrame = 0;
      if (!pendingPointer || !client) return;
      const point = pendingPointer;
      pendingPointer = null;
      send(() => client.send_pointer(point.x, point.y));
    });
  }
});

// Do not let the page scroll or open a context menu over the remote desktop.
canvas.addEventListener('contextmenu', (event) => event.preventDefault());
canvas.addEventListener(
  'wheel',
  (event) => {
    if (!client) return;
    event.preventDefault();
    const { dx, dy } = wheelDelta(event);
    send(() => client.send_wheel(dx, dy));
  },
  { passive: false },
);

canvas.addEventListener('keydown', (event) => {
  if (!client) return;
  const hid = jsCodeToHid(event.code);
  if (hid === undefined || hid === null) return;
  event.preventDefault();
  pressedKeys.add(hid);
  send(() => client.send_key(hid, true, modifiers(event)));
});

canvas.addEventListener('keyup', (event) => {
  if (!client) return;
  const hid = jsCodeToHid(event.code);
  if (hid === undefined || hid === null) return;
  event.preventDefault();
  pressedKeys.delete(hid);
  send(() => client.send_key(hid, false, modifiers(event)));
});

// Losing focus must not leave a key or button held on the remote host.
window.addEventListener('blur', releaseEverything);
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'hidden') releaseEverything();
});
window.addEventListener('beforeunload', () => {
  if (client) {
    client.close();
    client = null;
  }
});

function releaseEverything() {
  if (!client) return;
  for (const hid of pressedKeys) {
    send(() => client.send_key(hid, false, 0));
  }
  for (const button of pressedButtons) {
    send(() => client.send_button(button, false));
  }
  pressedKeys.clear();
  pressedButtons.clear();
  send(() => client.release_all());
}

async function connect() {
  if (client || connecting) return;
  connecting = true;
  connectButton.disabled = true;
  setStatus('creating session…');
  try {
    const node = normalizeNode(nodeInput.value);
    const spec = sessionSpec();
    const created = await requestJson(`${node}/api/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(spec),
    });
    const id = created.session?.id;
    if (!id) throw new Error('the node did not return a session id');
    const started = await requestJson(
      `${node}/api/v1/sessions/${encodeURIComponent(id)}/start`,
      { method: 'POST' },
    );
    const offer = started.offer ?? created.offer;
    if (!offer) {
      throw new Error('the node did not return a session offer (is the transport configured?)');
    }

    setStatus('reading WebTransport certificate…');
    const transport = await webtransportInfo(node);

    setStatus('connecting WebTransport + WebCodecs…');
    const connected = await OzmaClient.connect(
      canvas,
      JSON.stringify(offer),
      transport.certificate_der_base64,
    );
    connected.set_listener((event) => {
      // Ignore late events from a client the page has already detached.
      if (client !== connected) return;
      const detail = event.detail ?? '';
      if (event.type === 'error') {
        setStatus(detail, 'error');
      } else if (event.type === 'closed') {
        client = null;
        setStatus(`session closed: ${detail}`, 'error');
        teardown();
      } else {
        setStatus(`${event.type}: ${detail}`);
      }
    });
    client = connected;
    // Exposed for the demo's browser verification scripts. Nothing in the page
    // uses these globals.
    globalThis.ozmaClient = connected;
    globalThis.ozmaSession = { id, node };
    stage.classList.add('connected');
    connectButton.disabled = true;
    disconnectButton.disabled = false;
    startCaptureButton.disabled = false;
    stopCaptureButton.disabled = true;
    canvas.focus();
    setStatus(`connected (${transport.endpoint ?? 'offer endpoint'})`, 'good');
    lastSample = null;
    lastCaptureSample = null;
    statsTimer = setInterval(renderStats, 500);
    renderStats();
  } catch (error) {
    if (client) {
      try {
        client.close();
      } catch {
        // Already gone.
      }
      client = null;
    }
    setStatus(describeError(error), 'error');
    teardown();
  } finally {
    connecting = false;
    if (!client) connectButton.disabled = false;
  }
}

async function startCapture() {
  if (!client || capturing) return;
  const video = shareCamera.checked;
  const audio = shareMic.checked;
  if (!video && !audio) {
    setCaptureStatus('select the camera, the microphone, or both', 'error');
    return;
  }
  capturing = true;
  startCaptureButton.disabled = true;
  stopCaptureButton.disabled = false;
  setCaptureStatus(
    `requesting ${video ? 'camera' : ''}${video && audio ? ' + ' : ''}${audio ? 'microphone' : ''}…`,
  );
  lastCaptureSample = null;
  try {
    // This call must stay directly in the click handler's async chain: the
    // browser requires the getUserMedia/AudioContext calls to be in a user
    // gesture.
    await client.start_capture(video, audio);
    const state = client.capture_state();
    const parts = [];
    if (state.video) {
      parts.push(`${state.videoCodec ?? 'h264'} ${state.width || '?'}×${state.height || '?'}`);
    }
    if (state.audio) parts.push('opus 48 kHz stereo');
    setCaptureStatus(`capturing ${parts.join(' + ')}`, 'good');
  } catch (error) {
    const state = client?.capture_state?.();
    const running = [];
    if (state?.video) running.push('camera');
    if (state?.audio) running.push('microphone');
    setCaptureStatus(
      `${describeError(error)}${running.length ? ` (still capturing ${running.join(' + ')})` : ''}`,
      'error',
    );
  } finally {
    capturing = false;
    if (client) {
      startCaptureButton.disabled = false;
      stopCaptureButton.disabled = client.capture_state().video === false &&
        client.capture_state().audio === false;
    }
    renderStats();
  }
}

function stopCapture() {
  if (!client) return;
  try {
    client.stop_capture();
    setCaptureStatus('capture stopped');
  } catch (error) {
    setCaptureStatus(`stop: ${describeError(error)}`, 'error');
  }
  startCaptureButton.disabled = false;
  stopCaptureButton.disabled = true;
  lastCaptureSample = null;
  renderStats();
}

function disconnect(reason) {
  releaseEverything();
  if (client) {
    setStatus(`closing (${reason})…`);
    client.close();
    client = null;
  }
  teardown();
  setStatus(`disconnected (${reason})`);
}

function teardown() {
  if (statsTimer !== null) {
    clearInterval(statsTimer);
    statsTimer = null;
  }
  if (pointerFrame !== 0) {
    cancelAnimationFrame(pointerFrame);
    pointerFrame = 0;
  }
  pendingPointer = null;
  pressedKeys.clear();
  pressedButtons.clear();
  capturing = false;
  lastCaptureSample = null;
  startCaptureButton.disabled = true;
  stopCaptureButton.disabled = true;
  previewToggle.checked = false;
  previewVideo.hidden = true;
  setCaptureStatus('capture idle');
  globalThis.ozmaClient = null;
  globalThis.ozmaSession = null;
  stage.classList.remove('connected');
  disconnectButton.disabled = true;
  connectButton.disabled = false;
  statsLine.textContent = 'stats appear after connect';
}

function sessionSpec() {
  return {
    client_id: `browser-${randomId()}`,
    principal: 'browser-demo',
    // An empty track list lets the development node fall back to its fake
    // capture/encoder pipeline. Add a `capture` or `virtual_display` track for
    // a real source.
    tracks: [],
    audio: {
      enabled: true,
      // The microphone uplink needs the Mic permission and the mic channel.
      mic: true,
      format: { codec: 'opus', sample_rate: 48000, channels: 2, bitrate_kbps: 128 },
    },
    input: {
      keyboard: true,
      pointer: 'absolute',
      gamepad: false,
      touch: false,
      pen: false,
      clipboard: false,
      owner_only: true,
    },
    privacy: {},
    transport: 'auto',
    ttl_seconds: 3600,
  };
}

async function webtransportInfo(node) {
  // The WebTransport listener may present its own certificate, so prefer the
  // additive endpoint. Older nodes only expose the raw QUIC transport info.
  try {
    const info = await requestJson(`${node}/api/v1/webtransport`);
    if (info?.certificate_der_base64) return info;
  } catch (error) {
    if (![404, 501].includes(error.status)) throw error;
  }
  const transport = await requestJson(`${node}/api/v1/transport`);
  if (!transport.certificate_der_base64) {
    throw new Error('/api/v1/transport has no certificate_der_base64');
  }
  return transport;
}

async function requestJson(url, options = {}) {
  const response = await fetch(url, { ...options, mode: 'cors' });
  const text = await response.text();
  let body = null;
  if (text) {
    try {
      body = JSON.parse(text);
    } catch {
      body = text;
    }
  }
  if (!response.ok) {
    const apiError = body?.error;
    const error = new Error(
      apiError?.message ?? body?.message ?? `${response.status} ${response.statusText}`,
    );
    error.status = response.status;
    error.code = apiError?.code;
    error.remediation = apiError?.remediation;
    throw error;
  }
  return body;
}

function renderStats() {
  if (!client) return;
  const stats = client.stats();
  const capture = client.capture_state();
  const now = performance.now();
  let fps = 0;
  if (lastSample) {
    const seconds = (now - lastSample.at) / 1000;
    if (seconds > 0) {
      fps = (stats.videoFrames - lastSample.frames) / seconds;
    }
  }
  lastSample = { at: now, frames: stats.videoFrames };
  let captureFps = 0;
  if (lastCaptureSample) {
    const seconds = (now - lastCaptureSample.at) / 1000;
    if (seconds > 0) {
      captureFps = (stats.cameraFrames - lastCaptureSample.frames) / seconds;
    }
  }
  lastCaptureSample = { at: now, frames: stats.cameraFrames };
  const size = stats.width && stats.height ? `${stats.width}×${stats.height}` : '—';
  const cameraSize =
    stats.cameraWidth && stats.cameraHeight
      ? `${stats.cameraWidth}×${stats.cameraHeight}`
      : '—';
  const codec = capture.videoCodec ?? '—';
  statsLine.textContent =
    `status      ${client.status()}\n` +
    `video       ${stats.videoFrames} frames  ${fps.toFixed(1)} fps  ` +
    `${stats.videoDropped} dropped  ${stats.videoErrors} errors\n` +
    `audio       ${stats.audioFrames} buffers  ${stats.audioDropped} dropped  ` +
    `${stats.audioErrors} errors  level ${stats.audioLevel.toFixed(3)}  ` +
    `drift ${stats.avDriftMs.toFixed(1)} ms\n` +
    `canvas      ${size}\n` +
    `camera      ${stats.cameraFrames} frames  ${captureFps.toFixed(1)} fps  ` +
    `${formatBytes(stats.cameraBytes)}  ${stats.cameraDropped} dropped  ` +
    `${stats.cameraErrors} errors  ${cameraSize}  ${codec}\n` +
    `microphone  ${stats.micPackets} packets  ${formatBytes(stats.micBytes)}  ` +
    `${stats.micErrors} errors\n` +
    `received    ${formatBytes(stats.bytesIn)}`;
  if (capture.error) {
    const code = capture.errorCode ? `[${capture.errorCode}] ` : '';
    setCaptureStatus(`${code}${capture.error}`, 'error');
  }
}

function send(action) {
  try {
    action();
  } catch (error) {
    setStatus(`input: ${describeError(error)}`, 'error');
  }
}

function canvasPoint(event) {
  const rect = canvas.getBoundingClientRect();
  if (rect.width === 0 || rect.height === 0) return { x: 0, y: 0 };
  const x = ((event.clientX - rect.left) / rect.width) * canvas.width;
  const y = ((event.clientY - rect.top) / rect.height) * canvas.height;
  return {
    x: Math.max(0, Math.min(canvas.width - 1, Math.round(x))),
    y: Math.max(0, Math.min(canvas.height - 1, Math.round(y))),
  };
}

function wheelDelta(event) {
  let scale = 1;
  if (event.deltaMode === 1) scale = 40; // lines
  else if (event.deltaMode === 2) scale = 800; // pages
  return {
    dx: Math.round(event.deltaX * scale),
    dy: Math.round(event.deltaY * scale),
  };
}

function modifiers(event) {
  // The low bits of the USB HID boot keyboard modifier report. Browser
  // modifier flags cannot distinguish left from right, so the left bits are
  // used; the key's own HID usage identifies the physical side.
  let bits = 0;
  if (event.ctrlKey) bits |= 0x01;
  if (event.shiftKey) bits |= 0x02;
  if (event.altKey) bits |= 0x04;
  if (event.metaKey) bits |= 0x08;
  return bits;
}

function normalizeNode(value) {
  const trimmed = value.trim().replace(/\/+$/, '');
  if (!trimmed) throw new Error('enter the node API URL');
  return trimmed;
}

function randomId() {
  if (globalThis.crypto?.randomUUID) return crypto.randomUUID();
  return Math.random().toString(36).slice(2) + Date.now().toString(36);
}

function formatBytes(value) {
  if (!Number.isFinite(value)) return '—';
  const units = ['B', 'KiB', 'MiB', 'GiB'];
  let amount = value;
  let unit = 0;
  while (amount >= 1024 && unit < units.length - 1) {
    amount /= 1024;
    unit += 1;
  }
  return `${amount.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
}

function describeError(error) {
  if (!error) return 'unknown error';
  const parts = [];
  if (error.code) parts.push(`[${error.code}]`);
  parts.push(error.message ?? String(error));
  if (error.remediation) parts.push(`— ${error.remediation}`);
  return parts.join(' ');
}

function setStatus(message, kind = '') {
  statusLine.textContent = message;
  statusLine.className = kind;
}

function setCaptureStatus(message, kind = '') {
  captureStatusLine.textContent = message;
  captureStatusLine.className = kind;
}
