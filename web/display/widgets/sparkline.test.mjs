// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Tests for the dependency-free sparkline/history widget.
 *
 * Run with: node --test web/display/widgets/sparkline.test.mjs
 */

import test from 'node:test';
import assert from 'node:assert/strict';

import {
  areaPath,
  formatSparklineValue,
  normalizeHistory,
  renderSparkline,
  renderSparklineSVG,
  severityFor,
  sparklineGeometry,
  sparklinePath,
  thresholdSegments,
  updateSparkline,
} from './sparkline.js';

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

  get classList() {
    return String(this.getAttribute('class') || '').split(/\s+/).filter(Boolean);
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
  if (!root) return null;
  if (String(root.getAttribute?.('class') || '').split(/\s+/).includes(name)) return root;
  for (const child of root.children ?? []) {
    const found = byClass(child, name);
    if (found) return found;
  }
  return null;
}

function linePaths(svg) {
  return byClass(svg, 'ozma-sparkline-lines')?.children ?? [];
}

const TOKENS = Object.freeze({
  color: {
    accent: { $type: 'color', $value: '#5b6fff' },
    warn: { $type: 'color', $value: '#f0b94a' },
    crit: { $type: 'color', $value: '#f26464' },
    fg: { $type: 'color', $value: '#e5e7eb' },
    muted: { $type: 'color', $value: '#8d9bb3' },
    dim: { $type: 'color', $value: '#64728a' },
    track: { $type: 'color', $value: 'rgba(255, 255, 255, 0.12)' },
    grid: { $type: 'color', $value: 'rgba(255, 255, 255, 0.2)' },
  },
});

/* ------------------------------------------------------------------------- */
/* History                                                                    */
/* ------------------------------------------------------------------------- */

test('normalizeHistory preserves order, maps gaps, and limits the window', () => {
  assert.deepEqual(
    normalizeHistory([1, 2, null, 4, Number.NaN, 6, '7', 'nope', undefined, true, false]),
    [1, 2, null, 4, null, 6, 7, null, null, 1, 0],
  );
  assert.deepEqual(normalizeHistory({ samples: [1, 2, 3, 4] }), [1, 2, 3, 4]);
  assert.deepEqual(normalizeHistory({ values: [1, 2, 3, 4] }, { points: 2 }), [3, 4]);
  assert.deepEqual(normalizeHistory({ history: { samples: [5, 6] } }), [5, 6]);
  assert.deepEqual(normalizeHistory(new Float32Array([1.5, 2.5])), [1.5, 2.5]);
  assert.deepEqual(normalizeHistory('not history'), []);
  assert.deepEqual(normalizeHistory(null), []);
});

/* ------------------------------------------------------------------------- */
/* Geometry and paths                                                         */
/* ------------------------------------------------------------------------- */

test('sparklineGeometry supports auto ranges, fixed dimensions, and padded bounds', () => {
  const geometry = sparklineGeometry(
    { width: 120, height: 40, padding: 4 },
    [1, 3, 5],
  );

  assert.equal(geometry.width, 120);
  assert.equal(geometry.height, 40);
  assert.equal(geometry.min, 1);
  assert.equal(geometry.max, 5);
  assert.equal(geometry.definedCount, 3);
  assert.equal(geometry.points.length, 3);
  assert.ok(geometry.points.every((point) => point.y >= geometry.top && point.y <= geometry.bottom));
  assert.equal(geometry.points[0].y, geometry.bottom);
  assert.equal(geometry.points[2].y, geometry.top);
  assert.equal(geometry.current, 5);
});

test('sparklineGeometry respects explicit min/max and clamps out-of-range samples', () => {
  const geometry = sparklineGeometry(
    { min: 0, max: 100, width: 100, height: 30, clampValues: true },
    [-10, 50, 150],
  );

  assert.equal(geometry.min, 0);
  assert.equal(geometry.max, 100);
  assert.equal(geometry.points[0].value, -10);
  assert.equal(geometry.points[0].clamped, 0);
  assert.equal(geometry.points[2].clamped, 100);
  assert.equal(geometry.points[0].y, geometry.bottom);
  assert.equal(geometry.points[2].y, geometry.top);
  assert.ok(geometry.points[1].y > geometry.top && geometry.points[1].y < geometry.bottom);
});

test('gaps split the path into separate subpaths', () => {
  const geometry = sparklineGeometry({ min: 0, max: 10 }, [1, 2, null, 4, 5]);
  assert.equal(geometry.segments.length, 2);
  assert.equal(geometry.gapCount, 1);

  const path = sparklinePath(geometry);
  assert.equal((path.match(/\bM\b/g) || []).length, 2);
  assert.ok(path.includes('L'));

  const area = areaPath(geometry);
  assert.equal((area.match(/\bM\b/g) || []).length, 2);
  assert.equal((area.match(/\bZ\b/g) || []).length, 2);
  assert.ok(area.includes(String(geometry.baselineY ?? geometry.bottom)));
});

test('empty and single-sample histories render safely', () => {
  const empty = sparklineGeometry({ min: 0, max: 1 }, []);
  assert.equal(empty.definedCount, 0);
  assert.equal(sparklinePath(empty), '');
  assert.equal(areaPath(empty), '');

  const single = sparklineGeometry({ min: 0, max: 10 }, [5]);
  assert.equal(single.segments.length, 1);
  assert.equal(single.segments[0].length, 1);
  assert.ok(sparklinePath(single).startsWith('M '));
});

/* ------------------------------------------------------------------------- */
/* Thresholds and severity                                                    */
/* ------------------------------------------------------------------------- */

test('severityFor applies warning and critical thresholds', () => {
  const config = { warn: 70, crit: 90 };
  assert.equal(severityFor(10, config), 'normal');
  assert.equal(severityFor(70, config), 'warn');
  assert.equal(severityFor(89.9, config), 'warn');
  assert.equal(severityFor(90, config), 'crit');
  assert.equal(severityFor(Number.NaN, config), 'normal');
  assert.equal(severityFor(100, {}), 'normal');
});

test('thresholdSegments splits lines at thresholds and uses severity colours', () => {
  const geometry = sparklineGeometry(
    { min: 0, max: 100, warn: 50, crit: 80 },
    [10, 60, 90],
    TOKENS,
  );
  const segments = thresholdSegments(geometry);
  const severities = new Set(segments.map((segment) => segment.severity));

  assert.ok(severities.has('normal'));
  assert.ok(severities.has('warn'));
  assert.ok(severities.has('crit'));
  assert.equal(
    segments.find((segment) => segment.severity === 'warn').color,
    geometry.colors.warn,
  );
  assert.equal(
    segments.find((segment) => segment.severity === 'crit').color,
    geometry.colors.crit,
  );
  assert.ok(segments.every((segment) => segment.path.startsWith('M ')));
});

/* ------------------------------------------------------------------------- */
/* SVG rendering and accessibility                                            */
/* ------------------------------------------------------------------------- */

test('renderSparklineSVG emits accessible SVG with title, description, and aria label', () => {
  const markup = renderSparklineSVG(
    {
      label: 'CPU trend',
      unit: '%',
      showValue: true,
      baseline: 'min',
      area: true,
      ticks: 3,
      warn: 70,
      crit: 90,
      decimals: 0,
    },
    [12, 30, 72, 95],
    TOKENS,
    { id: 'cpu-trend' },
  );

  assert.ok(markup.includes('role="img"'));
  assert.ok(markup.includes('<title id="cpu-trend-title">CPU trend</title>'));
  assert.ok(markup.includes('<desc id="cpu-trend-desc">'));
  assert.ok(markup.includes('aria-label="CPU trend 95 %, critical"'));
  assert.ok(markup.includes('aria-describedby="cpu-trend-desc"'));
  assert.ok(markup.includes('95 %'));
  assert.ok(markup.includes('ozma-sparkline-baseline'));
  assert.ok(markup.includes('ozma-sparkline-area'));
  assert.ok(markup.includes('ozma-sparkline-tick'));
  assert.ok(markup.includes(TOKENS.color.warn.$value));
  assert.ok(markup.includes(TOKENS.color.crit.$value));
});

test('renderSparklineSVG honours reduced motion and marks animated output', () => {
  const reduced = renderSparklineSVG({ animate: true, label: 'Motion' }, [1, 2, 3], TOKENS, {
    reducedMotion: true,
  });
  assert.ok(reduced.includes('ozma-sparkline-reduced-motion'));
  assert.ok(reduced.includes('data-reduced-motion="true"'));
  assert.ok(!reduced.includes('transition: stroke'));

  const animated = renderSparklineSVG({ animate: true, label: 'Motion' }, [1, 2, 3], TOKENS, {
    reducedMotion: false,
  });
  assert.ok(animated.includes('data-animate="true"'));
  assert.ok(animated.includes('data-reduced-motion="false"'));
  assert.ok(animated.includes('transition: stroke'));
});

test('renderSparklineSVG escapes labels and handles no-data histories', () => {
  const markup = renderSparklineSVG(
    { label: 'A < B & C', showValue: true },
    [],
    TOKENS,
    { id: 'escape' },
  );
  assert.ok(markup.includes('A &lt; B &amp; C'));
  assert.ok(!markup.includes('<script'));
  assert.ok(markup.includes('no data'));
});

test('formatSparklineValue applies decimals and handles non-finite input', () => {
  assert.equal(formatSparklineValue(12.345, { decimals: 1 }), '12.3');
  assert.equal(formatSparklineValue(12.345, { decimals: 0 }), '12');
  assert.equal(formatSparklineValue(12, {}), '12');
  assert.equal(formatSparklineValue(12.5, {}), '12.5');
  assert.equal(formatSparklineValue(Number.NaN, {}), '—');
});

test('theme tokens override default sparkline colours and line width', () => {
  const tokens = {
    sparkline: {
      line: { $type: 'color', $value: '#123456' },
      fill: { $type: 'color', $value: '#abcdef' },
      grid: { $type: 'color', $value: '#333333' },
      width: { $type: 'dimension', $value: '3px' },
    },
  };
  const geometry = sparklineGeometry({ showValue: true, ticks: 2 }, [1, 2, 3], tokens);
  const markup = renderSparklineSVG({ showValue: true, ticks: 2 }, [1, 2, 3], tokens);
  assert.equal(geometry.colors.line, '#123456');
  assert.equal(geometry.colors.fill, '#abcdef');
  assert.equal(geometry.lineWidth, 3);
  assert.ok(markup.includes('stroke="#123456"'));
  assert.ok(markup.includes('stroke-width="3"'));
});

/* ------------------------------------------------------------------------- */
/* DOM update path                                                            */
/* ------------------------------------------------------------------------- */

test('renderSparkline and updateSparkline reuse the same SVG element', () => {
  const doc = new FakeDocument();
  const container = doc.createElement('div');
  const config = {
    width: 120,
    height: 40,
    min: 0,
    max: 100,
    warn: 50,
    crit: 80,
    showValue: true,
    area: true,
    baseline: 'min',
    label: 'Live',
    unit: '%',
  };

  const first = renderSparkline(container, config, [10, 20, 30], TOKENS, { id: 'live' });
  assert.equal(container.children.length, 1);
  assert.equal(container.firstElementChild, first);
  assert.equal(first.attributes.get('role'), 'img');
  assert.ok(linePaths(first).length > 0);
  const firstPath = linePaths(first)[0].getAttribute('d');

  const second = updateSparkline(container, [10, 60, 95], config, TOKENS, { id: 'live' });
  assert.equal(second, first);
  assert.equal(container.children.length, 1);
  assert.equal(container.firstElementChild, first);
  assert.equal(first.getAttribute('data-severity'), 'crit');
  assert.notEqual(linePaths(first)[0].getAttribute('d'), firstPath);
  assert.ok(byClass(first, 'ozma-sparkline-area'));
  assert.ok(byClass(first, 'ozma-sparkline-baseline'));
  assert.equal(byClass(first, 'ozma-sparkline-value').textContent, '95 %');
  assert.match(byClass(first, 'ozma-sparkline-desc').textContent, /critical/);

  const updatedAgain = updateSparkline(container, [10], config, TOKENS, { id: 'live' });
  assert.equal(updatedAgain, first);
  assert.equal(byClass(first, 'ozma-sparkline-value').textContent, '10 %');
});

test('updateSparkline falls back to a render when no SVG exists', () => {
  const doc = new FakeDocument();
  const container = doc.createElement('div');
  const svg = updateSparkline(container, [1, 2, 3], { label: 'Fresh' }, TOKENS, { id: 'fresh' });
  assert.equal(container.firstElementChild, svg);
  assert.equal(svg.attributes.get('role'), 'img');
});
