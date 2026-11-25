// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Tests for the dependency-free SVG gauge widget family.
 *
 * Run with: node --test web/display/widgets/gauge.test.mjs
 */

import test from 'node:test';
import assert from 'node:assert/strict';

import {
  arcPath,
  barGeometry,
  clamp,
  dualBarGeometry,
  formatGaugeValue,
  gaugeGeometry,
  normalizeGaugeConfig,
  polarPoint,
  renderGauge,
  renderGaugeSVG,
  resolveGaugeColors,
  severityFor,
  thresholdBands,
  thresholdColor,
  tickMarks,
  valueToAngle,
} from './gauge.js';

const tokens = {
  color: {
    accent: { $type: 'color', $value: '#5b6fff' },
    warn: { $type: 'color', $value: '#f0b94a' },
    crit: { $type: 'color', $value: '#f26464' },
    fg: { $type: 'color', $value: '#e5e7eb' },
    muted: { $type: 'color', $value: '#8d9bb3' },
    track: { $type: 'color', $value: 'rgba(255, 255, 255, 0.12)' },
  },
  'font.family': { $type: 'fontFamily', $value: ['Inter', 'sans-serif'] },
  gauge: {
    thickness: { $type: 'dimension', $value: '14px' },
  },
};

const close = (actual, expected, epsilon = 1e-9) => {
  assert.ok(Math.abs(actual - expected) <= epsilon, `${actual} is not within ${epsilon} of ${expected}`);
};

/* ------------------------------------------------------------------------- */
/* Geometry                                                                   */
/* ------------------------------------------------------------------------- */

test('polarPoint uses 0 degrees at twelve o clock and increases clockwise', () => {
  const top = polarPoint(100, 100, 50, 0);
  close(top.x, 100);
  close(top.y, 50);

  const right = polarPoint(100, 100, 50, 90);
  close(right.x, 150);
  close(right.y, 100);

  const bottom = polarPoint(100, 100, 50, 180);
  close(bottom.x, 100);
  close(bottom.y, 150);
});

test('arcPath emits start/end points, sweep and large-arc flags', () => {
  assert.equal(
    arcPath({ cx: 100, cy: 100, radius: 50, startAngle: 0, endAngle: 90 }),
    'M 100 50 A 50 50 0 0 1 150 100',
  );
  assert.equal(
    arcPath({ cx: 100, cy: 100, radius: 50, startAngle: 0, endAngle: 270 }),
    'M 100 50 A 50 50 0 1 1 50 100',
  );
  assert.equal(
    arcPath({ cx: 100, cy: 100, radius: 50, startAngle: 90, endAngle: 0 }),
    'M 150 100 A 50 50 0 0 0 100 50',
  );
});

test('arcPath splits a full circle into two arc commands', () => {
  const path = arcPath({ cx: 100, cy: 100, radius: 50, startAngle: 0, endAngle: 360 });
  assert.equal((path.match(/ A /g) || []).length, 2);
  assert.equal(path.startsWith('M 100 50 A'), true);
  assert.equal(path.endsWith('100 50'), true);
});

test('valueToAngle clamps outside the gauge range', () => {
  assert.equal(valueToAngle(50, 0, 100, -120, 120), 0);
  assert.equal(valueToAngle(-10, 0, 100, -120, 120), -120);
  assert.equal(valueToAngle(150, 0, 100, -120, 120), 120);
  assert.equal(valueToAngle(0, 50, 50, -120, 120), -120);
});

test('clamp and numeric formatting behave predictably', () => {
  assert.equal(clamp(150, 0, 100), 100);
  assert.equal(clamp(-1, 0, 100), 0);
  assert.equal(clamp(Number.NaN, 0, 100), 0);
  assert.equal(formatGaugeValue(42, {}), '42');
  assert.equal(formatGaugeValue(42.5, {}), '42.5');
  assert.equal(formatGaugeValue(42.5, { decimals: 2 }), '42.50');
  assert.equal(formatGaugeValue(Number.NaN, {}), '—');
});

/* ------------------------------------------------------------------------- */
/* Thresholds and value clamping                                              */
/* ------------------------------------------------------------------------- */

test('severityFor and thresholdColor follow warn/crit thresholds', () => {
  const config = { min: 0, max: 100, warn: 70, crit: 90 };
  assert.equal(severityFor(50, config), 'normal');
  assert.equal(severityFor(70, config), 'warn');
  assert.equal(severityFor(95, config), 'crit');

  assert.equal(thresholdColor(50, config, tokens), '#5b6fff');
  assert.equal(thresholdColor(75, config, tokens), '#f0b94a');
  assert.equal(thresholdColor(95, config, tokens), '#f26464');
});

test('thresholdBands covers the range with severity colours', () => {
  const bands = thresholdBands({ min: 0, max: 100, warn: 70, crit: 90 }, tokens);
  assert.equal(bands.length, 3);
  assert.deepEqual(
    bands.map((band) => [band.from, band.to, band.severity]),
    [[0, 70, 'normal'], [70, 90, 'warn'], [90, 100, 'crit']],
  );
  assert.deepEqual(bands.map((band) => band.color), ['#5b6fff', '#f0b94a', '#f26464']);
  assert.equal(bands[0].startAngle, -120);
  assert.equal(bands.at(-1).endAngle, 120);
});

test('gaugeGeometry clamps values and reports severity', () => {
  const geometry = gaugeGeometry(
    { min: 0, max: 100, startAngle: -120, endAngle: 120, warn: 70, crit: 90 },
    150,
    tokens,
  );
  assert.equal(geometry.value, 100);
  assert.equal(geometry.rawValue, 150);
  assert.equal(geometry.percent, 100);
  assert.equal(geometry.valueAngle, 120);
  assert.equal(geometry.severity, 'crit');
  assert.equal(geometry.colors.crit, '#f26464');
  assert.equal(geometry.thickness, 14);
});

test('gauge tokens resolve from flat and nested theme shapes', () => {
  const colors = resolveGaugeColors({}, {
    schema: 'org.ozma.theme/1',
    tokens: {
      color: {
        accent: { $type: 'color', $value: '#abcdef' },
      },
      gauge: {
        warn: { $type: 'color', $value: '{color.accent}' },
      },
    },
  });
  assert.equal(colors.normal, '#abcdef');
  assert.equal(colors.warn, '#abcdef');
});

/* ------------------------------------------------------------------------- */
/* Bar and tick geometry                                                      */
/* ------------------------------------------------------------------------- */

test('barGeometry computes track, fill, and clamped percentage', () => {
  const geometry = barGeometry({ width: 240, height: 64, padding: 8, min: 0, max: 100 }, 50);
  assert.equal(geometry.viewWidth, 240);
  assert.equal(geometry.viewHeight, 64);
  assert.equal(geometry.x, 8);
  close(geometry.trackHeight, 17.92);
  close(geometry.width, 224);
  close(geometry.y, 38.08);
  close(geometry.fillWidth, 112);
  assert.equal(geometry.percent, 50);

  const clamped = barGeometry({ width: 240, height: 64, min: 0, max: 100 }, 150);
  assert.equal(clamped.value, 100);
  assert.equal(clamped.percent, 100);
  close(clamped.fillWidth, 224);
});

test('dualBarGeometry produces two rows and clamps both values', () => {
  const geometry = dualBarGeometry(
    { width: 240, height: 96, padding: 8, min: 0, max: 1000, labels: ['RX', 'TX'] },
    [250, 5000],
    tokens,
  );
  assert.equal(geometry.rows.length, 2);
  assert.equal(geometry.rows[0].value, 250);
  assert.equal(geometry.rows[1].value, 1000);
  assert.equal(geometry.rows[0].label, 'RX');
  close(geometry.rows[0].percent, 25);
  close(geometry.rows[1].percent, 100);
});

test('tickMarks include endpoints and count', () => {
  const ticks = tickMarks({ width: 200, height: 200, tickCount: 5, startAngle: -120, endAngle: 120 }, tokens);
  assert.equal(ticks.length, 5);
  assert.equal(ticks[0].index, 0);
  close(ticks[0].angle, -120);
  close(ticks[4].angle, 120);
  assert.equal(ticks[0].major, true);
  assert.equal(ticks[4].major, true);
  assert.equal(ticks[2].major, false);
});

test('normalizeGaugeConfig ignores non-positive sizes and keeps modes', () => {
  const config = normalizeGaugeConfig({ width: -1, height: 0, style: 'dual_bar' });
  assert.equal(config.mode, 'dual-bar');
  assert.equal(config.width, 240);
  assert.equal(config.height, 96);
});

/* ------------------------------------------------------------------------- */
/* SVG output and accessibility                                               */
/* ------------------------------------------------------------------------- */

test('renderGaugeSVG creates an accessible arc gauge with thresholds', () => {
  const svg = renderGaugeSVG(
    { label: 'CPU', unit: '°C', min: 0, max: 100, warn: 70, crit: 90, decimals: 0 },
    95,
    tokens,
    { reducedMotion: true },
  );

  assert.match(svg, /<svg[^>]*role="img"/);
  assert.match(svg, /aria-label="CPU 95 °C, critical"/);
  assert.match(svg, /<title[^>]*>CPU<\/title>/);
  assert.match(svg, /<desc[^>]*>/);
  assert.match(svg, /data-reduced-motion="true"/);
  assert.match(svg, /stroke="#f26464"/);
  assert.match(svg, /data-severity="crit"/);
  assert.doesNotMatch(svg, /transition:/);
});

test('renderGaugeSVG clamps the displayed value', () => {
  const svg = renderGaugeSVG(
    { label: 'CPU', unit: '°C', min: 0, max: 100, warn: 70, crit: 90 },
    150,
    tokens,
    { reducedMotion: true },
  );
  assert.match(svg, /aria-label="CPU 100 °C, critical"/);
  assert.match(svg, />100 °C<\/text>/);
  assert.doesNotMatch(svg, />150 °C</);
});

test('reduced motion removes transitions even when animation is requested', () => {
  const moving = renderGaugeSVG(
    { label: 'CPU', min: 0, max: 100, animate: true },
    50,
    tokens,
    { reducedMotion: false },
  );
  assert.match(moving, /data-reduced-motion="false"/);
  assert.match(moving, /data-animate="true"/);
  assert.match(moving, /transition: stroke/);

  const still = renderGaugeSVG(
    { label: 'CPU', min: 0, max: 100, animate: true, reducedMotion: true },
    50,
    tokens,
  );
  assert.match(still, /data-reduced-motion="true"/);
  assert.match(still, /ozma-gauge-reduced-motion/);
  assert.doesNotMatch(still, /transition:/);
});

test('renderGaugeSVG supports bar and dual-bar modes', () => {
  const bar = renderGaugeSVG(
    { style: 'bar', label: 'Memory', unit: '%', min: 0, max: 100 },
    50,
    tokens,
    { reducedMotion: true },
  );
  assert.match(bar, /ozma-gauge-bar/);
  assert.match(bar, /aria-label="Memory 50 %"/);
  assert.match(bar, /viewBox="0 0 240 64"/);
  assert.match(bar, /role="img"/);

  const dual = renderGaugeSVG(
    {
      style: 'dual-bar',
      label: 'Network',
      labels: ['RX', 'TX'],
      units: ['Mb/s', 'Mb/s'],
      min: 0,
      max: 100,
    },
    [25, 75],
    tokens,
    { reducedMotion: true },
  );
  assert.match(dual, /ozma-gauge-dual-bar/);
  assert.match(dual, /aria-label="Network: RX 25 Mb\/s, TX 75 Mb\/s"/);
  assert.match(dual, /<title[^>]*>Network<\/title>/);
  assert.equal((dual.match(/ozma-gauge-value/g) || []).length, 2);
});

test('renderGauge writes markup into a container and returns an element when present', () => {
  const container = { innerHTML: '', firstElementChild: null };
  const result = renderGauge(
    container,
    { label: 'CPU', min: 0, max: 100 },
    42,
    tokens,
    { reducedMotion: true },
  );
  assert.equal(typeof result, 'string');
  assert.match(container.innerHTML, /role="img"/);
  assert.match(container.innerHTML, /aria-label="CPU 42"/);

  const element = { tagName: 'svg' };
  const elementContainer = { innerHTML: '', firstElementChild: element };
  assert.equal(
    renderGauge(elementContainer, { label: 'CPU' }, 42, tokens),
    element,
  );
});

test('renderGauge rejects a missing container', () => {
  assert.throws(() => renderGauge(null, {}, 0), TypeError);
});
