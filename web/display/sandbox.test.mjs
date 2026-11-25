// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Tests for the sandboxed third-party widget runtime and browser CSP.
 *
 * Run with: node --test web/display/sandbox.test.mjs
 *
 * No DOM library is used.  The tests provide the small fake window, document,
 * element, and timer surfaces the module actually needs.
 */

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  DEFAULT_MAX_PAYLOAD_BYTES,
  DEFAULT_MAX_PAYLOAD_DEPTH,
  SANDBOX_ATTRIBUTE_VALUE,
  SANDBOX_MESSAGE_TYPES,
  SANDBOX_PROTOCOL_VERSION,
  SandboxBridge,
  SandboxError,
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
} from './sandbox.js';

const SANDBOX_JS = readFileSync(new URL('./sandbox.js', import.meta.url), 'utf8');
const SANDBOX_HOST = readFileSync(new URL('./sandbox-host.html', import.meta.url), 'utf8');
const DISPLAY_INDEX = readFileSync(new URL('./index.html', import.meta.url), 'utf8');

const CHANNEL = 'ozma-test-channel';

/* ------------------------------------------------------------------------- */
/* Fakes                                                                      */
/* ------------------------------------------------------------------------- */

function fakeTarget() {
  return {
    messages: [],
    origins: [],
    postMessage(message, origin) {
      this.messages.push(message);
      this.origins.push(origin);
    },
  };
}

function fakeElement(tagName = 'iframe') {
  const attributes = new Map();
  const children = [];
  const element = {
    tagName: String(tagName).toUpperCase(),
    attributes,
    children,
    style: {},
    parentNode: null,
    contentWindow: fakeTarget(),
    setAttribute(name, value) {
      attributes.set(name, String(value));
    },
    getAttribute(name) {
      return attributes.has(name) ? attributes.get(name) : null;
    },
    append(child) {
      children.push(child);
      child.parentNode = element;
    },
    appendChild(child) {
      element.append(child);
    },
    removeChild(child) {
      const index = children.indexOf(child);
      if (index !== -1) children.splice(index, 1);
      child.parentNode = null;
    },
    remove() {
      if (element.parentNode) element.parentNode.removeChild(element);
    },
  };
  return element;
}

function fakeDocument() {
  const body = {
    children: [],
    append(child) {
      body.children.push(child);
      child.parentNode = body;
    },
    removeChild(child) {
      const index = body.children.indexOf(child);
      if (index !== -1) body.children.splice(index, 1);
      child.parentNode = null;
    },
  };
  return {
    baseURI: 'https://display.test/',
    body,
    defaultView: null,
    createElement(tagName) {
      return fakeElement(tagName);
    },
  };
}

function fakeWindow() {
  const listeners = new Map();
  return {
    listeners,
    addEventListener(type, listener) {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type).add(listener);
    },
    removeEventListener(type, listener) {
      if (listeners.has(type)) listeners.get(type).delete(listener);
    },
    listenerCount(type) {
      return listeners.has(type) ? listeners.get(type).size : 0;
    },
  };
}

function fakeTimers() {
  let now = 0;
  let sequence = 0;
  const tasks = new Map();

  return {
    now() {
      return now;
    },
    setTimeout(callback, delay) {
      sequence += 1;
      tasks.set(sequence, { callback, at: now + Math.max(0, Number(delay) || 0) });
      return sequence;
    },
    clearTimeout(id) {
      tasks.delete(id);
    },
    advance(milliseconds) {
      now += milliseconds;
      const due = Array.from(tasks.entries())
        .filter(([, task]) => task.at <= now)
        .sort((left, right) => left[1].at - right[1].at);
      for (const [id, task] of due) {
        if (!tasks.has(id)) continue;
        tasks.delete(id);
        task.callback();
      }
    },
    pending() {
      return tasks.size;
    },
  };
}

function message(overrides = {}) {
  return {
    v: SANDBOX_PROTOCOL_VERSION,
    channel: CHANNEL,
    type: 'data',
    id: 'req-1',
    payload: { value: 42 },
    ...overrides,
  };
}

function event(data, overrides = {}) {
  return {
    origin: 'null',
    data,
    source: undefined,
    ...overrides,
  };
}

function makeBridge(overrides = {}) {
  const timers = overrides.timers ?? fakeTimers();
  const win = overrides.window ?? fakeWindow();
  const bridge = new SandboxBridge({
    channel: CHANNEL,
    capabilities: ['render'],
    window: win,
    timeoutMs: 0,
    timers,
    rateLimit: 120,
    rateBurst: 120,
    ...overrides,
    timers,
    window: win,
  });
  return { bridge, timers, win };
}

/* ------------------------------------------------------------------------- */
/* Attributes                                                                 */
/* ------------------------------------------------------------------------- */

test('sandbox attributes grant scripts only', () => {
  assert.equal(SANDBOX_ATTRIBUTE_VALUE, 'sandbox="allow-scripts"');
  assert.equal(sandboxAttributes(), SANDBOX_ATTRIBUTE_VALUE);
  assert.equal(sandboxAttributes({ capabilities: ['render'] }), SANDBOX_ATTRIBUTE_VALUE);
  assert.equal(sandboxAttributes({ capabilities: ['actions'] }), SANDBOX_ATTRIBUTE_VALUE);
  assert.equal(sandboxAttributes(['render', 'actions']), SANDBOX_ATTRIBUTE_VALUE);

  const attributes = sandboxAttributeMap();
  assert.deepEqual(attributes, {
    sandbox: 'allow-scripts',
    referrerpolicy: 'no-referrer',
    allow: '',
    loading: 'lazy',
  });
  assert.ok(!attributes.sandbox.includes('allow-same-origin'));
  assert.ok(!attributes.sandbox.includes('allow-top-navigation'));
  assert.ok(!attributes.sandbox.includes('allow-popups'));
  assert.ok(!attributes.sandbox.includes('allow-forms'));
  assert.ok(!attributes.sandbox.includes('allow-modals'));
  assert.ok(!attributes.sandbox.includes('allow-downloads'));
});

test('sandbox attributes reject unknown and network capabilities', () => {
  assert.throws(
    () => sandboxAttributes({ capabilities: ['render', 'filesystem'] }),
    (error) => error instanceof SandboxError && error.code === 'SANDBOX_CAPABILITY',
  );
  assert.throws(
    () => sandboxAttributes({ capabilities: ['network'] }),
    (error) => error instanceof SandboxError && error.code === 'SANDBOX_CAPABILITY',
  );
  assert.throws(
    () => sandboxAttributeMap({ capabilities: { network: true } }),
    (error) => error.code === 'SANDBOX_CAPABILITY',
  );
});

/* ------------------------------------------------------------------------- */
/* Capability normalisation                                                   */
/* ------------------------------------------------------------------------- */

test('capabilities default to render only, with actions opt-in', () => {
  assert.deepEqual(normalizeCapabilities(), ['render']);
  assert.deepEqual(normalizeCapabilities(''), ['render']);
  assert.deepEqual(normalizeCapabilities(['render']), ['render']);
  assert.deepEqual(normalizeCapabilities(['actions']), ['render', 'actions']);
  assert.deepEqual(normalizeCapabilities('render,actions'), ['render', 'actions']);
  assert.deepEqual(normalizeCapabilities({ actions: true }), ['render', 'actions']);
  assert.deepEqual(normalizeCapabilities({ render: false }), ['render']);
  assert.deepEqual(normalizeCapabilities(new Set(['actions'])), ['render', 'actions']);
  assert.ok(Object.isFrozen(normalizeCapabilities(['actions'])));
});

test('capability rejection fails closed', () => {
  for (const capability of ['network', 'camera', 'storage', 'internet']) {
    assert.throws(
      () => normalizeCapabilities([capability]),
      (error) => error instanceof SandboxError && error.code === 'SANDBOX_CAPABILITY',
      capability,
    );
  }
  assert.throws(
    () => normalizeCapabilities(42),
    (error) => error.code === 'SANDBOX_CAPABILITY',
  );
  assert.throws(
    () => normalizeCapabilities({ actions: 'yes' }),
    (error) => error.code === 'SANDBOX_CAPABILITY',
  );
  assert.throws(
    () => normalizeCapabilities({ mystery: false }),
    (error) => error.code === 'SANDBOX_CAPABILITY',
  );
});

/* ------------------------------------------------------------------------- */
/* Message validation                                                         */
/* ------------------------------------------------------------------------- */

test('valid message passes and is normalised', () => {
  const valid = message();
  const result = validateSandboxMessage(valid, { channel: CHANNEL, origin: 'null' });
  assert.deepEqual(result, valid);

  const withActions = validateSandboxMessage(
    message({ type: 'action', payload: { key: 'mute' } }),
    { channel: CHANNEL, origin: 'null', capabilities: ['actions'] },
  );
  assert.equal(withActions.type, 'action');

  for (const type of SANDBOX_MESSAGE_TYPES) {
    const capabilities = type === 'action' ? ['actions'] : ['render'];
    assert.equal(
      checkSandboxMessage(message({ type }), {
        channel: CHANNEL,
        origin: 'null',
        capabilities,
      }).ok,
      true,
      type,
    );
  }
});

test('version, channel, type, shape, and id are validated', () => {
  const expectCode = (input, code, options = {}) => {
    assert.throws(
      () => validateSandboxMessage(input, { channel: CHANNEL, origin: 'null', ...options }),
      (error) => error instanceof SandboxError && error.code === code,
    );
  };

  expectCode(message({ v: 2 }), 'SANDBOX_VERSION');
  expectCode(message({ v: '1' }), 'SANDBOX_VERSION');
  expectCode(message({ channel: 'other' }), 'SANDBOX_CHANNEL');
  expectCode(message({ channel: '' }), 'SANDBOX_CHANNEL');
  expectCode(message({ type: 'eval' }), 'SANDBOX_MESSAGE_TYPE');
  expectCode(message({ id: 'bad id with spaces' }), 'SANDBOX_MESSAGE_ID');
  expectCode(message({ id: -1 }), 'SANDBOX_MESSAGE_ID');
  expectCode(message({ extra: true }), 'SANDBOX_MESSAGE_SHAPE');
  expectCode([], 'SANDBOX_MESSAGE_SHAPE');
  expectCode(null, 'SANDBOX_MESSAGE_SHAPE');
  assert.throws(
    () => validateSandboxMessage(message(), { origin: 'null' }),
    (error) => error.code === 'SANDBOX_CHANNEL',
  );
});

test('non-sandbox origins are rejected by validation', () => {
  for (const origin of ['https://display.test', 'https://evil.test', '', undefined]) {
    if (origin === undefined) continue;
    assert.throws(
      () => validateSandboxMessage(message(), { channel: CHANNEL, origin }),
      (error) => error.code === 'SANDBOX_ORIGIN',
      origin,
    );
  }
  assert.equal(isSandboxedOrigin('null'), true);
  assert.equal(isSandboxedOrigin('https://display.test'), false);
  assert.equal(isSandboxedOrigin(undefined), false);
  assert.equal(isSandboxedOrigin(''), false);
});

test('action messages require the actions capability', () => {
  assert.throws(
    () => validateSandboxMessage(message({ type: 'action' }), {
      channel: CHANNEL,
      origin: 'null',
    }),
    (error) => error.code === 'SANDBOX_CAPABILITY',
  );
  assert.equal(
    checkSandboxMessage(message({ type: 'action' }), {
      channel: CHANNEL,
      origin: 'null',
      capabilities: ['actions'],
    }).ok,
    true,
  );
});

test('payload types, prototypes, accessors, and special values fail closed', () => {
  const expectCode = (payload, code, options = {}) => {
    assert.throws(
      () => validateSandboxMessage(message({ payload }), {
        channel: CHANNEL,
        origin: 'null',
        ...options,
      }),
      (error) => error instanceof SandboxError && error.code === code,
    );
  };

  expectCode({ value: Number.NaN }, 'SANDBOX_PAYLOAD_NUMBER');
  expectCode({ value: Number.POSITIVE_INFINITY }, 'SANDBOX_PAYLOAD_NUMBER');
  expectCode({ value: undefined }, 'SANDBOX_PAYLOAD_TYPE');
  expectCode({ value: () => 1 }, 'SANDBOX_PAYLOAD_TYPE');
  expectCode({ value: Symbol('x') }, 'SANDBOX_PAYLOAD_TYPE');
  expectCode({ value: 1n }, 'SANDBOX_PAYLOAD_TYPE');
  expectCode({ value: new Date(0) }, 'SANDBOX_PAYLOAD_PROTOTYPE');
  expectCode(Object.create({ inherited: true }), 'SANDBOX_PAYLOAD_PROTOTYPE');
  expectCode(JSON.parse('{"__proto__":{"polluted":true}}'), 'SANDBOX_PAYLOAD_KEY');
  expectCode({ constructor: {} }, 'SANDBOX_PAYLOAD_KEY');
  expectCode({ onClick: 'x' }, 'SANDBOX_PAYLOAD_KEY');

  const accessor = {};
  Object.defineProperty(accessor, 'value', {
    enumerable: true,
    get() {
      return 1;
    },
  });
  expectCode(accessor, 'SANDBOX_PAYLOAD_ACCESSOR');

  const hidden = {};
  Object.defineProperty(hidden, 'value', {
    enumerable: false,
    value: 1,
  });
  expectCode(hidden, 'SANDBOX_PAYLOAD_HIDDEN');

  const cyclic = {};
  cyclic.self = cyclic;
  expectCode(cyclic, 'SANDBOX_PAYLOAD_CYCLE');
});

test('payload size, depth, and string limits are enforced', () => {
  assert.ok(DEFAULT_MAX_PAYLOAD_BYTES >= 256 * 1024);
  assert.equal(DEFAULT_MAX_PAYLOAD_DEPTH, 16);

  assert.throws(
    () => validateSandboxMessage(message({ payload: 'x'.repeat(300) }), {
      channel: CHANNEL,
      origin: 'null',
      maxPayloadBytes: 128,
    }),
    (error) => error.code === 'SANDBOX_PAYLOAD_SIZE',
  );

  assert.throws(
    () => validateSandboxMessage(message({ payload: 'x'.repeat(50) }), {
      channel: CHANNEL,
      origin: 'null',
      maxStringLength: 10,
    }),
    (error) => error.code === 'SANDBOX_PAYLOAD_STRING',
  );

  let deep = { leaf: true };
  for (let index = 0; index < 20; index += 1) deep = { child: deep };
  assert.throws(
    () => validateSandboxMessage(message({ payload: deep }), {
      channel: CHANNEL,
      origin: 'null',
    }),
    (error) => error.code === 'SANDBOX_PAYLOAD_DEPTH',
  );

  assert.equal(
    checkSandboxMessage(message({
      payload: { a: [1, 'two', true, null, { three: 3 }] },
    }), { channel: CHANNEL, origin: 'null' }).ok,
    true,
  );
});

test('checkSandboxMessage reports failures without throwing', () => {
  const result = checkSandboxMessage(message({ v: 0 }), { channel: CHANNEL, origin: 'null' });
  assert.equal(result.ok, false);
  assert.equal(result.message, null);
  assert.equal(result.error.code, 'SANDBOX_VERSION');
});

/* ------------------------------------------------------------------------- */
/* CSP                                                                        */
/* ------------------------------------------------------------------------- */

test('sandbox CSP denies everything except the inline host shim', () => {
  const csp = sandboxCsp();
  assert.match(csp, /(?:^|; )default-src 'none'(?:;|$)/);
  assert.match(csp, /script-src 'unsafe-inline'/);
  assert.match(csp, /style-src 'unsafe-inline'/);
  assert.match(csp, /img-src data:/);
  assert.match(csp, /connect-src 'none'/);
  assert.match(csp, /font-src 'none'/);
  assert.match(csp, /object-src 'none'/);
  assert.match(csp, /base-uri 'none'/);
  assert.match(csp, /form-action 'none'/);
  assert.match(csp, /frame-src 'none'/);
  assert.ok(!csp.includes('unsafe-eval'));
  assert.ok(!csp.includes('allow-same-origin'));

  const overridden = sandboxCsp({
    scriptSrc: "'self'",
    imgSrc: "'self' data:",
    connectSrc: "'self'",
  });
  assert.match(overridden, /script-src 'self'/);
  assert.match(overridden, /connect-src 'self'/);
  assert.match(overridden, /object-src 'none'/);

  assert.equal(sandboxCsp({ sandbox: 'allow-scripts' }).includes('sandbox allow-scripts'), true);
  assert.throws(
    () => sandboxCsp({ sandbox: 'allow-scripts allow-same-origin' }),
    (error) => error.code === 'SANDBOX_CSP',
  );
  assert.throws(
    () => sandboxCsp({ unknownSrc: "'self'" }),
    (error) => error.code === 'SANDBOX_CSP',
  );
});

/* ------------------------------------------------------------------------- */
/* Channel ids and frame creation                                             */
/* ------------------------------------------------------------------------- */

test('channel ids are unguessable and injectable for tests', () => {
  const channel = createChannelId();
  assert.match(channel, /^ozma-[0-9a-f]{32}$/);
  assert.notEqual(channel, createChannelId());

  const deterministic = createChannelId({
    bytes: 16,
    randomBytes(length) {
      return new Uint8Array(length).fill(0xab);
    },
  });
  assert.equal(deterministic, `ozma-${'ab'.repeat(16)}`);
});

test('createSandboxFrame applies locked attributes and a unique channel', () => {
  const doc = fakeDocument();
  const frame = createSandboxFrame({
    src: 'sandbox-host.html',
    title: 'Weather widget',
    capabilities: ['render'],
    document: doc,
    timeoutMs: 0,
  });

  assert.equal(frame.getAttribute('sandbox'), 'allow-scripts');
  assert.equal(frame.getAttribute('referrerpolicy'), 'no-referrer');
  assert.equal(frame.getAttribute('allow'), '');
  assert.equal(frame.getAttribute('loading'), 'lazy');
  assert.equal(frame.getAttribute('title'), 'Weather widget');
  assert.match(frame.getAttribute('src'), /^sandbox-host\.html#ozma-channel=ozma-[0-9a-f]{32}$/);
  assert.equal(frame.channel, frame.nonce);
  assert.match(frame.channel, /^ozma-[0-9a-f]{32}$/);
  assert.equal(frame.getAttribute('data-ozma-sandbox-channel'), frame.channel);
  assert.equal(doc.body.children.length, 1);
  assert.equal(doc.body.children[0], frame);
  assert.ok(frame.sandboxBridge instanceof SandboxBridge);
  assert.equal(frame.sandboxBridge.channel, frame.channel);
  assert.equal(frame.sandboxBridge.isReady(), false);

  const other = createSandboxFrame({
    src: '/sandbox-host.html',
    document: doc,
    timeoutMs: 0,
  });
  assert.notEqual(frame.channel, other.channel);
  assert.match(other.getAttribute('src'), /^\/sandbox-host\.html#ozma-channel=/);

  frame.teardown('test-cleanup');
});

test('createSandboxFrame rejects cross-origin, unknown, and network requests', () => {
  const doc = fakeDocument();
  assert.throws(
    () => createSandboxFrame({
      src: 'https://evil.test/sandbox-host.html',
      document: doc,
      timeoutMs: 0,
    }),
    (error) => error.code === 'SANDBOX_SRC',
  );
  assert.throws(
    () => createSandboxFrame({
      src: 'sandbox-host.html',
      capabilities: ['network'],
      document: doc,
      timeoutMs: 0,
    }),
    (error) => error.code === 'SANDBOX_CAPABILITY',
  );
  assert.throws(
    () => createSandboxFrame({
      src: 'sandbox-host.html',
      capabilities: ['camera'],
      document: doc,
      timeoutMs: 0,
    }),
    (error) => error.code === 'SANDBOX_CAPABILITY',
  );
  assert.throws(
    () => createSandboxFrame({ src: '', document: doc, timeoutMs: 0 }),
    (error) => error.code === 'SANDBOX_SRC',
  );
  assert.equal(doc.body.children.length, 0);
});

/* ------------------------------------------------------------------------- */
/* Bridge protocol                                                            */
/* ------------------------------------------------------------------------- */

test('bridge accepts valid ready/data messages and posts normalised messages', () => {
  const { bridge } = makeBridge();
  const target = bridge.frame ? bridge.frame.contentWindow : null;
  assert.equal(target, null);

  const frame = fakeElement();
  bridge.frame = frame;

  assert.equal(bridge.handleEvent(event(message({ type: 'ready', payload: {} }))), true);
  assert.equal(bridge.isReady(), true);
  assert.equal(bridge.stats.accepted, 1);

  const posted = bridge.mount({ nodes: [{ type: 'text', text: 'hi' }] });
  assert.equal(posted.v, 1);
  assert.equal(posted.channel, CHANNEL);
  assert.equal(posted.type, 'mount');
  assert.match(String(posted.id), /^req-/);
  assert.deepEqual(frame.contentWindow.messages.at(-1), posted);
  assert.equal(frame.contentWindow.origins.at(-1), '*');

  const data = bridge.data({ value: 3 });
  assert.equal(data.type, 'data');
  assert.throws(
    () => bridge.action({ key: 'mute' }),
    (error) => error.code === 'SANDBOX_CAPABILITY',
  );

  bridge.teardown('test-cleanup');
});

test('bridge ignores wrong origins, wrong sources, and invalid messages', () => {
  const win = fakeWindow();
  const timers = fakeTimers();
  const frame = fakeElement();
  const { bridge } = makeBridge({ window: win, timers, frame, rateBurst: 2, rateLimit: 1 });

  assert.equal(bridge.handleEvent(event(message(), { origin: 'https://evil.test' })), false);
  assert.equal(bridge.stats.wrongOrigin, 1);
  assert.equal(bridge.stats.rateLimited, 0);

  const otherSource = {};
  assert.equal(bridge.handleEvent(event(message(), { source: otherSource })), false);
  assert.equal(bridge.stats.wrongSource, 1);

  assert.equal(bridge.handleEvent(event(message({ channel: 'other' }))), false);
  assert.equal(bridge.stats.rejected, 1);

  assert.equal(bridge.handleEvent(event(message({ type: 'ready', payload: {} }))), true);
  assert.equal(bridge.handleEvent(event(message({ type: 'data' }))), false);
  assert.equal(bridge.stats.rateLimited, 1);

  bridge.teardown('test-cleanup');
});

test('bridge gates inbound action messages on the actions capability', () => {
  const withoutActions = makeBridge();
  assert.equal(
    withoutActions.bridge.handleEvent(event(message({ type: 'action', payload: { key: 'x' } }))),
    false,
  );
  assert.equal(withoutActions.bridge.stats.rejected, 1);
  withoutActions.bridge.teardown('test-cleanup');

  const seen = [];
  const withActions = makeBridge({
    capabilities: ['render', 'actions'],
    onAction(messageValue) {
      seen.push(messageValue);
    },
  });
  assert.equal(
    withActions.bridge.handleEvent(event(message({ type: 'action', payload: { key: 'x' } }))),
    true,
  );
  assert.equal(seen.length, 1);
  assert.equal(seen[0].payload.key, 'x');
  withActions.bridge.teardown('test-cleanup');
});

test('bridge request resolves on resize and rejects on remote error', async () => {
  const { bridge } = makeBridge();
  bridge.frame = fakeElement();

  const resizePromise = bridge.request('mount', { nodes: [] }, { timeoutMs: 0 });
  const resizeId = bridge.frame.contentWindow.messages.at(-1).id;
  bridge.handleEvent(event(message({ type: 'resize', id: resizeId, payload: { width: 10 } })));
  assert.deepEqual(await resizePromise, { width: 10 });

  const errorPromise = bridge.request('data', { value: 1 }, { timeoutMs: 0 });
  const errorId = bridge.frame.contentWindow.messages.at(-1).id;
  bridge.handleEvent(event(message({
    type: 'error',
    id: errorId,
    payload: { message: 'boom' },
  })));
  await assert.rejects(errorPromise, (error) => {
    assert.equal(error.code, 'SANDBOX_REMOTE');
    assert.equal(error.message, 'boom');
    return true;
  });

  bridge.teardown('test-cleanup');
});

test('bridge request and ready timeouts reject cleanly', async () => {
  const timers = fakeTimers();
  const { bridge } = makeBridge({
    timers,
    timeoutMs: 50,
    autoStart: true,
  });
  bridge.frame = fakeElement();

  const ready = bridge.whenReady();
  timers.advance(50);
  await assert.rejects(ready, (error) => error.code === 'SANDBOX_TIMEOUT');
  assert.equal(bridge.destroyed, true);

  const requestTimers = fakeTimers();
  const second = makeBridge({ timers: requestTimers, timeoutMs: 0 });
  second.bridge.frame = fakeElement();
  const pending = second.bridge.request('mount', { nodes: [] }, { timeoutMs: 25 });
  requestTimers.advance(25);
  await assert.rejects(pending, (error) => error.code === 'SANDBOX_REQUEST_TIMEOUT');
  second.bridge.teardown('test-cleanup');
});

test('teardown removes listeners and the iframe and rejects pending work', async () => {
  const win = fakeWindow();
  const doc = fakeDocument();
  const timers = fakeTimers();
  const frame = createSandboxFrame({
    src: 'sandbox-host.html',
    document: doc,
    window: win,
    timers,
    timeoutMs: 0,
  });

  assert.equal(win.listenerCount('message'), 1);
  assert.equal(doc.body.children.length, 1);

  const pending = frame.sandboxBridge.request('mount', { nodes: [] }, { timeoutMs: 0 });
  frame.teardown('operator');

  assert.equal(win.listenerCount('message'), 0);
  assert.equal(doc.body.children.length, 0);
  assert.equal(frame.sandboxBridge.destroyed, true);
  await assert.rejects(pending, (error) => error.code === 'SANDBOX_TEARDOWN');
  assert.equal(frame.sandboxBridge.teardown('again'), undefined);
});

test('bridge removes a remote teardown and stops accepting messages', () => {
  const win = fakeWindow();
  const doc = fakeDocument();
  const frame = createSandboxFrame({
    src: 'sandbox-host.html',
    document: doc,
    window: win,
    timeoutMs: 0,
  });

  const channel = frame.sandboxBridge.channel;
  assert.equal(
    frame.sandboxBridge.handleEvent(event(message({ channel, type: 'ready', payload: {} }))),
    true,
  );
  assert.equal(
    frame.sandboxBridge.handleEvent(event(message({
      channel,
      type: 'teardown',
      id: null,
      payload: null,
    }))),
    true,
  );
  assert.equal(frame.sandboxBridge.destroyed, true);
  assert.equal(win.listenerCount('message'), 0);
  assert.equal(doc.body.children.length, 0);
  assert.equal(frame.sandboxBridge.handleEvent(event(message())), false);
});

test('createSandboxBridge is a thin constructor', () => {
  const bridge = createSandboxBridge({
    channel: CHANNEL,
    window: fakeWindow(),
    timeoutMs: 0,
  });
  assert.ok(bridge instanceof SandboxBridge);
  assert.equal(bridge.channel, CHANNEL);
  bridge.teardown('test-cleanup');
});

/* ------------------------------------------------------------------------- */
/* Host document and page CSP                                                 */
/* ------------------------------------------------------------------------- */

test('sandbox-host.html has a strict CSP and no network or script escapes', () => {
  assert.match(SANDBOX_HOST, /http-equiv="Content-Security-Policy"/);
  assert.match(SANDBOX_HOST, /default-src 'none'/);
  assert.match(SANDBOX_HOST, /script-src 'unsafe-inline'/);
  assert.match(SANDBOX_HOST, /style-src 'unsafe-inline'/);
  assert.match(SANDBOX_HOST, /img-src data:/);
  assert.match(SANDBOX_HOST, /connect-src 'none'/);
  assert.match(SANDBOX_HOST, /object-src 'none'/);
  assert.match(SANDBOX_HOST, /base-uri 'none'/);
  assert.match(SANDBOX_HOST, /form-action 'none'/);

  assert.doesNotMatch(SANDBOX_HOST, /unsafe-eval/i);
  assert.doesNotMatch(SANDBOX_HOST, /<script[^>]+src\s*=/i);
  assert.doesNotMatch(SANDBOX_HOST, /\beval\s*\(/);
  assert.doesNotMatch(SANDBOX_HOST, /\bnew\s+Function\b/);
  assert.doesNotMatch(SANDBOX_HOST, /\bfetch\s*\(/);
  assert.doesNotMatch(SANDBOX_HOST, /XMLHttpRequest/);
  assert.doesNotMatch(SANDBOX_HOST, /window\.open/);
  assert.doesNotMatch(SANDBOX_HOST, /location\s*=/);
  assert.doesNotMatch(SANDBOX_HOST, /innerHTML/);
  assert.doesNotMatch(SANDBOX_HOST, /<foreignObject/i);
  assert.doesNotMatch(SANDBOX_HOST, /<script[^>]*>[^<]*alert/i);

  assert.match(SANDBOX_HOST, /parent\.postMessage/);
  assert.match(SANDBOX_HOST, /send\('ready'/);
  assert.match(SANDBOX_HOST, /message\.type === 'mount'/);
  assert.match(SANDBOX_HOST, /message\.type === 'data'/);
  assert.match(SANDBOX_HOST, /send\('resize'/);
  assert.match(SANDBOX_HOST, /send\('error'/);
  assert.match(SANDBOX_HOST, /createElementNS/);
  assert.match(SANDBOX_HOST, /textContent/);
});

test('index.html CSP keeps the WebSocket adapter working', () => {
  const match = DISPLAY_INDEX.match(
    /http-equiv="Content-Security-Policy"\s+content="([^"]+)"/,
  );
  assert.ok(match, 'index.html must declare a CSP meta tag');
  const csp = match[1];

  assert.match(csp, /default-src 'self'/);
  assert.match(csp, /script-src 'self'/);
  assert.match(csp, /style-src 'self' 'unsafe-inline'/);
  assert.match(csp, /img-src 'self' data:/);
  assert.match(csp, /connect-src 'self' ws: wss:/);
  assert.match(csp, /font-src 'self'/);
  assert.match(csp, /frame-src 'self'/);
  assert.match(csp, /object-src 'none'/);
  assert.match(csp, /base-uri 'none'/);
  assert.match(csp, /form-action 'none'/);
  assert.ok(!csp.includes('unsafe-eval'));

  assert.doesNotMatch(DISPLAY_INDEX, /\son[a-z]+\s*=/i);
  assert.match(DISPLAY_INDEX, /<script type="module" src="client\.js"><\/script>/);
});

test('sandbox.js contains no executable or main-origin escape tokens', () => {
  assert.doesNotMatch(SANDBOX_JS, /\beval\s*\(/);
  assert.doesNotMatch(SANDBOX_JS, /\bnew\s+Function\b/);
  assert.doesNotMatch(SANDBOX_JS, /innerHTML/);
  assert.doesNotMatch(SANDBOX_JS, /document\.write/);
  assert.doesNotMatch(SANDBOX_JS, /allow-same-origin/);
  assert.doesNotMatch(SANDBOX_JS, /allow-top-navigation/);
  assert.doesNotMatch(SANDBOX_JS, /allow-popups/);
  assert.doesNotMatch(SANDBOX_JS, /allow-forms/);
  assert.doesNotMatch(SANDBOX_JS, /allow-modals/);
  assert.doesNotMatch(SANDBOX_JS, /allow-downloads/);
});
