// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Integration helpers shared by the browser adapter's optional theme and gauge
 * modules.  These tests are DOM-free and only exercise the pure selectors.
 *
 * Run with: node --test web/display/integration.test.mjs
 */

import test from 'node:test';
import assert from 'node:assert/strict';

import {
  gaugeModeForType,
  historyCapacityForNode,
  selectThemeSelection,
  sparklineModeForType,
  widgetRenderKind,
} from './client.js';

/* ------------------------------------------------------------------------- */
/* Theme bootstrap selection                                                  */
/* ------------------------------------------------------------------------- */

test('query theme wins over data-theme and the default', () => {
  assert.deepEqual(
    selectThemeSelection({
      search: '?theme=midnight&server=ws%3A%2F%2Flocalhost%3A7391',
      dataTheme: 'light',
      hasThemeModule: true,
    }),
    { origin: 'query', value: 'midnight' },
  );
});

test('data-theme is used when the query has no theme', () => {
  assert.deepEqual(
    selectThemeSelection({
      search: '?server=ws%3A%2F%2Flocalhost%3A7391',
      dataTheme: 'core-light',
      hasThemeModule: true,
    }),
    { origin: 'data-theme', value: 'core-light' },
  );
});

test('default core-dark is selected only when the theme module exists', () => {
  assert.deepEqual(
    selectThemeSelection({ search: '', dataTheme: '', hasThemeModule: true }),
    { origin: 'default', value: 'core-dark' },
  );
  assert.equal(
    selectThemeSelection({ search: '', dataTheme: '', hasThemeModule: false }),
    null,
  );
});

test('empty or whitespace-only selections fall through safely', () => {
  assert.deepEqual(
    selectThemeSelection({
      search: '?theme=',
      dataTheme: '  ',
      hasThemeModule: true,
      defaultTheme: 'core-dark',
    }),
    { origin: 'default', value: 'core-dark' },
  );
  assert.equal(
    selectThemeSelection({ search: '?theme=%20%20', hasThemeModule: false }),
    null,
  );
});

/* ------------------------------------------------------------------------- */
/* Widget renderer selection                                                  */
/* ------------------------------------------------------------------------- */

test('gauge widget types use the gauge module when it is available', () => {
  for (const type of ['gauge', 'bar', 'vu_meter', 'GAUGE']) {
    assert.equal(
      widgetRenderKind({ type }, { hasGaugeModule: true }),
      'gauge',
      type,
    );
  }
  assert.deepEqual(
    ['gauge', 'bar', 'vu_meter', 'sparkline', 'text', 'number'].map((type) => [
      type,
      gaugeModeForType(type),
    ]),
    [
      ['gauge', 'arc'],
      ['bar', 'bar'],
      ['vu_meter', 'bar'],
      ['sparkline', null],
      ['text', null],
      ['number', null],
    ],
  );
});

test('non-gauge widgets and a missing gauge module use the fallback', () => {
  assert.equal(widgetRenderKind({ type: 'sparkline' }, { hasGaugeModule: true }), 'fallback');
  assert.equal(widgetRenderKind({ type: 'gauge' }, { hasGaugeModule: false }), 'fallback');
  assert.equal(widgetRenderKind({ type: 'bar' }, { hasGaugeModule: false }), 'fallback');
  assert.equal(widgetRenderKind(undefined, { hasGaugeModule: true }), 'fallback');
});

test('missing optional modules never throw while selecting a renderer', () => {
  assert.equal(widgetRenderKind({ type: 'vu_meter', label: 'L' }), 'fallback');
  assert.equal(gaugeModeForType(undefined), null);
  assert.equal(selectThemeSelection(), null);
});

test('a missing theme module preserves the existing appearance', () => {
  assert.equal(
    selectThemeSelection({ search: '?theme=midnight', hasThemeModule: false }),
    null,
  );
  assert.equal(
    selectThemeSelection({ dataTheme: 'core-light', hasThemeModule: false }),
    null,
  );
});

/* ------------------------------------------------------------------------- */
/* Sparkline/history selection                                                */
/* ------------------------------------------------------------------------- */

test('sparkline, history, and chart-with-history map to the sparkline widget', () => {
  assert.equal(sparklineModeForType('sparkline'), 'sparkline');
  assert.equal(sparklineModeForType('SPARKLINE'), 'sparkline');
  assert.equal(sparklineModeForType('history'), 'sparkline');
  assert.equal(sparklineModeForType('chart'), null);
  assert.equal(sparklineModeForType('chart', { history: { points: 60 } }), 'sparkline');
  assert.equal(sparklineModeForType('chart', { history: { window: 30 } }), 'sparkline');
  assert.equal(sparklineModeForType('chart', { history: [1, 2, 3] }), 'sparkline');
  assert.equal(sparklineModeForType('chart', { history: {} }), null);
  assert.equal(sparklineModeForType('text'), null);
  assert.equal(sparklineModeForType(undefined), null);
});

test('sparkline renderer wins for mapped types only when the module is available', () => {
  assert.equal(
    widgetRenderKind({ type: 'sparkline' }, { hasSparklineModule: true }),
    'sparkline',
  );
  assert.equal(
    widgetRenderKind({ type: 'history' }, { hasSparklineModule: true }),
    'sparkline',
  );
  assert.equal(
    widgetRenderKind({ type: 'chart', history: { points: 20 } }, { hasSparklineModule: true }),
    'sparkline',
  );
  assert.equal(
    widgetRenderKind({ type: 'chart', history: { points: 20 } }, { hasGaugeModule: true }),
    'fallback',
  );
  assert.equal(
    widgetRenderKind({ type: 'chart' }, { hasSparklineModule: true }),
    'fallback',
  );
  assert.equal(
    widgetRenderKind({ type: 'gauge' }, { hasGaugeModule: true, hasSparklineModule: true }),
    'gauge',
  );
});

test('historyCapacityForNode uses points/window and stays bounded', () => {
  assert.equal(historyCapacityForNode({}), 120);
  assert.equal(historyCapacityForNode({ history: { points: 30 } }), 30);
  assert.equal(historyCapacityForNode({ history: { window: 45 } }), 45);
  assert.equal(historyCapacityForNode({ history: 10 }), 10);
  assert.equal(historyCapacityForNode({ history: { points: 0 } }), 120);
  assert.equal(historyCapacityForNode({ history: { points: -5 } }, 40), 40);
  assert.equal(historyCapacityForNode({ history: { points: 99_999 } }), 4096);
  assert.equal(historyCapacityForNode({ history: { points: '15' } }), 15);
});
