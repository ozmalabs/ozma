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
  allowlistedSandboxSvg,
  chartConfigForNode,
  chartRenderKind,
  chartSeriesForNode,
  gaugeModeForType,
  historyCapacityForNode,
  sandboxCapabilitiesForNode,
  sandboxMountPayload,
  sandboxRenderPlan,
  sandboxSeverity,
  selectThemeSelection,
  shouldSandboxWidget,
  sparklineModeForType,
  vuConfigForNode,
  vuDeltaMs,
  vuRenderKind,
  vuSampleForNode,
  vuStepForTimestamp,
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

/* ------------------------------------------------------------------------- */
/* VU meter selection and configuration                                       */
/* ------------------------------------------------------------------------- */

test('vu_meter selects the VU renderer when the module is available', () => {
  assert.equal(vuRenderKind({ type: 'vu_meter' }, { hasVuModule: true }), 'vu');
  assert.equal(vuRenderKind({ type: 'VUE_METER' }, { hasVuModule: true }), 'vu');
  assert.equal(vuRenderKind({ type: 'vu_meter' }), 'vu');
  assert.equal(vuRenderKind({ type: 'gauge' }, { hasVuModule: true }), null);
  assert.equal(vuRenderKind({ type: 'vu_meter' }, { hasVuModule: false }), null);

  assert.equal(
    widgetRenderKind(
      { type: 'vu_meter' },
      { hasVuModule: true, hasGaugeModule: true },
    ),
    'vu',
  );
  assert.equal(
    widgetRenderKind({ type: 'vu_meter' }, { hasVuModule: true }),
    'vu',
  );
});

test('a missing VU module falls back to the gauge bar renderer', () => {
  assert.equal(
    widgetRenderKind(
      { type: 'vu_meter' },
      { hasVuModule: false, hasGaugeModule: true },
    ),
    'gauge',
  );
  assert.equal(
    widgetRenderKind(
      { type: 'vue_meter' },
      { hasVuModule: false, hasGaugeModule: true },
    ),
    'gauge',
  );
  assert.equal(gaugeModeForType('vu_meter'), 'bar');
  assert.equal(gaugeModeForType('vue_meter'), 'bar');
  assert.equal(widgetRenderKind({ type: 'vu_meter' }), 'fallback');
});

test('VU selection does not change gauge or bar routing', () => {
  const options = { hasVuModule: true, hasGaugeModule: true, hasSparklineModule: true };
  assert.equal(widgetRenderKind({ type: 'gauge' }, options), 'gauge');
  assert.equal(widgetRenderKind({ type: 'bar' }, options), 'gauge');
  assert.equal(widgetRenderKind({ type: 'sparkline' }, options), 'sparkline');
  assert.equal(gaugeModeForType('gauge'), 'arc');
  assert.equal(gaugeModeForType('bar'), 'bar');
});

test('vuConfigForNode serialises definition fields and ballistics', () => {
  const config = vuConfigForNode(
    {
      type: 'vu_meter',
      min: -60,
      max: 0,
      warn: -12,
      crit: -3,
      attack_ms: 5,
      release_ms: 420,
      falloff_db_per_sec: 30,
      peak_hold_ms: 900,
      rms_window_ms: 250,
      clip_db: -0.5,
      segments: 18,
      orientation: 'vertical',
      unit: 'dB',
      decimals: 2,
    },
    { unit: 'dB' },
    'Master',
  );

  assert.equal(config.channels, 'mono');
  assert.equal(config.min, -60);
  assert.equal(config.max, 0);
  assert.equal(config.warn, -12);
  assert.equal(config.crit, -3);
  assert.equal(config.attackMs, 5);
  assert.equal(config.releaseMs, 420);
  assert.equal(config.falloffDbPerSec, 30);
  assert.equal(config.peakHoldMs, 900);
  assert.equal(config.rmsWindowMs, 250);
  assert.equal(config.clipThresholdDb, -0.5);
  assert.equal(config.segments, 18);
  assert.equal(config.orientation, 'vertical');
  assert.equal(config.unit, 'dB');
  assert.equal(config.decimals, 2);
  assert.equal(config.label, 'Master');
});

test('vuConfigForNode detects stereo bindings and linear sources', () => {
  assert.equal(
    vuConfigForNode({ type: 'vu_meter', right_field: 'LEVEL_R' }).channels,
    'stereo',
  );
  assert.equal(
    vuConfigForNode({ type: 'vu_meter', right_metric: 'LEVEL_R' }).channels,
    'stereo',
  );
  assert.equal(vuConfigForNode({ type: 'vu_meter', channels: 2 }).channels, 'stereo');
  assert.equal(
    vuConfigForNode({ type: 'vu_meter', levels: [0.25, 0.5] }).channels,
    'stereo',
  );
  assert.equal(
    vuConfigForNode({ type: 'vu_meter', min: 0, max: 1, unit: '' }).input,
    'linear',
  );
  assert.equal(
    vuConfigForNode({ type: 'vu_meter', min: -60, max: 0, unit: 'dB' }).input,
    undefined,
  );
});

test('vuSampleForNode supports single values, pairs, levels, and objects', () => {
  assert.equal(vuSampleForNode({ type: 'vu_meter' }, -12), -12);
  assert.deepEqual(
    vuSampleForNode({ type: 'vu_meter', channels: 2 }, -12, -24),
    { l: -12, r: -24 },
  );
  assert.deepEqual(
    vuSampleForNode({ type: 'vu_meter' }, [-12, -24]),
    { l: -12, r: -24 },
  );
  assert.deepEqual(
    vuSampleForNode({ type: 'vu_meter', levels: [-3, -4] }, undefined),
    { l: -3, r: -4 },
  );
  assert.deepEqual(
    vuSampleForNode({ type: 'vu_meter' }, { l: -5, r: -6 }),
    { l: -5, r: -6 },
  );
});

test('VU clock deltas are monotonic and the timestamp helper is injectable', () => {
  assert.equal(vuDeltaMs(150, 100), 50);
  assert.equal(vuDeltaMs(90, 100), 0);
  assert.equal(vuDeltaMs(Number.NaN, 100), 0);
  assert.equal(vuDeltaMs(150, Number.NaN), 0);

  const record = { lastMs: 100, state: {} };
  let seen = null;
  const delta = vuStepForTimestamp(
    record,
    'sample',
    { attackMs: 10 },
    175,
    (state, sample, config, dt) => {
      seen = { state, sample, config, dt };
    },
  );
  assert.equal(delta, 75);
  assert.equal(record.lastMs, 175);
  assert.deepEqual(seen, {
    state: record.state,
    sample: 'sample',
    config: { attackMs: 10 },
    dt: 75,
  });
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

/* ------------------------------------------------------------------------- */
/* Multi-series chart selection and configuration                             */
/* ------------------------------------------------------------------------- */

test('chart widgets use the chart module when a history or series config exists', () => {
  for (const node of [
    { type: 'chart', history: { window: 60 } },
    { type: 'chart', history: { points: 60 } },
    { type: 'chart', series: ['cpu', 'mem'] },
    { type: 'chart', metrics: ['cpu', 'mem'] },
    { type: 'chart', history: { series: ['cpu'] } },
  ]) {
    assert.equal(chartRenderKind(node, { hasChartModule: true }), 'chart', JSON.stringify(node));
  }

  assert.equal(chartRenderKind({ type: 'chart' }, { hasChartModule: true }), null);
  assert.equal(chartRenderKind({ type: 'chart', history: {} }, { hasChartModule: true }), null);
  assert.equal(chartRenderKind({ type: 'chart', history: { points: 20 } }, { hasChartModule: false }), null);
  assert.equal(chartRenderKind({ type: 'text' }, { hasChartModule: true }), null);
  assert.equal(chartRenderKind(undefined, { hasChartModule: true }), null);
});

test('simple single-series widgets stay on the sparkline renderer', () => {
  assert.equal(
    chartRenderKind({ type: 'sparkline', history: { points: 20 } }, { hasChartModule: true }),
    null,
  );
  assert.equal(
    chartRenderKind({ type: 'history', history: { points: 20 } }, { hasChartModule: true }),
    null,
  );
  assert.equal(
    chartRenderKind(
      { type: 'sparkline', series: [{ metric: 'cpu' }, { metric: 'mem' }] },
      { hasChartModule: true },
    ),
    'chart',
  );
});

test('chart renderer wins over sparkline only when its module is available', () => {
  const options = { hasChartModule: true, hasSparklineModule: true };
  assert.equal(
    widgetRenderKind({ type: 'chart', history: { points: 20 } }, options),
    'chart',
  );
  assert.equal(
    widgetRenderKind({ type: 'sparkline', history: { points: 20 } }, options),
    'sparkline',
  );
  assert.equal(
    widgetRenderKind({ type: 'chart', history: { points: 20 } }, { hasSparklineModule: true }),
    'sparkline',
  );
  assert.equal(
    widgetRenderKind({ type: 'chart', history: { points: 20 } }, { hasChartModule: true }),
    'chart',
  );
  assert.equal(widgetRenderKind({ type: 'chart' }, options), 'fallback');
  assert.equal(
    widgetRenderKind({ type: 'gauge' }, { ...options, hasGaugeModule: true }),
    'gauge',
  );
  assert.equal(
    widgetRenderKind({ type: 'vu_meter' }, { ...options, hasVuModule: true }),
    'vu',
  );
});

test('chartConfigForNode serialises ranges, thresholds, style, and history capacity', () => {
  const config = chartConfigForNode(
    {
      type: 'chart',
      width: 300,
      height: 150,
      min: 0,
      max: 100,
      warn: 70,
      crit: 90,
      unit: '%',
      decimals: 1,
      style: 'step',
      area: true,
      baseline: 'zero',
      showLegend: false,
      showGrid: true,
      ticks: 6,
      xTicks: 4,
      timeWindow: 60_000,
      history: { points: 200 },
      thresholds: [50],
      animate: true,
    },
    { unit: 'C' },
    'CPU',
  );

  assert.equal(config.width, 300);
  assert.equal(config.height, 150);
  assert.equal(config.min, 0);
  assert.equal(config.max, 100);
  assert.equal(config.warn, 70);
  assert.equal(config.crit, 90);
  assert.equal(config.unit, '%');
  assert.equal(config.decimals, 1);
  assert.equal(config.label, 'CPU');
  assert.equal(config.style, 'step');
  assert.equal(config.area, true);
  assert.equal(config.baseline, 'zero');
  assert.equal(config.showLegend, false);
  assert.equal(config.showGrid, true);
  assert.equal(config.ticks, 6);
  assert.equal(config.xTicks, 4);
  assert.equal(config.timeWindow, 60_000);
  assert.equal(config.capacity, 200);
  assert.deepEqual(config.thresholds, [50]);
  assert.equal(config.animate, true);
});

test('chartSeriesForNode maps metrics with optional labels and colours', () => {
  const metrics = chartSeriesForNode({
    type: 'chart',
    metrics: ['cpu', 'mem'],
    series: [
      { label: 'CPU', color: '#111111' },
      { label: 'Memory', color: '#222222' },
    ],
  });
  assert.deepEqual(
    metrics.map((item) => [item.metric, item.label, item.color]),
    [
      ['cpu', 'CPU', '#111111'],
      ['mem', 'Memory', '#222222'],
    ],
  );

  const explicit = chartSeriesForNode({
    type: 'chart',
    series: [
      { metric: 'cpu', label: 'CPU', color: '#123456', style: 'area' },
      { field: 'mem', label: 'Memory', unit: 'MB' },
    ],
  });
  assert.deepEqual(
    explicit.map((item) => [item.metric, item.label, item.unit]),
    [['cpu', 'CPU', undefined], ['mem', 'Memory', 'MB']],
  );

  const fromHistory = chartSeriesForNode({
    type: 'chart',
    history: { series: ['rpm', { metric: 'load', label: 'Load' }] },
  });
  assert.deepEqual(fromHistory.map((item) => item.metric), ['rpm', 'load']);
});

test('chartSeriesForNode falls back to a single field or metric', () => {
  const fromField = chartSeriesForNode({ type: 'chart' }, { id: 3, key: 'load', unit: '%' }, 'Load');
  assert.deepEqual(fromField, [
    { metric: 'load', fieldId: 3, label: 'Load', unit: '%' },
  ]);

  const fromMetric = chartSeriesForNode({ type: 'chart', metric: 'cpu', label: 'CPU' }, null, '');
  assert.deepEqual(fromMetric, [{ metric: 'cpu', label: 'CPU' }]);

  const fromHistory = chartSeriesForNode({ type: 'chart', history: [1, 2, 3] }, null, 'Static');
  assert.deepEqual(fromHistory, [{ label: 'Static', values: [1, 2, 3] }]);

  const bounded = chartSeriesForNode({
    type: 'chart',
    metrics: Array.from({ length: 40 }, (_, index) => `m${index}`),
  });
  assert.equal(bounded.length, 16);
});

/* ------------------------------------------------------------------------- */
/* Sandbox routing                                                            */
/* ------------------------------------------------------------------------- */

test('shouldSandboxWidget detects executable and explicit sandbox markers', () => {
  const sandboxed = [
    { executable: true },
    { sandbox: 'iframe' },
    { sandbox: 'IFRAME' },
    { script: 'alert(1)' },
    { scripts: ['alert(1)'] },
    { code: 'return 1' },
    { js: 'alert(1)' },
    { javascript: 'alert(1)' },
    { html: '<script>alert(1)</script>' },
    { html: '<div onclick="alert(1)">x</div>' },
    { html: '<iframe src="https://evil.test"></iframe>' },
    { renderer: 'third_party' },
    { renderer: 'third-party' },
    { renderer: 'thirdparty' },
    { renderer: { type: 'third_party' } },
    { type: 'iframe_widget' },
  ];
  for (const node of sandboxed) {
    assert.equal(shouldSandboxWidget(node), true, JSON.stringify(node));
  }
});

test('shouldSandboxWidget leaves built-in and declarative widgets alone', () => {
  const normal = [
    null,
    undefined,
    'gauge',
    [],
    {},
    { type: 'gauge' },
    { type: 'sparkline', history: { points: 30 } },
    { executable: false },
    { sandbox: 'none' },
    { renderer: 'builtin' },
    { renderer: 'core' },
    { html: '<b>safe declarative markup</b>' },
    { html: '<div class="notice">No executable content here</div>' },
    { script: '' },
    { script: false },
    { code: null },
    { javascript: '   ' },
  ];
  for (const node of normal) {
    assert.equal(shouldSandboxWidget(node), false, JSON.stringify(node));
  }
});

test('sandbox render plan fails closed without the sandbox module', () => {
  assert.deepEqual(sandboxRenderPlan({ type: 'gauge' }), { kind: 'builtin' });

  const available = sandboxRenderPlan(
    { executable: true },
    { hasSandboxModule: true },
  );
  assert.deepEqual(available, { kind: 'sandbox' });

  const missing = sandboxRenderPlan(
    { script: 'alert(1)' },
    { hasSandboxModule: false },
  );
  assert.equal(missing.kind, 'error');
  assert.equal(missing.code, 'SANDBOX_MODULE_UNAVAILABLE');
  assert.equal(missing.message, 'Sandboxed widget unavailable');
});

test('sandboxed widgets never fall through to gauge, sparkline, or card paths', () => {
  const options = { hasGaugeModule: true, hasSparklineModule: true };

  assert.equal(widgetRenderKind({ type: 'gauge', executable: true }, options), 'sandbox');
  assert.equal(widgetRenderKind({ type: 'sparkline', script: 'x' }, options), 'sandbox');
  assert.equal(widgetRenderKind({ type: 'bar', sandbox: 'iframe' }, options), 'sandbox');
  assert.equal(
    widgetRenderKind({ type: 'unknown', renderer: 'third_party' }, options),
    'sandbox',
  );

  assert.equal(widgetRenderKind({ type: 'gauge' }, options), 'gauge');
  assert.equal(widgetRenderKind({ type: 'sparkline' }, options), 'sparkline');
  assert.equal(widgetRenderKind({ type: 'text' }, options), 'fallback');
  assert.equal(widgetRenderKind({ type: 'gauge' }), 'fallback');
  assert.equal(widgetRenderKind({ type: 'sparkline' }), 'fallback');
});

test('sandbox capabilities request actions only when the node asks', () => {
  assert.deepEqual(sandboxCapabilitiesForNode({}), ['render']);
  assert.deepEqual(sandboxCapabilitiesForNode({ actions: false }), ['render']);
  assert.deepEqual(sandboxCapabilitiesForNode({ actions: true }), ['render', 'actions']);
  assert.deepEqual(sandboxCapabilitiesForNode({ actions: ['mute'] }), ['render', 'actions']);
  assert.deepEqual(sandboxCapabilitiesForNode({ actions: { mute: true } }), ['render', 'actions']);
  assert.deepEqual(sandboxCapabilitiesForNode({ actions: 'none' }), ['render']);
});

test('sandbox mount payload copies only declarative allowlisted fields', () => {
  const node = {
    type: 'third_party_widget',
    label: 'CPU temperature',
    unit: '°C',
    warn: 70,
    crit: 90,
    script: 'alert(1)',
    scripts: ['alert(2)'],
    code: 'return alert(3)',
    js: 'alert(4)',
    javascript: 'alert(5)',
    html: '<script>alert(6)</script>',
    url: 'https://evil.test/widget',
    src: 'https://evil.test/widget.js',
    image: 'https://evil.test/pixel.png',
    executable: true,
    actions: true,
    svg: '<svg xmlns="http://www.w3.org/2000/svg"><script>alert(7)</script></svg>',
  };
  const field = { id: 3, key: 'cpu', unit: 'C' };
  const payload = sandboxMountPayload(node, field, 95);

  assert.deepEqual(payload, {
    kind: 'third_party_widget',
    label: 'CPU temperature',
    value: '95',
    unit: '°C',
    severity: 'crit',
  });
  assert.deepEqual(
    Object.keys(payload).sort(),
    ['kind', 'label', 'severity', 'unit', 'value'],
  );
  for (const forbidden of [
    'script', 'scripts', 'code', 'js', 'javascript', 'html',
    'url', 'src', 'image', 'executable', 'sandbox', 'actions', 'svg',
  ]) {
    assert.equal(Object.prototype.hasOwnProperty.call(payload, forbidden), false, forbidden);
  }
  const serialised = JSON.stringify(payload);
  assert.doesNotMatch(serialised, /alert|evil|https?:|<\/?script/i);
});

test('sandbox mount payload falls back to field metadata and formats values', () => {
  const payload = sandboxMountPayload(
    { type: 'gauge', decimals: 1 },
    { id: 4, key: 'load', unit: '%' },
    42.56,
  );
  assert.equal(payload.kind, 'gauge');
  assert.equal(payload.label, 'load');
  assert.equal(payload.value, '42.6');
  assert.equal(payload.unit, '%');
  assert.equal(payload.severity, 'normal');

  const empty = sandboxMountPayload({ type: 'gauge' }, null, undefined);
  assert.equal(empty.value, '—');
  assert.equal(empty.severity, 'normal');
  assert.equal(empty.label, '');

  const callable = sandboxMountPayload(
    { type: () => 'alert(1)', value: () => 'alert(2)' },
    null,
    () => 'alert(3)',
  );
  assert.equal(callable.kind, 'widget');
  assert.equal(callable.value, '—');
  assert.doesNotMatch(JSON.stringify(callable), /alert/);
});

test('sandbox severity respects warn and crit thresholds', () => {
  assert.equal(sandboxSeverity({ warn: 70, crit: 90 }, 50), 'normal');
  assert.equal(sandboxSeverity({ warn: 70, crit: 90 }, 70), 'warn');
  assert.equal(sandboxSeverity({ warn: 70, crit: 90 }, 90), 'crit');
  assert.equal(sandboxSeverity({ warn: '70', crit: '90' }, '95'), 'crit');
  assert.equal(sandboxSeverity({ warn: 70, crit: 90 }, 'hot'), 'normal');
  assert.equal(sandboxSeverity({ warn: 0, crit: 1 }, true), 'normal');
});

test('sandbox SVG allowlist rejects scriptable and external content', () => {
  const safe = '<svg viewBox="0 0 10 10"><rect x="0" y="0" width="10" height="10" fill="#fff"/></svg>';
  assert.equal(allowlistedSandboxSvg(safe), safe);

  const unsafe = [
    '<svg><script>alert(1)</script></svg>',
    '<svg><foreignObject><body>x</body></foreignObject></svg>',
    '<svg><image href="https://evil.test/x.png"/></svg>',
    '<svg><rect onclick="alert(1)"/></svg>',
    '<svg><use xlink:href="https://evil.test/x"/></svg>',
    '<svg><rect fill="url(https://evil.test/x)"/></svg>',
    'javascript:alert(1)',
    '<div>not an svg</div>',
    '',
  ];
  for (const value of unsafe) {
    assert.equal(allowlistedSandboxSvg(value), null, value);
  }
  assert.equal(allowlistedSandboxSvg(42), null);
  assert.equal(allowlistedSandboxSvg(null), null);

  const payload = sandboxMountPayload({ type: 'gauge', svg: safe }, null, 1);
  assert.equal(payload.svg, safe);
});
