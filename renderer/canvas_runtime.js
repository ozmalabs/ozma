/*
 * Optional canvas runtime for the Ozma renderer.
 *
 * The PNG/JPEG render paths depend on the native `@napi-rs/canvas` package, but
 * the template and semantic-IR modules themselves are dependency-free (the SVG
 * path never needs a canvas).  This module keeps the native dependency lazy and
 * injectable so importing `templates.js` works even when the optional package is
 * not installed.
 */

'use strict';

const PACKAGE_NAME = '@napi-rs/canvas';
const INSTALL_MESSAGE =
  `PNG/JPEG rendering requires the optional "${PACKAGE_NAME}" dependency. ` +
  `Install it in the renderer package (renderer/) with: npm install ${PACKAGE_NAME}. ` +
  'SVG rendering does not require canvas.';

/**
 * Error raised when a canvas is requested but no factory is available.
 * The `code` is intentionally stable so HTTP callers can map it to a 503.
 */
class CanvasUnavailableError extends Error {
  constructor(message = INSTALL_MESSAGE, options = {}) {
    super(message, options);
    this.name = 'CanvasUnavailableError';
    this.code = 'canvas_unavailable';
  }
}

const FORMAT_ALIASES = Object.freeze({
  png: 'png',
  'image/png': 'png',
  jpeg: 'jpeg',
  jpg: 'jpeg',
  'image/jpeg': 'jpeg',
  'image/jpg': 'jpeg',
  svg: 'svg',
  'image/svg+xml': 'svg',
});

let injectedFactory = null;
let cachedFactory = null;

function normaliseFormat(format) {
  const key = String(format || 'png').trim().toLowerCase();
  return FORMAT_ALIASES[key] || null;
}

function isBufferLike(value) {
  return Buffer.isBuffer(value) || value instanceof Uint8Array;
}

function expectBuffer(value, format) {
  if (Buffer.isBuffer(value)) return value;
  if (value instanceof Uint8Array) return Buffer.from(value);
  if (typeof value === 'string') return Buffer.from(value);
  throw Object.assign(
    new Error(`canvas returned no encodable data for ${format}`),
    { code: 'canvas_encode_error' },
  );
}

function callToBuffer(canvas, mime, format) {
  if (!canvas || typeof canvas.toBuffer !== 'function') {
    throw Object.assign(
      new Error(`canvas does not support ${format} encoding`),
      { code: 'canvas_encode_error' },
    );
  }
  return expectBuffer(canvas.toBuffer(mime), format);
}

/**
 * Resolve the canvas factory, lazily loading `@napi-rs/canvas` on first use.
 *
 * @returns {(width: number, height: number) => object}
 * @throws {CanvasUnavailableError} when no injected factory exists and the
 *   optional native dependency cannot be loaded.
 */
function loadCanvasFactory() {
  if (injectedFactory) return injectedFactory;
  if (cachedFactory) return cachedFactory;

  let mod;
  try {
    // eslint-disable-next-line global-require, import/no-unresolved
    mod = require(PACKAGE_NAME);
  } catch (cause) {
    throw new CanvasUnavailableError(INSTALL_MESSAGE, { cause });
  }

  const factory = mod && (mod.createCanvas || (mod.default && mod.default.createCanvas));
  if (typeof factory !== 'function') {
    throw new CanvasUnavailableError(
      INSTALL_MESSAGE + ` (${PACKAGE_NAME} did not export createCanvas)`,
    );
  }

  cachedFactory = factory;
  return factory;
}

/**
 * Inject a canvas factory for tests or alternate runtimes.
 * Passing a non-function is a programming error and throws immediately.
 */
function setCanvasFactory(factory) {
  if (typeof factory !== 'function') {
    throw new TypeError('setCanvasFactory() expects a createCanvas function');
  }
  injectedFactory = factory;
  return factory;
}

/** Drop an injected or cached factory, restoring lazy native loading. */
function resetCanvasFactory() {
  injectedFactory = null;
  cachedFactory = null;
}

/**
 * Encode a canvas-like object.
 *
 * Supported formats: png, jpeg (jpg), svg.  PNG/JPEG use the canvas
 * `toBuffer(mime)` API; SVG prefers `toSVG()` and falls back to a canvas
 * implementation that can emit `image/svg+xml`.  Unknown formats are rejected.
 */
function encodeCanvas(canvas, format = 'png') {
  const normalised = normaliseFormat(format);
  if (!normalised) {
    throw Object.assign(
      new Error(`unsupported render format "${format}"`),
      { code: 'unsupported_format' },
    );
  }

  if (normalised === 'png') return callToBuffer(canvas, 'image/png', 'png');
  if (normalised === 'jpeg') return callToBuffer(canvas, 'image/jpeg', 'jpeg');

  if (canvas && typeof canvas.toSVG === 'function') {
    return expectBuffer(canvas.toSVG(), 'svg');
  }
  if (canvas && typeof canvas.toBuffer === 'function') {
    try {
      return callToBuffer(canvas, 'image/svg+xml', 'svg');
    } catch (cause) {
      throw Object.assign(
        new Error('canvas does not support svg encoding'),
        { code: 'canvas_encode_error', cause },
      );
    }
  }
  throw Object.assign(
    new Error('canvas does not support svg encoding'),
    { code: 'canvas_encode_error' },
  );
}

module.exports = {
  CanvasUnavailableError,
  encodeCanvas,
  loadCanvasFactory,
  normaliseFormat,
  resetCanvasFactory,
  setCanvasFactory,
};
