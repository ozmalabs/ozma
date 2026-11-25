// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Tests for the dependency-free layout editor.
 *
 * Run with: node --test web/display/editor/editor.test.mjs
 */

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  DEFAULT_GRID,
  DEFAULT_HISTORY_LIMIT,
  MAX_GRID,
  MAX_ZOOM,
  MIN_GRID,
  MIN_ZOOM,
  WIDGET_KINDS,
  addWidget,
  createEditorState,
  defaultLayout,
  duplicateWidget,
  getWidgetId,
  lowerWidget,
  moveWidget,
  parseLayout,
  raiseWidget,
  redo,
  removeWidget,
  resetLayout,
  resizeWidget,
  sanitizeColor,
  selectWidget,
  serializeLayout,
  setGrid,
  setLayoutMeta,
  setZoom,
  snapRect,
  undo,
  updateWidget,
  widgetDefaults,
} from './editor.js';

const HERE = new URL('./', import.meta.url);

function makeLayout(overrides = {}) {
  return {
    id: 'test-layout',
    name: 'Test Layout',
    width: 200,
    height: 150,
    background: '#0a0a0f',
    refresh_hz: 10,
    widgets: [],
    ...overrides,
  };
}

function widgetIdOf(state, index = 0) {
  return getWidgetId(state.layout.widgets[index]);
}

/* ------------------------------------------------------------------------- */
/* Widget defaults                                                           */
/* ------------------------------------------------------------------------- */

test('widgetDefaults covers every supported widget kind', () => {
  for (const kind of WIDGET_KINDS) {
    const widget = widgetDefaults(kind);
    assert.equal(widget.type, kind);
    assert.ok(widget.w > 0 && widget.h > 0, `${kind} needs a positive size`);
    assert.equal(typeof widget.x, 'number');
    assert.equal(typeof widget.y, 'number');
    assert.equal(typeof widget.min, 'number');
    assert.equal(typeof widget.max, 'number');
    assert.equal(typeof widget.color, 'string');
    assert.equal(typeof widget.decimals, 'number');
  }
});

test('widgetDefaults returns independent objects and applies overrides', () => {
  const first = widgetDefaults('gauge');
  const second = widgetDefaults('gauge');
  first.x = 40;
  first.label = 'Changed';
  assert.equal(second.x, 0);
  assert.equal(second.label, 'CPU');

  const custom = widgetDefaults('bar', { x: 30, y: 40, label: 'Load', color: '#ff0000' });
  assert.equal(custom.type, 'bar');
  assert.equal(custom.x, 30);
  assert.equal(custom.y, 40);
  assert.equal(custom.label, 'Load');
  assert.equal(custom.color, '#ff0000');
  assert.throws(() => widgetDefaults('not-a-widget'), RangeError);
});

test('gauge, sparkline, and VU defaults carry their configuration fields', () => {
  const gauge = widgetDefaults('gauge');
  assert.equal(gauge.min, 0);
  assert.equal(gauge.max, 100);
  assert.equal(gauge.warn, 70);
  assert.equal(gauge.crit, 90);

  const sparkline = widgetDefaults('sparkline');
  assert.equal(sparkline.history_points, 120);
  assert.equal(sparkline.history_seconds, 60);
  assert.equal(sparkline.fill, false);

  const vu = widgetDefaults('vu_meter');
  assert.equal(vu.attack_ms, 10);
  assert.equal(vu.release_ms, 300);
  assert.equal(vu.falloff_db_per_sec, 24);
  assert.equal(vu.peak_hold_ms, 1500);
  assert.equal(vu.peak_falloff_db_per_sec, 12);
  assert.equal(vu.segments, 24);
  assert.equal(vu.min, -60);
  assert.equal(vu.max, 0);
});

test('sanitizeColor accepts colour literals and rejects network or code values', () => {
  assert.equal(sanitizeColor('#abc'), '#abc');
  assert.equal(sanitizeColor('#5b6fff'), '#5b6fff');
  assert.equal(sanitizeColor('rgb(10, 20, 30)'), 'rgb(10, 20, 30)');
  assert.equal(sanitizeColor('steelblue'), 'steelblue');
  assert.equal(sanitizeColor('url(https://example.test/a.png)'), '#5b6fff');
  assert.equal(sanitizeColor('javascript:alert(1)'), '#5b6fff');
  assert.equal(sanitizeColor('expression(alert(1))'), '#5b6fff');
  assert.equal(sanitizeColor('', '#123456'), '#123456');
});

/* ------------------------------------------------------------------------- */
/* Add, remove, select                                                       */
/* ------------------------------------------------------------------------- */

test('createEditorState creates an empty default layout or parses a document', () => {
  const state = createEditorState();
  assert.equal(state.layout.id, 'editor-layout');
  assert.ok(state.layout.width > 0 && state.layout.height > 0);
  assert.deepEqual(state.layout.widgets, []);
  assert.equal(state.selectionId, null);
  assert.equal(state.grid, DEFAULT_GRID);
  assert.equal(state.zoom, 1);
  assert.equal(state.history.past.length, 0);

  const parsed = createEditorState(makeLayout());
  assert.equal(parsed.layout.id, 'test-layout');
  assert.equal(parsed.layout.width, 200);

  const aliased = createEditorState({ gridSize: 20, zoomLevel: 0.5, maxHistory: 7 });
  assert.equal(aliased.grid, 20);
  assert.equal(aliased.zoom, 0.5);
  assert.equal(aliased.history.limit, 7);
});

test('addWidget appends a default widget, selects it, and records history', () => {
  let state = createEditorState(makeLayout());
  state = addWidget(state, 'gauge');

  assert.equal(state.layout.widgets.length, 1);
  const widget = state.layout.widgets[0];
  assert.equal(widget.type, 'gauge');
  assert.equal(widget.label, 'CPU');
  assert.ok(widget.x % state.grid === 0);
  assert.ok(widget.y % state.grid === 0);
  assert.equal(state.selectionId, widgetIdOf(state));
  assert.equal(state.dirty, true);
  assert.equal(state.history.past.length, 1);
  assert.equal(state.nextId, 2);

  state = addWidget(state, 'bar', { x: 20, y: 20, w: 80, h: 40 });
  assert.equal(state.layout.widgets.length, 2);
  assert.notEqual(widgetIdOf(state, 0), widgetIdOf(state, 1));
  assert.equal(state.layout.widgets[1].x, 20);
  assert.equal(state.layout.widgets[1].y, 20);
  assert.ok(!serializeLayout(state).includes('_editorId'));
});

test('addWidget clamps and snaps initial geometry to the canvas', () => {
  let state = createEditorState(makeLayout({ width: 100, height: 100 }));
  state = addWidget(state, 'bar', { x: 95, y: -20, w: 80, h: 40 });
  const widget = state.layout.widgets[0];
  assert.equal(widget.x, 20);
  assert.equal(widget.y, 0);
  assert.equal(widget.w, 80);
  assert.equal(widget.h, 40);
});

test('removeWidget removes by id and undo restores the widget', () => {
  let state = createEditorState(makeLayout());
  state = addWidget(state, 'gauge');
  state = addWidget(state, 'bar');
  const firstId = widgetIdOf(state, 0);

  state = removeWidget(state, firstId);
  assert.equal(state.layout.widgets.length, 1);
  assert.equal(state.selectionId, widgetIdOf(state, 0));

  state = undo(state);
  assert.equal(state.layout.widgets.length, 2);
  assert.equal(state.layout.widgets[0].type, 'gauge');

  const unchanged = removeWidget(state, 'missing-id');
  assert.equal(unchanged, state);
});

test('selectWidget selects, clears, and never records history', () => {
  let state = createEditorState(makeLayout());
  state = addWidget(state, 'gauge');
  const id = widgetIdOf(state);
  const historyLength = state.history.past.length;

  state = selectWidget(state, id);
  assert.equal(state.selectionId, id);
  assert.equal(state.history.past.length, historyLength);

  state = selectWidget(state, 'missing');
  assert.equal(state.selectionId, null);
  assert.equal(state.history.past.length, historyLength);

  state = selectWidget(state, id);
  state = selectWidget(state, null);
  assert.equal(state.selectionId, null);
});

/* ------------------------------------------------------------------------- */
/* Snap, move, resize, clamp                                                 */
/* ------------------------------------------------------------------------- */

test('snapRect snaps to the grid and clamps to bounds', () => {
  assert.deepEqual(
    snapRect({ x: 13, y: 27, w: 37, h: 44 }, 10),
    { x: 10, y: 30, w: 40, h: 40 },
  );
  assert.deepEqual(
    snapRect({ x: -20, y: -30, w: 10, h: 10 }, 10),
    { x: 0, y: 0, w: 10, h: 10 },
  );
  assert.deepEqual(
    snapRect({ x: 95, y: 95, w: 80, h: 80 }, 10, { width: 100, height: 100 }),
    { x: 20, y: 20, w: 80, h: 80 },
  );
  assert.deepEqual(
    snapRect({ x: 0, y: 0, w: 500, h: 500 }, 10, { width: 100, height: 90 }),
    { x: 0, y: 0, w: 100, h: 90 },
  );
  assert.deepEqual(
    snapRect({ x: 0, y: 0, w: 0, h: 0 }, 10),
    { x: 0, y: 0, w: 10, h: 10 },
  );
  // Invalid grids fall back to a sane step.
  assert.deepEqual(
    snapRect({ x: 7, y: 7, w: 7, h: 7 }, 0),
    { x: 7, y: 7, w: 7, h: 7 },
  );
  // The bounds may be supplied as width/height arguments for convenience.
  assert.deepEqual(
    snapRect({ x: 90, y: 90, w: 40, h: 40 }, 10, 100, 100),
    { x: 60, y: 60, w: 40, h: 40 },
  );
});

test('moveWidget snaps, clamps, and accepts object positions', () => {
  let state = createEditorState(makeLayout());
  state = addWidget(state, 'gauge', { x: 10, y: 10, w: 50, h: 50 });
  const id = widgetIdOf(state);

  state = moveWidget(state, id, 13, 27);
  assert.equal(state.layout.widgets[0].x, 10);
  assert.equal(state.layout.widgets[0].y, 30);

  state = moveWidget(state, id, { x: 999, y: 999 });
  assert.equal(state.layout.widgets[0].x, 150);
  assert.equal(state.layout.widgets[0].y, 100);

  state = moveWidget(state, id, -50, -50);
  assert.equal(state.layout.widgets[0].x, 0);
  assert.equal(state.layout.widgets[0].y, 0);

  const unchanged = moveWidget(state, id, 0, 0);
  assert.equal(unchanged, state);
});

test('resizeWidget snaps size, enforces a minimum, and clamps to bounds', () => {
  let state = createEditorState(makeLayout());
  state = addWidget(state, 'gauge', { x: 10, y: 10, w: 50, h: 50 });
  const id = widgetIdOf(state);

  state = resizeWidget(state, id, { w: 37, h: 44 });
  assert.equal(state.layout.widgets[0].w, 40);
  assert.equal(state.layout.widgets[0].h, 40);
  assert.equal(state.layout.widgets[0].x, 10);
  assert.equal(state.layout.widgets[0].y, 10);

  state = resizeWidget(state, id, 0, 0);
  assert.equal(state.layout.widgets[0].w, state.grid);
  assert.equal(state.layout.widgets[0].h, state.grid);

  state = resizeWidget(state, id, { w: 1000, h: 1000 });
  assert.equal(state.layout.widgets[0].w, 200);
  assert.equal(state.layout.widgets[0].h, 150);

  const unchanged = resizeWidget(state, id, { w: 200, h: 150 });
  assert.equal(unchanged, state);
});

test('updateWidget changes VU ballistics and keeps unknown widget fields', () => {
  let state = createEditorState(makeLayout());
  state = addWidget(state, 'vu_meter', { custom_field: { keep: true } });
  const id = widgetIdOf(state);
  const customBefore = state.layout.widgets[0].custom_field;

  state = updateWidget(state, id, {
    attack_ms: 5,
    release_ms: 250,
    falloff_db_per_sec: 18,
    peak_hold_ms: 900,
    peak_falloff_db_per_sec: 9,
    segments: 32,
  });

  const widget = state.layout.widgets[0];
  assert.equal(widget.attack_ms, 5);
  assert.equal(widget.release_ms, 250);
  assert.equal(widget.falloff_db_per_sec, 18);
  assert.equal(widget.peak_hold_ms, 900);
  assert.equal(widget.peak_falloff_db_per_sec, 9);
  assert.equal(widget.segments, 32);
  assert.deepEqual(widget.custom_field, customBefore);
  assert.deepEqual(customBefore, { keep: true });
});

test('updateWidget sanitises colours and ignores prototype keys', () => {
  let state = createEditorState(makeLayout());
  state = addWidget(state, 'bar');
  const id = widgetIdOf(state);

  state = updateWidget(state, id, {
    color: 'url(https://example.test/evil.png)',
    '__proto__': { polluted: true },
    constructor: 'nope',
    unknown: 'kept out',
  });
  assert.equal(state.layout.widgets[0].color, '#5b6fff');
  assert.equal(state.layout.widgets[0].unknown, undefined);
  assert.equal({}.polluted, undefined);
});

/* ------------------------------------------------------------------------- */
/* Undo and redo                                                             */
/* ------------------------------------------------------------------------- */

test('undo and redo step through layout changes', () => {
  let state = createEditorState(makeLayout());
  state = addWidget(state, 'gauge', { w: 50, h: 50 });
  const id = widgetIdOf(state);
  state = moveWidget(state, id, 40, 40);

  state = undo(state);
  assert.equal(state.layout.widgets[0].x, 10);

  state = redo(state);
  assert.equal(state.layout.widgets[0].x, 40);

  state = undo(state);
  state = undo(state);
  assert.equal(state.layout.widgets.length, 0);
  assert.equal(state.selectionId, null);
});

test('history is bounded and a new action clears redo', () => {
  let state = createEditorState(makeLayout(), { historyLimit: 3 });
  for (let index = 0; index < 5; index += 1) state = addWidget(state, 'card');

  assert.equal(state.layout.widgets.length, 5);
  assert.equal(state.history.past.length, 3);
  assert.equal(state.history.limit, 3);

  state = undo(state);
  state = undo(state);
  state = undo(state);
  assert.equal(state.layout.widgets.length, 2);
  assert.equal(state.history.past.length, 0);

  const unchanged = undo(state);
  assert.equal(unchanged, state);

  state = redo(state);
  assert.equal(state.layout.widgets.length, 3);
  assert.equal(state.history.future.length, 2);

  state = addWidget(state, 'status');
  assert.equal(state.history.future.length, 0);
  const noRedo = redo(state);
  assert.equal(noRedo, state);
});

test('DEFAULT_HISTORY_LIMIT bounds a long edit run', () => {
  let state = createEditorState(makeLayout());
  for (let index = 0; index < DEFAULT_HISTORY_LIMIT + 5; index += 1) {
    state = addWidget(state, 'card');
  }
  assert.equal(state.history.past.length, DEFAULT_HISTORY_LIMIT);
});

/* ------------------------------------------------------------------------- */
/* Parse and serialise                                                       */
/* ------------------------------------------------------------------------- */

const SOURCE_TEXT = `{
  "id": "custom-layout",
  "name": "Custom",
  "width": 300,
  "height": 200,
  "background": "#101010",
  "refresh_hz": 4,
  "extensions": { "org.example.vendor": { "keep": true } },
  "unknown_top": ["a", 1, false],
  "widgets": [
    {
      "type": "gauge",
      "x": 10, "y": 20, "w": 80, "h": 80,
      "metric": "@active.TCPU",
      "unknown_widget": { "nested": [1, 2, 3] },
      "_editorId": "source-provided",
      "extensions": { "org.example.widget": "keep" }
    }
  ]
}`;

test('parseLayout and serializeLayout round-trip an unchanged source byte-for-byte', () => {
  const state = parseLayout(SOURCE_TEXT);
  assert.equal(state.layout.widgets.length, 1);
  assert.equal(serializeLayout(state), SOURCE_TEXT);
});

test('changing a widget preserves unknown top-level and widget fields', () => {
  let state = parseLayout(SOURCE_TEXT);
  const id = widgetIdOf(state);
  state = moveWidget(state, id, 50, 60);
  state = updateWidget(state, id, { label: 'Edited' });

  const output = JSON.parse(serializeLayout(state));
  assert.equal(output.id, 'custom-layout');
  assert.deepEqual(output.extensions, { 'org.example.vendor': { keep: true } });
  assert.deepEqual(output.unknown_top, ['a', 1, false]);
  assert.deepEqual(output.widgets[0].unknown_widget, { nested: [1, 2, 3] });
  assert.deepEqual(output.widgets[0].extensions, { 'org.example.widget': 'keep' });
  assert.equal(output.widgets[0]._editorId, 'source-provided');
  assert.equal(output.widgets[0].x, 50);
  assert.equal(output.widgets[0].y, 60);
  assert.equal(output.widgets[0].label, 'Edited');
});

test('parseLayout accepts a wrapped layout and a definition_json payload', () => {
  const layout = makeLayout({
    widgets: [{ type: 'text', x: 0, y: 0, w: 100, h: 40, text: 'hello' }],
  });
  const wrapped = parseLayout({ type: 'definition', layout });
  assert.equal(wrapped.layout.id, 'test-layout');
  assert.equal(wrapped.layout.widgets[0].text, 'hello');
  assert.equal(JSON.parse(serializeLayout(wrapped)).layout, undefined);

  const nested = parseLayout({ definition_json: JSON.stringify(layout) });
  assert.equal(nested.layout.widgets.length, 1);
  assert.equal(nested.layout.widgets[0].type, 'text');
});

test('parseLayout tolerates a layout without optional fields', () => {
  const state = parseLayout('{"id":"minimal","widgets":[]}');
  assert.equal(state.layout.id, 'minimal');
  assert.deepEqual(state.layout.widgets, []);
  assert.equal(serializeLayout(state), '{"id":"minimal","widgets":[]}');
});

test('parseLayout rejects invalid documents', () => {
  assert.throws(() => parseLayout('{not json'), /Invalid layout JSON/);
  assert.throws(() => parseLayout('[]'), /must be an object/);
  assert.throws(() => parseLayout('{}'), /does not look like/);
  assert.throws(() => parseLayout({ id: 'x', widgets: {} }), /widgets must be an array/);
  assert.throws(() => parseLayout({ id: 'x', widgets: [{}] }), /missing a type/);
  assert.throws(() => parseLayout({ id: 'x', widgets: [null] }), /must be an object/);
});

test('layout metadata updates are preserved and clamps widgets when shrunk', () => {
  let state = createEditorState(makeLayout({ width: 400, height: 300 }));
  state = addWidget(state, 'gauge', { x: 300, y: 200, w: 100, h: 100 });
  state = setLayoutMeta(state, {
    id: 'resized',
    name: 'Resized',
    width: 180,
    height: 160,
    background: '#abcdef',
    refresh_hz: 5,
  });

  assert.equal(state.layout.id, 'resized');
  assert.equal(state.layout.name, 'Resized');
  assert.equal(state.layout.width, 180);
  assert.equal(state.layout.height, 160);
  assert.equal(state.layout.background, '#abcdef');
  assert.equal(state.layout.refresh_hz, 5);

  const widget = state.layout.widgets[0];
  assert.equal(widget.x, 80);
  assert.equal(widget.y, 60);
  assert.equal(widget.w, 100);
  assert.equal(widget.h, 100);
});

/* ------------------------------------------------------------------------- */
/* Grid, zoom, z-order, duplicate, reset                                     */
/* ------------------------------------------------------------------------- */

test('grid and zoom are clamped view settings and never serialised', () => {
  let state = createEditorState(makeLayout());
  assert.equal(state.grid, DEFAULT_GRID);
  assert.equal(state.zoom, 1);

  state = setGrid(state, 0);
  assert.equal(state.grid, MIN_GRID);
  state = setGrid(state, 999);
  assert.equal(state.grid, MAX_GRID);
  state = setGrid(state, 12.6);
  assert.equal(state.grid, 13);

  state = setZoom(state, 0);
  assert.equal(state.zoom, MIN_ZOOM);
  state = setZoom(state, 99);
  assert.equal(state.zoom, MAX_ZOOM);
  state = setZoom(state, 1.25);
  assert.equal(state.zoom, 1.25);

  const output = JSON.parse(serializeLayout(state));
  assert.equal(output.grid, undefined);
  assert.equal(output.zoom, undefined);
});

test('duplicateWidget offsets a copy and z-order helpers reorder widgets', () => {
  let state = createEditorState(makeLayout());
  state = addWidget(state, 'gauge', { x: 10, y: 10, w: 40, h: 40 });
  state = addWidget(state, 'bar', { x: 60, y: 10, w: 40, h: 40 });
  const firstId = widgetIdOf(state, 0);

  state = duplicateWidget(state, firstId);
  assert.equal(state.layout.widgets.length, 3);
  assert.equal(state.layout.widgets[2].x, 20);
  assert.equal(state.layout.widgets[2].y, 20);
  assert.equal(state.selectionId, widgetIdOf(state, 2));

  state = lowerWidget(state, getWidgetId(state.layout.widgets[2]));
  assert.equal(getWidgetId(state.layout.widgets[1]), state.selectionId);
  state = raiseWidget(state, state.selectionId);
  assert.equal(getWidgetId(state.layout.widgets[2]), state.selectionId);
  const unchanged = raiseWidget(state, state.selectionId);
  assert.equal(unchanged, state);
});

test('resetLayout restores the default document and remains undoable', () => {
  let state = createEditorState(makeLayout());
  state = addWidget(state, 'gauge');
  const before = state.layout.widgets.length;
  state = resetLayout(state);
  assert.equal(state.layout.widgets.length, 0);
  assert.equal(state.layout.id, defaultLayout().id);
  assert.equal(state.selectionId, null);
  state = undo(state);
  assert.equal(state.layout.widgets.length, before);
});

/* ------------------------------------------------------------------------- */
/* Static security and DOM wiring smoke checks                               */
/* ------------------------------------------------------------------------- */

const JS_SOURCE = readFileSync(new URL('editor.js', HERE), 'utf8');
const HTML_SOURCE = readFileSync(new URL('index.html', HERE), 'utf8');
const CSS_SOURCE = readFileSync(new URL('editor.css', HERE), 'utf8');

test('index.html declares the required local-static CSP', () => {
  const csp = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; object-src 'none'; base-uri 'none'; form-action 'none'";
  assert.ok(HTML_SOURCE.includes('http-equiv="Content-Security-Policy"'));
  assert.ok(HTML_SOURCE.includes(csp), 'CSP content must match the required policy');
  assert.ok(HTML_SOURCE.includes('<script type="module" src="editor.js"></script>'));
  assert.ok(HTML_SOURCE.includes('<link rel="stylesheet" href="editor.css">'));
});

test('sources contain no code execution, HTML injection, remote assets, or gridstack', () => {
  for (const source of [JS_SOURCE, HTML_SOURCE, CSS_SOURCE]) {
    assert.doesNotMatch(source, /\beval\s*\(/);
    assert.doesNotMatch(source, /new\s+Function\s*\(/);
    assert.doesNotMatch(source, /innerHTML/);
    assert.doesNotMatch(source, /outerHTML/);
    assert.doesNotMatch(source, /insertAdjacentHTML/);
    assert.doesNotMatch(source, /document\.write/);
    assert.doesNotMatch(source, /gridstack/i);
    assert.doesNotMatch(source, /https?:\/\//i);
    assert.doesNotMatch(source, /@import/i);
    assert.doesNotMatch(source, /fetch\s*\(/);
    assert.doesNotMatch(source, /XMLHttpRequest/);
    assert.doesNotMatch(source, /WebSocket/);
  }
  assert.doesNotMatch(CSS_SOURCE, /url\s*\(/i);
  assert.doesNotMatch(JS_SOURCE, /\.innerText\s*=\s*[^;]*\+/);
});

test('accessibility hooks are present', () => {
  assert.ok(HTML_SOURCE.includes('aria-live="polite"'));
  assert.ok(HTML_SOURCE.includes('role="alert"'));
  assert.ok(HTML_SOURCE.includes('role="status"'));
  assert.ok(HTML_SOURCE.includes('tabindex="0"'));
  assert.match(HTML_SOURCE, /<label[^>]+for="[^"]+"/);
  assert.match(CSS_SOURCE, /:focus-visible/);
  assert.match(CSS_SOURCE, /@media\s*\(prefers-reduced-motion:\s*reduce\)/);
  assert.match(CSS_SOURCE, /transition:\s*none\s*!important/);
});

test('every element id referenced by the controller exists in index.html', () => {
  const referenced = new Set();
  const pattern = /byId\('([^']+)'\)/g;
  let match = pattern.exec(JS_SOURCE);
  while (match) {
    referenced.add(match[1]);
    match = pattern.exec(JS_SOURCE);
  }
  assert.ok(referenced.size > 20, 'expected the controller to look up its DOM');
  for (const id of referenced) {
    assert.ok(HTML_SOURCE.includes(`id="${id}"`), `index.html is missing id="${id}"`);
  }
});

test('the pure model API is exported for Node tests', () => {
  for (const name of [
    'createEditorState',
    'addWidget',
    'removeWidget',
    'moveWidget',
    'resizeWidget',
    'snapRect',
    'selectWidget',
    'undo',
    'redo',
    'serializeLayout',
    'parseLayout',
    'widgetDefaults',
  ]) {
    assert.match(JS_SOURCE, new RegExp(`export function ${name}\\s*\\(`));
  }
});
