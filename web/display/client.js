// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Browser display adapter for the Ozma protobuf Display protocol.
 *
 * The controller accepts a JSON registration frame, then sends binary
 * `[tag][protobuf]` frames on the same WebSocket.  This client renders a
 * pragmatic subset of the definition document and sends ActionInvoke frames
 * when a bound widget is activated.
 */

import {
  TAG_DEFINITION,
  TAG_DATA_UPDATE,
  TAG_ACTION_INVOKE,
  decodeFrame,
  encodeActionInvoke,
  encodeFrame,
} from './protocol.js';

const DEFAULT_SERVER = 'ws://localhost:7391';
const SERVER_STORAGE_KEY = 'ozma.display.server';
const DEVICE_STORAGE_KEY = 'ozma.display.deviceId';
const MAX_LOG_LINES = 300;

const CONTAINER_TYPES = new Set([
  'column',
  'row',
  'grid',
  'stack',
  'layout',
  'container',
  'panel',
  'group',
  'list',
]);

const WIDGET_TYPES = new Set([
  'text',
  'label',
  'number',
  'gauge',
  'bar',
  'vu_meter',
  'sparkline',
  'chart',
  'history',
  'icon',
  'scenario',
  'clock',
  'image',
  'button',
  'toggle',
  'switch',
]);

const DEFAULT_THEME = 'core-dark';
const GAUGE_MODULE_URL = './widgets/gauge.js';
const SPARKLINE_MODULE_URL = './widgets/sparkline.js';
const THEME_MODULE_URL = './theme.js';
export const DEFAULT_HISTORY_POINTS = 120;
export const MAX_HISTORY_POINTS = 4096;

const els = typeof document !== 'undefined'
  ? {
    form: document.getElementById('connect-form'),
    server: document.getElementById('server'),
    connect: document.getElementById('connect-button'),
    status: document.getElementById('status'),
    statusText: document.getElementById('status-text'),
    connectionDetail: document.getElementById('connection-detail'),
    screen: document.getElementById('screen'),
    empty: document.getElementById('empty-state'),
    definitionMeta: document.getElementById('definition-meta'),
    stats: document.getElementById('stats'),
    fieldsEmpty: document.getElementById('fields-empty'),
    fieldsTable: document.getElementById('fields-table'),
    fieldsBody: document.getElementById('fields-body'),
    log: document.getElementById('log'),
    clearLog: document.getElementById('clear-log'),
  }
  : {};

const state = {
  ws: null,
  connected: false,
  manualClose: false,
  reconnectAttempt: 0,
  reconnectTimer: null,
  definition: null,
  document: null,
  frameCount: 0,
  updateCount: 0,
  lastUpdate: null,
  deviceId: getOrCreateDeviceId(),
  fieldById: new Map(),
  fieldByKey: new Map(),
  values: new Map(),
  valuesByKey: new Map(),
  bindings: [],
  textBindings: [],
  fieldRows: new Map(),
  actionsById: new Map(),
  actionsByKey: new Map(),
  gaugeModule: null,
  sparklineModule: null,
  themeModule: null,
  theme: null,
  themeTokens: {},
  themeCanvas: null,
  historyByField: new Map(),
};

let actionSequence = 0n;
let gaugeSequence = 0;
let sparklineSequence = 0;

/* ------------------------------------------------------------------------- */
/* Small utilities                                                            */
/* ------------------------------------------------------------------------- */

function getOrCreateDeviceId() {
  if (typeof window === 'undefined') {
    return `browser-${Math.random().toString(36).slice(2, 10)}`;
  }
  try {
    const existing = localStorage.getItem(DEVICE_STORAGE_KEY);
    if (existing) return existing;
    const id = `browser-${Math.random().toString(36).slice(2, 10)}`;
    localStorage.setItem(DEVICE_STORAGE_KEY, id);
    return id;
  } catch {
    return `browser-${Math.random().toString(36).slice(2, 10)}`;
  }
}

function storageGet(key) {
  if (typeof window === 'undefined') return null;
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function storageSet(key, value) {
  if (typeof window === 'undefined') return;
  try {
    localStorage.setItem(key, value);
  } catch {
    // Storage can be unavailable in private windows; it is not required.
  }
}

function normalizeServer(input) {
  let raw = String(input || '').trim();
  if (!raw) return '';
  if (raw.startsWith('http://')) raw = `ws://${raw.slice(7)}`;
  else if (raw.startsWith('https://')) raw = `wss://${raw.slice(8)}`;
  else if (!/^wss?:\/\//i.test(raw)) raw = `ws://${raw}`;
  try {
    const url = new URL(raw);
    if (url.protocol !== 'ws:' && url.protocol !== 'wss:') return '';
    return url.toString();
  } catch {
    return '';
  }
}

function updateUrl(server) {
  try {
    const url = new URL(window.location.href);
    url.searchParams.set('server', server);
    history.replaceState(null, '', url);
  } catch {
    // URL updates are cosmetic.
  }
}

function cssToken(value) {
  return String(value || '')
    .toLowerCase()
    .replace(/[^a-z0-9_-]+/g, '-')
    .replace(/^-+|-+$/g, '') || 'widget';
}

function formatValue(value) {
  if (value === undefined || value === null) return '—';
  if (typeof value === 'boolean') return value ? 'On' : 'Off';
  if (typeof value === 'bigint') return value.toString();
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) return String(value);
    return Number.isInteger(value) ? String(value) : String(Number(value.toFixed(4)));
  }
  if (value instanceof Uint8Array) {
    const preview = Array.from(value.slice(0, 8), (byte) => byte.toString(16).padStart(2, '0')).join(' ');
    return `${value.length} bytes${preview ? ` (${preview}${value.length > 8 ? '…' : ''})` : ''}`;
  }
  if (typeof value === 'object') {
    try {
      return JSON.stringify(value);
    } catch {
      return String(value);
    }
  }
  return String(value);
}

function formatWidgetValue(value, field, node) {
  if (value === undefined || value === null) return '—';

  const type = field?.type || node?.type || '';
  if (type === 'timestamp' && (typeof value === 'number' || typeof value === 'bigint')) {
    const date = new Date(Number(value) / 1000);
    if (!Number.isNaN(date.getTime())) return date.toLocaleString();
  }

  let text;
  if (typeof value === 'number' && Number.isFinite(value)) {
    const decimals = Number.isInteger(node?.decimals)
      ? Math.max(0, Math.min(6, node.decimals))
      : null;
    text = decimals === null
      ? (Number.isInteger(value) ? String(value) : String(Number(value.toFixed(2))))
      : value.toFixed(decimals);
  } else if (typeof value === 'boolean') {
    text = value ? (node?.on_text || 'On') : (node?.off_text || 'Off');
  } else {
    text = formatValue(value);
  }

  const unit = node?.unit || field?.unit || '';
  return unit && text !== '—' ? `${text} ${unit}` : text;
}

function clampPercent(value, min, max) {
  const lower = Number.isFinite(Number(min)) ? Number(min) : 0;
  const upper = Number.isFinite(Number(max)) ? Number(max) : 100;
  if (upper === lower) return 0;
  return Math.max(0, Math.min(100, ((Number(value) - lower) / (upper - lower)) * 100));
}

function hasPlaceholders(text) {
  return /\{[^{}]+\}/.test(String(text || ''));
}

/* ------------------------------------------------------------------------- */
/* Optional theme and gauge module selection (pure helpers)                   */
/* ------------------------------------------------------------------------- */

/**
 * Pick the theme requested at startup. The query string wins, then a
 * `data-theme` attribute, then the default theme when the theme module exists.
 * Returns null when no theme should be loaded.
 */
export function selectThemeSelection({
  search = '',
  dataTheme = '',
  hasThemeModule = false,
  defaultTheme = DEFAULT_THEME,
} = {}) {
  if (!hasThemeModule) return null;

  const params = new URLSearchParams(String(search || '').replace(/^\?/, ''));
  const requested = String(params.get('theme') || '').trim();
  if (requested) return { origin: 'query', value: requested };

  const attribute = String(dataTheme || '').trim();
  if (attribute) return { origin: 'data-theme', value: attribute };

  if (defaultTheme) return { origin: 'default', value: defaultTheme };
  return null;
}

/**
 * Map a widget type to a gauge rendering mode, or null when it should use the
 * adapter's existing card/meter renderer.
 */
export function gaugeModeForType(type) {
  const name = String(type || '').toLowerCase();
  if (name === 'gauge') return 'arc';
  if (name === 'bar' || name === 'vu_meter') return 'bar';
  return null;
}

/**
 * Map a widget type to a sparkline rendering mode, or null when it should use
 * another renderer. `chart` requires an explicit history configuration so a
 * plain chart does not accidentally become a rolling history widget.
 */
export function sparklineModeForType(type, node = null) {
  const name = String(type || '').toLowerCase();
  if (name === 'sparkline' || name === 'history') return 'sparkline';
  if (name === 'chart') {
    const history = node?.history;
    if (history === undefined || history === null || history === false) return null;
    if (Array.isArray(history)) return history.length ? 'sparkline' : null;
    if (typeof history === 'object') return Object.keys(history).length ? 'sparkline' : null;
    return 'sparkline';
  }
  return null;
}

/**
 * Bounded history capacity for a node. Widgets can set
 * `history.points` or `history.window`; a numeric `history` shorthand is also
 * accepted. Values are clamped to a safe, bounded ring size.
 */
export function historyCapacityForNode(node, fallback = DEFAULT_HISTORY_POINTS) {
  const history = node?.history;
  let raw;
  if (typeof history === 'number') raw = history;
  else if (typeof history === 'string' && history.trim()) raw = history;
  else if (history && typeof history === 'object') {
    raw = history.points ?? history.window ?? history.capacity ?? history.limit;
  }
  if (raw === undefined || raw === null || raw === '') {
    raw = node?.historyPoints ?? node?.history_points;
  }

  const number = Number(raw);
  if (!Number.isFinite(number) || number <= 0) {
    return Math.max(1, Math.min(MAX_HISTORY_POINTS, Math.floor(Number(fallback) || DEFAULT_HISTORY_POINTS)));
  }
  return Math.max(1, Math.min(MAX_HISTORY_POINTS, Math.floor(number)));
}

/**
 * Choose between the SVG sparkline/gauge renderers and the legacy fallback.
 */
export function widgetRenderKind(
  node,
  { hasGaugeModule = false, hasSparklineModule = false } = {},
) {
  const type = typeof node === 'string' ? node : node?.type;
  if (hasSparklineModule && sparklineModeForType(type, typeof node === 'string' ? null : node)) {
    return 'sparkline';
  }
  if (hasGaugeModule && gaugeModeForType(type)) return 'gauge';
  return 'fallback';
}

/* ------------------------------------------------------------------------- */
/* Status, stats, and log                                                     */
/* ------------------------------------------------------------------------- */

function connectionActive() {
  const ws = state.ws;
  return Boolean(
    ws && (ws.readyState === WebSocket.CONNECTING || ws.readyState === WebSocket.OPEN),
  );
}

function updateConnectButton() {
  els.connect.textContent = connectionActive() ? 'Disconnect' : 'Connect';
}

function setStatus(kind, text, detail) {
  els.status.dataset.state = kind;
  els.statusText.textContent = text;
  if (detail !== undefined) els.connectionDetail.textContent = detail;
  updateConnectButton();
}

function updateStats() {
  const frames = `${state.frameCount} frame${state.frameCount === 1 ? '' : 's'}`;
  const updates = `${state.updateCount} update${state.updateCount === 1 ? '' : 's'}`;
  const last = state.lastUpdate ? ` · last ${state.lastUpdate.toLocaleTimeString()}` : '';
  els.stats.textContent = `${frames} · ${updates}${last}`;
}

function log(message, level = 'info') {
  const item = document.createElement('li');
  item.className = `log-entry log-${level}`;

  const time = document.createElement('time');
  time.textContent = new Date().toLocaleTimeString();

  const text = document.createElement('span');
  text.textContent = message;

  item.append(time, text);
  els.log.append(item);

  while (els.log.children.length > MAX_LOG_LINES) {
    els.log.firstElementChild.remove();
  }
  els.log.scrollTop = els.log.scrollHeight;
}

/* ------------------------------------------------------------------------- */
/* WebSocket lifecycle                                                        */
/* ------------------------------------------------------------------------- */

function clearReconnect() {
  if (state.reconnectTimer !== null) {
    clearTimeout(state.reconnectTimer);
    state.reconnectTimer = null;
  }
}

function scheduleReconnect() {
  if (state.manualClose || state.reconnectTimer !== null) return;
  const attempt = state.reconnectAttempt;
  const delay = Math.min(30_000, 1_000 * 2 ** Math.min(attempt, 5)) + Math.random() * 250;
  state.reconnectAttempt += 1;
  setStatus('reconnecting', `Retrying in ${Math.ceil(delay / 1000)}s`, els.server.value);
  log(`Disconnected; retrying in ${Math.ceil(delay / 1000)}s`, 'warn');
  state.reconnectTimer = setTimeout(() => {
    state.reconnectTimer = null;
    connect();
  }, delay);
}

function connect() {
  const url = normalizeServer(els.server.value);
  if (!url) {
    setStatus('error', 'Invalid address', 'Enter a ws:// or wss:// address');
    log('Cannot connect: enter a ws:// or wss:// address', 'error');
    return;
  }

  clearReconnect();
  state.manualClose = false;
  els.server.value = url;
  storageSet(SERVER_STORAGE_KEY, url);
  updateUrl(url);

  if (state.ws) {
    try {
      state.ws.close(1000, 'reconnecting');
    } catch {
      // The old socket may already be closed.
    }
  }

  let ws;
  try {
    ws = new WebSocket(url);
  } catch (error) {
    setStatus('error', 'Connection failed', error.message);
    log(`WebSocket construction failed: ${error.message}`, 'error');
    scheduleReconnect();
    return;
  }

  state.ws = ws;
  ws.binaryType = 'arraybuffer';
  setStatus('connecting', 'Connecting…', url);
  log(`Connecting to ${url}`);
  updateConnectButton();

  ws.addEventListener('open', () => {
    if (state.ws !== ws) return;
    state.connected = true;
    state.reconnectAttempt = 0;
    setStatus('connected', 'Connected', `${url} · waiting for definition`);
    log('WebSocket connected; registering protobuf display', 'ok');
    sendRegistration(ws);
    updateConnectButton();
  });

  ws.addEventListener('message', (event) => {
    if (state.ws !== ws) return;
    handleMessage(event.data).catch((error) => {
      log(`Message handling failed: ${error.message}`, 'error');
      console.error(error);
    });
  });

  ws.addEventListener('close', (event) => {
    if (state.ws !== ws) return;
    state.connected = false;
    state.ws = null;
    const reason = event.reason ? ` (${event.reason})` : '';
    setStatus('offline', 'Disconnected', `${url}${reason}`);
    log(`WebSocket closed (${event.code})${reason}`, event.wasClean ? 'warn' : 'error');
    updateConnectButton();
    if (!state.manualClose) scheduleReconnect();
  });

  ws.addEventListener('error', () => {
    if (state.ws !== ws) return;
    log('WebSocket error', 'error');
  });
}

function disconnect() {
  state.manualClose = true;
  clearReconnect();
  const ws = state.ws;
  state.ws = null;
  state.connected = false;
  if (ws) {
    try {
      ws.close(1000, 'client disconnect');
    } catch {
      // Ignore double-close.
    }
  }
  setStatus('offline', 'Offline', els.server.value);
  log('Disconnected by user', 'warn');
}

function sendRegistration(ws) {
  const width = Math.max(1, Math.round(window.innerWidth));
  const height = Math.max(1, Math.round(window.innerHeight));
  const registration = {
    type: 'register',
    device_id: state.deviceId,
    name: 'Browser Display',
    width,
    height,
    protocol: 'protobuf',
    wire_format: 'protobuf',
    capabilities: [
      'label',
      'text',
      'number',
      'gauge',
      'bar',
      'vu_meter',
      'sparkline',
      'chart',
      'history',
      'icon',
      'protobuf',
    ],
  };
  ws.send(JSON.stringify(registration));
  log(`Registered ${state.deviceId} (${width}×${height}, protobuf)`);
}

async function handleMessage(data) {
  if (typeof data === 'string') {
    handleText(data);
    return;
  }

  let buffer = null;
  if (data instanceof ArrayBuffer) {
    buffer = new Uint8Array(data);
  } else if (ArrayBuffer.isView(data)) {
    buffer = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  } else if (typeof Blob !== 'undefined' && data instanceof Blob) {
    buffer = new Uint8Array(await data.arrayBuffer());
  }

  if (!buffer) {
    log('Ignoring a message with an unsupported WebSocket data type', 'warn');
    return;
  }
  handleBinary(buffer);
}

function handleText(text) {
  let message;
  try {
    message = JSON.parse(text);
  } catch {
    log('Ignoring malformed JSON message from controller', 'warn');
    return;
  }
  if (!message || typeof message !== 'object') return;

  switch (message.type) {
    case 'registered':
      setStatus(
        'connected',
        'Connected',
        `${els.server.value} · ${message.protocol || message.wire_format || 'protobuf'} · ${message.layout_id || 'no layout'}`,
      );
      log(`Registered by controller as ${message.device_id || state.deviceId}`);
      break;
    case 'layout':
    case 'data':
      log(`Ignoring legacy JSON ${message.type} frame; reconnect with protocol=protobuf`, 'warn');
      break;
    case 'scenario':
      log(`Scenario: ${message.name || message.id || 'unknown'}`);
      break;
    default:
      log(`Controller message: ${message.type || 'unnamed'}`);
      break;
  }
}

function handleBinary(buffer) {
  state.frameCount += 1;
  let frame;
  try {
    frame = decodeFrame(buffer);
  } catch (error) {
    log(`Frame decode failed: ${error.message}`, 'error');
    updateStats();
    return;
  }

  if (frame.tag === TAG_DEFINITION) {
    const definition = frame.definition;
    renderDefinition(definition);
    const json = definition.definition || {};
    const fieldCount = Array.isArray(json.fields) ? json.fields.length : 0;
    const actionCount = Array.isArray(json.actions) ? json.actions.length : 0;
    log(
      `Definition ${definition.id || '(unnamed)'} v${definition.version} — ${fieldCount} fields, ${actionCount} actions`,
      'ok',
    );
  } else if (frame.tag === TAG_DATA_UPDATE) {
    applyDataUpdate(frame.dataUpdate);
  } else {
    log(`Ignoring ${frame.kind || 'unknown'} frame (tag ${frame.tag})`, 'warn');
  }

  updateStats();
}

/* ------------------------------------------------------------------------- */
/* Definition rendering                                                       */
/* ------------------------------------------------------------------------- */

function normaliseFields(value) {
  if (Array.isArray(value)) {
    return value.filter((field) => field && typeof field === 'object');
  }
  if (value && typeof value === 'object') {
    return Object.entries(value).map(([key, field], index) => ({
      ...(field && typeof field === 'object' ? field : {}),
      key: field?.key || key,
      id: field?.id ?? index + 1,
    }));
  }
  return [];
}

function normaliseActions(value) {
  if (Array.isArray(value)) {
    return value.filter((action) => action && typeof action === 'object');
  }
  if (value && typeof value === 'object') {
    return Object.entries(value).map(([key, action], index) => ({
      ...(action && typeof action === 'object' ? action : {}),
      key: action?.key || key,
      id: action?.id ?? index + 1,
    }));
  }
  return [];
}

function renderDefinition(definition) {
  const documentData = definition.definition || {};
  state.definition = definition;
  state.document = documentData;

  state.fieldById.clear();
  state.fieldByKey.clear();
  state.values.clear();
  state.valuesByKey.clear();
  state.historyByField.clear();
  state.bindings = [];
  state.textBindings = [];
  state.fieldRows.clear();
  state.actionsById.clear();
  state.actionsByKey.clear();

  const fields = normaliseFields(documentData.fields);
  for (const raw of fields) {
    const id = Number(raw.id);
    if (!Number.isFinite(id)) continue;
    const field = { ...raw, id, key: String(raw.key ?? raw.id ?? id) };
    state.fieldById.set(id, field);
    state.fieldByKey.set(field.key, field);
  }

  const actions = normaliseActions(documentData.actions);
  for (const raw of actions) {
    const id = Number(raw.id);
    if (!Number.isFinite(id)) continue;
    const action = { ...raw, id, key: String(raw.key ?? raw.id ?? id) };
    state.actionsById.set(id, action);
    state.actionsByKey.set(action.key, action);
  }

  const layout = documentData.layout && typeof documentData.layout === 'object'
    ? documentData.layout
    : documentData;
  const title = layout.name || documentData.name || definition.id || 'Display';

  els.definitionMeta.textContent =
    `${definition.id || '(unnamed)'} · v${definition.version} · ${fields.length} fields · ${actions.length} actions`;
  els.screen.innerHTML = '';
  els.screen.hidden = false;
  els.empty.hidden = true;
  els.screen.style.background = layout.background || '';
  document.title = `${title} · Ozma Display`;

  const root = renderNode(layout);
  if (root) {
    els.screen.append(root);
  } else {
    const message = document.createElement('p');
    message.className = 'muted';
    message.textContent = 'The definition did not contain any renderable widgets.';
    els.screen.append(message);
  }

  renderFieldTable(fields);
  updateTextBindings();
  updateStats();
}

function renderNode(node) {
  if (!node || typeof node !== 'object') return null;

  const type = String(node.type || '').toLowerCase();
  const children = Array.isArray(node.children)
    ? node.children
    : Array.isArray(node.widgets)
      ? node.widgets
      : null;
  const container = children !== null && (
    CONTAINER_TYPES.has(type) || !WIDGET_TYPES.has(type)
  );

  if (container) {
    const element = document.createElement('section');
    element.className = `node node-container node-${cssToken(type || 'container')}`;
    if (type) element.dataset.nodeType = type;

    const heading = node.title || node.label || node.name;
    if (heading && typeof heading === 'string') {
      const headingElement = document.createElement('h3');
      headingElement.className = 'node-heading';
      headingElement.textContent = heading;
      element.append(headingElement);
    }

    for (const child of children) {
      const childElement = renderNode(child);
      if (childElement) element.append(childElement);
    }
    return element;
  }

  return renderWidget(node);
}

function renderWidget(node) {
  const type = String(node.type || 'widget').toLowerCase();
  const field = resolveField(node);
  const action = resolveAction(node);
  const renderKind = widgetRenderKind(node, {
    hasGaugeModule: Boolean(state.gaugeModule),
    hasSparklineModule: Boolean(state.sparklineModule),
  });
  const sparklineMode = renderKind === 'sparkline' ? sparklineModeForType(type, node) : null;
  const gaugeMode = renderKind === 'gauge' ? gaugeModeForType(type) : null;

  const card = document.createElement('article');
  card.className = `widget widget-${cssToken(type)}`;
  if (type) card.dataset.widgetType = type;

  const headingText = widgetHeading(node, type, field);
  if (headingText) {
    const heading = document.createElement('div');
    heading.className = 'widget-label';
    heading.textContent = headingText;
    card.append(heading);
  }

  if (sparklineMode) {
    const host = document.createElement('div');
    host.className = 'sparkline-host';
    host.dataset.sparkline = sparklineMode;
    try {
      mountSparkline(host, node, field, sparklineMode, headingText);
      card.classList.add('widget-meter', 'widget-sparkline-svg');
      card.append(host);
      if (action) attachAction(card, action, node);
      return card;
    } catch {
      // Fall through to the existing card/meter renderer.
    }
  }

  if (gaugeMode) {
    const host = document.createElement('div');
    host.className = 'gauge-host';
    host.dataset.gauge = gaugeMode;
    try {
      mountGauge(host, node, field, gaugeMode, headingText);
      card.classList.add('widget-meter', 'widget-gauge-svg');
      card.append(host);
      if (action) attachAction(card, action, node);
      return card;
    } catch {
      // Fall through to the existing card/meter renderer.
    }
  }

  const value = document.createElement('div');
  value.className = 'widget-value';
  card.append(value);

  let track = null;
  let fill = null;

  if (type === 'gauge' || type === 'bar' || type === 'vu_meter' || type === 'sparkline') {
    card.classList.add('widget-meter');
    track = document.createElement('div');
    track.className = 'meter-track';
    fill = document.createElement('div');
    fill.className = 'meter-fill';
    track.append(fill);
    card.append(track);
    value.classList.add('meter-value');
  } else if (type === 'number') {
    value.classList.add('number-value');
  } else if (type === 'label' || type === 'text') {
    value.classList.add('label-value');
  } else if (type === 'icon' || type === 'toggle' || type === 'switch') {
    value.classList.add('boolean-value');
  }

  if (type === 'label' || type === 'text') {
    const template = node.text ?? node.value ?? '';
    if (typeof template === 'string' && template && hasPlaceholders(template)) {
      registerTextBinding(value, template);
      if (field) bindValue(field, node, value, fill, card);
    } else if (template !== '' && template !== null && template !== undefined) {
      value.textContent = String(template);
      if (field) bindValue(field, node, value, fill, card);
    } else if (field) {
      bindValue(field, node, value, fill, card);
    } else {
      value.textContent = node.metric ? String(node.metric) : '—';
    }
  } else if (field) {
    bindValue(field, node, value, fill, card);
  } else {
    value.textContent = node.text ?? node.label ?? node.name ?? '—';
    if (!fill && node.value !== undefined) value.textContent = formatValue(node.value);
  }

  if (action) attachAction(card, action, node);
  return card;
}

function gaugeConfigForNode(node, field, mode, label) {
  return {
    mode,
    min: node.min,
    max: node.max,
    warn: node.warn ?? node.warn_at ?? node.warnAt,
    crit: node.crit ?? node.critical ?? node.crit_at ?? node.critAt,
    value: node.value,
    unit: node.unit ?? field?.unit,
    decimals: node.decimals,
    label: label || node.label || node.title || node.name || '',
    showLabel: false,
    width: node.width,
    height: node.height,
    thickness: node.thickness,
    tickCount: node.tickCount ?? node.tick_count,
    animate: node.animate,
  };
}

function mountGauge(host, node, field, mode, label) {
  const gauge = state.gaugeModule;
  if (!gauge || typeof gauge.renderGauge !== 'function') {
    throw new Error('gauge module unavailable');
  }

  const config = gaugeConfigForNode(node, field, mode, label);
  const tokens = state.themeTokens || {};
  gaugeSequence += 1;
  const options = { id: node.id || node.key || `${mode}-${gaugeSequence}` };
  const initial = field ? state.values.get(field.id) : node.value;
  gauge.renderGauge(host, config, initial, tokens, options);

  if (!field || field.id === undefined || field.id === null) return;

  const apply = (value) => updateGauge(host, config, value, tokens, options);
  state.bindings.push({ fieldId: field.id, apply });
  apply(initial);
}

function updateGauge(host, config, value, tokens, options) {
  const gauge = state.gaugeModule;
  const svg = host.querySelector('svg.ozma-gauge');
  if (!gauge) return;
  if (!svg) {
    gauge.renderGauge(host, config, value, tokens, options);
    return;
  }

  const geometry = config.mode === 'bar'
    ? gauge.barGeometry(config, value, tokens)
    : gauge.gaugeGeometry(config, value, tokens);
  const formatted = gauge.formatGaugeValue(geometry.value, geometry);
  const unit = geometry.unit;
  const severity = geometry.severity;
  const reading = `${formatted}${unit ? ` ${unit}` : ''}`;
  const valueElement = svg.querySelector('.ozma-gauge-value');

  if (valueElement) {
    if (config.mode === 'bar') {
      valueElement.setAttribute('width', String(geometry.fillWidth));
      valueElement.setAttribute('fill', geometry.colors[severity]);
    } else {
      valueElement.setAttribute('d', gauge.arcPath({
        cx: geometry.cx,
        cy: geometry.cy,
        radius: geometry.radius,
        startAngle: geometry.startAngle,
        endAngle: geometry.valueAngle,
      }));
      valueElement.setAttribute('stroke', geometry.colors[severity]);
    }
    valueElement.setAttribute('data-severity', severity);
  }

  const texts = svg.querySelectorAll('text');
  if (texts.length && geometry.showValue !== false) {
    texts[texts.length - 1].textContent = reading;
  }

  let aria = [geometry.label, reading].filter(Boolean).join(' ').trim() || `Gauge ${reading}`;
  if (severity === 'crit') aria = `${aria}, critical`;
  else if (severity === 'warn') aria = `${aria}, warning`;
  svg.setAttribute('aria-label', aria);

  const desc = svg.querySelector('desc');
  if (desc) {
    const severityWord = severity === 'crit' ? 'critical' : severity === 'warn' ? 'warning' : 'normal';
    desc.textContent = `${geometry.label || 'Gauge'} reading ${reading}; range ${gauge.formatGaugeValue(geometry.min, geometry)} to ${gauge.formatGaugeValue(geometry.max, geometry)}${unit ? ` ${unit}` : ''}; ${severityWord}.`;
  }
}

function historySample(value) {
  if (value === undefined || value === null || value === '') return null;
  if (typeof value === 'boolean') return value ? 1 : 0;
  if (typeof value === 'number') return Number.isFinite(value) ? value : null;
  if (typeof value === 'bigint') {
    const number = Number(value);
    return Number.isFinite(number) ? number : null;
  }
  return null;
}

function ensureHistoryRing(fieldId, capacity) {
  let ring = state.historyByField.get(fieldId);
  if (!ring) {
    ring = { capacity: null, samples: [] };
    state.historyByField.set(fieldId, ring);
  }

  const requested = Number(capacity);
  if (Number.isFinite(requested) && requested > 0) {
    const bounded = Math.max(1, Math.min(MAX_HISTORY_POINTS, Math.floor(requested)));
    ring.capacity = ring.capacity === null ? bounded : Math.max(ring.capacity, bounded);
  }
  if (ring.capacity === null) ring.capacity = DEFAULT_HISTORY_POINTS;
  if (ring.samples.length > ring.capacity) {
    ring.samples = ring.samples.slice(-ring.capacity);
  }
  return ring;
}

function historyForField(fieldId) {
  return state.historyByField.get(fieldId)?.samples ?? [];
}

function recordHistory(fieldId, value) {
  if (fieldId === undefined || fieldId === null) return;
  const ring = ensureHistoryRing(fieldId);
  ring.samples.push(historySample(value));
  if (ring.samples.length > ring.capacity) {
    ring.samples.splice(0, ring.samples.length - ring.capacity);
  }
}

function sparklineConfigForNode(node, field, mode, label) {
  return {
    mode,
    min: node.min,
    max: node.max,
    warn: node.warn ?? node.warn_at ?? node.warnAt,
    crit: node.crit ?? node.critical ?? node.crit_at ?? node.critAt,
    value: node.value,
    unit: node.unit ?? field?.unit,
    decimals: node.decimals,
    label: label || node.label || node.title || node.name || '',
    width: node.width,
    height: node.height,
    showValue: node.showValue ?? node.show_value,
    area: node.area ?? node.areaFill ?? node.area_fill,
    fill: node.fill,
    baseline: node.baseline,
    ticks: node.ticks ?? node.tickCount ?? node.tick_count,
    showTicks: node.showTicks ?? node.show_ticks,
    showTickLabels: node.showTickLabels ?? node.tickLabels,
    lineWidth: node.lineWidth ?? node.line_width ?? node.strokeWidth ?? node.stroke_width,
    mark: node.mark ?? node.chartMark ?? node.chart_mark ?? node.chart,
    animate: node.animate,
    history: node.history,
  };
}

function mountSparkline(host, node, field, mode, label) {
  const sparkline = state.sparklineModule;
  if (!sparkline || typeof sparkline.renderSparkline !== 'function') {
    throw new Error('sparkline module unavailable');
  }

  const config = sparklineConfigForNode(node, field, mode, label);
  const tokens = state.themeTokens || {};
  const capacity = historyCapacityForNode(node);
  const fieldId = field?.id;
  let history = [];

  if (fieldId !== undefined && fieldId !== null) {
    ensureHistoryRing(fieldId, capacity);
    history = historyForField(fieldId);
  } else if (Array.isArray(node.history)) {
    history = node.history;
  }

  sparklineSequence += 1;
  const options = { id: node.id || node.key || `${mode}-${sparklineSequence}` };
  const svg = sparkline.renderSparkline(host, config, history, tokens, options);

  if (fieldId === undefined || fieldId === null) return svg;

  const apply = () => {
    const current = historyForField(fieldId);
    if (typeof sparkline.updateSparkline === 'function') {
      sparkline.updateSparkline(host, current, config, tokens, options);
    } else {
      sparkline.renderSparkline(host, config, current, tokens, options);
    }
  };
  state.bindings.push({ fieldId, apply });
  if (history.length) apply();
  return svg;
}

function widgetHeading(node, type, field) {
  if (node.label !== undefined && node.label !== null && node.label !== '') {
    return String(node.label);
  }
  if (node.title !== undefined && node.title !== null && node.title !== '') {
    return String(node.title);
  }
  if (node.name !== undefined && node.name !== null && node.name !== '') {
    return String(node.name);
  }
  if (field?.key) return field.key;
  if (node.metric) return String(node.metric);
  if (type === 'label' || type === 'text') return '';
  return type ? type.replace(/_/g, ' ') : '';
}

function resolveField(node) {
  const candidates = [
    node.field,
    node.field_id,
    node.fieldId,
    node.metric,
    node.metric_key,
    node.metricKey,
    node.binding,
    node.data_key,
    node.dataKey,
  ];

  for (const candidate of candidates) {
    if (candidate === undefined || candidate === null || candidate === '') continue;

    if (typeof candidate === 'number') {
      return state.fieldById.get(candidate) || { id: candidate, key: String(candidate) };
    }

    const text = String(candidate).trim();
    if (!text) continue;
    const byKey = state.fieldByKey.get(text);
    if (byKey) return byKey;

    if (/^\d+$/.test(text)) {
      const id = Number(text);
      const byId = state.fieldById.get(id);
      if (byId) return byId;
      return { id, key: text };
    }
    return { id: null, key: text };
  }

  return null;
}

function resolveAction(node) {
  const candidates = [
    node.action_id,
    node.actionId,
    node.action,
    node.tap_action,
    node.press_action,
    node.on_press,
    node.on_tap,
    node.on_click,
    node.activate_action,
  ];

  for (const candidate of candidates) {
    if (candidate === undefined || candidate === null || candidate === '') continue;

    if (typeof candidate === 'number' || (typeof candidate === 'string' && /^\d+$/.test(candidate))) {
      const id = Number(candidate);
      if (id > 0) return { id, action: state.actionsById.get(id) || null };
      continue;
    }

    if (typeof candidate === 'string') {
      const action = state.actionsByKey.get(candidate.trim());
      if (action) return { id: action.id, action };
    }
  }

  return null;
}

function bindValue(field, node, valueElement, fillElement, card) {
  if (!field || field.id === undefined || field.id === null) return;

  const apply = (value) => {
    valueElement.textContent = formatWidgetValue(value, field, node);
    if (fillElement) {
      if (value === undefined || value === null) {
        fillElement.style.width = '0%';
      } else if (typeof value === 'boolean') {
        fillElement.style.width = value ? '100%' : '0%';
      } else {
        const numeric = Number(value);
        fillElement.style.width = Number.isFinite(numeric)
          ? `${clampPercent(numeric, node.min, node.max)}%`
          : '0%';
      }
    }
    card.classList.toggle('value-true', value === true);
    card.classList.toggle('value-false', value === false);
  };

  state.bindings.push({ fieldId: field.id, apply });
  apply(state.values.get(field.id));
}

function registerTextBinding(element, template) {
  state.textBindings.push({ element, template: String(template) });
}

function updateTextBindings() {
  for (const binding of state.textBindings) {
    binding.element.textContent = binding.template.replace(/\{([^{}]+)\}/g, (match, rawKey) => {
      const key = rawKey.trim();
      const field = state.fieldByKey.get(key);
      const value = field ? state.values.get(field.id) : state.valuesByKey.get(key);
      return value === undefined ? match : formatValue(value);
    });
  }
}

function actionParams(node) {
  const raw = node.params ?? node.action_params ?? node.actionParams ?? {};
  if (typeof raw === 'string') {
    try {
      return JSON.parse(raw);
    } catch {
      return { value: raw };
    }
  }
  if (raw && typeof raw === 'object' && !Array.isArray(raw) && !(raw instanceof Uint8Array)) {
    return raw;
  }
  return {};
}

function attachAction(card, action, node) {
  card.classList.add('has-action');
  card.tabIndex = 0;
  card.setAttribute('role', 'button');
  card.setAttribute(
    'aria-label',
    `${action.action?.label || action.action?.key || 'Action'} ${action.id}`,
  );

  const invoke = (event) => {
    event.preventDefault();
    sendAction(action.id, actionParams(node));
  };

  card.addEventListener('click', invoke);
  card.addEventListener('keydown', (event) => {
    if (event.key === 'Enter' || event.key === ' ') {
      invoke(event);
    }
  });

  const hint = document.createElement('span');
  hint.className = 'widget-action';
  hint.textContent = action.action?.label || action.action?.key || `Action ${action.id}`;
  card.append(hint);
}

function nextIdempotencyKey() {
  actionSequence += 1n;
  return (BigInt(Date.now()) << 16n) | (actionSequence & 0xffffn);
}

function sendAction(actionId, params) {
  const ws = state.ws;
  if (!ws || ws.readyState !== WebSocket.OPEN) {
    log('Cannot invoke action: WebSocket is not connected', 'warn');
    return;
  }

  const payload = encodeActionInvoke({
    actionId,
    params,
    idempotencyKey: nextIdempotencyKey(),
  });
  ws.send(encodeFrame(TAG_ACTION_INVOKE, payload));
  log(`→ ActionInvoke #${actionId} ${JSON.stringify(params)}`, 'action');
}

/* ------------------------------------------------------------------------- */
/* Data updates and fallback field table                                      */
/* ------------------------------------------------------------------------- */

function applyDataUpdate(update) {
  if (!update || !Array.isArray(update.values)) return;
  state.updateCount += 1;

  for (const item of update.values) {
    state.values.set(item.fieldId, item.value);
    const field = state.fieldById.get(item.fieldId);
    if (field) state.valuesByKey.set(field.key, item.value);
    recordHistory(item.fieldId, item.value);
    updateFieldRow(item.fieldId, item.value);
  }

  for (const binding of state.bindings) {
    if (state.values.has(binding.fieldId)) {
      binding.apply(state.values.get(binding.fieldId));
    }
  }

  updateTextBindings();
  state.lastUpdate = new Date();
}

function renderFieldTable(fields) {
  els.fieldsBody.innerHTML = '';
  state.fieldRows.clear();

  if (!fields.length) {
    els.fieldsTable.hidden = true;
    els.fieldsEmpty.hidden = false;
    return;
  }

  els.fieldsEmpty.hidden = true;
  els.fieldsTable.hidden = false;
  for (const field of fields) {
    appendFieldRow(field);
  }
}

function appendFieldRow(field) {
  const id = Number(field.id);
  if (!Number.isFinite(id) || state.fieldRows.has(id)) return;

  const row = document.createElement('tr');
  row.dataset.fieldId = String(id);

  const keyCell = document.createElement('td');
  keyCell.className = 'field-key';
  keyCell.textContent = field.key || `field ${id}`;

  const idCell = document.createElement('td');
  idCell.className = 'field-id';
  idCell.textContent = String(id);

  const typeCell = document.createElement('td');
  typeCell.className = 'field-type';
  typeCell.textContent = field.unit ? `${field.type || 'unknown'}${field.unit ? ` · ${field.unit}` : ''}` : (field.type || 'unknown');

  const valueCell = document.createElement('td');
  valueCell.className = 'field-value';
  valueCell.textContent = formatFieldTableValue(state.values.get(id), field);

  row.append(keyCell, idCell, typeCell, valueCell);
  els.fieldsBody.append(row);
  state.fieldRows.set(id, { row, valueCell, field });

  els.fieldsEmpty.hidden = true;
  els.fieldsTable.hidden = false;
}

function updateFieldRow(fieldId, value) {
  let row = state.fieldRows.get(fieldId);
  if (!row) {
    const field = state.fieldById.get(fieldId) || {
      id: fieldId,
      key: `field ${fieldId}`,
      type: 'unknown',
    };
    appendFieldRow(field);
    row = state.fieldRows.get(fieldId);
  }
  if (!row) return;
  row.valueCell.textContent = formatFieldTableValue(value, row.field);
}

function formatFieldTableValue(value, field) {
  if (value === undefined) return '—';
  if (field?.type === 'timestamp' && (typeof value === 'number' || typeof value === 'bigint')) {
    const date = new Date(Number(value) / 1000);
    if (!Number.isNaN(date.getTime())) return date.toLocaleString();
  }
  return formatValue(value);
}

/* ------------------------------------------------------------------------- */
/* Optional module bootstrap                                                  */
/* ------------------------------------------------------------------------- */

async function importOptional(url) {
  try {
    return await import(url);
  } catch {
    return null;
  }
}

function loadThemeModule() {
  if (typeof window !== 'undefined') {
    // client.js owns startup bootstrapping, so stop theme.js from fetching the
    // selected theme a second time through its automatic entry point.
    window.__ozmaThemeBooted = true;
  }
  return importOptional(THEME_MODULE_URL);
}

async function applySelectedTheme(themeModule) {
  const doc = typeof document !== 'undefined' ? document : null;
  if (!doc || !themeModule || typeof themeModule.resolveThemeUrl !== 'function') return null;

  const selection = selectThemeSelection({
    search: typeof window !== 'undefined' ? window.location?.search || '' : '',
    dataTheme: doc.documentElement?.dataset?.theme || doc.body?.dataset?.theme || '',
    hasThemeModule: true,
  });
  if (!selection) return null;

  try {
    const baseUrl = doc.baseURI || (typeof window !== 'undefined' ? window.location?.href : undefined);
    const source = themeModule.resolveThemeUrl(selection.value, { baseUrl });
    const theme = await themeModule.loadTheme(source);
    themeModule.applyTheme(theme, doc);
    state.theme = theme;
    state.themeTokens = typeof themeModule.resolveThemeTokens === 'function'
      ? themeModule.resolveThemeTokens(theme)
      : {};
    state.themeCanvas = theme.canvas || null;
    log(`Theme loaded: ${theme.name || theme.id || selection.value}`, 'ok');
    return theme;
  } catch (error) {
    const code = error?.code || 'THEME_BOOT';
    if (doc.documentElement?.dataset) {
      doc.documentElement.dataset.ozmaThemeError = code;
    }
    log(`Theme not applied (${code}): ${error?.message || error}`, 'warn');
    return null;
  }
}

async function bootstrapOptionalModules() {
  const [themeModule, gaugeModule, sparklineModule] = await Promise.all([
    loadThemeModule(),
    importOptional(GAUGE_MODULE_URL),
    importOptional(SPARKLINE_MODULE_URL),
  ]);
  if (gaugeModule) state.gaugeModule = gaugeModule;
  if (sparklineModule) state.sparklineModule = sparklineModule;
  if (themeModule) {
    state.themeModule = themeModule;
    await applySelectedTheme(themeModule);
  }
}

/* ------------------------------------------------------------------------- */
/* Boot                                                                       */
/* ------------------------------------------------------------------------- */

function bootAdapter() {
  const initialServer =
    new URLSearchParams(window.location.search).get('server') ||
    storageGet(SERVER_STORAGE_KEY) ||
    DEFAULT_SERVER;

  els.server.value = initialServer;
  els.form.addEventListener('submit', (event) => {
    event.preventDefault();
    if (connectionActive()) disconnect();
    else connect();
  });
  els.clearLog.addEventListener('click', () => {
    els.log.innerHTML = '';
    log('Log cleared');
  });
  window.addEventListener('beforeunload', () => {
    state.manualClose = true;
    clearReconnect();
    if (state.ws) {
      try {
        state.ws.close(1000, 'page unload');
      } catch {
        // Ignore.
      }
    }
  });

  updateStats();
  updateConnectButton();
  log('Browser Display adapter ready');
  log(`Using device id ${state.deviceId}`);
  bootstrapOptionalModules().catch((error) => {
    log(`Optional modules unavailable: ${error?.message || error}`, 'warn');
  });
  if (new URLSearchParams(window.location.search).get('autoconnect') !== '0') {
    connect();
  }
}

if (typeof window !== 'undefined' && typeof document !== 'undefined') {
  bootAdapter();
}
