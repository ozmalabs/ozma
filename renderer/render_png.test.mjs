// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Canvas-backed PNG/JPEG render tests using a recording fake canvas.
 *
 * The native `@napi-rs/canvas` package is optional and may be absent in CI.
 * These tests prove the semantic IR draw path is testable without it, that the
 * runtime can be injected, and that SVG rendering never requires a canvas.
 *
 * Run with: node --test renderer/*.test.mjs
 */

import test, { afterEach } from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

import templates from './templates.js';
import canvasRuntime from './canvas_runtime.js';

const { renderTemplate, renderIr } = templates;
const {
  CanvasUnavailableError,
  encodeCanvas,
  loadCanvasFactory,
  resetCanvasFactory,
  setCanvasFactory,
} = canvasRuntime;

const nodeRequire = createRequire(import.meta.url);

const CONTEXT_METHODS = [
  'save',
  'restore',
  'beginPath',
  'closePath',
  'arc',
  'fill',
  'stroke',
  'fillRect',
  'strokeRect',
  'moveTo',
  'lineTo',
  'quadraticCurveTo',
  'rect',
  'clip',
  'fillText',
  'roundRect',
];

const CONTEXT_PROPERTIES = [
  'fillStyle',
  'strokeStyle',
  'lineWidth',
  'lineCap',
  'font',
  'textAlign',
  'textBaseline',
  'globalAlpha',
];

function createRecordingContext(calls) {
  const ctx = {};
  for (const method of CONTEXT_METHODS) {
    ctx[method] = (...args) => {
      calls.push({ op: method, args });
    };
  }
  for (const property of CONTEXT_PROPERTIES) {
    let value;
    Object.defineProperty(ctx, property, {
      enumerable: true,
      get() {
        return value;
      },
      set(next) {
        value = next;
        calls.push({ op: 'set', property, value: next });
      },
    });
  }
  return ctx;
}

function createFakeCanvasFactory() {
  const calls = [];
  const canvases = [];
  const ctx = createRecordingContext(calls);

  function createCanvas(width, height) {
    const canvas = {
      width,
      height,
      getContext(type) {
        calls.push({ op: 'getContext', args: [type] });
        return ctx;
      },
      toBuffer(format) {
        calls.push({ op: 'toBuffer', args: [format] });
        return Buffer.from(`fake-canvas:${format}:${width}x${height}`);
      },
      toSVG() {
        calls.push({ op: 'toSVG', args: [] });
        return `<svg width="${width}" height="${height}"/>`;
      },
    };
    canvases.push(canvas);
    return canvas;
  }

  return { createCanvas, calls, canvases, ctx };
}

function callsNamed(calls, name) {
  return calls.filter((call) => call.op === name);
}

function textValues(calls) {
  return callsNamed(calls, 'fillText').map((call) => String(call.args[0]));
}

function layout(widgets, overrides = {}) {
  return {
    id: 'canvas-runtime-test',
    name: 'Canvas runtime test',
    width: 320,
    height: 240,
    background: '#0a0a0f',
    widgets,
    ...overrides,
  };
}

async function renderWidget(widget, data = {}, options = {}) {
  const { width = 320, height = 240, format = 'png' } = options;
  const fake = createFakeCanvasFactory();
  const buffer = await renderIr(
    layout([widget], { width, height }),
    data,
    width,
    height,
    format,
    { createCanvas: fake.createCanvas },
  );
  return { buffer, ...fake };
}

afterEach(() => {
  resetCanvasFactory();
});

/* ------------------------------------------------------------------------- */
/* Lazy/injectable canvas runtime                                             */
/* ------------------------------------------------------------------------- */

test('loadCanvasFactory throws CanvasUnavailableError when canvas is absent', (t) => {
  resetCanvasFactory();
  let installed = false;
  try {
    nodeRequire.resolve('@napi-rs/canvas');
    installed = true;
  } catch {
    installed = false;
  }
  if (installed) {
    t.skip('@napi-rs/canvas is installed; absence cannot be exercised here');
    return;
  }

  assert.throws(
    () => loadCanvasFactory(),
    (err) => {
      assert.ok(err instanceof CanvasUnavailableError);
      assert.equal(err.name, 'CanvasUnavailableError');
      assert.equal(err.code, 'canvas_unavailable');
      assert.match(err.message, /npm install @napi-rs\/canvas/);
      return true;
    },
  );
});

test('loadCanvasFactory returns the injected factory after setCanvasFactory', () => {
  resetCanvasFactory();
  const fake = createFakeCanvasFactory();
  setCanvasFactory(fake.createCanvas);

  const factory = loadCanvasFactory();
  assert.equal(factory, fake.createCanvas);

  const canvas = factory(12, 34);
  assert.equal(canvas.width, 12);
  assert.equal(canvas.height, 34);
});

test('encodeCanvas selects PNG/JPEG/SVG and rejects unknown formats', () => {
  const fake = createFakeCanvasFactory();
  const canvas = fake.createCanvas(16, 8);

  const png = encodeCanvas(canvas, 'png');
  const jpeg = encodeCanvas(canvas, 'image/jpeg');
  const jpg = encodeCanvas(canvas, 'jpg');
  assert.ok(Buffer.isBuffer(png));
  assert.ok(Buffer.isBuffer(jpeg));
  assert.ok(Buffer.isBuffer(jpg));

  assert.deepEqual(
    callsNamed(fake.calls, 'toBuffer').map((call) => call.args[0]),
    ['image/png', 'image/jpeg', 'image/jpeg'],
  );

  assert.equal(encodeCanvas({ toSVG: () => '<svg/>' }, 'svg').toString(), '<svg/>');
  assert.equal(
    encodeCanvas({ toBuffer: (mime) => Buffer.from(`<%${mime}%>`) }, 'image/svg+xml').toString(),
    '<%image/svg+xml%>',
  );

  assert.throws(
    () => encodeCanvas(canvas, 'gif'),
    (err) => {
      assert.equal(err.code, 'unsupported_format');
      assert.match(err.message, /unsupported render format "gif"/);
      return true;
    },
  );
});

/* ------------------------------------------------------------------------- */
/* Semantic IR draw operations                                                */
/* ------------------------------------------------------------------------- */

test('gauge renders track/value arcs, ticks, value text and label', async () => {
  const { calls } = await renderWidget({
    type: 'gauge',
    x: 10,
    y: 10,
    w: 180,
    h: 180,
    metric: 'cpu',
    min: 0,
    max: 100,
    label: 'CPU',
    unit: '%',
    decimals: 0,
    warn: 70,
    crit: 90,
  }, { cpu: 50 });

  const arcs = callsNamed(calls, 'arc');
  assert.ok(arcs.length >= 2);
  assert.equal(arcs[0].args[2], arcs[1].args[2]);

  const start = (135 * Math.PI) / 180;
  const trackEnd = (405 * Math.PI) / 180;
  const valueEnd = (270 * Math.PI) / 180;
  assert.ok(Math.abs(arcs[0].args[3] - start) < 1e-9);
  assert.ok(Math.abs(arcs[0].args[4] - trackEnd) < 1e-9);
  assert.ok(Math.abs(arcs[1].args[3] - start) < 1e-9);
  assert.ok(Math.abs(arcs[1].args[4] - valueEnd) < 1e-9);

  // Five tick marks are drawn as moveTo/lineTo pairs.
  assert.ok(callsNamed(calls, 'moveTo').length >= 5);
  assert.ok(callsNamed(calls, 'lineTo').length >= 5);

  const texts = textValues(calls);
  assert.ok(texts.includes('50%'));
  assert.ok(texts.includes('CPU'));
  assert.ok(callsNamed(calls, 'fillRect').length >= 1);
});

test('bar renders a track and proportional fill with value text', async () => {
  const { calls } = await renderWidget({
    type: 'bar',
    x: 10,
    y: 10,
    w: 180,
    h: 20,
    metric: 'cpu',
    min: 0,
    max: 100,
    label: 'CPU',
    unit: '%',
    decimals: 0,
    radius: 3,
  }, { cpu: 50 });

  const rects = callsNamed(calls, 'roundRect');
  assert.equal(rects.length, 2);
  assert.deepEqual(rects[0].args.slice(0, 4), [10, 10, 180, 20]);
  assert.deepEqual(rects[1].args.slice(0, 4), [10, 10, 90, 20]);
  assert.ok(callsNamed(calls, 'fill').length >= 2);

  const texts = textValues(calls);
  assert.ok(texts.includes('50%'));
  assert.ok(texts.includes('CPU'));
});

test('text widget interpolates data and draws it', async () => {
  const { calls } = await renderWidget({
    type: 'text',
    x: 0,
    y: 0,
    w: 200,
    h: 40,
    text: 'CPU {cpu}',
    metric: 'cpu',
    font_size: 16,
  }, { cpu: 42 });

  const fillText = callsNamed(calls, 'fillText');
  assert.ok(fillText.length >= 1);
  assert.equal(fillText[0].args[0], 'CPU 42.0');
  assert.equal(fillText[0].args[3], 200);
});

test('sparkline renders one moveTo plus per-point lineTo and value text', async () => {
  const { calls } = await renderWidget({
    type: 'sparkline',
    x: 10,
    y: 10,
    w: 180,
    h: 60,
    values: [0, 50, 100],
    label: 'Load',
    unit: '%',
    decimals: 0,
  });

  assert.equal(callsNamed(calls, 'moveTo').length, 1);
  assert.equal(callsNamed(calls, 'lineTo').length, 2);

  const texts = textValues(calls);
  assert.ok(texts.includes('Load'));
  assert.ok(texts.includes('100%'));
});

test('status renders a dot and online/offline text', async () => {
  const widget = {
    type: 'status',
    x: 10,
    y: 10,
    w: 200,
    h: 40,
    metric: 'node',
    label: 'Node A',
  };

  const online = await renderWidget(widget, { node: true });
  assert.ok(callsNamed(online.calls, 'arc').length >= 1);
  assert.ok(textValues(online.calls).includes('Online'));

  const offline = await renderWidget(widget, { node: false });
  assert.ok(callsNamed(offline.calls, 'arc').length >= 1);
  assert.ok(textValues(offline.calls).includes('Offline'));
});

test('icon renders a surface, glyph circle, initial and label', async () => {
  const { calls } = await renderWidget({
    type: 'icon',
    x: 10,
    y: 10,
    w: 64,
    h: 64,
    icon: 'cpu',
    label: 'CPU',
  });

  assert.ok(callsNamed(calls, 'roundRect').length >= 1);
  assert.ok(callsNamed(calls, 'arc').length >= 1);

  const texts = textValues(calls);
  assert.ok(texts.includes('C'));
  assert.ok(texts.includes('CPU'));
});

/* ------------------------------------------------------------------------- */
/* Template and format integration                                            */
/* ------------------------------------------------------------------------- */

test('renderTemplate still loads and renders with an injected fake factory', async () => {
  const fake = createFakeCanvasFactory();
  setCanvasFactory(fake.createCanvas);

  const buffer = await renderTemplate('text', { text: 'Hello\nWorld' }, 120, 60, 'png');
  assert.ok(Buffer.isBuffer(buffer));
  assert.equal(fake.canvases.length, 1);

  const toBuffer = callsNamed(fake.calls, 'toBuffer');
  assert.equal(toBuffer.length, 1);
  assert.equal(toBuffer[0].args[0], 'image/png');

  const texts = textValues(fake.calls);
  assert.ok(texts.includes('Hello'));
  assert.ok(texts.includes('World'));

  const jpegFake = createFakeCanvasFactory();
  await renderTemplate(
    'text',
    { text: 'JPEG' },
    120,
    60,
    'jpeg',
    { createCanvas: jpegFake.createCanvas },
  );
  assert.equal(callsNamed(jpegFake.calls, 'toBuffer')[0].args[0], 'image/jpeg');
});

test('renderIr selects PNG/JPEG encoders through encodeCanvas', async () => {
  const widget = {
    type: 'bar',
    x: 10,
    y: 10,
    w: 180,
    h: 20,
    metric: 'cpu',
    min: 0,
    max: 100,
  };

  const pngFake = createFakeCanvasFactory();
  await renderIr(layout([widget]), { cpu: 50 }, 320, 240, 'png', {
    createCanvas: pngFake.createCanvas,
  });
  assert.equal(callsNamed(pngFake.calls, 'toBuffer')[0].args[0], 'image/png');

  const jpegFake = createFakeCanvasFactory();
  await renderIr(layout([widget]), { cpu: 50 }, 320, 240, 'jpeg', {
    createCanvas: jpegFake.createCanvas,
  });
  assert.equal(callsNamed(jpegFake.calls, 'toBuffer')[0].args[0], 'image/jpeg');
});

test('renderIr SVG path works without a canvas', async () => {
  resetCanvasFactory();
  let canvasUsed = false;
  setCanvasFactory(() => {
    canvasUsed = true;
    throw new Error('canvas must not be used for SVG rendering');
  });

  const svg = await renderIr(
    layout([{
      type: 'gauge',
      x: 10,
      y: 10,
      w: 180,
      h: 180,
      metric: 'cpu',
      min: 0,
      max: 100,
      label: 'CPU',
      unit: '%',
      decimals: 0,
    }]),
    { cpu: 50 },
    320,
    240,
    'svg',
  );

  assert.equal(canvasUsed, false);
  assert.ok(Buffer.isBuffer(svg));
  assert.match(svg.toString('utf8'), /<svg/);
  assert.match(svg.toString('utf8'), /50%/);
});
