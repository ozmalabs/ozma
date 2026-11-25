// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Real raster tests for the renderer using the optional `@napi-rs/canvas`.
 *
 * These tests close the "fake canvas only" gap in `render_png.test.mjs`: they
 * encode real PNG/JPEG buffers, parse the container headers, decode the images
 * with the native canvas, and sample pixels for the background, gauge/bar
 * fills, threshold/status colours, and text.
 *
 * The native dependency is optional.  When it is absent (for example in a
 * checkout that did not run `npm ci --prefix renderer`) the raster tests skip
 * with an explicit reason instead of failing the pure renderer suite.
 *
 * Run with: node --test renderer/raster.test.mjs
 */

import test, { afterEach } from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

import templates from './templates.js';
import canvasRuntime from './canvas_runtime.js';

const { renderIr, renderTemplate } = templates;
const {
  encodeCanvas,
  loadCanvasFactory,
  resetCanvasFactory,
  setCanvasFactory,
} = canvasRuntime;

const nodeRequire = createRequire(import.meta.url);

let native = null;
try {
  native = nodeRequire('@napi-rs/canvas');
} catch {
  native = null;
}

const hasNativeCanvas = Boolean(native && typeof native.createCanvas === 'function');
const RASTER = hasNativeCanvas ? {} : { skip: '@napi-rs/canvas is not installed' };

const WIDTH = 320;
const HEIGHT = 200;

// A deterministic 320x200 fixture that exercises every sampled region.
// - gauge value 95 is above its critical threshold (95 >= 90) -> #f26464
// - bar value 50 uses the normal accent fill -> #5b6fff
// - status online dot -> #42d392
// - text uses the default foreground -> #e5e7eb
const LAYOUT = {
  id: 'org.ozma.layout.raster_fixture',
  name: 'Raster fixture',
  width: WIDTH,
  height: HEIGHT,
  background: '#0a0a0f',
  widgets: [
    {
      id: 'gauge',
      type: 'gauge',
      x: 8,
      y: 8,
      w: 100,
      h: 100,
      metric: 'cpu',
      min: 0,
      max: 100,
      label: 'CPU',
      unit: 'percent',
      decimals: 0,
      warn: 70,
      crit: 90,
      font_size: 16,
      tick_count: 0,
    },
    {
      id: 'bar',
      type: 'bar',
      x: 120,
      y: 20,
      w: 180,
      h: 24,
      metric: 'ram',
      min: 0,
      max: 100,
      label: 'RAM',
      unit: 'percent',
      decimals: 0,
      radius: 4,
      font_size: 12,
    },
    {
      id: 'status',
      type: 'status',
      x: 120,
      y: 56,
      w: 160,
      h: 30,
      metric: 'online',
      label: 'Online',
      font_size: 12,
    },
    {
      id: 'text',
      type: 'text',
      x: 8,
      y: 130,
      w: 300,
      h: 30,
      metric: 'node',
      text: 'Node {node}',
      font_size: 20,
      align: 'left',
    },
  ],
};

const DATA = {
  cpu: 95,
  ram: 50,
  online: true,
  node: 'atlas-01',
};

afterEach(() => {
  resetCanvasFactory();
});

/* ------------------------------------------------------------------------- */
/* Container helpers                                                          */
/* ------------------------------------------------------------------------- */

function readPngHeader(buffer) {
  assert.ok(Buffer.isBuffer(buffer), 'PNG output must be a Buffer');
  assert.equal(buffer.readUInt32BE(0), 0x89504e47, 'PNG signature bytes 0-3');
  assert.equal(buffer.toString('latin1', 4, 8), '\r\n\x1a\n', 'PNG signature bytes 4-7');
  assert.equal(buffer.readUInt32BE(8), 13, 'IHDR chunk length');
  assert.equal(buffer.toString('latin1', 12, 16), 'IHDR', 'first chunk must be IHDR');
  return {
    width: buffer.readUInt32BE(16),
    height: buffer.readUInt32BE(20),
    bitDepth: buffer[24],
    colorType: buffer[25],
    compression: buffer[26],
    filter: buffer[27],
    interlace: buffer[28],
  };
}

function readJpegHeader(buffer) {
  assert.ok(Buffer.isBuffer(buffer), 'JPEG output must be a Buffer');
  assert.equal(buffer[0], 0xff, 'JPEG SOI marker byte 0');
  assert.equal(buffer[1], 0xd8, 'JPEG SOI marker byte 1');

  let app0 = false;
  let width;
  let height;
  let offset = 2;
  while (offset + 4 <= buffer.length) {
    if (buffer[offset] !== 0xff) break;
    const marker = buffer[offset + 1];
    if (marker === 0xd9) break;
    const length = buffer.readUInt16BE(offset + 2);
    if (marker === 0xe0) app0 = true;
    // SOF0/SOF1/SOF2 carry the decoded dimensions.
    if (marker === 0xc0 || marker === 0xc1 || marker === 0xc2) {
      height = buffer.readUInt16BE(offset + 5);
      width = buffer.readUInt16BE(offset + 7);
    }
    offset += 2 + length;
  }

  assert.ok(app0, 'JPEG must contain an APP0 (JFIF) marker');
  assert.equal(buffer[buffer.length - 2], 0xff, 'JPEG EOI marker byte -2');
  assert.equal(buffer[buffer.length - 1], 0xd9, 'JPEG EOI marker byte -1');
  return { width, height };
}

async function decodeRaster(buffer) {
  const image = await native.loadImage(buffer);
  const canvas = native.createCanvas(image.width, image.height);
  const context = canvas.getContext('2d');
  context.drawImage(image, 0, 0);
  const { data } = context.getImageData(0, 0, image.width, image.height);
  return { width: image.width, height: image.height, data };
}

/* ------------------------------------------------------------------------- */
/* Pixel helpers                                                              */
/* ------------------------------------------------------------------------- */

function pixel(raster, x, y) {
  const index = (y * raster.width + x) * 4;
  return [
    raster.data[index],
    raster.data[index + 1],
    raster.data[index + 2],
    raster.data[index + 3],
  ];
}

function assertNear(actual, expected, tolerance, label) {
  const channels = ['r', 'g', 'b', 'a'];
  for (let index = 0; index < expected.length; index += 1) {
    const delta = Math.abs(actual[index] - expected[index]);
    assert.ok(
      delta <= tolerance,
      `${label}: ${channels[index]} channel ${actual[index]} is not within `
        + `${tolerance} of ${expected[index]}`,
    );
  }
}

function countNear(raster, region, expected, tolerance) {
  let count = 0;
  for (let y = region.y0; y < region.y1; y += 1) {
    for (let x = region.x0; x < region.x1; x += 1) {
      const sample = pixel(raster, x, y);
      if (expected.every((value, index) => Math.abs(sample[index] - value) <= tolerance)) {
        count += 1;
      }
    }
  }
  return count;
}

function countNonBackground(raster, background, tolerance) {
  let count = 0;
  for (let y = 0; y < raster.height; y += 1) {
    for (let x = 0; x < raster.width; x += 1) {
      const sample = pixel(raster, x, y);
      if (background.some((value, index) => Math.abs(sample[index] - value) > tolerance)) {
        count += 1;
      }
    }
  }
  return count;
}

/* ------------------------------------------------------------------------- */
/* PNG rendering                                                              */
/* ------------------------------------------------------------------------- */

test('renderIr rasterizes a 320x200 PNG with background, fills, threshold, status and text pixels', RASTER, async () => {
  resetCanvasFactory();
  const png = await renderIr(LAYOUT, DATA, WIDTH, HEIGHT, 'png');

  const header = readPngHeader(png);
  assert.equal(header.width, WIDTH, 'PNG IHDR width');
  assert.equal(header.height, HEIGHT, 'PNG IHDR height');
  assert.equal(header.bitDepth, 8, 'PNG bit depth');
  assert.ok([2, 6].includes(header.colorType), `unexpected PNG colour type ${header.colorType}`);
  assert.equal(header.compression, 0, 'PNG compression method');
  assert.equal(header.filter, 0, 'PNG filter method');
  assert.equal(header.interlace, 0, 'PNG interlace method');

  const raster = await decodeRaster(png);
  assert.equal(raster.width, WIDTH, 'decoded width');
  assert.equal(raster.height, HEIGHT, 'decoded height');

  // Background: the canvas fill is opaque #0a0a0f.
  assertNear(pixel(raster, 0, 0), [10, 10, 15, 255], 4, 'background top-left');
  assertNear(pixel(raster, WIDTH - 1, HEIGHT - 1), [10, 10, 15, 255], 4, 'background bottom-right');

  // Normal bar fill: 50% of a 180px bar at #5b6fff. Sample away from the text.
  assertNear(pixel(raster, 180, 38), [91, 111, 255, 255], 8, 'bar fill');

  // Critical gauge arc: scan the top of the gauge ring only, away from the
  // centre value text, so the red pixels can only come from the value arc.
  const criticalArc = countNear(raster, { x0: 8, y0: 8, x1: 108, y1: 35 }, [242, 100, 100], 24);
  assert.ok(criticalArc >= 20, `expected critical gauge arc pixels, found ${criticalArc}`);

  // Online status dot at (x + r + 2, y + h / 2) with r = min(w, h) * 0.18.
  assertNear(pixel(raster, 127, 71), [66, 211, 146, 255], 8, 'online status dot');

  // Foreground text: count near-#e5e7eb pixels in the text widget's bounds.
  const foreground = countNear(raster, { x0: 8, y0: 130, x1: 308, y1: 160 }, [229, 231, 235], 24);
  assert.ok(foreground >= 20, `expected foreground text pixels, found ${foreground}`);
});

test('renderIr emits a decodable 320x200 JPEG with SOI, APP0 and EOI', RASTER, async () => {
  resetCanvasFactory();
  const jpeg = await renderIr(LAYOUT, DATA, WIDTH, HEIGHT, 'jpeg');

  const header = readJpegHeader(jpeg);
  assert.equal(header.width, WIDTH, 'JPEG SOF width');
  assert.equal(header.height, HEIGHT, 'JPEG SOF height');

  const raster = await decodeRaster(jpeg);
  assert.equal(raster.width, WIDTH, 'decoded JPEG width');
  assert.equal(raster.height, HEIGHT, 'decoded JPEG height');

  // JPEG is lossy, so use a wide tolerance to prove the bar content survived.
  assertNear(pixel(raster, 180, 38), [91, 111, 255, 255], 24, 'decoded JPEG bar fill');
});

/* ------------------------------------------------------------------------- */
/* SVG rendering                                                              */
/* ------------------------------------------------------------------------- */

test('renderIr renders SVG without touching the canvas factory', async () => {
  resetCanvasFactory();
  let canvasUsed = false;
  setCanvasFactory(() => {
    canvasUsed = true;
    throw new Error('canvas must not be used for SVG rendering');
  });

  const svgBuffer = await renderIr(LAYOUT, DATA, WIDTH, HEIGHT, 'svg');
  assert.equal(canvasUsed, false, 'SVG path must not call the canvas factory');
  assert.ok(Buffer.isBuffer(svgBuffer), 'SVG output must be a Buffer');

  const svg = svgBuffer.toString('utf8');
  assert.match(svg, /^<svg /);
  assert.match(svg, /width="320"/);
  assert.match(svg, /height="200"/);
  assert.match(svg, /Node atlas-01/);
});

/* ------------------------------------------------------------------------- */
/* Template rendering                                                         */
/* ------------------------------------------------------------------------- */

test('renderTemplate rasterizes the status template with the native canvas', RASTER, async () => {
  resetCanvasFactory();
  const png = await renderTemplate(
    'status',
    {
      name: 'Raster',
      cpu_usage: 55,
      gpu_usage: 44,
      cpu_temp: 61,
      gpu_temp: 57,
      ram_used: 4,
      ram_total: 8,
    },
    WIDTH,
    HEIGHT,
    'png',
  );

  const header = readPngHeader(png);
  assert.equal(header.width, WIDTH, 'template PNG width');
  assert.equal(header.height, HEIGHT, 'template PNG height');

  const raster = await decodeRaster(png);
  assert.equal(raster.width, WIDTH);
  assert.equal(raster.height, HEIGHT);
  assertNear(pixel(raster, 0, 0), [10, 10, 15, 255], 4, 'template background');

  const drawn = countNonBackground(raster, [10, 10, 15], 12);
  assert.ok(drawn > 1000, `expected the template to draw visible pixels, found ${drawn}`);
});

/* ------------------------------------------------------------------------- */
/* Canvas runtime and encoders                                                */
/* ------------------------------------------------------------------------- */

test('canvas runtime loads, injects, and resets the native factory', RASTER, () => {
  resetCanvasFactory();

  const nativeFactory = loadCanvasFactory();
  assert.equal(typeof nativeFactory, 'function');
  const canvas = nativeFactory(24, 12);
  assert.equal(canvas.width, 24);
  assert.equal(canvas.height, 12);

  const customFactory = (width, height) => ({ width, height });
  assert.equal(setCanvasFactory(customFactory), customFactory);
  assert.equal(loadCanvasFactory(), customFactory);

  resetCanvasFactory();
  const reloaded = loadCanvasFactory();
  assert.notEqual(reloaded, customFactory);
  assert.equal(typeof reloaded, 'function');

  assert.throws(
    () => setCanvasFactory(null),
    (error) => error instanceof TypeError,
  );
});

test('encodeCanvas emits decodable PNG/JPEG and handles SVG and unknown formats', RASTER, async () => {
  const canvas = native.createCanvas(32, 16);
  const context = canvas.getContext('2d');
  context.fillStyle = '#5b6fff';
  context.fillRect(0, 0, 32, 16);

  for (const format of ['png', 'image/png']) {
    const buffer = encodeCanvas(canvas, format);
    const header = readPngHeader(buffer);
    assert.equal(header.width, 32, `PNG width for ${format}`);
    assert.equal(header.height, 16, `PNG height for ${format}`);
    const raster = await decodeRaster(buffer);
    assert.equal(raster.width, 32);
    assert.equal(raster.height, 16);
  }

  for (const format of ['jpeg', 'jpg', 'image/jpeg', 'image/jpg']) {
    const buffer = encodeCanvas(canvas, format);
    const header = readJpegHeader(buffer);
    assert.equal(header.width, 32, `JPEG width for ${format}`);
    assert.equal(header.height, 16, `JPEG height for ${format}`);
    const raster = await decodeRaster(buffer);
    assert.equal(raster.width, 32);
    assert.equal(raster.height, 16);
  }

  const svgCanvas = { toSVG: () => '<svg width="32" height="16"/>' };
  assert.equal(encodeCanvas(svgCanvas, 'svg').toString(), '<svg width="32" height="16"/>');
  assert.equal(encodeCanvas(svgCanvas, 'image/svg+xml').toString(), '<svg width="32" height="16"/>');

  assert.throws(
    () => encodeCanvas(canvas, 'svg'),
    (error) => error.code === 'canvas_encode_error',
  );
  assert.throws(
    () => encodeCanvas(canvas, 'gif'),
    (error) => error.code === 'unsupported_format',
  );
});
