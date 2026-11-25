// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Ozma layout editor.
 *
 * A dependency-free, offline editor for `ScreenLayout` documents.  The module
 * is split into a pure, testable model (the exported functions below) and a
 * browser-only controller that wires the model to the static page.
 *
 * Only `ScreenLayout` documents are saved.  Imported unknown fields, top-level
 * `extensions`, and unknown widget keys are preserved; they are never
 * interpreted as code.
 */

export const WIDGET_KINDS = Object.freeze([
  'gauge',
  'bar',
  'number',
  'text',
  'sparkline',
  'vu_meter',
  'status',
  'icon',
  'table',
  'card',
]);

export const DEFAULT_GRID = 10;
export const MIN_GRID = 1;
export const MAX_GRID = 200;
export const MIN_ZOOM = 0.25;
export const MAX_ZOOM = 2;
export const DEFAULT_HISTORY_LIMIT = 100;

const INTERNAL_ID = Symbol('ozma.editor.widgetId');
const DEFAULT_LAYOUT_WIDTH = 480;
const DEFAULT_LAYOUT_HEIGHT = 480;
const DEFAULT_BACKGROUND = '#0a0a0f';
const DEFAULT_ACCENT = '#5b6fff';
const MAX_TEXT_LENGTH = 500;

const BASE_WIDGET = Object.freeze({
  x: 0,
  y: 0,
  w: 120,
  h: 80,
  label: '',
  metric: '',
  unit: '',
  min: 0,
  max: 100,
  warn: 70,
  crit: 90,
  color: DEFAULT_ACCENT,
  decimals: 1,
});

const KIND_DEFAULTS = Object.freeze({
  gauge: Object.freeze({
    w: 180,
    h: 180,
    label: 'CPU',
    unit: '%',
    decimals: 0,
  }),
  bar: Object.freeze({
    w: 320,
    h: 56,
    label: 'Memory',
    unit: '%',
    decimals: 0,
  }),
  number: Object.freeze({
    w: 160,
    h: 80,
    label: 'Value',
    unit: '',
    decimals: 1,
  }),
  text: Object.freeze({
    w: 220,
    h: 48,
    text: 'Text',
    decimals: 0,
  }),
  sparkline: Object.freeze({
    w: 240,
    h: 64,
    label: 'History',
    history_points: 120,
    history_seconds: 60,
    fill: false,
    decimals: 1,
  }),
  vu_meter: Object.freeze({
    w: 320,
    h: 96,
    label: 'Level',
    unit: 'dB',
    min: -60,
    max: 0,
    warn: -12,
    crit: -3,
    decimals: 1,
    orientation: 'horizontal',
    segments: 24,
    attack_ms: 10,
    release_ms: 300,
    falloff_db_per_sec: 24,
    peak_hold_ms: 1500,
    peak_falloff_db_per_sec: 12,
  }),
  status: Object.freeze({
    w: 160,
    h: 64,
    label: 'Status',
    decimals: 0,
  }),
  icon: Object.freeze({
    w: 64,
    h: 64,
    icon: '',
    decimals: 0,
  }),
  table: Object.freeze({
    w: 320,
    h: 160,
    label: 'Table',
    decimals: 0,
  }),
  card: Object.freeze({
    w: 240,
    h: 120,
    label: 'Card',
    decimals: 0,
  }),
});

const TEXT_PATCH_KEYS = new Set(['label', 'metric', 'unit', 'text', 'icon']);
const NUMBER_PATCH_LIMITS = Object.freeze({
  min: [-1e9, 1e9],
  max: [-1e9, 1e9],
  warn: [-1e9, 1e9],
  crit: [-1e9, 1e9],
  decimals: [0, 6],
  history_points: [1, 4096],
  history_seconds: [0, 3600],
  font_size: [6, 200],
  attack_ms: [0, 60000],
  release_ms: [0, 60000],
  falloff_db_per_sec: [0, 1000],
  peak_hold_ms: [0, 60000],
  peak_falloff_db_per_sec: [0, 1000],
  segments: [1, 128],
});
const ALLOWED_PATCH_KEYS = new Set([
  'type',
  'label',
  'metric',
  'unit',
  'text',
  'icon',
  'color',
  'min',
  'max',
  'warn',
  'crit',
  'decimals',
  'history_points',
  'history_seconds',
  'fill',
  'font_size',
  'orientation',
  'segments',
  'attack_ms',
  'release_ms',
  'falloff_db_per_sec',
  'peak_hold_ms',
  'peak_falloff_db_per_sec',
  'x',
  'y',
  'w',
  'h',
]);

const BLOCKED_KEYS = new Set(['__proto__', 'constructor', 'prototype']);

/* ------------------------------------------------------------------------- */
/* Small helpers                                                             */
/* ------------------------------------------------------------------------- */

function isPlainObject(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function finiteNumber(value, fallback) {
  if (value === undefined || value === null || value === '') return fallback;
  const number = Number(value);
  return Number.isFinite(number) ? number : fallback;
}

function clampNumber(value, min, max, fallback) {
  const number = finiteNumber(value, fallback);
  return Math.max(min, Math.min(max, number));
}

function clampInteger(value, min, max, fallback) {
  return Math.round(clampNumber(value, min, max, fallback));
}

function cloneJson(value) {
  return JSON.parse(JSON.stringify(value));
}

function setWidgetId(widget, id) {
  Object.defineProperty(widget, INTERNAL_ID, {
    value: id,
    enumerable: false,
    configurable: true,
    writable: true,
  });
}

function widgetId(widget) {
  return widget && typeof widget === 'object' ? widget[INTERNAL_ID] : undefined;
}

/** Return the editor's opaque widget id (not part of the serialised layout). */
export function getWidgetId(widget) {
  return widgetId(widget);
}

/**
 * Return a colour that is safe to assign to a CSS custom property.
 *
 * Only colour literals are accepted.  Values that could trigger a network load
 * (`url(...)`), expression evaluation, or other CSS functions are rejected.
 */
export function sanitizeColor(value, fallback = DEFAULT_ACCENT) {
  const text = String(value === undefined || value === null ? '' : value).trim();
  if (!text) return fallback;
  if (/^#(?:[0-9a-f]{3}|[0-9a-f]{4}|[0-9a-f]{6}|[0-9a-f]{8})$/i.test(text)) {
    return text;
  }
  if (/^(?:rgb|rgba|hsl|hsla)\(\s*[0-9.,%/\s+-]+\)$/i.test(text)) {
    return text;
  }
  if (
    /^[a-z]{3,20}$/i.test(text)
    && !/^(?:url|expression|javascript|image|image-set|element|var|calc)$/i.test(text)
  ) {
    return text.toLowerCase();
  }
  return fallback;
}

function layoutWidth(layout) {
  return finiteNumber(layout && layout.width, DEFAULT_LAYOUT_WIDTH) > 0
    ? finiteNumber(layout.width, DEFAULT_LAYOUT_WIDTH)
    : DEFAULT_LAYOUT_WIDTH;
}

function layoutHeight(layout) {
  return finiteNumber(layout && layout.height, DEFAULT_LAYOUT_HEIGHT) > 0
    ? finiteNumber(layout.height, DEFAULT_LAYOUT_HEIGHT)
    : DEFAULT_LAYOUT_HEIGHT;
}

function boundsFor(layout) {
  return { width: layoutWidth(layout), height: layoutHeight(layout) };
}

function rectOf(widget) {
  return {
    x: finiteNumber(widget && widget.x, 0),
    y: finiteNumber(widget && widget.y, 0),
    w: finiteNumber(widget && widget.w, 100),
    h: finiteNumber(widget && widget.h, 100),
  };
}

function sameRect(left, right) {
  return left.x === right.x
    && left.y === right.y
    && left.w === right.w
    && left.h === right.h;
}

function sortedEntries(object) {
  return Object.entries(object && typeof object === 'object' ? object : {});
}

/* ------------------------------------------------------------------------- */
/* Pure model                                                               */
/* ------------------------------------------------------------------------- */

/**
 * Snap and clamp a rectangle to the grid and optional canvas bounds.
 *
 * @param {{x?: number, y?: number, w?: number, h?: number}} rect
 * @param {number} [grid]
 * @param {{width?: number, height?: number}|null} [bounds]
 * @returns {{x: number, y: number, w: number, h: number}}
 */
export function snapRect(rect, grid = DEFAULT_GRID, bounds = null, boundHeight = null) {
  const step = clampInteger(grid, MIN_GRID, MAX_GRID, DEFAULT_GRID);
  if (typeof bounds === 'number' && typeof boundHeight === 'number') {
    bounds = { width: bounds, height: boundHeight };
  }
  const source = rect && typeof rect === 'object' ? rect : {};
  const snapValue = (value, fallback) => {
    const number = finiteNumber(value, fallback);
    return Math.round(number / step) * step;
  };

  let x = snapValue(source.x, 0);
  let y = snapValue(source.y, 0);
  let w = Math.max(step, snapValue(source.w, step));
  let h = Math.max(step, snapValue(source.h, step));

  const width = bounds && Number.isFinite(Number(bounds.width))
    ? Math.max(0, Number(bounds.width))
    : null;
  const height = bounds && Number.isFinite(Number(bounds.height))
    ? Math.max(0, Number(bounds.height))
    : null;

  if (width !== null) {
    w = Math.min(w, width);
    if (x + w > width) x = width - w;
    if (x < 0) x = 0;
  } else if (x < 0) {
    x = 0;
  }

  if (height !== null) {
    h = Math.min(h, height);
    if (y + h > height) y = height - h;
    if (y < 0) y = 0;
  } else if (y < 0) {
    y = 0;
  }

  return {
    x: Math.round(x),
    y: Math.round(y),
    w: Math.max(0, Math.round(w)),
    h: Math.max(0, Math.round(h)),
  };
}

/**
 * Build a fresh widget object for a known kind.
 *
 * The returned object is a plain `ScreenLayout` widget.  Overrides are shallow
 * and cannot introduce prototype keys or an internal editor identifier.
 */
export function widgetDefaults(kind = 'gauge', overrides = {}) {
  const type = String(kind || 'gauge');
  const spec = KIND_DEFAULTS[type];
  if (!spec) throw new RangeError(`Unknown widget kind: ${type}`);

  const widget = { ...BASE_WIDGET, ...spec, type };
  for (const [key, value] of sortedEntries(overrides)) {
    if (BLOCKED_KEYS.has(key) || key === INTERNAL_ID) continue;
    widget[key] = value;
  }

  widget.type = type;
  widget.x = Math.round(finiteNumber(widget.x, 0));
  widget.y = Math.round(finiteNumber(widget.y, 0));
  widget.w = Math.max(1, Math.round(finiteNumber(widget.w, 120)));
  widget.h = Math.max(1, Math.round(finiteNumber(widget.h, 80)));
  widget.color = sanitizeColor(widget.color, DEFAULT_ACCENT);
  return widget;
}

/** A new, empty layout document. */
export function defaultLayout() {
  return {
    id: 'editor-layout',
    name: 'Untitled Layout',
    width: 680,
    height: 480,
    background: DEFAULT_BACKGROUND,
    refresh_hz: 10,
    widgets: [],
  };
}

function newState(layout, options = {}) {
  const gridValue = options.grid === undefined ? options.gridSize : options.grid;
  const zoomValue = options.zoom === undefined ? options.zoomLevel : options.zoom;
  const historyValue = options.historyLimit === undefined ? options.maxHistory : options.historyLimit;
  const state = {
    layout,
    selectionId: null,
    grid: clampInteger(gridValue, MIN_GRID, MAX_GRID, DEFAULT_GRID),
    zoom: clampNumber(zoomValue, MIN_ZOOM, MAX_ZOOM, 1),
    history: {
      past: [],
      future: [],
      limit: clampInteger(historyValue, 1, 1000, DEFAULT_HISTORY_LIMIT),
    },
    nextId: layout.widgets.length + 1,
    source: typeof options.source === 'string' ? options.source : null,
    dirty: false,
    lastAction: 'Ready',
    error: null,
  };
  return state;
}

function looksLikeLayoutDocument(value) {
  if (!isPlainObject(value)) return false;
  return 'widgets' in value
    || 'layout' in value
    || 'definition_json' in value
    || ('id' in value && ('width' in value || 'height' in value));
}

function normalizeLayoutDocument(input) {
  let raw;
  let source = null;
  let exact = false;

  if (typeof input === 'string') {
    try {
      raw = JSON.parse(input);
    } catch (error) {
      throw new Error(`Invalid layout JSON: ${error.message}`);
    }
    source = input;
    exact = true;
  } else if (isPlainObject(input)) {
    raw = cloneJson(input);
    source = JSON.stringify(input, null, 2);
    exact = true;
  } else {
    throw new TypeError('Layout input must be a JSON string or object.');
  }

  if (!isPlainObject(raw)) {
    throw new Error('Layout JSON must be an object.');
  }

  let layout = raw;
  let unwrapped = false;
  for (let depth = 0; depth < 5; depth += 1) {
    if (isPlainObject(layout) && typeof layout.definition_json === 'string') {
      try {
        layout = JSON.parse(layout.definition_json);
      } catch (error) {
        throw new Error(`Invalid definition_json: ${error.message}`);
      }
      unwrapped = true;
      continue;
    }
    if (isPlainObject(layout) && isPlainObject(layout.layout)) {
      layout = layout.layout;
      unwrapped = true;
      continue;
    }
    break;
  }

  if (!isPlainObject(layout)) {
    throw new Error('Layout JSON must be an object.');
  }
  if (!('widgets' in layout) && !('id' in layout) && !('width' in layout) && !('height' in layout)) {
    throw new Error('The document does not look like a ScreenLayout.');
  }

  const normalized = cloneJson(layout);
  let changed = false;

  if (typeof normalized.id !== 'string' || !normalized.id.trim()) {
    normalized.id = 'editor-layout';
    changed = true;
  }
  if (normalized.widgets === undefined) {
    normalized.widgets = [];
  } else if (!Array.isArray(normalized.widgets)) {
    throw new Error('Layout widgets must be an array.');
  }

  normalized.widgets = normalized.widgets.map((widget, index) => {
    if (!isPlainObject(widget)) {
      throw new Error(`Widget ${index} must be an object.`);
    }
    if (typeof widget.type !== 'string' || !widget.type.trim()) {
      throw new Error(`Widget ${index} is missing a type.`);
    }
    const copy = cloneJson(widget);
    setWidgetId(copy, `w${index + 1}`);
    return copy;
  });

  if (unwrapped || changed) exact = false;
  return { layout: normalized, source: exact ? source : null };
}

/**
 * Parse a `ScreenLayout` / display-definition JSON document into editor state.
 *
 * A string source that needs no normalisation round-trips byte-for-byte through
 * `serializeLayout` until the state is changed.
 */
export function parseLayout(input, settings = {}) {
  const { layout, source } = normalizeLayoutDocument(input);
  return newState(layout, { ...settings, source });
}

/**
 * Create editor state.  With no argument a new, empty layout is used.
 *
 * An object containing `layout`, `widgets`, or `definition_json` is treated as
 * a document; an object containing only settings (`grid`, `zoom`,
 * `historyLimit`) configures the new editor.
 */
export function createEditorState(input = null, settings = {}) {
  if (input === null || input === undefined) {
    return newState(defaultLayout(), settings);
  }
  if (typeof input === 'string') {
    return parseLayout(input, settings);
  }
  if (isPlainObject(input) && !looksLikeLayoutDocument(input)) {
    const options = { ...input, ...settings };
    return newState(defaultLayout(), {
      grid: options.grid === undefined ? options.gridSize : options.grid,
      zoom: options.zoom === undefined ? options.zoomLevel : options.zoom,
      historyLimit: options.historyLimit === undefined ? options.maxHistory : options.historyLimit,
    });
  }
  return parseLayout(input, settings);
}

function snapshot(state) {
  return {
    layout: cloneLayout(state.layout),
    selectionId: state.selectionId,
    source: state.source,
    dirty: state.dirty,
  };
}

function cloneLayout(layout) {
  const copy = cloneJson(layout);
  const originalWidgets = Array.isArray(layout && layout.widgets) ? layout.widgets : [];
  copy.widgets = Array.isArray(copy.widgets) ? copy.widgets : [];
  copy.widgets.forEach((widget, index) => {
    const originalId = widgetId(originalWidgets[index]);
    if (originalId) setWidgetId(widget, originalId);
  });
  return copy;
}

function commitLayout(state, layout, selectionId, action) {
  const past = state.history.past.concat([snapshot(state)]);
  while (past.length > state.history.limit) past.shift();
  return {
    ...state,
    layout,
    selectionId: selectionId === undefined ? state.selectionId : selectionId,
    dirty: true,
    history: { past, future: [], limit: state.history.limit },
    lastAction: action,
    error: null,
  };
}

function findWidgetIndex(layout, id) {
  if (!id) return -1;
  return layout.widgets.findIndex((widget) => widgetId(widget) === id);
}

function findWidget(state, id) {
  const index = findWidgetIndex(state.layout, id);
  return index < 0 ? null : state.layout.widgets[index];
}

function normalizeWidgetId(state, id) {
  if (id && typeof id === 'object') return widgetId(id) || null;
  if (id === undefined || id === null) return state.selectionId;
  return String(id);
}

function allocateWidgetId(state, layout) {
  const used = new Set();
  for (const widget of layout.widgets) {
    const id = widgetId(widget);
    if (id) used.add(id);
  }
  let counter = state.nextId;
  while (used.has(`w${counter}`)) counter += 1;
  return { id: `w${counter}`, nextId: counter + 1 };
}

function findFreePosition(layout, width, height, grid) {
  const maxX = Math.max(0, layoutWidth(layout) - width);
  const maxY = Math.max(0, layoutHeight(layout) - height);
  let candidates = 0;
  for (let y = grid; y <= maxY && candidates < 4096; y += grid) {
    for (let x = grid; x <= maxX && candidates < 4096; x += grid) {
      candidates += 1;
      const overlaps = layout.widgets.some((widget) => {
        const rect = rectOf(widget);
        return x < rect.x + rect.w
          && x + width > rect.x
          && y < rect.y + rect.h
          && y + height > rect.y;
      });
      if (!overlaps) return { x, y };
    }
  }
  const index = layout.widgets.length;
  return {
    x: grid + (index % 8) * grid,
    y: grid + (Math.floor(index / 8) % 12) * grid,
  };
}

/** Add a widget of `kind` and select it. */
export function addWidget(state, kind = 'gauge', options = {}) {
  const layout = cloneLayout(state.layout);
  const widget = widgetDefaults(kind, options);
  const bounds = boundsFor(layout);
  const requested = snapRect(rectOf(widget), state.grid, bounds);

  if (options && options.x === undefined && options.y === undefined) {
    const free = findFreePosition(layout, requested.w, requested.h, state.grid);
    requested.x = free.x;
    requested.y = free.y;
  }

  const placed = snapRect(requested, state.grid, bounds);
  widget.x = placed.x;
  widget.y = placed.y;
  widget.w = placed.w;
  widget.h = placed.h;

  const allocated = allocateWidgetId(state, layout);
  setWidgetId(widget, allocated.id);
  layout.widgets.push(widget);

  return {
    ...commitLayout(state, layout, allocated.id, `Added ${widget.type}`),
    nextId: allocated.nextId,
  };
}

/** Remove a widget.  Defaults to the current selection. */
export function removeWidget(state, id) {
  const selected = normalizeWidgetId(state, id);
  const index = findWidgetIndex(state.layout, selected);
  if (index < 0) return state;

  const layout = cloneLayout(state.layout);
  layout.widgets.splice(index, 1);
  const selectionId = state.selectionId === selected ? null : state.selectionId;
  return commitLayout(state, layout, selectionId, 'Removed widget');
}

/** Move a widget to a (snapped, clamped) position. */
export function moveWidget(state, id, x, y) {
  const selected = normalizeWidgetId(state, id);
  const index = findWidgetIndex(state.layout, selected);
  if (index < 0) return state;

  const position = x && typeof x === 'object' ? x : { x, y };
  const layout = cloneLayout(state.layout);
  const widget = layout.widgets[index];
  const current = rectOf(widget);
  const next = snapRect({
    ...current,
    x: finiteNumber(position.x, current.x),
    y: finiteNumber(position.y, current.y),
  }, state.grid, boundsFor(layout));

  if (next.x === current.x && next.y === current.y) return state;
  widget.x = next.x;
  widget.y = next.y;
  return commitLayout(state, layout, state.selectionId, 'Moved widget');
}

/** Resize a widget.  Accepts a rect or a width/height pair. */
export function resizeWidget(state, id, rect, height) {
  const selected = normalizeWidgetId(state, id);
  const index = findWidgetIndex(state.layout, selected);
  if (index < 0) return state;

  let patch;
  if (rect && typeof rect === 'object') {
    patch = rect;
  } else {
    patch = { w: rect, h: height };
  }

  const layout = cloneLayout(state.layout);
  const widget = layout.widgets[index];
  const current = rectOf(widget);
  const next = snapRect({
    x: finiteNumber(patch.x, current.x),
    y: finiteNumber(patch.y, current.y),
    w: finiteNumber(patch.w, current.w),
    h: finiteNumber(patch.h, current.h),
  }, state.grid, boundsFor(layout));

  if (sameRect(next, current)) return state;
  widget.x = next.x;
  widget.y = next.y;
  widget.w = next.w;
  widget.h = next.h;
  return commitLayout(state, layout, state.selectionId, 'Resized widget');
}

function normalizePatchValue(key, value) {
  if (key === 'type') {
    const type = String(value || '');
    return WIDGET_KINDS.includes(type) ? type : undefined;
  }
  if (key === 'color') return sanitizeColor(value, DEFAULT_ACCENT);
  if (key === 'fill') return Boolean(value);
  if (key === 'orientation') {
    return /^vert/i.test(String(value || '')) ? 'vertical' : 'horizontal';
  }
  if (TEXT_PATCH_KEYS.has(key)) {
    if (value === undefined || value === null) return '';
    return String(value).slice(0, MAX_TEXT_LENGTH);
  }
  if (Object.prototype.hasOwnProperty.call(NUMBER_PATCH_LIMITS, key)) {
    if (value === undefined || value === null || value === '') return undefined;
    const [min, max] = NUMBER_PATCH_LIMITS[key];
    const number = Number(value);
    if (!Number.isFinite(number)) return undefined;
    const integerKeys = new Set([
      'decimals',
      'history_points',
      'segments',
      'font_size',
    ]);
    return integerKeys.has(key)
      ? Math.round(clampNumber(number, min, max, min))
      : clampNumber(number, min, max, min);
  }
  return undefined;
}

/** Update widget properties without replacing unknown fields. */
export function updateWidget(state, id, patch = {}) {
  const selected = normalizeWidgetId(state, id);
  const index = findWidgetIndex(state.layout, selected);
  if (index < 0 || !isPlainObject(patch)) return state;

  const layout = cloneLayout(state.layout);
  const widget = layout.widgets[index];
  let changed = false;

  const geometry = {};
  for (const key of ['x', 'y', 'w', 'h']) {
    if (Object.prototype.hasOwnProperty.call(patch, key)) {
      const number = Number(patch[key]);
      if (Number.isFinite(number)) geometry[key] = number;
    }
  }
  if (Object.keys(geometry).length) {
    const current = rectOf(widget);
    const next = snapRect({ ...current, ...geometry }, state.grid, boundsFor(layout));
    if (!sameRect(next, current)) {
      widget.x = next.x;
      widget.y = next.y;
      widget.w = next.w;
      widget.h = next.h;
      changed = true;
    }
  }

  for (const [key, value] of sortedEntries(patch)) {
    if (key === 'x' || key === 'y' || key === 'w' || key === 'h') continue;
    if (BLOCKED_KEYS.has(key) || !ALLOWED_PATCH_KEYS.has(key)) continue;
    const normalized = normalizePatchValue(key, value);
    if (normalized === undefined) continue;
    if (widget[key] !== normalized) {
      widget[key] = normalized;
      changed = true;
    }
  }

  if (!changed) return state;
  return commitLayout(state, layout, state.selectionId, 'Updated widget');
}

/** Update layout metadata.  Shrinking the canvas clamps widgets. */
export function setLayoutMeta(state, patch = {}) {
  if (!isPlainObject(patch)) return state;
  const layout = cloneLayout(state.layout);
  let changed = false;

  if (Object.prototype.hasOwnProperty.call(patch, 'id')) {
    const id = String(patch.id === undefined || patch.id === null ? '' : patch.id)
      .trim()
      .slice(0, 120) || 'layout';
    if (layout.id !== id) {
      layout.id = id;
      changed = true;
    }
  }
  if (Object.prototype.hasOwnProperty.call(patch, 'name')) {
    const name = String(patch.name === undefined || patch.name === null ? '' : patch.name)
      .slice(0, 200);
    if (layout.name !== name) {
      layout.name = name;
      changed = true;
    }
  }
  for (const key of ['width', 'height']) {
    if (!Object.prototype.hasOwnProperty.call(patch, key)) continue;
    const number = clampInteger(patch[key], 1, 8192, key === 'width' ? 680 : 480);
    if (layout[key] !== number) {
      layout[key] = number;
      changed = true;
    }
  }
  if (Object.prototype.hasOwnProperty.call(patch, 'background')) {
    const background = sanitizeColor(patch.background, DEFAULT_BACKGROUND);
    if (layout.background !== background) {
      layout.background = background;
      changed = true;
    }
  }
  if (Object.prototype.hasOwnProperty.call(patch, 'refresh_hz')) {
    const refresh = clampNumber(patch.refresh_hz, 0.1, 120, 10);
    if (layout.refresh_hz !== refresh) {
      layout.refresh_hz = refresh;
      changed = true;
    }
  }

  if (!changed) return state;

  if (patch.width !== undefined || patch.height !== undefined) {
    const bounds = boundsFor(layout);
    layout.widgets = layout.widgets.map((widget) => {
      const rect = snapRect(rectOf(widget), state.grid, bounds);
      widget.x = rect.x;
      widget.y = rect.y;
      widget.w = rect.w;
      widget.h = rect.h;
      return widget;
    });
  }

  return commitLayout(state, layout, state.selectionId, 'Updated layout');
}

/** Select a widget, or clear the selection with `null`. */
export function selectWidget(state, id) {
  if (id === null) {
    return state.selectionId === null ? state : { ...state, selectionId: null };
  }
  const selected = normalizeWidgetId(state, id);
  if (!selected) {
    return state.selectionId === null ? state : { ...state, selectionId: null };
  }
  if (findWidgetIndex(state.layout, selected) < 0) {
    return state.selectionId === null ? state : { ...state, selectionId: null };
  }
  return state.selectionId === selected ? state : { ...state, selectionId: selected };
}

function restoreSnapshot(state, entry, action, history) {
  return {
    ...state,
    layout: cloneLayout(entry.layout),
    selectionId: entry.selectionId,
    source: entry.source,
    dirty: entry.dirty,
    history,
    lastAction: action,
  };
}

/** Undo the most recent layout change. */
export function undo(state) {
  if (!state.history.past.length) return state;
  const past = state.history.past.slice(0, -1);
  const entry = state.history.past[state.history.past.length - 1];
  const future = [snapshot(state), ...state.history.future].slice(0, state.history.limit);
  return restoreSnapshot(state, entry, 'Undo', {
    past,
    future,
    limit: state.history.limit,
  });
}

/** Redo the most recently undone layout change. */
export function redo(state) {
  if (!state.history.future.length) return state;
  const [entry, ...future] = state.history.future;
  const past = state.history.past.concat([snapshot(state)]);
  while (past.length > state.history.limit) past.shift();
  return restoreSnapshot(state, entry, 'Redo', {
    past,
    future,
    limit: state.history.limit,
  });
}

/** Duplicate a widget with a small offset and select the copy. */
export function duplicateWidget(state, id) {
  const selected = normalizeWidgetId(state, id);
  const source = findWidget(state, selected);
  if (!source) return state;

  const layout = cloneLayout(state.layout);
  const copy = cloneJson(source);
  const allocated = allocateWidgetId(state, layout);
  setWidgetId(copy, allocated.id);

  const bounds = boundsFor(layout);
  const rect = snapRect({
    ...rectOf(source),
    x: rectOf(source).x + state.grid,
    y: rectOf(source).y + state.grid,
  }, state.grid, bounds);
  copy.x = rect.x;
  copy.y = rect.y;
  copy.w = rect.w;
  copy.h = rect.h;
  layout.widgets.push(copy);

  return {
    ...commitLayout(state, layout, allocated.id, 'Duplicated widget'),
    nextId: allocated.nextId,
  };
}

/** Move a widget one step towards the top of the z-order. */
export function raiseWidget(state, id) {
  const selected = normalizeWidgetId(state, id);
  const index = findWidgetIndex(state.layout, selected);
  if (index < 0 || index >= state.layout.widgets.length - 1) return state;
  const layout = cloneLayout(state.layout);
  const [widget] = layout.widgets.splice(index, 1);
  layout.widgets.push(widget);
  return commitLayout(state, layout, state.selectionId, 'Raised widget');
}

/** Move a widget one step towards the bottom of the z-order. */
export function lowerWidget(state, id) {
  const selected = normalizeWidgetId(state, id);
  const index = findWidgetIndex(state.layout, selected);
  if (index <= 0) return state;
  const layout = cloneLayout(state.layout);
  const [widget] = layout.widgets.splice(index, 1);
  layout.widgets.splice(index - 1, 0, widget);
  return commitLayout(state, layout, state.selectionId, 'Lowered widget');
}

/** Set the grid size (a view setting; it is never serialised). */
export function setGrid(state, value) {
  const grid = clampInteger(value, MIN_GRID, MAX_GRID, DEFAULT_GRID);
  return grid === state.grid ? state : { ...state, grid };
}

/** Set the zoom level (a view setting; it is never serialised). */
export function setZoom(state, value) {
  const zoom = clampNumber(value, MIN_ZOOM, MAX_ZOOM, 1);
  return zoom === state.zoom ? state : { ...state, zoom };
}

/** Replace the layout with a fresh default document (undoable). */
export function resetLayout(state) {
  const layout = defaultLayout();
  layout.widgets.forEach((widget, index) => setWidgetId(widget, `w${index + 1}`));
  return {
    ...commitLayout(state, layout, null, 'Reset layout'),
    nextId: 1,
  };
}

/**
 * Serialise state as a valid `ScreenLayout` document.
 *
 * The original source text is returned byte-for-byte when nothing has changed.
 */
export function serializeLayout(state) {
  if (!state || !isPlainObject(state.layout)) {
    throw new TypeError('serializeLayout requires editor state.');
  }
  if (state.source && !state.dirty) return state.source;
  return `${JSON.stringify(state.layout, null, 2)}\n`;
}

/* ------------------------------------------------------------------------- */
/* Browser controller                                                        */
/* ------------------------------------------------------------------------- */

function byId(id) {
  return typeof document === 'undefined' ? null : document.getElementById(id);
}

const els = {
  addWidget: byId('add-widget'),
  widgetKind: byId('widget-kind'),
  duplicateWidget: byId('duplicate-widget'),
  deleteWidget: byId('delete-widget'),
  raiseWidget: byId('raise-widget'),
  lowerWidget: byId('lower-widget'),
  undo: byId('undo'),
  redo: byId('redo'),
  gridSize: byId('grid-size'),
  zoom: byId('zoom'),
  zoomOutput: byId('zoom-output'),
  importButton: byId('import-button'),
  exportButton: byId('export-button'),
  resetButton: byId('reset-button'),
  fileInput: byId('file-input'),
  layoutId: byId('layout-id'),
  layoutName: byId('layout-name'),
  layoutWidth: byId('layout-width'),
  layoutHeight: byId('layout-height'),
  layoutBackground: byId('layout-background'),
  layoutRefresh: byId('layout-refresh'),
  palette: byId('palette'),
  canvas: byId('editor-canvas'),
  canvasScroll: byId('canvas-scroll'),
  canvasEmpty: byId('canvas-empty'),
  selectionEmpty: byId('selection-empty'),
  propertiesForm: byId('properties-form'),
  propType: byId('prop-type'),
  propLabel: byId('prop-label'),
  propText: byId('prop-text'),
  propMetric: byId('prop-metric'),
  propUnit: byId('prop-unit'),
  propColor: byId('prop-color'),
  propMin: byId('prop-min'),
  propMax: byId('prop-max'),
  propWarn: byId('prop-warn'),
  propCrit: byId('prop-crit'),
  propDecimals: byId('prop-decimals'),
  propX: byId('prop-x'),
  propY: byId('prop-y'),
  propW: byId('prop-w'),
  propH: byId('prop-h'),
  propHistoryPoints: byId('prop-history-points'),
  propHistorySeconds: byId('prop-history-seconds'),
  propFill: byId('prop-fill'),
  sparkFields: byId('spark-fields'),
  propAttack: byId('prop-attack'),
  propRelease: byId('prop-release'),
  propFalloff: byId('prop-falloff'),
  propPeakHold: byId('prop-peak-hold'),
  propPeakFalloff: byId('prop-peak-falloff'),
  propSegments: byId('prop-segments'),
  vuFields: byId('vu-fields'),
  status: byId('status'),
  error: byId('error'),
  importDialog: byId('import-dialog'),
  importForm: byId('import-form'),
  importText: byId('import-text'),
  importError: byId('import-error'),
  importCancel: byId('import-cancel'),
  importApply: byId('import-apply'),
  importFile: byId('import-file'),
};

const PREVIEW_HEIGHTS = [18, 32, 24, 46, 38, 60, 44, 54, 36, 66, 48, 40, 58, 30, 50, 42, 64, 34];

function typeClass(type) {
  return WIDGET_KINDS.includes(type) ? type.replace(/_/g, '-') : 'card';
}

function widgetAriaLabel(widget, selected) {
  const type = String(widget && widget.type ? widget.type : 'widget');
  const label = widget && widget.label ? ` ${String(widget.label)}` : '';
  const rect = rectOf(widget);
  const selection = selected ? ', selected' : '';
  return `${type}${label}, x ${rect.x}, y ${rect.y}, ${rect.w} by ${rect.h}${selection}`;
}

function createElement(tag, className, text) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined) element.textContent = String(text);
  return element;
}

function buildPreview(widget) {
  const type = String(widget.type || 'card');
  const host = createElement('div', 'widget-preview');

  if (type === 'gauge') {
    host.appendChild(createElement('div', 'preview-gauge'));
    return host;
  }
  if (type === 'bar') {
    const track = createElement('div', 'preview-bar');
    const fill = createElement('div', 'preview-bar-fill');
    fill.style.width = '62%';
    track.appendChild(fill);
    host.appendChild(track);
    return host;
  }
  if (type === 'sparkline') {
    const spark = createElement('div', 'preview-sparkline');
    PREVIEW_HEIGHTS.forEach((height) => {
      const bar = createElement('span');
      bar.style.height = `${height}%`;
      spark.appendChild(bar);
    });
    host.appendChild(spark);
    return host;
  }
  if (type === 'vu_meter') {
    const meter = createElement('div', 'preview-vu');
    for (let index = 0; index < 16; index += 1) {
      const segment = createElement('span');
      if (index < 10) segment.dataset.lit = 'true';
      if (index >= 10 && index < 13) segment.dataset.level = 'warn';
      if (index >= 13) segment.dataset.level = 'crit';
      meter.appendChild(segment);
    }
    host.appendChild(meter);
    return host;
  }
  if (type === 'number') {
    host.appendChild(createElement('span', 'preview-value', '42'));
    return host;
  }
  if (type === 'status') {
    const status = createElement('div', 'preview-status');
    status.appendChild(createElement('span', 'preview-dot'));
    status.appendChild(createElement('span', 'preview-status-text', 'OK'));
    host.appendChild(status);
    return host;
  }
  if (type === 'icon') {
    host.appendChild(createElement('span', 'preview-icon', widget.icon || '◇'));
    return host;
  }
  if (type === 'table') {
    const table = createElement('div', 'preview-table');
    for (let index = 0; index < 3; index += 1) {
      table.appendChild(createElement('span', 'preview-row'));
    }
    host.appendChild(table);
    return host;
  }
  if (type === 'text') {
    host.appendChild(createElement('span', 'preview-text', widget.text || widget.label || 'Text'));
    return host;
  }
  host.appendChild(createElement('span', 'preview-text', widget.label || 'Card'));
  return host;
}

function buildWidgetNode(widget, state) {
  const id = widgetId(widget);
  const rect = rectOf(widget);
  const selected = state.selectionId === id;
  const node = createElement('div', `widget-box kind-${typeClass(widget.type)}`);
  node.tabIndex = 0;
  node.dataset.widgetId = id;
  node.dataset.selected = selected ? 'true' : 'false';
  node.setAttribute('role', 'group');
  node.setAttribute('aria-label', widgetAriaLabel(widget, selected));
  node.setAttribute('aria-roledescription', 'layout widget');
  node.style.left = `${rect.x * state.zoom}px`;
  node.style.top = `${rect.y * state.zoom}px`;
  node.style.width = `${rect.w * state.zoom}px`;
  node.style.height = `${rect.h * state.zoom}px`;
  node.style.setProperty('--widget-accent', sanitizeColor(widget.color, DEFAULT_ACCENT));

  const head = createElement('div', 'widget-head');
  head.appendChild(createElement('span', 'widget-badge', widget.type));
  head.appendChild(createElement('span', 'widget-label', widget.label || widget.text || widget.type));
  node.appendChild(head);
  node.appendChild(buildPreview(widget));

  const meta = [widget.metric, widget.unit]
    .filter((value) => value !== undefined && value !== null && String(value) !== '')
    .map((value) => String(value))
    .join(' · ');
  if (meta) node.appendChild(createElement('span', 'widget-meta', meta));

  for (const direction of ['nw', 'n', 'ne', 'e', 'se', 's', 'sw', 'w']) {
    const handle = createElement('span', `resize-handle handle-${direction}`);
    handle.dataset.dir = direction;
    handle.setAttribute('aria-hidden', 'true');
    node.appendChild(handle);
  }
  return node;
}

function safeFilename(value) {
  const name = String(value || 'layout')
    .replace(/[^a-z0-9._-]+/gi, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 80);
  return `${name || 'layout'}.json`;
}

function initEditor() {
  let state = createEditorState();
  let drag = null;
  let renderedSelection = null;

  function setStatus(message, tone = 'info') {
    if (els.status) {
      els.status.textContent = String(message);
      els.status.dataset.tone = tone;
    }
    if (els.error) {
      if (tone === 'error') {
        els.error.textContent = String(message);
        els.error.hidden = false;
      } else {
        els.error.textContent = '';
        els.error.hidden = true;
      }
    }
  }

  function selectedWidget() {
    return findWidget(state, state.selectionId);
  }

  function updateSelectionClasses() {
    if (!els.canvas) return;
    for (const node of els.canvas.querySelectorAll('.widget-box')) {
      const widget = findWidget(state, node.dataset.widgetId);
      const selected = node.dataset.widgetId === state.selectionId;
      node.dataset.selected = selected ? 'true' : 'false';
      if (widget) node.setAttribute('aria-label', widgetAriaLabel(widget, selected));
    }
  }

  function renderCanvas() {
    if (!els.canvas) return;
    const layout = state.layout;
    els.canvas.style.width = `${layoutWidth(layout) * state.zoom}px`;
    els.canvas.style.height = `${layoutHeight(layout) * state.zoom}px`;
    els.canvas.style.setProperty('--grid-size', `${state.grid * state.zoom}px`);
    els.canvas.style.setProperty(
      '--canvas-bg',
      sanitizeColor(layout.background, DEFAULT_BACKGROUND),
    );
    els.canvas.replaceChildren();
    layout.widgets.forEach((widget) => {
      els.canvas.appendChild(buildWidgetNode(widget, state));
    });
    if (els.canvasEmpty) els.canvasEmpty.hidden = layout.widgets.length !== 0;
  }

  function setInputValue(element, value, force) {
    if (!element) return;
    if (!force && document.activeElement === element) return;
    element.value = value === undefined || value === null ? '' : String(value);
  }

  function renderLayoutMeta() {
    const layout = state.layout;
    setInputValue(els.layoutId, layout.id);
    setInputValue(els.layoutName, layout.name === undefined ? '' : layout.name);
    setInputValue(els.layoutWidth, layoutWidth(layout));
    setInputValue(els.layoutHeight, layoutHeight(layout));
    setInputValue(els.layoutBackground, layout.background || DEFAULT_BACKGROUND);
    setInputValue(els.layoutRefresh, layout.refresh_hz === undefined ? 10 : layout.refresh_hz);
  }

  function renderProperties() {
    const widget = selectedWidget();
    const force = renderedSelection !== state.selectionId;
    if (els.selectionEmpty) els.selectionEmpty.hidden = Boolean(widget);
    if (els.propertiesForm) els.propertiesForm.hidden = !widget;
    if (!widget) {
      renderedSelection = state.selectionId;
      return;
    }

    setInputValue(els.propType, widget.type, force);
    setInputValue(els.propLabel, widget.label === undefined ? '' : widget.label, force);
    setInputValue(els.propText, widget.text === undefined ? '' : widget.text, force);
    setInputValue(els.propMetric, widget.metric === undefined ? '' : widget.metric, force);
    setInputValue(els.propUnit, widget.unit === undefined ? '' : widget.unit, force);
    setInputValue(els.propColor, widget.color === undefined ? DEFAULT_ACCENT : widget.color, force);
    setInputValue(els.propMin, widget.min === undefined ? '' : widget.min, force);
    setInputValue(els.propMax, widget.max === undefined ? '' : widget.max, force);
    setInputValue(els.propWarn, widget.warn === undefined ? '' : widget.warn, force);
    setInputValue(els.propCrit, widget.crit === undefined ? '' : widget.crit, force);
    setInputValue(els.propDecimals, widget.decimals === undefined ? '' : widget.decimals, force);
    setInputValue(els.propX, rectOf(widget).x, force);
    setInputValue(els.propY, rectOf(widget).y, force);
    setInputValue(els.propW, rectOf(widget).w, force);
    setInputValue(els.propH, rectOf(widget).h, force);
    setInputValue(els.propHistoryPoints, widget.history_points === undefined ? '' : widget.history_points, force);
    setInputValue(els.propHistorySeconds, widget.history_seconds === undefined ? '' : widget.history_seconds, force);
    if (els.propFill) els.propFill.checked = Boolean(widget.fill);
    setInputValue(els.propAttack, widget.attack_ms === undefined ? '' : widget.attack_ms, force);
    setInputValue(els.propRelease, widget.release_ms === undefined ? '' : widget.release_ms, force);
    setInputValue(els.propFalloff, widget.falloff_db_per_sec === undefined ? '' : widget.falloff_db_per_sec, force);
    setInputValue(els.propPeakHold, widget.peak_hold_ms === undefined ? '' : widget.peak_hold_ms, force);
    setInputValue(els.propPeakFalloff, widget.peak_falloff_db_per_sec === undefined ? '' : widget.peak_falloff_db_per_sec, force);
    setInputValue(els.propSegments, widget.segments === undefined ? '' : widget.segments, force);

    if (els.sparkFields) els.sparkFields.hidden = widget.type !== 'sparkline';
    if (els.vuFields) els.vuFields.hidden = widget.type !== 'vu_meter';
    renderedSelection = state.selectionId;
  }

  function renderToolbar() {
    const hasSelection = Boolean(selectedWidget());
    if (els.duplicateWidget) els.duplicateWidget.disabled = !hasSelection;
    if (els.deleteWidget) els.deleteWidget.disabled = !hasSelection;
    if (els.raiseWidget) els.raiseWidget.disabled = !hasSelection;
    if (els.lowerWidget) els.lowerWidget.disabled = !hasSelection;
    if (els.undo) els.undo.disabled = state.history.past.length === 0;
    if (els.redo) els.redo.disabled = state.history.future.length === 0;
    if (els.gridSize && document.activeElement !== els.gridSize) {
      els.gridSize.value = String(state.grid);
    }
    if (els.zoom && document.activeElement !== els.zoom) {
      els.zoom.value = String(state.zoom);
    }
    if (els.zoomOutput) els.zoomOutput.textContent = `${Math.round(state.zoom * 100)}%`;
  }

  function focusWidget(id) {
    if (!id || !els.canvas) return;
    const node = els.canvas.querySelector(`[data-widget-id="${id}"]`);
    if (node && typeof node.focus === 'function') {
      node.focus({ preventScroll: true });
    }
  }

  function renderAll(options = {}) {
    renderLayoutMeta();
    renderCanvas();
    renderProperties();
    renderToolbar();
    if (options.focusId) focusWidget(options.focusId);
  }

  function selectAndRender(id, message) {
    state = selectWidget(state, id);
    renderAll();
    if (message) setStatus(message);
  }

  function onPropertyChange() {
    const widget = selectedWidget();
    if (!widget) return;
    const patch = {};
    const compareText = (key, element) => {
      if (!element) return;
      const current = widget[key] === undefined || widget[key] === null ? '' : String(widget[key]);
      if (element.value !== current) patch[key] = element.value;
    };
    const compareNumber = (key, element) => {
      if (!element || element.value === '') return;
      const number = Number(element.value);
      if (!Number.isFinite(number)) return;
      const current = Number(widget[key]);
      if (!Number.isFinite(current) || current !== number) patch[key] = number;
    };

    compareText('label', els.propLabel);
    compareText('text', els.propText);
    compareText('metric', els.propMetric);
    compareText('unit', els.propUnit);
    compareText('color', els.propColor);
    compareNumber('min', els.propMin);
    compareNumber('max', els.propMax);
    compareNumber('warn', els.propWarn);
    compareNumber('crit', els.propCrit);
    compareNumber('decimals', els.propDecimals);
    compareNumber('x', els.propX);
    compareNumber('y', els.propY);
    compareNumber('w', els.propW);
    compareNumber('h', els.propH);
    compareNumber('history_points', els.propHistoryPoints);
    compareNumber('history_seconds', els.propHistorySeconds);
    compareNumber('attack_ms', els.propAttack);
    compareNumber('release_ms', els.propRelease);
    compareNumber('falloff_db_per_sec', els.propFalloff);
    compareNumber('peak_hold_ms', els.propPeakHold);
    compareNumber('peak_falloff_db_per_sec', els.propPeakFalloff);
    compareNumber('segments', els.propSegments);

    if (els.propFill && els.propFill.checked !== Boolean(widget.fill)) {
      patch.fill = els.propFill.checked;
    }
    if (els.propType && els.propType.value !== widget.type) {
      patch.type = els.propType.value;
    }

    if (!Object.keys(patch).length) return;
    const next = updateWidget(state, state.selectionId, patch);
    if (next === state) return;
    state = next;
    renderAll();
    setStatus('Widget updated.');
  }

  function onLayoutChange() {
    const patch = {};
    if (els.layoutId && els.layoutId.value.trim()) patch.id = els.layoutId.value;
    if (els.layoutName) patch.name = els.layoutName.value;
    if (els.layoutWidth && els.layoutWidth.value !== '') patch.width = els.layoutWidth.value;
    if (els.layoutHeight && els.layoutHeight.value !== '') patch.height = els.layoutHeight.value;
    if (els.layoutBackground) patch.background = els.layoutBackground.value;
    if (els.layoutRefresh && els.layoutRefresh.value !== '') {
      patch.refresh_hz = els.layoutRefresh.value;
    }
    const next = setLayoutMeta(state, patch);
    if (next === state) return;
    state = next;
    renderAll();
    setStatus('Layout updated.');
  }

  function applyDragRect(node, rect) {
    if (!node) return;
    node.style.left = `${rect.x * state.zoom}px`;
    node.style.top = `${rect.y * state.zoom}px`;
    node.style.width = `${rect.w * state.zoom}px`;
    node.style.height = `${rect.h * state.zoom}px`;
  }

  function dragRect(dx, dy) {
    const base = drag.rect;
    const minimum = state.grid;
    let x = base.x;
    let y = base.y;
    let w = base.w;
    let h = base.h;

    if (drag.mode === 'move') {
      x += dx;
      y += dy;
    } else {
      const direction = drag.dir;
      if (direction.includes('e')) w += dx;
      if (direction.includes('s')) h += dy;
      if (direction.includes('w')) {
        x += dx;
        w -= dx;
        if (w < minimum) {
          x = base.x + base.w - minimum;
          w = minimum;
        }
      }
      if (direction.includes('n')) {
        y += dy;
        h -= dy;
        if (h < minimum) {
          y = base.y + base.h - minimum;
          h = minimum;
        }
      }
    }

    return snapRect({ x, y, w, h }, state.grid, boundsFor(state.layout));
  }

  function onPointerMove(event) {
    if (!drag) return;
    const dx = (event.clientX - drag.startX) / state.zoom;
    const dy = (event.clientY - drag.startY) / state.zoom;
    drag.preview = dragRect(dx, dy);
    applyDragRect(drag.node, drag.preview);
  }

  function finishDrag(event, cancelled) {
    if (!drag) return;
    const active = drag;
    drag = null;
    const node = active.node;
    node.removeEventListener('pointermove', onPointerMove);
    node.removeEventListener('pointerup', onPointerUp);
    node.removeEventListener('pointercancel', onPointerCancel);
    if (node.hasPointerCapture && node.hasPointerCapture(event.pointerId)) {
      node.releasePointerCapture(event.pointerId);
    }
    node.classList.remove('dragging', 'resizing');
    if (cancelled) {
      renderAll({ focusId: active.id });
      return;
    }

    const preview = active.preview || active.rect;
    const before = state;
    if (active.mode === 'move') {
      state = moveWidget(state, active.id, preview.x, preview.y);
    } else {
      state = resizeWidget(state, active.id, preview);
    }
    renderAll({ focusId: active.id });
    if (state !== before) {
      setStatus(active.mode === 'move' ? 'Moved widget.' : 'Resized widget.');
    }
  }

  function onPointerUp(event) {
    finishDrag(event, false);
  }

  function onPointerCancel(event) {
    finishDrag(event, true);
  }

  function startDrag(event, widget, mode, direction) {
    if (event.button !== undefined && event.button !== 0) return;
    if (!els.canvas) return;
    event.preventDefault();
    event.stopPropagation();
    const node = event.target.closest('.widget-box');
    if (!node) return;
    const id = widgetId(widget);
    if (!id) return;

    if (typeof node.focus === 'function') node.focus({ preventScroll: true });

    if (state.selectionId !== id) {
      state = selectWidget(state, id);
      updateSelectionClasses();
      renderProperties();
      renderToolbar();
    }

    drag = {
      mode,
      dir: direction,
      id,
      node,
      startX: event.clientX,
      startY: event.clientY,
      rect: rectOf(widget),
      preview: null,
      zoom: state.zoom,
    };
    node.classList.add(mode === 'move' ? 'dragging' : 'resizing');
    if (typeof node.setPointerCapture === 'function') {
      try {
        node.setPointerCapture(event.pointerId);
      } catch (error) {
        // Pointer capture is best-effort; the page still works without it.
      }
    }
    node.addEventListener('pointermove', onPointerMove);
    node.addEventListener('pointerup', onPointerUp);
    node.addEventListener('pointercancel', onPointerCancel);
  }

  function invoke(action, message, focusId) {
    const before = state;
    state = action(state);
    renderAll({ focusId: focusId === undefined ? state.selectionId : focusId });
    if (state !== before) setStatus(message);
  }

  if (els.addWidget) {
    els.addWidget.addEventListener('click', () => {
      const kind = els.widgetKind ? els.widgetKind.value : 'gauge';
      state = addWidget(state, kind);
      renderAll({ focusId: state.selectionId });
      setStatus(`Added ${kind}.`);
    });
  }

  if (els.palette) {
    els.palette.addEventListener('click', (event) => {
      const button = event.target.closest('[data-kind]');
      if (!button) return;
      const kind = button.dataset.kind;
      state = addWidget(state, kind);
      renderAll({ focusId: state.selectionId });
      setStatus(`Added ${kind}.`);
    });
  }

  if (els.duplicateWidget) {
    els.duplicateWidget.addEventListener('click', () => {
      invoke((current) => duplicateWidget(current, current.selectionId), 'Duplicated widget.');
    });
  }
  if (els.deleteWidget) {
    els.deleteWidget.addEventListener('click', () => {
      invoke((current) => removeWidget(current, current.selectionId), 'Removed widget.', null);
    });
  }
  if (els.raiseWidget) {
    els.raiseWidget.addEventListener('click', () => {
      invoke((current) => raiseWidget(current, current.selectionId), 'Raised widget.');
    });
  }
  if (els.lowerWidget) {
    els.lowerWidget.addEventListener('click', () => {
      invoke((current) => lowerWidget(current, current.selectionId), 'Lowered widget.');
    });
  }
  if (els.undo) {
    els.undo.addEventListener('click', () => {
      invoke((current) => undo(current), 'Undo.', null);
    });
  }
  if (els.redo) {
    els.redo.addEventListener('click', () => {
      invoke((current) => redo(current), 'Redo.', null);
    });
  }
  if (els.resetButton) {
    els.resetButton.addEventListener('click', () => {
      invoke((current) => resetLayout(current), 'Reset layout. Undo is available.', null);
    });
  }

  if (els.gridSize) {
    els.gridSize.addEventListener('change', () => {
      state = setGrid(state, els.gridSize.value);
      renderAll();
      setStatus(`Grid ${state.grid} px.`);
    });
  }

  if (els.zoom) {
    els.zoom.addEventListener('input', () => {
      state = setZoom(state, els.zoom.value);
      renderCanvas();
      renderToolbar();
    });
  }

  if (els.layoutId) els.layoutId.addEventListener('change', onLayoutChange);
  if (els.layoutName) els.layoutName.addEventListener('change', onLayoutChange);
  if (els.layoutWidth) els.layoutWidth.addEventListener('change', onLayoutChange);
  if (els.layoutHeight) els.layoutHeight.addEventListener('change', onLayoutChange);
  if (els.layoutBackground) els.layoutBackground.addEventListener('change', onLayoutChange);
  if (els.layoutRefresh) els.layoutRefresh.addEventListener('change', onLayoutChange);

  if (els.propertiesForm) {
    els.propertiesForm.addEventListener('submit', (event) => event.preventDefault());
    els.propertiesForm.addEventListener('change', onPropertyChange);
  }

  if (els.canvas) {
    els.canvas.addEventListener('pointerdown', (event) => {
      if (event.target === els.canvas || event.target === els.canvasEmpty) {
        state = selectWidget(state, null);
        renderAll();
        return;
      }
      const node = event.target.closest ? event.target.closest('.widget-box') : null;
      if (!node) return;
      const widget = findWidget(state, node.dataset.widgetId);
      if (!widget) return;
      const handle = event.target.closest('.resize-handle');
      if (handle) {
        startDrag(event, widget, 'resize', handle.dataset.dir);
      } else {
        startDrag(event, widget, 'move', null);
      }
    });
    els.canvas.addEventListener('click', (event) => {
      const node = event.target.closest('.widget-box');
      if (node) {
        selectAndRender(node.dataset.widgetId);
        return;
      }
      if (event.target === els.canvas || event.target === els.canvasEmpty) {
        selectAndRender(null);
      }
    });
  }

  function clearImportError() {
    if (!els.importError) return;
    els.importError.textContent = '';
    els.importError.hidden = true;
  }

  function showImportError(message) {
    if (!els.importError) return;
    els.importError.textContent = String(message);
    els.importError.hidden = false;
  }

  function openImportDialog() {
    clearImportError();
    if (!els.importDialog) return;
    if (els.importText) els.importText.value = '';
    if (typeof els.importDialog.showModal === 'function') {
      els.importDialog.showModal();
    } else {
      els.importDialog.setAttribute('open', '');
    }
    if (els.importText) els.importText.focus();
  }

  function closeImportDialog() {
    if (!els.importDialog) return;
    if (typeof els.importDialog.close === 'function') {
      els.importDialog.close();
    } else {
      els.importDialog.removeAttribute('open');
    }
  }

  function importText(text) {
    try {
      const incoming = parseLayout(text, { grid: state.grid, zoom: state.zoom });
      const past = state.history.past.concat([snapshot(state)]);
      while (past.length > state.history.limit) past.shift();
      state = {
        ...incoming,
        grid: state.grid,
        zoom: state.zoom,
        selectionId: null,
        history: { past, future: [], limit: state.history.limit },
        lastAction: 'Imported layout',
      };
      renderedSelection = null;
      renderAll();
      closeImportDialog();
      setStatus(`Imported ${incoming.layout.widgets.length} widget(s).`);
      return true;
    } catch (error) {
      showImportError(error.message);
      return false;
    }
  }

  if (els.importButton) els.importButton.addEventListener('click', openImportDialog);
  if (els.importCancel) {
    els.importCancel.addEventListener('click', () => {
      clearImportError();
      closeImportDialog();
    });
  }
  if (els.importForm) {
    els.importForm.addEventListener('submit', (event) => {
      event.preventDefault();
      const text = els.importText ? els.importText.value : '';
      if (!text.trim()) {
        showImportError('Paste a layout JSON document or choose a file first.');
        return;
      }
      importText(text);
    });
  }
  if (els.importDialog) {
    els.importDialog.addEventListener('cancel', (event) => {
      event.preventDefault();
      clearImportError();
      closeImportDialog();
    });
  }

  if (els.importFile && els.fileInput) {
    els.importFile.addEventListener('click', () => els.fileInput.click());
    els.fileInput.addEventListener('change', () => {
      const file = els.fileInput.files && els.fileInput.files[0];
      if (!file) return;
      file.text()
        .then((text) => importText(text))
        .catch((error) => showImportError(error.message))
        .finally(() => {
          els.fileInput.value = '';
        });
    });
  }

  if (els.exportButton) {
    els.exportButton.addEventListener('click', () => {
      try {
        const text = serializeLayout(state);
        const blob = new Blob([text], { type: 'application/json' });
        const url = URL.createObjectURL(blob);
        const link = document.createElement('a');
        link.href = url;
        link.download = safeFilename(state.layout.id);
        link.rel = 'noopener';
        document.body.appendChild(link);
        link.click();
        link.remove();
        window.setTimeout(() => URL.revokeObjectURL(url), 1000);
        setStatus(`Exported ${link.download}.`);
      } catch (error) {
        setStatus(`Export failed: ${error.message}`, 'error');
      }
    });
  }

  function isTypingTarget(target) {
    if (!target || !target.tagName) return false;
    const tag = target.tagName.toLowerCase();
    return tag === 'input'
      || tag === 'textarea'
      || tag === 'select'
      || target.isContentEditable === true;
  }

  document.addEventListener('keydown', (event) => {
    if (isTypingTarget(event.target)) return;
    const modifier = event.ctrlKey || event.metaKey;
    const key = event.key;

    if (modifier && key.toLowerCase() === 'z') {
      event.preventDefault();
      const before = state;
      state = event.shiftKey ? redo(state) : undo(state);
      renderAll({ focusId: state.selectionId });
      if (state !== before) setStatus(event.shiftKey ? 'Redo.' : 'Undo.');
      return;
    }
    if (modifier && key.toLowerCase() === 'y') {
      event.preventDefault();
      const before = state;
      state = redo(state);
      renderAll({ focusId: state.selectionId });
      if (state !== before) setStatus('Redo.');
      return;
    }
    if (modifier && key.toLowerCase() === 'd') {
      event.preventDefault();
      invoke((current) => duplicateWidget(current, current.selectionId), 'Duplicated widget.');
      return;
    }
    if (modifier) return;

    if (key === 'Escape') {
      state = selectWidget(state, null);
      renderAll();
      setStatus('Selection cleared.');
      return;
    }

    const node = event.target.closest ? event.target.closest('.widget-box') : null;
    const id = node ? node.dataset.widgetId : state.selectionId;
    if (!id) return;
    const focusIsCanvas = event.target === els.canvas
      || event.target === document.body
      || Boolean(node);
    if (!focusIsCanvas) return;

    if (key === 'Delete' || key === 'Backspace') {
      event.preventDefault();
      state = removeWidget(state, id);
      renderAll();
      setStatus('Removed widget.');
      return;
    }
    if ((key === 'Enter' || key === ' ') && node) {
      event.preventDefault();
      state = selectWidget(state, id);
      renderAll({ focusId: id });
      setStatus('Selected widget.');
      return;
    }

    const directions = {
      ArrowLeft: [-1, 0],
      ArrowRight: [1, 0],
      ArrowUp: [0, -1],
      ArrowDown: [0, 1],
    };
    const direction = directions[key];
    if (!direction) return;
    const widget = findWidget(state, id);
    if (!widget) return;
    event.preventDefault();
    const rect = rectOf(widget);
    const step = event.shiftKey ? state.grid * 5 : state.grid;
    const next = moveWidget(
      state,
      id,
      rect.x + direction[0] * step,
      rect.y + direction[1] * step,
    );
    if (next !== state) {
      state = next;
      renderAll({ focusId: id });
      setStatus('Nudged widget.');
    }
  });

  renderAll();
  setStatus('Ready. Add a widget or import a layout.');
}

if (typeof document !== 'undefined' && byId('editor-canvas')) {
  initEditor();
}
