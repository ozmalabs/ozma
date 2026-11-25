// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Tests for the dependency-free multi-series chart widget.
 *
 * Run with: node --test web/display/widgets/chart.test.mjs
 */

import test from 'node:test';
import assert from 'node:assert/strict';

import {
  appendSample,
  chartAreaPath,
  chartGeometry,
  chartPath,
  normalizeSeries,
  niceTicks,
  renderChart,
  renderChartSVG,
  safeChartColour,
  scaleValue,
  timeTicks,
  updateChart,
} from './chart.js';

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

function allByClass(root, name, found = []) {
  if (!root) return found;
  if (String(root.getAttribute?.('class') || '').split(/\s+/).includes(name)) found.push(root);
  for (const child of root.children ?? []) allByClass(child, name, found);
  return found;
}

const TOKENS = Object.freeze({
  color: {
    accent: { $type: 'color', $value: '#5b6fff' },
    ok: { $type: 'color', $value: '#42d392' },
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
/* Series model                                                               */
/* ------------------------------------------------------------------------- */

test('normalizeSeries accepts numeric arrays, maps, and typed arrays', () => {
  const single = normalizeSeries([1, 2, null, 4, Number.NaN, '6', 'nope']);
  assert.equal(single.length, 1);
  assert.deepEqual(single[0].values, [1, 2, null, 4, null, 6, null]);
  assert.equal(single[0].label, 'Series 1');
  assert.equal(single[0].definedCount, 4);
  assert.equal(single[0].gapCount, 3);

  const mapped = normalizeSeries({ cpu: [1, 2], mem: [3, 4] });
  assert.deepEqual(mapped.map((item) => item.label), ['cpu', 'mem']);
  assert.deepEqual(mapped.map((item) => item.values), [[1, 2], [3, 4]]);

  const typed = normalizeSeries(new Float32Array([1.5, 2.5]));
  assert.deepEqual(typed[0].values, [1.5, 2.5]);
});

test('normalizeSeries carries labels, colours, styles, and timestamps', () => {
  const series = normalizeSeries([
    {
      label: 'CPU',
      color: '#123456',
      style: 'step',
      values: [{ x: 1000, y: 10 }, { x: 2000, y: 20 }],
    },
    { label: 'Memory', color: 'url(javascript:alert(1))', values: [5, 6], area: true },
  ]);

  assert.equal(series[0].label, 'CPU');
  assert.equal(series[0].color, '#123456');
  assert.equal(series[0].style, 'step');
  assert.deepEqual(series[0].times, [1000, 2000]);
  assert.equal(series[1].color, '#42d392');
  assert.equal(series[1].area, true);
  assert.equal(series[1].style, 'area');
});

test('normalizeSeries bounds the sample window and series count', () => {
  const bounded = normalizeSeries({ values: [1, 2, 3, 4, 5] }, { capacity: 2 });
  assert.deepEqual(bounded[0].values, [4, 5]);
  assert.equal(bounded[0].capacity, 2);

  const many = Array.from({ length: 40 }, (_, index) => ({ values: [index] }));
  const capped = normalizeSeries(many, { maxSeries: 3 });
  assert.equal(capped.length, 3);
});

test('appendSample appends to numeric and object rings without mutating', () => {
  const source = [1, 2, 3];
  const next = appendSample(source, 4, { capacity: 3 });
  assert.deepEqual(source, [1, 2, 3]);
  assert.deepEqual(next, [2, 3, 4]);

  const ring = { capacity: 3, samples: [1, 2, 3], label: 'CPU' };
  const updated = appendSample(ring, { x: 5000, y: 9 });
  assert.deepEqual(ring.samples, [1, 2, 3]);
  assert.deepEqual(updated.samples, [2, 3, 9]);
  assert.deepEqual(updated.times, [null, null, 5000]);
  assert.equal(updated.label, 'CPU');
  assert.equal(updated.capacity, 3);
});

test('appendSample broadcasts scalars and selects keyed or indexed samples', () => {
  const series = [
    { id: 'cpu', label: 'CPU', values: [1] },
    { id: 'mem', label: 'Memory', values: [2] },
  ];
  const broadcast = appendSample(series, 9);
  assert.deepEqual(broadcast.map((item) => item.values), [[1, 9], [2, 9]]);

  const keyed = appendSample(series, { cpu: 10, mem: null });
  assert.deepEqual(keyed.map((item) => item.values), [[1, 10], [2, null]]);

  const indexed = appendSample(series, [11, 12]);
  assert.deepEqual(indexed.map((item) => item.values), [[1, 11], [2, 12]]);
});

/* ------------------------------------------------------------------------- */
/* Scales and ticks                                                           */
/* ------------------------------------------------------------------------- */

test('scaleValue maps scalar and array domains and handles zero-width ranges', () => {
  assert.equal(scaleValue(5, 0, 10, 0, 100), 50);
  assert.equal(scaleValue(0, [0, 10], [20, 40]), 20);
  assert.equal(scaleValue(10, [0, 10], [20, 40]), 40);
  assert.equal(scaleValue(5, 5, 5, 0, 100), 50);
  assert.equal(scaleValue(Number.NaN, 0, 10, 0, 100), 0);
});

test('niceTicks stays inside the domain, increases, and is bounded', () => {
  const ticks = niceTicks(0, 100, 5);
  assert.ok(ticks.length >= 2 && ticks.length <= 12);
  assert.equal(ticks[0], 0);
  assert.equal(ticks[ticks.length - 1], 100);
  assert.ok(ticks.every((value, index) => index === 0 || value > ticks[index - 1]));

  const negative = niceTicks(-50, 50, 4);
  assert.ok(negative.every((value) => value >= -50 && value <= 50));
  assert.deepEqual(niceTicks(2, 2, 5), [2]);

  const huge = niceTicks(0, 1, 1000);
  assert.ok(huge.length <= 12);
});

test('timeTicks produces aligned, bounded, ordered timestamps', () => {
  const ticks = timeTicks(0, 10 * 60_000, 6);
  assert.ok(ticks.length >= 2 && ticks.length <= 12);
  assert.ok(ticks.every((value, index) => index === 0 || value > ticks[index - 1]));
  assert.ok(ticks.every((value) => value >= 0 && value <= 10 * 60_000));
  assert.deepEqual(timeTicks(1000, 1000, 5), [1000]);
});

/* ------------------------------------------------------------------------- */
/* Geometry and paths                                                        */
/* ------------------------------------------------------------------------- */

test('chartGeometry supports auto ranges, explicit ranges, and padded bounds', () => {
  const auto = chartGeometry({ width: 320, height: 180 }, [{ values: [1, 3, 5] }]);
  assert.equal(auto.min, 1);
  assert.equal(auto.max, 5);
  assert.equal(auto.definedCount, 3);
  assert.equal(auto.series[0].last, 5);
  assert.ok(auto.series[0].points.every(
    (point) => point.y >= auto.top && point.y <= auto.bottom,
  ));
  assert.ok(auto.yTicks.length >= 2);
  assert.ok(auto.xTicks.length >= 2);

  const fixed = chartGeometry({ min: 0, max: 100 }, [{ values: [-10, 50, 150] }]);
  assert.equal(fixed.min, 0);
  assert.equal(fixed.max, 100);
  assert.equal(fixed.series[0].points[0].clamped, 0);
  assert.equal(fixed.series[0].points[2].clamped, 100);
});

test('chartGeometry splits gaps into segments and keeps multiple series separate', () => {
  const geometry = chartGeometry({ min: 0, max: 10 }, [
    { label: 'A', values: [1, 2, null, 4] },
    { label: 'B', values: [3, 4, 5, 6] },
  ]);
  assert.equal(geometry.series[0].segments.length, 2);
  assert.equal(geometry.series[0].gapCount, 1);
  assert.equal(geometry.series[1].segments.length, 1);
  assert.equal((chartPath(geometry.series[0]).match(/\bM\b/g) || []).length, 2);
  assert.equal((chartPath(geometry).match(/\bM\b/g) || []).length, 3);
});

test('chartGeometry uses a time window from sample timestamps', () => {
  const geometry = chartGeometry(
    { timeWindow: 3000, sampleMs: 1000 },
    [{ values: [{ x: 1000, y: 1 }, { x: 2000, y: 2 }, { x: 5000, y: 5 }] }],
  );
  assert.equal(geometry.xIsTime, true);
  assert.equal(geometry.xMin, 2000);
  assert.equal(geometry.xMax, 5000);
  assert.ok(geometry.xTicks.every((tick) => tick.value >= 2000 && tick.value <= 5000));
});

test('chartGeometry resolves thresholds and severity bands', () => {
  const geometry = chartGeometry(
    { min: 0, max: 100, warn: 60, crit: 85 },
    [{ values: [10, 70, 95] }],
  );
  const severities = geometry.thresholds.map((item) => item.severity);
  assert.deepEqual(severities, ['warn', 'crit']);
  assert.equal(geometry.bands.length, 2);
  assert.ok(geometry.bands.every((band) => band.height >= 1));
  assert.equal(geometry.bands[0].color, geometry.colors.warn);
  assert.equal(geometry.bands[1].color, geometry.colors.crit);
});

test('chartPath supports step interpolation and area paths honour the baseline', () => {
  const geometry = chartGeometry({ min: 0, max: 10 }, [{ values: [1, 5, 2] }]);
  const line = chartPath(geometry, { step: true });
  assert.match(line, /^M .* L .* L .* L .* L /);
  const area = chartAreaPath(geometry);
  assert.equal((area.match(/\bM\b/g) || []).length, 1);
  assert.equal((area.match(/\bZ\b/g) || []).length, 1);
});

/* ------------------------------------------------------------------------- */
/* SVG rendering and accessibility                                            */
/* ------------------------------------------------------------------------- */

test('renderChartSVG emits accessible axes, legend, markers, and thresholds', () => {
  const markup = renderChartSVG(
    {
      label: 'Network',
      unit: 'Mb/s',
      min: 0,
      max: 100,
      warn: 70,
      crit: 90,
      ticks: 4,
      showValue: true,
      area: true,
    },
    [
      { label: 'RX', color: '#42d392', values: [10, 30, 80] },
      { label: 'TX', color: '#5b6fff', values: [5, 20, 40] },
    ],
    TOKENS,
    { id: 'network' },
  );

  assert.ok(markup.includes('role="img"'));
  assert.ok(markup.includes('id="network-title"'));
  assert.ok(markup.includes('>Network</title>'));
  assert.ok(markup.includes('id="network-desc"'));
  assert.ok(markup.includes('aria-label="Network: RX: 80 Mb/s, TX: 40 Mb/s"'));
  assert.ok(markup.includes('aria-describedby="network-desc"'));
  assert.ok(markup.includes('ozma-chart-axis-label'));
  assert.ok(markup.includes('ozma-chart-legend-label'));
  assert.ok(markup.includes('RX 80 Mb/s'));
  assert.ok(markup.includes('TX 40 Mb/s'));
  assert.ok(markup.includes('ozma-chart-last'));
  assert.ok(markup.includes('ozma-chart-threshold'));
  assert.ok(markup.includes('ozma-chart-band'));
  assert.ok(markup.includes('ozma-chart-area'));
  assert.ok(markup.includes('ozma-chart-tooltip'));
  assert.ok(markup.includes('stroke="#42d392"'));
  assert.ok(markup.includes('stroke="#5b6fff"'));
});

test('renderChartSVG escapes labels, rejects unsafe colours, and handles no data', () => {
  const markup = renderChartSVG(
    { label: 'A < B & C' },
    [{ label: '<script>alert(1)</script>', color: 'url(javascript:alert(1))', values: [1, 2] }],
    TOKENS,
    { id: 'escape' },
  );
  assert.ok(markup.includes('A &lt; B &amp; C'));
  assert.ok(markup.includes('&lt;script&gt;alert(1)&lt;/script&gt;'));
  assert.ok(!markup.includes('<script'));
  assert.ok(!markup.includes('javascript:'));

  const empty = renderChartSVG({ label: 'Empty' }, [], TOKENS, { id: 'empty' });
  assert.ok(empty.includes('no data'));
  assert.ok(empty.includes('aria-label="Empty: no data"'));
});

test('renderChartSVG honours reduced motion', () => {
  const reduced = renderChartSVG({ label: 'Motion', animate: true }, [{ values: [1, 2, 3] }], TOKENS, {
    reducedMotion: true,
  });
  assert.ok(reduced.includes('ozma-chart-reduced-motion'));
  assert.ok(reduced.includes('data-reduced-motion="true"'));
  assert.ok(!reduced.includes('transition: opacity'));

  const animated = renderChartSVG({ label: 'Motion', animate: true }, [{ values: [1, 2, 3] }], TOKENS, {
    reducedMotion: false,
  });
  assert.ok(animated.includes('data-animate="true"'));
  assert.ok(animated.includes('data-reduced-motion="false"'));
  assert.ok(animated.includes('transition: opacity'));
});

test('safeChartColour only accepts bounded colour syntax', () => {
  assert.equal(safeChartColour('#abc'), '#abc');
  assert.equal(safeChartColour('rgba(1, 2, 3, 0.5)'), 'rgba(1, 2, 3, 0.5)');
  assert.equal(safeChartColour('red'), 'red');
  assert.equal(safeChartColour('url(javascript:alert(1))', '#000'), '#000');
  assert.equal(safeChartColour('<script>', '#111'), '#111');
});

test('theme tokens override chart colours', () => {
  const tokens = {
    chart: {
      line: { $type: 'color', $value: '#123456' },
      grid: { $type: 'color', $value: '#333333' },
    },
  };
  const geometry = chartGeometry({ ticks: 2 }, [{ values: [1, 2, 3] }], tokens);
  const markup = renderChartSVG({ ticks: 2 }, [{ values: [1, 2, 3] }], tokens);
  assert.equal(geometry.colors.line, '#123456');
  assert.equal(geometry.colors.grid, '#333333');
  assert.ok(markup.includes('stroke="#123456"'));
});

/* ------------------------------------------------------------------------- */
/* DOM update path                                                            */
/* ------------------------------------------------------------------------- */

test('renderChart and updateChart reuse the same SVG element', () => {
  const doc = new FakeDocument();
  const container = doc.createElement('div');
  const config = {
    width: 240,
    height: 120,
    min: 0,
    max: 100,
    warn: 50,
    crit: 80,
    label: 'Live',
    unit: '%',
  };
  const series = [{ label: 'A', values: [10, 20, 30] }];

  const first = renderChart(container, config, series, TOKENS, { id: 'live' });
  assert.equal(container.children.length, 1);
  assert.equal(container.firstElementChild, first);
  assert.equal(first.getAttribute('role'), 'img');
  const firstPath = byClass(first, 'ozma-chart-line').getAttribute('d');

  const second = updateChart(
    container,
    [{ label: 'A', values: [10, 60, 95] }, { label: 'B', values: [5, 15, 25] }],
    config,
    TOKENS,
    { id: 'live' },
  );
  assert.equal(second, first);
  assert.equal(container.children.length, 1);
  assert.equal(container.firstElementChild, first);
  assert.notEqual(byClass(first, 'ozma-chart-line').getAttribute('d'), firstPath);
  assert.equal(allByClass(first, 'ozma-chart-line').length, 2);
  assert.ok(byClass(first, 'ozma-chart-legend'));
  assert.match(byClass(first, 'ozma-chart-desc').textContent, /2 series/);
});

test('updateChart falls back to a render when no chart exists', () => {
  const doc = new FakeDocument();
  const container = doc.createElement('div');
  const svg = updateChart(container, [{ values: [1, 2, 3] }], { label: 'Fresh' }, TOKENS, { id: 'fresh' });
  assert.equal(container.firstElementChild, svg);
  assert.equal(svg.getAttribute('role'), 'img');
});

test('chart geometry and rendering reject oversize input', () => {
  const width = chartGeometry({ width: 1e9, height: 1e9, ticks: 1000, xTicks: 1000 }, [
    { values: Array.from({ length: 10_000 }, (_, index) => index) },
  ]);
  assert.ok(width.width <= 4096);
  assert.ok(width.height <= 4096);
  assert.ok(width.yTicks.length <= 12);
  assert.ok(width.xTicks.length <= 12);
  assert.ok(width.series[0].values.length <= 4096);

  const many = chartGeometry({}, Array.from({ length: 40 }, (_, index) => ({
    label: `S${index}`,
    values: [index],
  })));
  assert.ok(many.series.length <= 16);
});
