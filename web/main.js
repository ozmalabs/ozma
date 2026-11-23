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

let client = null;
let statsTimer = null;
let lastSample = null;
let connecting = false;

const pressedKeys = new Set();
const pressedButtons = new Set();
let pendingPointer = null;
let pointerFrame = 0;

await init();

connectButton.addEventListener('click', () => {
  connect().catch((error) => setStatus(describeError(error), 'error'));
});
disconnectButton.addEventListener('click', () => disconnect('user requested'));

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
    stage.classList.add('connected');
    connectButton.disabled = true;
    disconnectButton.disabled = false;
    canvas.focus();
    setStatus(`connected (${transport.endpoint ?? 'offer endpoint'})`, 'good');
    lastSample = null;
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
      mic: false,
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
  const now = performance.now();
  let fps = 0;
  if (lastSample) {
    const seconds = (now - lastSample.at) / 1000;
    if (seconds > 0) {
      fps = (stats.videoFrames - lastSample.frames) / seconds;
    }
  }
  lastSample = { at: now, frames: stats.videoFrames };
  const size = stats.width && stats.height ? `${stats.width}×${stats.height}` : '—';
  statsLine.textContent =
    `status      ${client.status()}\n` +
    `video       ${stats.videoFrames} frames  ${fps.toFixed(1)} fps  ` +
    `${stats.videoDropped} dropped  ${stats.videoErrors} errors\n` +
    `audio       ${stats.audioFrames} buffers  ${stats.audioDropped} dropped  ` +
    `${stats.audioErrors} errors\n` +
    `canvas      ${size}\n` +
    `received    ${formatBytes(stats.bytesIn)}`;
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
