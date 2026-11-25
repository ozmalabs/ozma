// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Sandboxed third-party widget runtime.
 *
 * Executable widget packs are never loaded into the main display origin.  A
 * pack is mounted in a `sandbox="allow-scripts"` iframe with no same-origin
 * access, no top navigation, no popups, no forms, no modals, and no downloads.
 * The iframe speaks a tiny versioned postMessage protocol and the parent
 * validates every field before acting on it.
 *
 * The module is dependency-free.  All protocol validation and capability
 * handling is pure so it can be exercised with `node --test` and a fake
 * window/document; only `createSandboxFrame` touches a DOM, and only the
 * iframe element it creates itself.
 *
 * Security notes
 * --------------
 * A sandboxed document has an opaque origin, so browsers report
 * `event.origin === "null"` for its messages.  `"null"` is not a useful
 * authentication value: any other sandboxed document also reports it.  The
 * real capability is the unguessable channel id embedded in the frame URL and
 * required in every message.  Callers must therefore treat the channel as a
 * secret, only create frames for local `sandbox-host.html`, and never grant
 * `internet`/network access to a frame in the main origin.
 */

export const SANDBOX_PROTOCOL_VERSION = 1;
export const SANDBOX_MESSAGE_TYPES = Object.freeze([
  'ready',
  'mount',
  'data',
  'action',
  'resize',
  'error',
  'teardown',
]);
export const SANDBOX_MESSAGE_TYPE_SET = new Set(SANDBOX_MESSAGE_TYPES);
export const PROTOCOL_VERSION = SANDBOX_PROTOCOL_VERSION;
export const SANDBOX_VERSION = SANDBOX_PROTOCOL_VERSION;
export const MESSAGE_TYPES = SANDBOX_MESSAGE_TYPES;

export const SANDBOX_CAPABILITIES = Object.freeze(['render', 'actions']);
export const SANDBOX_DEFAULT_CAPABILITIES = Object.freeze(['render']);

export const SANDBOX_ORIGIN = 'null';
export const SANDBOX_ALLOW_VALUE = 'allow-scripts';
export const SANDBOX_ATTRIBUTE_VALUE = `sandbox="${SANDBOX_ALLOW_VALUE}"`;
export const SANDBOX_REFERRER_POLICY = 'no-referrer';
export const SANDBOX_ALLOW_ATTRIBUTE = '';
export const SANDBOX_LOADING = 'lazy';

export const DEFAULT_MAX_PAYLOAD_BYTES = 256 * 1024;
export const DEFAULT_MAX_PAYLOAD_DEPTH = 16;
export const DEFAULT_MAX_STRING_LENGTH = 64 * 1024;
export const DEFAULT_MAX_KEY_LENGTH = 256;
export const DEFAULT_MAX_ID_LENGTH = 128;
export const DEFAULT_READY_TIMEOUT_MS = 5000;
export const DEFAULT_MESSAGE_RATE = 120;
export const DEFAULT_MESSAGE_BURST = 120;
export const DEFAULT_REQUEST_TIMEOUT_MS = 5000;

export const DEFAULT_CHANNEL_PARAM = 'ozma-channel';

const FORBIDDEN_KEYS = new Set([
  '__proto__',
  'prototype',
  'constructor',
  '__defineGetter__',
  '__defineSetter__',
  '__lookupGetter__',
  '__lookupSetter__',
  'toJSON',
]);

const CHANNEL_RE = /^[A-Za-z0-9_.:-]+$/;
const ID_RE = /^[A-Za-z0-9_.:-]+$/;

const encoder = typeof TextEncoder === 'function' ? new TextEncoder() : null;

/* ------------------------------------------------------------------------- */
/* Errors                                                                     */
/* ------------------------------------------------------------------------- */

export class SandboxError extends Error {
  constructor(code, message, details = {}) {
    super(message);
    this.name = 'SandboxError';
    this.code = code;
    if (details && typeof details === 'object') {
      for (const [key, value] of Object.entries(details)) {
        if (key !== 'name' && key !== 'message' && key !== 'stack') this[key] = value;
      }
    }
  }
}

function fail(code, message, details) {
  throw new SandboxError(code, message, details);
}

/* ------------------------------------------------------------------------- */
/* Small type helpers                                                         */
/* ------------------------------------------------------------------------- */

function isPlainObject(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  if (prototype === null) return true;
  // Accept Object.prototype from another realm as well as this realm's, but
  // reject class instances and custom prototypes.
  if (Object.getPrototypeOf(prototype) !== null) return false;
  return Object.prototype.hasOwnProperty.call(prototype, 'hasOwnProperty')
    && typeof prototype.hasOwnProperty === 'function';
}

function isSafeArray(value) {
  if (!Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  if (prototype === null) return true;
  return Object.prototype.hasOwnProperty.call(prototype, 'push')
    && typeof prototype.push === 'function';
}

function ownDescriptor(object, key) {
  return Object.getOwnPropertyDescriptor(object, key);
}

function ownValue(object, key) {
  const descriptor = ownDescriptor(object, key);
  return descriptor && Object.prototype.hasOwnProperty.call(descriptor, 'value')
    ? descriptor.value
    : undefined;
}

function hasOwn(object, key) {
  return Object.prototype.hasOwnProperty.call(object, key);
}

function utf8Length(value) {
  if (encoder) return encoder.encode(value).length;
  // Deterministic fallback when TextEncoder is unavailable.
  let bytes = 0;
  for (let index = 0; index < value.length; index += 1) {
    const code = value.codePointAt(index);
    if (code > 0xffff) {
      bytes += 4;
      index += 1;
    } else if (code > 0x7ff) {
      bytes += 3;
    } else if (code > 0x7f) {
      bytes += 2;
    } else {
      bytes += 1;
    }
  }
  return bytes;
}

function assertSafeKey(key, path) {
  if (key.length > DEFAULT_MAX_KEY_LENGTH) {
    fail('SANDBOX_PAYLOAD_KEY', `Key exceeds ${DEFAULT_MAX_KEY_LENGTH} characters`, { path });
  }
  if (FORBIDDEN_KEYS.has(key) || /^on[a-z]/i.test(key)) {
    fail('SANDBOX_PAYLOAD_KEY', `Unsafe payload key "${key}"`, { path });
  }
}

/* ------------------------------------------------------------------------- */
/* Capabilities                                                               */
/* ------------------------------------------------------------------------- */

/**
 * Normalise a capability request.
 *
 * `render` is always present.  `actions` adds inbound/outbound `action`
 * messages.  `network` is recognised but never grantable inside the main
 * origin; requesting it fails closed.  Unknown capabilities also fail closed.
 *
 * @param {string|string[]|Set<string>|object|undefined|null} capabilities
 * @returns {string[]} frozen capability array
 */
export function normalizeCapabilities(capabilities) {
  let requested = [];

  if (capabilities === undefined || capabilities === null || capabilities === '') {
    requested = [];
  } else if (typeof capabilities === 'string') {
    requested = capabilities.split(/[\s,]+/).filter(Boolean);
  } else if (Array.isArray(capabilities)) {
    requested = capabilities.slice();
  } else if (capabilities instanceof Set) {
    requested = Array.from(capabilities);
  } else if (isPlainObject(capabilities)) {
    for (const [key, enabled] of Object.entries(capabilities)) {
      if (key === 'network') {
        if (enabled === true) {
          fail(
            'SANDBOX_CAPABILITY',
            'The network capability is never granted inside the main display origin',
            { capability: key },
          );
        }
        if (enabled !== false && enabled !== undefined && enabled !== null) {
          fail('SANDBOX_CAPABILITY', 'Capability "network" must be a boolean', {
            capability: key,
          });
        }
        continue;
      }
      if (!SANDBOX_CAPABILITIES.includes(key)) {
        fail('SANDBOX_CAPABILITY', `Unknown sandbox capability "${key}"`, { capability: key });
      }
      if (enabled === true) requested.push(key);
      else if (enabled !== false && enabled !== undefined && enabled !== null) {
        fail('SANDBOX_CAPABILITY', `Capability "${key}" must be a boolean`, { capability: key });
      }
    }
  } else {
    fail('SANDBOX_CAPABILITY', 'Capabilities must be a string, array, set, or object');
  }

  const result = new Set(['render']);
  for (const capability of requested) {
    if (typeof capability !== 'string' || !capability) {
      fail('SANDBOX_CAPABILITY', 'Capability names must be non-empty strings');
    }
    if (capability === 'network') {
      fail(
        'SANDBOX_CAPABILITY',
        'The network capability is never granted inside the main display origin',
        { capability },
      );
    }
    if (!SANDBOX_CAPABILITIES.includes(capability)) {
      fail('SANDBOX_CAPABILITY', `Unknown sandbox capability "${capability}"`, { capability });
    }
    result.add(capability);
  }

  return Object.freeze(Array.from(result));
}

/* ------------------------------------------------------------------------- */
/* Origin                                                                     */
/* ------------------------------------------------------------------------- */

/**
 * True only for the opaque origin browsers report for a sandboxed frame.
 * This is a transport check, not authentication: the channel id is the
 * capability.
 */
export function isSandboxedOrigin(origin) {
  return origin === SANDBOX_ORIGIN;
}

/* ------------------------------------------------------------------------- */
/* Payload inspection                                                         */
/* ------------------------------------------------------------------------- */

function payloadLimits(options = {}) {
  return {
    maxBytes: Number.isFinite(options.maxPayloadBytes)
      ? options.maxPayloadBytes
      : DEFAULT_MAX_PAYLOAD_BYTES,
    maxDepth: Number.isFinite(options.maxPayloadDepth)
      ? options.maxPayloadDepth
      : DEFAULT_MAX_PAYLOAD_DEPTH,
    maxStringLength: Number.isFinite(options.maxStringLength)
      ? options.maxStringLength
      : DEFAULT_MAX_STRING_LENGTH,
  };
}

/**
 * Iteratively scan a payload for size, depth, primitive type, prototype, and
 * accessor safety.  The scan never serialises the value, so a hostile
 * `toJSON` or getter cannot run.  Returns the approximate UTF-8 byte size.
 */
function scanPayload(root, options = {}) {
  const limits = payloadLimits(options);
  const seen = new WeakSet();
  const stack = [{ value: root, depth: 1, path: 'payload' }];
  let bytes = 0;

  while (stack.length > 0) {
    const { value, depth, path } = stack.pop();

    if (depth > limits.maxDepth) {
      fail('SANDBOX_PAYLOAD_DEPTH', `Payload exceeds depth ${limits.maxDepth}`, { path });
    }

    if (value === null) {
      bytes += 4;
      continue;
    }

    const type = typeof value;
    if (type === 'string') {
      if (value.length > limits.maxStringLength) {
        fail('SANDBOX_PAYLOAD_STRING', `String exceeds ${limits.maxStringLength} characters`, {
          path,
        });
      }
      bytes += utf8Length(value) + 2;
      continue;
    }
    if (type === 'number') {
      if (!Number.isFinite(value)) {
        fail('SANDBOX_PAYLOAD_NUMBER', 'Numbers must be finite', { path });
      }
      bytes += 8;
      continue;
    }
    if (type === 'boolean') {
      bytes += 5;
      continue;
    }
    if (type !== 'object') {
      fail('SANDBOX_PAYLOAD_TYPE', `Unsupported payload value of type ${type}`, { path });
    }

    if (seen.has(value)) {
      fail('SANDBOX_PAYLOAD_CYCLE', 'Payload contains a cycle', { path });
    }
    seen.add(value);

    if (!isSafeArray(value) && !isPlainObject(value)) {
      fail('SANDBOX_PAYLOAD_PROTOTYPE', 'Payload values must be plain objects or arrays', { path });
    }

    if (Object.getOwnPropertySymbols(value).some((symbol) => {
      const descriptor = ownDescriptor(value, symbol);
      return descriptor && descriptor.enumerable;
    })) {
      fail('SANDBOX_PAYLOAD_KEY', 'Symbol keys are not allowed', { path });
    }

    bytes += 2;
    const keys = Object.getOwnPropertyNames(value);
    for (const key of keys) {
      if (isSafeArray(value) && key === 'length') continue;
      const childPath = `${path}.${key}`;
      assertSafeKey(key, childPath);
      const descriptor = ownDescriptor(value, key);
      if (!descriptor) continue;
      if (descriptor.get || descriptor.set) {
        fail('SANDBOX_PAYLOAD_ACCESSOR', 'Accessors are not allowed in payloads', {
          path: childPath,
        });
      }
      if (!descriptor.enumerable) {
        fail('SANDBOX_PAYLOAD_HIDDEN', 'Non-enumerable payload fields are not allowed', {
          path: childPath,
        });
      }
      bytes += utf8Length(key) + 3;
      if (bytes > limits.maxBytes) {
        fail('SANDBOX_PAYLOAD_SIZE', `Payload exceeds ${limits.maxBytes} bytes`, {
          path: childPath,
          bytes,
        });
      }
      stack.push({ value: descriptor.value, depth: depth + 1, path: childPath });
    }

    if (bytes > limits.maxBytes) {
      fail('SANDBOX_PAYLOAD_SIZE', `Payload exceeds ${limits.maxBytes} bytes`, { path, bytes });
    }
  }

  if (bytes > limits.maxBytes) {
    fail('SANDBOX_PAYLOAD_SIZE', `Payload exceeds ${limits.maxBytes} bytes`, { path: 'payload', bytes });
  }
  return bytes;
}

/* ------------------------------------------------------------------------- */
/* Message validation                                                         */
/* ------------------------------------------------------------------------- */

function normaliseId(id) {
  if (id === undefined || id === null) return null;
  if (typeof id === 'number') {
    if (!Number.isInteger(id) || !Number.isFinite(id) || id < 0) {
      fail('SANDBOX_MESSAGE_ID', 'Numeric message ids must be non-negative integers');
    }
    return id;
  }
  if (typeof id !== 'string' || !id || id.length > DEFAULT_MAX_ID_LENGTH || !ID_RE.test(id)) {
    fail('SANDBOX_MESSAGE_ID', 'Message id must be a short safe string or integer');
  }
  return id;
}

/**
 * Validate an inbound sandbox message.
 *
 * Throws a {@link SandboxError} on any invalid field.  The caller should
 * ignore (not surface) messages whose `event.origin` is not `"null"`; passing
 * an `origin` option enforces that here as well.
 *
 * @param {*} message
 * @param {object} [options] `{channel, origin, capabilities, maxPayloadBytes,
 *   maxPayloadDepth, maxStringLength}`
 * @returns {object} validated message `{v, channel, type, id, payload}`
 */
export function validateSandboxMessage(message, options = {}) {
  if (options.origin !== undefined && !isSandboxedOrigin(options.origin)) {
    fail('SANDBOX_ORIGIN', 'Message origin is not a sandboxed opaque origin', {
      origin: options.origin,
    });
  }

  if (!isPlainObject(message)) {
    fail('SANDBOX_MESSAGE_SHAPE', 'Message must be a plain object');
  }

  const channel = options.channel;
  if (typeof channel !== 'string' || !channel) {
    fail('SANDBOX_CHANNEL', 'An expected channel id is required');
  }

  if (Object.getOwnPropertySymbols(message).length > 0) {
    fail('SANDBOX_MESSAGE_SHAPE', 'Symbol message fields are not allowed');
  }

  const keys = Object.getOwnPropertyNames(message);
  const allowedKeys = new Set(['v', 'channel', 'type', 'id', 'payload']);
  for (const key of keys) {
    if (!allowedKeys.has(key)) {
      fail('SANDBOX_MESSAGE_SHAPE', `Unknown message field "${key}"`, { field: key });
    }
    const descriptor = ownDescriptor(message, key);
    if (!descriptor || descriptor.get || descriptor.set) {
      fail('SANDBOX_MESSAGE_SHAPE', 'Accessors are not allowed in messages', { field: key });
    }
    if (!descriptor.enumerable) {
      fail('SANDBOX_MESSAGE_SHAPE', 'Non-enumerable message fields are not allowed', {
        field: key,
      });
    }
  }

  const version = ownValue(message, 'v');
  if (version !== SANDBOX_PROTOCOL_VERSION) {
    fail('SANDBOX_VERSION', `Expected protocol version ${SANDBOX_PROTOCOL_VERSION}`, {
      version,
    });
  }

  const messageChannel = ownValue(message, 'channel');
  if (typeof messageChannel !== 'string' || !messageChannel
    || messageChannel.length > 256) {
    fail('SANDBOX_CHANNEL', 'Message channel must be a short non-empty string');
  }
  if (messageChannel !== channel) {
    fail('SANDBOX_CHANNEL', 'Message channel does not match this bridge');
  }

  const type = ownValue(message, 'type');
  if (typeof type !== 'string' || !SANDBOX_MESSAGE_TYPE_SET.has(type)) {
    fail('SANDBOX_MESSAGE_TYPE', 'Unknown sandbox message type', { type });
  }

  const capabilities = normalizeCapabilities(options.capabilities);
  if (type === 'action' && !capabilities.includes('actions')) {
    fail('SANDBOX_CAPABILITY', 'The action message requires the "actions" capability', {
      type,
    });
  }

  const id = normaliseId(ownValue(message, 'id'));

  const payload = hasOwn(message, 'payload') ? ownValue(message, 'payload') : null;
  if (payload !== null && payload !== undefined) {
    scanPayload(payload, options);
  }

  return {
    v: SANDBOX_PROTOCOL_VERSION,
    channel,
    type,
    id,
    payload: payload === undefined ? null : payload,
  };
}

/**
 * Non-throwing validation helper.
 *
 * @returns {{ok: boolean, message: object|null, error: SandboxError|null}}
 */
export function checkSandboxMessage(message, options = {}) {
  try {
    return { ok: true, message: validateSandboxMessage(message, options), error: null };
  } catch (error) {
    return {
      ok: false,
      message: null,
      error: error instanceof SandboxError
        ? error
        : new SandboxError('SANDBOX_MESSAGE', error?.message || 'Invalid sandbox message'),
    };
  }
}

/* ------------------------------------------------------------------------- */
/* Attributes                                                                 */
/* ------------------------------------------------------------------------- */

/**
 * Return the iframe `sandbox` attribute.  The value is fixed: only scripts
 * are enabled.  There is deliberately no option that can add same-origin,
 * top-navigation, popups, forms, modals, or downloads.
 *
 * @param {object|string[]|string} [options]
 * @returns {string} `sandbox="allow-scripts"`
 */
export function sandboxAttributes(options = {}) {
  if (Array.isArray(options) || typeof options === 'string') {
    normalizeCapabilities(options);
  } else if (options && typeof options === 'object') {
    normalizeCapabilities(options.capabilities);
  }
  return SANDBOX_ATTRIBUTE_VALUE;
}

/**
 * The full fixed attribute set applied to a sandbox frame.
 *
 * @param {object|string[]|string} [options]
 * @returns {{sandbox: string, referrerpolicy: string, allow: string, loading: string}}
 */
export function sandboxAttributeMap(options = {}) {
  if (Array.isArray(options) || typeof options === 'string') {
    normalizeCapabilities(options);
  } else if (options && typeof options === 'object') {
    normalizeCapabilities(options.capabilities);
  }
  return {
    sandbox: SANDBOX_ALLOW_VALUE,
    referrerpolicy: SANDBOX_REFERRER_POLICY,
    allow: SANDBOX_ALLOW_ATTRIBUTE,
    loading: SANDBOX_LOADING,
  };
}

/* ------------------------------------------------------------------------- */
/* CSP                                                                        */
/* ------------------------------------------------------------------------- */

const BASE_CSP_DIRECTIVES = [
  'default-src',
  'script-src',
  'style-src',
  'img-src',
  'connect-src',
  'font-src',
  'media-src',
  'object-src',
  'base-uri',
  'form-action',
  'frame-src',
  'child-src',
  'worker-src',
  'manifest-src',
];

const CSP_KEY_TO_DIRECTIVE = Object.freeze({
  defaultSrc: 'default-src',
  scriptSrc: 'script-src',
  styleSrc: 'style-src',
  imgSrc: 'img-src',
  connectSrc: 'connect-src',
  fontSrc: 'font-src',
  mediaSrc: 'media-src',
  objectSrc: 'object-src',
  baseUri: 'base-uri',
  formAction: 'form-action',
  frameSrc: 'frame-src',
  childSrc: 'child-src',
  workerSrc: 'worker-src',
  manifestSrc: 'manifest-src',
  sandbox: 'sandbox',
  'default-src': 'default-src',
  'script-src': 'script-src',
  'style-src': 'style-src',
  'img-src': 'img-src',
  'connect-src': 'connect-src',
  'font-src': 'font-src',
  'media-src': 'media-src',
  'object-src': 'object-src',
  'base-uri': 'base-uri',
  'form-action': 'form-action',
  'frame-src': 'frame-src',
  'child-src': 'child-src',
  'worker-src': 'worker-src',
  'manifest-src': 'manifest-src',
});

const CSP_FORCED = Object.freeze({
  'default-src': "'none'",
  'object-src': "'none'",
  'base-uri': "'none'",
  'form-action': "'none'",
  'frame-src': "'none'",
  'child-src': "'none'",
  'worker-src': "'none'",
  'manifest-src': "'none'",
});

function normaliseCspValue(value, directive) {
  if (value === undefined || value === null) return null;
  if (typeof value !== 'string') {
    fail('SANDBOX_CSP', `CSP directive "${directive}" must be a string`, { directive });
  }
  const trimmed = value.trim();
  if (!trimmed) return null;
  if (/[;\r\n]/.test(trimmed)) {
    fail('SANDBOX_CSP', `CSP directive "${directive}" contains an invalid separator`, {
      directive,
    });
  }
  if (directive === 'sandbox') {
    for (const token of trimmed.split(/\s+/)) {
      if (token !== SANDBOX_ALLOW_VALUE) {
        fail('SANDBOX_CSP', `CSP sandbox may only grant "${SANDBOX_ALLOW_VALUE}"`, {
          directive,
        });
      }
    }
  }
  return trimmed;
}

/**
 * Build the CSP used by `sandbox-host.html`.
 *
 * The host shim is inline, so `script-src 'unsafe-inline'` is the default.
 * Everything else is denied: no network, no objects, no base changes, no
 * forms, and no nested frames.  A caller may override individual directives
 * but the dangerous baseline directives are forced.
 *
 * @param {object|string} [policy] directive overrides (camelCase or kebab-case)
 * @returns {string} CSP header/meta content
 */
export function sandboxCsp(policy = {}) {
  const directives = new Map();
  for (const directive of BASE_CSP_DIRECTIVES) directives.set(directive, null);

  directives.set('default-src', "'none'");
  directives.set('script-src', "'unsafe-inline'");
  directives.set('style-src', "'unsafe-inline'");
  directives.set('img-src', 'data:');
  directives.set('connect-src', "'none'");
  directives.set('font-src', "'none'");
  directives.set('media-src', "'none'");

  const apply = (key, value) => {
    const directive = CSP_KEY_TO_DIRECTIVE[key] || key;
    if (!BASE_CSP_DIRECTIVES.includes(directive) && directive !== 'sandbox') {
      fail('SANDBOX_CSP', `Unknown CSP directive "${key}"`, { directive: key });
    }
    const normalised = normaliseCspValue(value, directive);
    if (normalised !== null) directives.set(directive, normalised);
  };

  if (typeof policy === 'string') {
    for (const part of policy.split(';')) {
      const trimmed = part.trim();
      if (!trimmed) continue;
      const space = trimmed.indexOf(' ');
      if (space === -1) continue;
      apply(trimmed.slice(0, space), trimmed.slice(space + 1));
    }
  } else if (policy && typeof policy === 'object') {
    for (const [key, value] of Object.entries(policy)) {
      apply(key, value);
    }
  } else if (policy !== undefined && policy !== null) {
    fail('SANDBOX_CSP', 'CSP policy must be an object or string');
  }

  for (const [directive, value] of Object.entries(CSP_FORCED)) {
    directives.set(directive, value);
  }

  const parts = [];
  for (const directive of BASE_CSP_DIRECTIVES) {
    const value = directives.get(directive);
    if (value !== null && value !== undefined) parts.push(`${directive} ${value}`);
  }
  const sandbox = directives.get('sandbox');
  if (sandbox) parts.push(`sandbox ${sandbox}`);

  return parts.join('; ');
}

export const DEFAULT_SANDBOX_CSP = sandboxCsp();

/* ------------------------------------------------------------------------- */
/* Channel ids                                                                */
/* ------------------------------------------------------------------------- */

function defaultRandomBytes(length) {
  const bytes = new Uint8Array(length);
  const cryptoObject = typeof globalThis !== 'undefined' ? globalThis.crypto : null;
  if (cryptoObject && typeof cryptoObject.getRandomValues === 'function') {
    cryptoObject.getRandomValues(bytes);
    return bytes;
  }
  for (let index = 0; index < length; index += 1) {
    bytes[index] = Math.floor(Math.random() * 256);
  }
  return bytes;
}

/**
 * Create an unguessable channel id.  This id is the real capability for a
 * sandboxed frame; keep it out of logs and only put it in the frame URL hash.
 */
export function createChannelId(options = {}) {
  const length = Number.isInteger(options.bytes) ? Math.min(Math.max(options.bytes, 16), 64) : 16;
  const randomBytes = typeof options.randomBytes === 'function'
    ? options.randomBytes
    : defaultRandomBytes;
  const bytes = randomBytes(length);
  let hex = '';
  for (let index = 0; index < bytes.length; index += 1) {
    hex += Number(bytes[index]).toString(16).padStart(2, '0');
  }
  return `ozma-${hex}`;
}

/* ------------------------------------------------------------------------- */
/* Frame creation                                                             */
/* ------------------------------------------------------------------------- */

function setAttribute(element, name, value) {
  if (element && typeof element.setAttribute === 'function') {
    element.setAttribute(name, String(value));
  } else if (element) {
    element[name] = value;
  }
}

function appendChild(container, element) {
  if (!container || !element) return;
  if (typeof container.append === 'function') container.append(element);
  else if (typeof container.appendChild === 'function') container.appendChild(element);
}

function validateFrameSrc(src, options = {}) {
  if (typeof src !== 'string' || !src.trim()) {
    fail('SANDBOX_SRC', 'A sandbox frame src is required');
  }
  const raw = src.trim();
  if (/[\u0000-\u0020\u007f]/.test(raw)) {
    fail('SANDBOX_SRC', 'Sandbox frame src contains control characters');
  }

  const schemeMatch = raw.match(/^([A-Za-z][A-Za-z0-9+.-]*):/);
  const base = options.baseUrl
    ?? (options.document && options.document.baseURI)
    ?? (typeof document !== 'undefined' ? document.baseURI : undefined);

  if (raw.startsWith('//') || schemeMatch) {
    let resolved;
    try {
      resolved = new URL(raw, base);
    } catch {
      fail('SANDBOX_SRC', 'Sandbox frame src must be a local URL', { src: raw });
    }
    if (resolved.protocol !== 'http:' && resolved.protocol !== 'https:') {
      fail('SANDBOX_SRC', 'Only local HTTP(S) sandbox frames are allowed', { src: raw });
    }
    if (!base) {
      fail('SANDBOX_SRC', 'An absolute sandbox frame src requires a known base URL', { src: raw });
    }
    let baseUrl;
    try {
      baseUrl = new URL(base);
    } catch {
      fail('SANDBOX_SRC', 'Invalid document base URL', { src: raw });
    }
    if (resolved.origin !== baseUrl.origin) {
      fail('SANDBOX_SRC', 'Cross-origin sandbox frames are not allowed', { src: raw });
    }
  }

  return raw;
}

function withChannelInUrl(src, channel, options = {}) {
  const parameter = options.channelParam ?? DEFAULT_CHANNEL_PARAM;
  const encoded = encodeURIComponent(channel);
  const separator = src.includes('#') ? '&' : '#';
  return `${src}${separator}${parameter}=${encoded}`;
}

/**
 * Create a locked-down iframe for a third-party widget.
 *
 * The returned iframe is also decorated with `channel`, `nonce`,
 * `sandboxBridge`, and `teardown` so callers can drive the protocol without a
 * second lookup.  The iframe loads in this origin (which gives it an opaque
 * origin); no remote document is ever mounted.
 *
 * @param {object} options
 * @param {string} options.src local `sandbox-host.html`
 * @param {string} [options.title]
 * @param {string|string[]|object} [options.capabilities]
 * @param {number} [options.timeoutMs] ready timeout
 * @param {Function} [options.onMessage] `(message, event) => void`
 * @param {Document} [options.document]
 * @param {Window} [options.window]
 * @param {Element} [options.container] defaults to `document.body`
 * @returns {HTMLIFrameElement}
 */
export function createSandboxFrame(options = {}) {
  const doc = options.document ?? options.doc
    ?? (typeof document !== 'undefined' ? document : null);
  if (!doc || typeof doc.createElement !== 'function') {
    fail('SANDBOX_DOM', 'A document with createElement is required');
  }

  const capabilities = normalizeCapabilities(options.capabilities);
  const channel = options.channel ?? createChannelId(options);
  if (typeof channel !== 'string' || !channel || channel.length > 256 || !CHANNEL_RE.test(channel)) {
    fail('SANDBOX_CHANNEL', 'Invalid sandbox channel id', { channel });
  }

  const src = validateFrameSrc(options.src, options);
  const srcWithChannel = withChannelInUrl(src, channel, options);
  const title = options.title === undefined ? 'Sandboxed widget' : options.title;
  if (typeof title !== 'string' || title.length > 256) {
    fail('SANDBOX_TITLE', 'Sandbox frame title must be a short string');
  }

  const frame = doc.createElement('iframe');
  const attributes = sandboxAttributeMap(capabilities);
  for (const [name, value] of Object.entries(attributes)) {
    setAttribute(frame, name, value);
  }
  setAttribute(frame, 'title', title);
  setAttribute(frame, 'src', srcWithChannel);
  setAttribute(frame, 'data-ozma-sandbox-channel', channel);
  if (frame.dataset && typeof frame.dataset === 'object') {
    frame.dataset.ozmaSandboxChannel = channel;
  }
  if (frame.style) {
    frame.style.width = '100%';
    frame.style.height = '100%';
    frame.style.border = '0';
  }

  const container = options.container
    ?? (doc.body && typeof doc.body === 'object' ? doc.body : null);
  appendChild(container, frame);

  const bridge = options.bridge ?? new SandboxBridge({
    ...options,
    window: options.window
      ?? (doc.defaultView && typeof doc.defaultView === 'object' ? doc.defaultView : null),
    frame,
    channel,
    capabilities,
    timeoutMs: options.timeoutMs,
    onMessage: options.onMessage,
  });

  frame.channel = channel;
  frame.nonce = channel;
  frame.sandboxBridge = bridge;
  frame.bridge = bridge;
  frame.iframe = frame;
  frame.frame = frame;
  frame.element = frame;
  frame.teardown = (reason) => bridge.teardown(reason);
  frame.destroy = frame.teardown;

  return frame;
}

/* ------------------------------------------------------------------------- */
/* Bridge                                                                     */
/* ------------------------------------------------------------------------- */

function safeCall(callback, ...args) {
  if (typeof callback !== 'function') return;
  try {
    callback(...args);
  } catch {
    // A host callback must never break protocol handling or leak a stack.
  }
}

function defaultTimerApi(options = {}) {
  const timers = options.timers && typeof options.timers === 'object' ? options.timers : {};
  return {
    setTimeout: options.setTimeout
      ?? timers.setTimeout
      ?? (typeof globalThis.setTimeout === 'function' ? globalThis.setTimeout.bind(globalThis) : null),
    clearTimeout: options.clearTimeout
      ?? timers.clearTimeout
      ?? (typeof globalThis.clearTimeout === 'function'
        ? globalThis.clearTimeout.bind(globalThis)
        : null),
    now: options.now
      ?? timers.now
      ?? (() => Date.now()),
  };
}

/**
 * Parent-side protocol bridge.
 *
 * Inbound events are rate-limited, origin-filtered, channel-checked, and
 * payload-validated before any callback runs.  Pending requests reject when
 * the bridge is torn down or times out.
 */
export class SandboxBridge {
  constructor(options = {}) {
    if (options === null || typeof options !== 'object') {
      fail('SANDBOX_OPTIONS', 'SandboxBridge options must be an object');
    }

    const channel = options.channel ?? createChannelId(options);
    if (typeof channel !== 'string' || !channel || channel.length > 256 || !CHANNEL_RE.test(channel)) {
      fail('SANDBOX_CHANNEL', 'Invalid sandbox channel id', { channel });
    }

    this.channel = channel;
    this.nonce = channel;
    this.capabilities = normalizeCapabilities(options.capabilities);
    this.frame = options.frame ?? options.iframe ?? null;
    this.target = options.target ?? null;
    this.window = options.window ?? options.win
      ?? (typeof globalThis.window !== 'undefined' ? globalThis.window : null);
    this.origin = SANDBOX_ORIGIN;
    this.onMessage = options.onMessage;
    this.onError = options.onError;
    this.onAction = options.onAction;
    this.onResize = options.onResize;
    this.onTeardown = options.onTeardown;

    this.maxPayloadBytes = Number.isFinite(options.maxPayloadBytes)
      ? options.maxPayloadBytes
      : DEFAULT_MAX_PAYLOAD_BYTES;
    this.maxPayloadDepth = Number.isFinite(options.maxPayloadDepth)
      ? options.maxPayloadDepth
      : DEFAULT_MAX_PAYLOAD_DEPTH;
    this.maxStringLength = Number.isFinite(options.maxStringLength)
      ? options.maxStringLength
      : DEFAULT_MAX_STRING_LENGTH;

    this.rateLimit = Number.isFinite(options.rateLimit)
      ? options.rateLimit
      : DEFAULT_MESSAGE_RATE;
    this.rateBurst = Number.isFinite(options.rateBurst)
      ? options.rateBurst
      : DEFAULT_MESSAGE_BURST;
    if (this.rateLimit <= 0 || this.rateBurst <= 0) {
      fail('SANDBOX_RATE', 'Rate limit and burst must be positive');
    }

    this._timers = defaultTimerApi(options);
    this._requestTimeoutMs = Number.isFinite(options.requestTimeoutMs)
      ? options.requestTimeoutMs
      : DEFAULT_REQUEST_TIMEOUT_MS;
    this._readyTimeoutMs = Number.isFinite(options.timeoutMs)
      ? options.timeoutMs
      : DEFAULT_READY_TIMEOUT_MS;

    this._bucket = {
      tokens: this.rateBurst,
      last: this._timers.now(),
    };
    this._sequence = 0;
    this._pending = new Map();
    this._listening = false;
    this._destroyed = false;
    this._ready = false;
    this._readyTimer = null;

    this.stats = {
      received: 0,
      accepted: 0,
      rejected: 0,
      wrongOrigin: 0,
      wrongSource: 0,
      rateLimited: 0,
    };

    this.readyPromise = new Promise((resolve, reject) => {
      this._resolveReady = resolve;
      this._rejectReady = reject;
    });
    // Keep an unhandled rejection from surfacing when callers use callbacks.
    this.readyPromise.catch(() => {});

    this._handleEvent = (event) => this.handleEvent(event);
    this.handleMessage = this._handleEvent;

    if (options.autoStart !== false) this.start();
  }

  get destroyed() {
    return this._destroyed;
  }

  isReady() {
    return this._ready;
  }

  whenReady() {
    return this.readyPromise;
  }

  start() {
    if (this._destroyed || this._listening) return this;
    if (this.window && typeof this.window.addEventListener === 'function') {
      this.window.addEventListener('message', this._handleEvent);
      this._listening = true;
    }
    this._armReadyTimeout();
    return this;
  }

  _armReadyTimeout() {
    if (this._ready || this._destroyed || this._readyTimer !== null) return;
    if (!Number.isFinite(this._readyTimeoutMs) || this._readyTimeoutMs <= 0) return;
    if (typeof this._timers.setTimeout !== 'function') return;
    this._readyTimer = this._timers.setTimeout(() => {
      this._readyTimer = null;
      if (this._ready || this._destroyed) return;
      const error = new SandboxError(
        'SANDBOX_TIMEOUT',
        `Sandbox did not report ready within ${this._readyTimeoutMs} ms`,
        { timeoutMs: this._readyTimeoutMs },
      );
      safeCall(this.onError, error);
      this._rejectReady(error);
      this.teardown('timeout', { notify: false });
    }, this._readyTimeoutMs);
    if (this._readyTimer && typeof this._readyTimer.unref === 'function') {
      this._readyTimer.unref();
    }
  }

  _clearReadyTimeout() {
    if (this._readyTimer === null) return;
    if (typeof this._timers.clearTimeout === 'function') {
      this._timers.clearTimeout(this._readyTimer);
    }
    this._readyTimer = null;
  }

  _consumeToken() {
    const now = this._timers.now();
    const elapsed = Math.max(0, now - this._bucket.last) / 1000;
    this._bucket.tokens = Math.min(
      this.rateBurst,
      this._bucket.tokens + elapsed * this.rateLimit,
    );
    this._bucket.last = now;
    if (this._bucket.tokens < 1) return false;
    this._bucket.tokens -= 1;
    return true;
  }

  _sourceMatches(event) {
    if (!event || event.source === undefined || event.source === null) return true;
    const frame = this.frame;
    if (!frame || !frame.contentWindow) return true;
    return event.source === frame.contentWindow;
  }

  /**
   * Handle one MessageEvent-like object.  Returns true when the message was
   * accepted.  Invalid, wrong-origin, wrong-source, and rate-limited messages
   * are ignored and recorded in `stats`.
   */
  handleEvent(event) {
    if (this._destroyed) return false;
    if (!event || typeof event !== 'object') {
      this.stats.rejected += 1;
      return false;
    }

    this.stats.received += 1;

    if (!isSandboxedOrigin(event.origin)) {
      this.stats.wrongOrigin += 1;
      return false;
    }

    if (!this._sourceMatches(event)) {
      this.stats.wrongSource += 1;
      return false;
    }

    if (!this._consumeToken()) {
      this.stats.rateLimited += 1;
      safeCall(this.onError, new SandboxError(
        'SANDBOX_RATE',
        'Sandbox message rate limit exceeded',
        { rateLimit: this.rateLimit, burst: this.rateBurst },
      ));
      return false;
    }

    let message;
    try {
      message = validateSandboxMessage(event.data, {
        channel: this.channel,
        origin: event.origin,
        capabilities: this.capabilities,
        maxPayloadBytes: this.maxPayloadBytes,
        maxPayloadDepth: this.maxPayloadDepth,
        maxStringLength: this.maxStringLength,
      });
    } catch (error) {
      this.stats.rejected += 1;
      const sandboxError = error instanceof SandboxError
        ? error
        : new SandboxError('SANDBOX_MESSAGE', error?.message || 'Invalid sandbox message');
      safeCall(this.onError, sandboxError);
      return false;
    }

    this.stats.accepted += 1;

    if (message.type === 'ready') {
      this._ready = true;
      this._clearReadyTimeout();
      this._resolveReady(message);
    } else if (message.type === 'error') {
      this._handleRemoteError(message);
    } else if (message.type === 'resize') {
      this._handleResize(message);
    }

    safeCall(this.onMessage, message, event);

    if (message.type === 'action') safeCall(this.onAction, message, event);
    if (message.type === 'teardown') {
      this.teardown('remote', { notify: false });
    }

    return true;
  }

  _handleRemoteError(message) {
    const payload = message.payload;
    const remote = new SandboxError(
      'SANDBOX_REMOTE',
      (payload && typeof payload.message === 'string' && payload.message)
        ? payload.message.slice(0, 512)
        : 'Sandbox reported an error',
      { id: message.id, payload },
    );

    if (message.id !== null && this._pending.has(message.id)) {
      const pending = this._pending.get(message.id);
      this._pending.delete(message.id);
      if (pending.timer !== null && typeof this._timers.clearTimeout === 'function') {
        this._timers.clearTimeout(pending.timer);
      }
      pending.reject(remote);
    } else {
      safeCall(this.onError, remote);
    }
  }

  _handleResize(message) {
    if (message.id !== null && this._pending.has(message.id)) {
      const pending = this._pending.get(message.id);
      this._pending.delete(message.id);
      if (pending.timer !== null && typeof this._timers.clearTimeout === 'function') {
        this._timers.clearTimeout(pending.timer);
      }
      pending.resolve(message.payload);
      return;
    }
    safeCall(this.onResize, message.payload, message);
  }

  _nextId() {
    this._sequence += 1;
    return `req-${this._sequence}`;
  }

  _target(options = {}) {
    if (options.target && typeof options.target.postMessage === 'function') {
      return options.target;
    }
    if (this.frame && this.frame.contentWindow
      && typeof this.frame.contentWindow.postMessage === 'function') {
      return this.frame.contentWindow;
    }
    if (this.target && typeof this.target.postMessage === 'function') {
      return this.target;
    }
    return null;
  }

  /**
   * Post a validated message to the sandbox.  The target is deliberately `*`
   * because a sandboxed frame's origin is opaque; the channel in the message
   * is the capability.
   */
  post(type, payload = null, options = {}) {
    if (this._destroyed) {
      fail('SANDBOX_TEARDOWN', 'Sandbox bridge is torn down');
    }
    const id = options.id === undefined ? this._nextId() : options.id;
    const message = {
      v: SANDBOX_PROTOCOL_VERSION,
      channel: this.channel,
      type,
      id,
      payload: payload === undefined ? null : payload,
    };

    validateSandboxMessage(message, {
      channel: this.channel,
      capabilities: this.capabilities,
      maxPayloadBytes: this.maxPayloadBytes,
      maxPayloadDepth: this.maxPayloadDepth,
      maxStringLength: this.maxStringLength,
    });

    const target = this._target(options);
    if (!target) {
      fail('SANDBOX_TARGET', 'No sandbox frame target is available');
    }
    target.postMessage(message, options.targetOrigin ?? '*');
    return message;
  }

  mount(payload, options = {}) {
    return this.post('mount', payload, options);
  }

  data(payload, options = {}) {
    return this.post('data', payload, options);
  }

  action(payload, options = {}) {
    return this.post('action', payload, options);
  }

  request(type, payload = null, options = {}) {
    if (this._destroyed) {
      return Promise.reject(new SandboxError('SANDBOX_TEARDOWN', 'Sandbox bridge is torn down'));
    }
    const id = this._nextId();
    const timeoutMs = Number.isFinite(options.timeoutMs)
      ? options.timeoutMs
      : this._requestTimeoutMs;

    return new Promise((resolve, reject) => {
      let timer = null;
      if (timeoutMs > 0 && typeof this._timers.setTimeout === 'function') {
        timer = this._timers.setTimeout(() => {
          if (!this._pending.has(id)) return;
          this._pending.delete(id);
          reject(new SandboxError(
            'SANDBOX_REQUEST_TIMEOUT',
            `Sandbox request "${type}" timed out`,
            { id, type, timeoutMs },
          ));
        }, timeoutMs);
        if (timer && typeof timer.unref === 'function') timer.unref();
      }

      this._pending.set(id, { resolve, reject, timer, type });
      try {
        this.post(type, payload, { ...options, id });
      } catch (error) {
        this._pending.delete(id);
        if (timer !== null && typeof this._timers.clearTimeout === 'function') {
          this._timers.clearTimeout(timer);
        }
        throw error;
      }
    });
  }

  _rejectPending(error) {
    for (const [id, pending] of this._pending) {
      this._pending.delete(id);
      if (pending.timer !== null && typeof this._timers.clearTimeout === 'function') {
        this._timers.clearTimeout(pending.timer);
      }
      pending.reject(error);
    }
  }

  teardown(reason = 'teardown', options = {}) {
    if (this._destroyed) return;
    this._destroyed = true;
    this._clearReadyTimeout();

    if (options.notify !== false) {
      try {
        const target = this._target(options);
        if (target) {
          target.postMessage({
            v: SANDBOX_PROTOCOL_VERSION,
            channel: this.channel,
            type: 'teardown',
            id: null,
            payload: { reason: String(reason).slice(0, 128) },
          }, options.targetOrigin ?? '*');
        }
      } catch {
        // Removal continues even if the frame is already gone.
      }
    }

    if (this._listening && this.window && typeof this.window.removeEventListener === 'function') {
      this.window.removeEventListener('message', this._handleEvent);
    }
    this._listening = false;

    const error = new SandboxError('SANDBOX_TEARDOWN', 'Sandbox bridge was torn down', {
      reason,
    });
    this._rejectPending(error);

    if (!this._ready) {
      this._rejectReady(error);
    }

    const frame = this.frame;
    if (frame) {
      const parent = frame.parentNode;
      if (parent && typeof parent.removeChild === 'function') {
        try {
          parent.removeChild(frame);
        } catch {
          // Already detached.
        }
      } else if (typeof frame.remove === 'function') {
        frame.remove();
      }
    }

    safeCall(this.onTeardown, reason);
  }

  send(type, payload = null, options = {}) {
    return this.post(type, payload, options);
  }

  close(reason = 'teardown') {
    this.teardown(reason);
  }

  destroy(reason = 'teardown') {
    this.teardown(reason);
  }
}

/**
 * Create a parent-side bridge without a frame.  Useful when the caller owns
 * the frame lifecycle separately.
 */
export function createSandboxBridge(options = {}) {
  return new SandboxBridge(options);
}

export default {
  SandboxBridge,
  checkSandboxMessage,
  createChannelId,
  createSandboxBridge,
  createSandboxFrame,
  isSandboxedOrigin,
  normalizeCapabilities,
  sandboxAttributeMap,
  sandboxAttributes,
  sandboxCsp,
  validateSandboxMessage,
};
