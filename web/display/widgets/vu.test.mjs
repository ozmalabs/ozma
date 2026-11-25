// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Tests for the dependency-free audio VU meter widget.
 *
 * Run with: node --test web/display/widgets/vu.test.mjs
 */

import test from 'node:test';
import assert from 'node:assert/strict';

import {
  clearClip,
  createVuState,
  dbToLinear,
  formatVuDb,
  linearToDb,
  normalizeVuConfig,
  renderVu,
  renderVuSVG,
  severityFor,
  stepVuState,
  updateVu,
  vuGeometry,
} from './vu.js';

const TOKENS = Object.freeze({
  color: {
    accent: { $type: 'color', $value: '#e0a45c' },
    warn: { $type: 'color', $value: '#f0b34d' },
    crit: { $type: 'color', $value: '#e0644a' },
    fg: { $type: 'color', $value: '#f7e9d7' },
    muted: { $type: 'color', $value: '#c7ad91' },
    track: { $type: 'color', $value: 'rgba(247, 233, 215, 0.14)' },
  },
});

const close = (actual, expected, epsilon = 1e-6) => {
  assert.ok(
    Math.abs(actual - expected) <= epsilon,
    `${actual} is not within ${epsilon} of ${expected}`,
  );
};

/* ------------------------------------------------------------------------- */
/* Minimal DOM double                                                         */
/* ------------------------------------------------------------------------- */

class FakeElement {
  constructor(tagName, ownerDocument) {
    this.tagName = String(tagName).toLowerCase();
    this.ownerDocument = ownerDocument;
    this.attributes = new Map();
    this.children = [];
    this.parentNode = null;
    this.textContent = '';
  }

  setAttribute(name, value) {
    this.attributes.set(String(name), String(value));
  }

  getAttribute(name) {
    return this.attributes.has(String(name)) ? this.attributes.get(String(name)) : null;
  }

  removeAttribute(name) {
    this.attributes.delete(String(name));
  }

  appendChild(child) {
    child.parentNode = this;
    this.children.push(child);
    return child;
  }

  removeChild(child) {
    const index = this.children.indexOf(child);
    if (index >= 0) this.children.splice(index, 1);
    child.parentNode = null;
    return child;
  }

  replaceChildren(...nodes) {
    for (const child of this.children) child.parentNode = null;
    this.children = [];
    for (const node of nodes) this.appendChild(node);
  }

  get firstChild() {
    return this.children[0] ?? null;
  }

  get firstElementChild() {
    return this.children[0] ?? null;
  }
}

class FakeDocument {
  createElementNS(namespace, tagName) {
    const element = new FakeElement(tagName, this);
    element.namespaceURI = namespace;
    return element;
  }

  createElement(tagName) {
    return new FakeElement(tagName, this);
  }
}

function byClass(root, name) {
  const classes = String(root?.getAttribute?.('class') || '').split(/\s+/);
  if (classes.includes(name)) return root;
  for (const child of root?.children ?? []) {
    const found = byClass(child, name);
    if (found) return found;
  }
  return null;
}

function allByClass(root, name) {
  const found = [];
  const classes = String(root?.getAttribute?.('class') || '').split(/\s+/);
  if (classes.includes(name)) found.push(root);
  for (const child of root?.children ?? []) {
    found.push(...allByClass(child, name));
  }
  return found;
}

/* ------------------------------------------------------------------------- */
/* dB helpers                                                                 */
/* ------------------------------------------------------------------------- */

test('dB conversion handles zero, silence, and clamping', () => {
  close(dbToLinear(0), 1);
  close(dbToLinear(-20), 0.1);
  close(dbToLinear(-6), 0.5011872336);
  assert.equal(dbToLinear(-Infinity), 0);
  assert.equal(dbToLinear(Number.NaN), 0);
  close(dbToLinear(6, -60, 0), 1);
  close(dbToLinear(-90, -60, 0), 0.001);

  close(linearToDb(1), 0);
  close(linearToDb(0.1), -20);
  assert.equal(linearToDb(0), -Infinity);
  assert.equal(linearToDb(-1), -Infinity);
  assert.equal(linearToDb(Number.NaN), -Infinity);
  assert.equal(linearToDb(0, -60, 0), -60);
  close(linearToDb(0.001, -60, 0), -60);
  assert.equal(linearToDb(2, -60, 0), 0);
  assert.equal(linearToDb(Number.NaN, -60, 0), -60);
});

test('formatted dB values are finite and use configured decimals', () => {
  assert.equal(formatVuDb(-12, {}), '-12.0');
  assert.equal(formatVuDb(-12.345, { decimals: 2 }), '-12.35');
  assert.equal(formatVuDb(Number.NaN, {}), '—');
  assert.equal(formatVuDb(-Infinity, {}), '—');
});

/* ------------------------------------------------------------------------- */
/* Configuration                                                              */
/* ------------------------------------------------------------------------- */

test('normalizeVuConfig applies audio defaults and aliases', () => {
  const config = normalizeVuConfig({});
  assert.equal(config.channels, 'mono');
  assert.equal(config.input, 'db');
  assert.equal(config.min, -60);
  assert.equal(config.max, 0);
  assert.equal(config.attackMs, 10);
  assert.equal(config.releaseMs, 300);
  assert.equal(config.falloffDbPerSec, 24);
  assert.equal(config.peakHoldMs, 1500);
  assert.equal(config.peakFalloffDbPerSec, 12);
  assert.equal(config.rmsWindowMs, 300);
  assert.equal(config.clipThresholdDb, 0);
  assert.equal(config.clipAutoClearMs, 0);
  assert.equal(config.orientation, 'horizontal');
  assert.equal(config.segments, 24);
  assert.equal(config.decimals, 1);

  const compact = normalizeVuConfig({
    channels: 2,
    attack_ms: 5,
    release_ms: 450,
    falloff_db_per_sec: 30,
    peak_hold_ms: 900,
    peak_falloff_db_per_sec: 8,
    rms_window_ms: 250,
    clip_db: -0.5,
    segments: 999,
    orientation: 'vertical',
    warn: -12,
    crit: -3,
    unit: 'dB',
    reduced_motion: true,
  });
  assert.equal(compact.channels, 'stereo');
  assert.equal(compact.attackMs, 5);
  assert.equal(compact.releaseMs, 450);
  assert.equal(compact.falloffDbPerSec, 30);
  assert.equal(compact.peakHoldMs, 900);
  assert.equal(compact.peakFalloffDbPerSec, 8);
  assert.equal(compact.rmsWindowMs, 250);
  assert.equal(compact.clipThresholdDb, -0.5);
  assert.equal(compact.segments, 128);
  assert.equal(compact.orientation, 'vertical');
  assert.equal(compact.reducedMotion, true);
  assert.equal(compact.reducedMotionSet, true);
});

test('normalizeVuConfig detects linear input and swaps inverted ranges', () => {
  const linear = normalizeVuConfig({ input: 'linear', min: 0, max: 1 });
  assert.equal(linear.input, 'linear');
  const inverted = normalizeVuConfig({ min: 0, max: -60 });
  assert.equal(inverted.min, -60);
  assert.equal(inverted.max, 0);
  const flag = normalizeVuConfig({ linear: true });
  assert.equal(flag.input, 'linear');
});

/* ------------------------------------------------------------------------- */
/* Ballistics                                                                 */
/* ------------------------------------------------------------------------- */

test('attack is faster than release for the same delta time', () => {
  const config = normalizeVuConfig({
    attackMs: 10,
    releaseMs: 300,
    falloffDbPerSec: 100_000,
  });

  const rising = createVuState(config);
  stepVuState(rising, -60, config, 0);
  stepVuState(rising, 0, config, 10);
  const rise = rising.l.displayDb - -60;

  const falling = createVuState(config);
  stepVuState(falling, 0, config, 0);
  stepVuState(falling, -60, config, 10);
  const release = 0 - falling.l.displayDb;

  assert.ok(rise > release * 10, `rise ${rise} should be much larger than release ${release}`);
  assert.ok(rising.l.displayDb > -30);
  assert.ok(falling.l.displayDb < 0);
});

test('release and falloff are measured in dB per second', () => {
  const config = normalizeVuConfig({
    attackMs: 0,
    releaseMs: 1,
    falloffDbPerSec: 24,
  });

  const state = createVuState(config);
  stepVuState(state, 0, config, 0);
  stepVuState(state, -60, config, 1000);
  close(state.l.displayDb, -24, 0.01);

  stepVuState(state, -60, config, 1000);
  close(state.l.displayDb, -48, 0.01);

  stepVuState(state, -60, config, 1000);
  close(state.l.displayDb, -60, 0.01);
});

test('long release decays gradually instead of snapping to silence', () => {
  const config = normalizeVuConfig({
    attackMs: 0,
    releaseMs: 300,
    falloffDbPerSec: 24,
  });
  const state = createVuState(config);
  stepVuState(state, 0, config, 0);

  for (let index = 0; index < 10; index += 1) {
    stepVuState(state, -60, config, 100);
  }
  close(state.l.displayDb, -24, 0.01);

  for (let index = 0; index < 10; index += 1) {
    stepVuState(state, -60, config, 100);
  }
  close(state.l.displayDb, -48, 0.01);
  assert.ok(state.l.displayDb > -60);
});

test('peak hold keeps the marker then decays in dB per second', () => {
  const config = normalizeVuConfig({
    attackMs: 0,
    releaseMs: 0,
    falloffDbPerSec: 100_000,
    peakHoldMs: 1500,
    peakFalloffDbPerSec: 12,
  });
  const state = createVuState(config);
  stepVuState(state, 0, config, 0);

  stepVuState(state, -60, config, 1400);
  close(state.l.peakDb, 0, 0.01);
  close(state.l.displayDb, -60, 0.01);

  stepVuState(state, -60, config, 200);
  close(state.l.peakDb, -1.2, 0.01);
  assert.ok(state.l.peakDb > state.l.displayDb);
});

test('clip latches, clears explicitly, and can auto-clear', () => {
  const config = normalizeVuConfig({ attackMs: 0, releaseMs: 0, clipThresholdDb: 0 });
  const state = createVuState(config);
  stepVuState(state, 0, config, 0);
  assert.equal(state.l.clip, true);

  stepVuState(state, -60, config, 1000);
  assert.equal(state.l.clip, true);

  clearClip(state, 'l');
  assert.equal(state.l.clip, false);

  const autoConfig = normalizeVuConfig({
    attackMs: 0,
    releaseMs: 0,
    clipAutoClearMs: 500,
  });
  const auto = createVuState(autoConfig);
  stepVuState(auto, 0, autoConfig, 0);
  stepVuState(auto, -60, autoConfig, 400);
  assert.equal(auto.l.clip, true);
  stepVuState(auto, -60, autoConfig, 200);
  assert.equal(auto.l.clip, false);

  const held = createVuState(autoConfig);
  stepVuState(held, 0, autoConfig, 0);
  stepVuState(held, 0, autoConfig, 600);
  assert.equal(held.l.clip, true);
  stepVuState(held, -60, autoConfig, 600);
  assert.equal(held.l.clip, false);
});

test('RMS uses a bounded recent-sample window', () => {
  const config = normalizeVuConfig({
    attackMs: 0,
    releaseMs: 0,
    falloffDbPerSec: 0,
    rmsWindowMs: 300,
  });
  const state = createVuState(config);
  stepVuState(state, 0, config, 0);
  stepVuState(state, -60, config, 100);
  close(state.l.rmsDb, -3.0103, 0.001);
  stepVuState(state, -60, config, 100);
  close(state.l.rmsDb, -4.7712, 0.001);

  stepVuState(state, -60, config, 400);
  close(state.l.rmsDb, -60, 0.001);
  assert.ok(state.l.rms.length <= 1);

  const bounded = createVuState(normalizeVuConfig({
    attackMs: 0,
    releaseMs: 0,
    rmsWindowMs: 10_000,
  }));
  const boundedConfig = normalizeVuConfig({
    attackMs: 0,
    releaseMs: 0,
    rmsWindowMs: 10_000,
  });
  for (let index = 0; index < 2000; index += 1) {
    stepVuState(bounded, -12, boundedConfig, 1);
  }
  assert.ok(bounded.l.rms.length <= 512);
});

test('non-finite samples and deltas are clamped safely', () => {
  const config = normalizeVuConfig({ attackMs: 0, releaseMs: 0 });
  const state = createVuState(config);
  stepVuState(state, -Infinity, config, 0);
  close(state.l.displayDb, -60);
  close(state.l.peakDb, -60);

  stepVuState(state, 0, config, 0);
  const elapsed = state.elapsedMs;
  stepVuState(state, Number.NaN, config, Number.NaN);
  stepVuState(state, 0, config, -100);
  stepVuState(state, 0, config, Infinity);
  assert.equal(state.elapsedMs, elapsed);
  assert.ok(Number.isFinite(state.l.displayDb));
});

/* ------------------------------------------------------------------------- */
/* Stereo and geometry                                                        */
/* ------------------------------------------------------------------------- */

test('stereo state keeps independent L/R values and mono uses one channel', () => {
  const stereoConfig = normalizeVuConfig({ channels: 2, warn: -12, crit: -3 });
  const stereoState = createVuState(stereoConfig);
  stepVuState(stereoState, { l: -12, r: -24 }, stereoConfig, 0);
  const stereo = vuGeometry(stereoState, stereoConfig, TOKENS);
  assert.equal(stereo.channels.length, 2);
  assert.equal(stereo.channels[0].key, 'l');
  assert.equal(stereo.channels[1].key, 'r');
  close(stereo.channels[0].displayDb, -12);
  close(stereo.channels[1].displayDb, -24);
  assert.equal(stereo.channels[0].severity, 'warn');

  const monoConfig = normalizeVuConfig({ channels: 'mono' });
  const monoState = createVuState(monoConfig);
  stepVuState(monoState, [-12, -24], monoConfig, 0);
  const mono = vuGeometry(monoState, monoConfig, TOKENS);
  assert.equal(mono.channels.length, 1);
  close(mono.channels[0].displayDb, -12);
});

test('geometry produces segmented threshold colours, peak, and clip state', () => {
  const config = normalizeVuConfig({
    min: -60,
    max: 0,
    warn: -12,
    crit: -3,
    segments: 10,
    showValue: true,
    unit: 'dB',
  });
  const state = createVuState(config);
  stepVuState(state, -6, config, 0);
  const geometry = vuGeometry(state, config, TOKENS);
  const channel = geometry.channels[0];
  assert.equal(channel.segments.length, 10);
  assert.equal(channel.severity, 'warn');
  assert.equal(channel.segments.at(-1).severity, 'crit');
  assert.equal(channel.segments.at(-1).active, false);
  assert.ok(channel.segments.filter((segment) => segment.active).length > 0);
  assert.equal(geometry.colors.normal, '#e0a45c');
  assert.equal(geometry.colors.warn, '#f0b34d');
  assert.ok(channel.peak.x1 >= channel.track.x);
  assert.match(channel.readout, /-6\.0 dB/);

  stepVuState(state, 0, config, 0);
  const clipped = vuGeometry(state, config, TOKENS);
  assert.equal(clipped.clip, true);
  assert.equal(clipped.severity, 'crit');
  assert.equal(clipped.channels[0].clip, true);
  assert.equal(severityFor(0, config), 'crit');
});

test('vertical orientation stacks segments bottom-up', () => {
  const config = normalizeVuConfig({ orientation: 'vertical', segments: 5, channels: 2 });
  const state = createVuState(config);
  stepVuState(state, { l: -12, r: -24 }, config, 0);
  const geometry = vuGeometry(state, config, TOKENS);
  assert.equal(geometry.orientation, 'vertical');
  const segments = geometry.channels[0].segments;
  assert.ok(segments[0].y > segments.at(-1).y);
});

/* ------------------------------------------------------------------------- */
/* SVG output and accessibility                                               */
/* ------------------------------------------------------------------------- */

test('renderVuSVG is accessible and suppresses transitions under reduced motion', () => {
  const config = normalizeVuConfig({
    label: 'Master',
    unit: 'dB',
    warn: -12,
    crit: -3,
    channels: 2,
  });
  const state = createVuState(config);
  stepVuState(state, { l: -12, r: -24 }, config, 0);
  const svg = renderVuSVG(config, state, TOKENS, { reducedMotion: true, id: 'master' });

  assert.match(svg, /<svg[^>]*role="img"/);
  assert.match(svg, /aria-label="Master: L -12\.0 dB, R -24\.0 dB, warning"/);
  assert.match(svg, /<title id="master-title">Master<\/title>/);
  assert.match(svg, /<desc id="master-desc">/);
  assert.match(svg, /data-reduced-motion="true"/);
  assert.match(svg, /ozma-vu-reduced-motion/);
  assert.match(svg, /ozma-vu-stereo/);
  assert.equal((svg.match(/class="ozma-vu-channel"/g) || []).length, 2);
  assert.match(svg, /-12\.0 dB/);
  assert.doesNotMatch(svg, /transition:/);

  const moving = renderVuSVG(config, state, TOKENS, { reducedMotion: false, id: 'master' });
  assert.match(moving, /data-reduced-motion="false"/);
  assert.match(moving, /data-animate="true"/);
  assert.match(moving, /transition: opacity/);
});

test('clip and severity are exposed in the accessible label', () => {
  const config = normalizeVuConfig({ label: 'Master', unit: 'dB', clipThresholdDb: 0 });
  const state = createVuState(config);
  stepVuState(state, 0, config, 0);
  const svg = renderVuSVG(config, state, TOKENS, { reducedMotion: true });
  assert.match(svg, /aria-label="Master: 0\.0 dB, clipping"/);
  assert.match(svg, /data-clip="true"/);
  assert.match(svg, /data-severity="crit"/);
});

test('renderVu and updateVu reuse the same SVG element', () => {
  const doc = new FakeDocument();
  const container = doc.createElement('div');
  const config = normalizeVuConfig({ label: 'Left', unit: 'dB', segments: 12 });
  const state = createVuState(config);
  stepVuState(state, -24, config, 0);

  const first = renderVu(container, config, state, TOKENS, { id: 'left' });
  assert.equal(container.children.length, 1);
  assert.equal(container.firstElementChild, first);
  assert.equal(allByClass(first, 'ozma-vu-segment').length, 12);
  const before = first.getAttribute('aria-label');

  stepVuState(state, -6, config, 100);
  const second = updateVu(container, state, config, TOKENS, { id: 'left' });
  assert.equal(second, first);
  assert.equal(container.children.length, 1);
  assert.equal(allByClass(first, 'ozma-vu-segment').length, 12);
  assert.notEqual(first.getAttribute('aria-label'), before);
  assert.equal(allByClass(first, 'ozma-vu-segment').filter((node) => node.getAttribute('data-active') === 'true').length > 0, true);

  const changed = normalizeVuConfig({ label: 'Left', unit: 'dB', segments: 8 });
  stepVuState(state, -6, changed, 100);
  const third = updateVu(container, state, changed, TOKENS, { id: 'left' });
  assert.equal(third, first);
  assert.equal(allByClass(first, 'ozma-vu-segment').length, 8);
});

test('renderVu writes markup when there is no DOM and rejects a missing container', () => {
  const container = { innerHTML: '', firstElementChild: null };
  const config = normalizeVuConfig({ label: 'Left', unit: 'dB' });
  const result = renderVu(container, config, createVuState(config), TOKENS, { reducedMotion: true });
  assert.equal(typeof result, 'string');
  assert.match(container.innerHTML, /role="img"/);
  assert.match(container.innerHTML, /aria-label="Left: -60\.0 dB"/);

  assert.throws(() => renderVu(null, config), TypeError);
  assert.throws(() => updateVu(null, createVuState(config), config), TypeError);
});
