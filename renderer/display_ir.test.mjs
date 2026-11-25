// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Pure Node tests for the semantic display IR render adapter.
 *
 * Run with: node --test renderer/*.test.mjs
 */

import test from 'node:test';
import assert from 'node:assert/strict';

import displayIr from './display_ir.js';

const {
  DisplayIrError,
  MAX_DIMENSION,
  MAX_WIDGETS,
  applyData,
  irToRenderPlan,
  planToLvgl,
  planToSvg,
} = displayIr;

function layout(widgets, overrides = {}) {
  return {
    id: 'test-layout',
    name: 'Test',
    width: 320,
    height: 240,
    background: '#0a0a0f',
    widgets,
    ...overrides,
  };
}

function flatten(ops) {
  const output = [];
  for (const op of ops) {
    if (op.op === 'group') output.push(...flatten(op.children));
    else output.push(op);
  }
  return output;
}

function allOps(plan) {
  return flatten(plan.ops);
}

function gauge(overrides = {}) {
  return {
    type: 'gauge',
    x: 10,
    y: 10,
    w: 180,
    h: 180,
    metric: 'cpu',
    min: 0,
    max: 100,
    label: 'CPU',
    unit: '%',
    decimals: 0,
    warn: 70,
    crit: 90,
    ...overrides,
  };
}

/* ------------------------------------------------------------------------- */
/* Gauge geometry and thresholds                                              */
/* ------------------------------------------------------------------------- */

test('gauge emits track, value arc, ticks, value text and label', () => {
  const plan = irToRenderPlan(layout([gauge()]), { data: { cpu: 50 } });
  const ops = allOps(plan);

  const arcs = ops.filter((op) => op.op === 'arc');
  assert.equal(arcs.length, 2);
  const valueArc = arcs.find((op) => op.role === 'gauge-value');
  assert.ok(valueArc);
  assert.equal(valueArc.startAngle, 135);
  assert.equal(valueArc.endAngle, 270);
  assert.equal(valueArc.stroke, '#5b6fff');

  assert.equal(ops.filter((op) => op.op === 'line').length, 5);
  const valueText = ops.find((op) => op.id.endsWith('.value-text'));
  assert.equal(valueText.text, '50%');
  assert.equal(ops.find((op) => op.id.endsWith('.label')).text, 'CPU');
});

test('gauge threshold colour follows warn/crit and value is clamped', () => {
  const plan = irToRenderPlan(layout([gauge()]), { data: { cpu: 75 } });
  let valueArc = allOps(plan).find((op) => op.role === 'gauge-value');
  assert.equal(valueArc.stroke, '#f0b94a');

  const high = irToRenderPlan(layout([gauge()]), { data: { cpu: 150 } });
  valueArc = allOps(high).find((op) => op.role === 'gauge-value');
  assert.equal(valueArc.endAngle, 405);
  assert.equal(allOps(high).find((op) => op.role === 'value-text').text, '100%');

  const low = irToRenderPlan(layout([gauge()]), { data: { cpu: -50 } });
  valueArc = allOps(low).find((op) => op.role === 'gauge-value');
  assert.equal(valueArc.endAngle, 135);
});

test('domain thresholds and severity bands are preserved in the plan', () => {
  const widget = gauge({
    warn: undefined,
    crit: undefined,
    domain: {
      min: 0,
      max: 100,
      thresholds: [
        { at: 80, severity: 'warning', color: '#ffaa00' },
        { at: 95, severity: 'critical', color: '#ff0000' },
      ],
    },
  });
  const plan = irToRenderPlan(layout([widget]), { data: { cpu: 96 } });
  const valueArc = allOps(plan).find((op) => op.role === 'gauge-value');
  assert.equal(valueArc.stroke, '#ff0000');
  assert.deepEqual(valueArc.thresholds.map((item) => item.at), [80, 95]);
});

/* ------------------------------------------------------------------------- */
/* Bar, number, text                                                          */
/* ------------------------------------------------------------------------- */

test('bar geometry carries value, range, orientation and threshold colour', () => {
  const bar = {
    type: 'bar',
    x: 20,
    y: 20,
    w: 200,
    h: 40,
    metric: 'mem',
    min: 0,
    max: 100,
    label: 'Memory',
    unit: '%',
    warn: 80,
    crit: 95,
    decimals: 0,
  };
  const plan = irToRenderPlan(layout([bar]), { data: { mem: 50 } });
  const op = allOps(plan).find((item) => item.op === 'bar');
  assert.equal(op.value, 50);
  assert.equal(op.w, 200);
  assert.equal(op.orientation, 'horizontal');
  assert.equal(op.fill, '#5b6fff');

  const svg = planToSvg(plan);
  assert.match(svg, /width="100"/);
  assert.ok(svg.includes('Memory'));
  assert.ok(svg.includes('50%'));

  const warning = irToRenderPlan(layout([bar]), { data: { mem: 90 } });
  assert.equal(allOps(warning).find((item) => item.op === 'bar').fill, '#f0b94a');
});

test('number and text widgets emit formatted value and template text', () => {
  const number = {
    type: 'number',
    x: 0,
    y: 0,
    w: 160,
    h: 80,
    metric: 'temp',
    min: 0,
    max: 120,
    decimals: 1,
    unit: 'celsius',
    label: 'Temp',
  };
  const text = {
    type: 'text',
    x: 0,
    y: 100,
    w: 300,
    h: 40,
    text: 'Node {node_name}: {temp}',
    font_size: 18,
  };
  const plan = irToRenderPlan(layout([number, text]), {
    data: { temp: 42.25, node_name: 'alpha' },
  });
  const ops = allOps(plan);
  assert.equal(ops.find((op) => op.id.endsWith('.value-text')).text, '42.3°C');
  assert.equal(ops.find((op) => op.id.endsWith('.text')).text, 'Node alpha: 42.3');

  const plan2 = applyData(plan, { temp: 50, node_name: 'beta' });
  assert.equal(allOps(plan2).find((op) => op.id.endsWith('.text')).text, 'Node beta: 50.0');
});

/* ------------------------------------------------------------------------- */
/* Sparkline and status                                                       */
/* ------------------------------------------------------------------------- */

test('sparkline maps values into bounded geometry', () => {
  const spark = {
    type: 'sparkline',
    x: 10,
    y: 10,
    w: 200,
    h: 100,
    metric: 'history',
    min: 0,
    max: 100,
    label: 'Load',
    decimals: 0,
  };
  const plan = irToRenderPlan(layout([spark]), {
    data: { history: [0, 50, 100] },
  });
  const line = allOps(plan).find((op) => op.role === 'sparkline');
  assert.equal(line.points.length, 3);
  assert.deepEqual(line.points[0], [10, 110]);
  assert.deepEqual(line.points[1], [110, 60]);
  assert.deepEqual(line.points[2], [210, 10]);

  applyData(plan, { history: [100, 100] });
  assert.equal(line.points.length, 2);
  assert.deepEqual(line.points[0], [10, 10]);
  assert.equal(allOps(plan).find((op) => op.id.endsWith('.value-text')).text, '100');
});

test('status supports online, offline and icon primitives', () => {
  const status = {
    type: 'status',
    x: 0,
    y: 0,
    w: 160,
    h: 40,
    metric: 'online',
    label: 'Node',
  };
  const online = irToRenderPlan(layout([status]), { data: { online: true } });
  const dot = allOps(online).find((op) => op.role === 'status-dot');
  assert.equal(dot.fill, '#42d392');
  assert.equal(allOps(online).find((op) => op.role === 'status-text').text, 'Online');

  const offline = irToRenderPlan(layout([status]), { data: { online: false } });
  assert.equal(allOps(offline).find((op) => op.role === 'status-dot').fill, '#f26464');
  assert.equal(allOps(offline).find((op) => op.role === 'status-text').text, 'Offline');

  const icon = irToRenderPlan(layout([{
    type: 'icon',
    x: 0,
    y: 0,
    w: 64,
    h: 64,
    icon: 'cpu',
    label: 'CPU',
  }]));
  assert.ok(allOps(icon).some((op) => op.op === 'icon' && op.name === 'cpu'));
});

/* ------------------------------------------------------------------------- */
/* Fallbacks, theme, clipping and determinism                                 */
/* ------------------------------------------------------------------------- */

test('image widgets emit safe image references and placeholders', () => {
  const safe = irToRenderPlan(layout([{
    type: 'image',
    x: 0,
    y: 0,
    w: 64,
    h: 64,
    url: 'data:image/png;base64,AAAA',
    label: 'Logo',
  }]));
  const svg = planToSvg(safe);
  assert.ok(svg.includes('<image '));
  assert.ok(svg.includes('data:image/png;base64,AAAA'));

  const unsafe = irToRenderPlan(layout([{
    type: 'image',
    x: 0,
    y: 0,
    w: 64,
    h: 64,
    url: 'javascript:alert(1)',
  }]));
  const unsafeSvg = planToSvg(unsafe);
  assert.equal(unsafeSvg.includes('javascript:'), false);
  assert.ok(allOps(unsafe).some((op) => op.op === 'rect'));
});

test('table, card and unknown widget kinds fall back safely', () => {
  const plan = irToRenderPlan(layout([
    { type: 'table', x: 0, y: 0, w: 200, h: 100, label: 'Facts', rows: [['a', '1'], ['b', '2']] },
    { type: 'card', x: 210, y: 0, w: 100, h: 100, label: 'Card', metric: 'x', min: 0, max: 10 },
    { type: 'mystery', x: 0, y: 110, w: 100, h: 50, label: 'Unknown' },
  ]), { data: { x: 3 } });
  const ops = allOps(plan);
  assert.ok(ops.some((op) => op.id.endsWith('.header')));
  assert.ok(ops.some((op) => op.id.endsWith('.r0.c1') && op.text === '1'));
  assert.ok(plan.warnings.some((warning) => warning.includes('card fallback')));
});

test('theme tokens resolve references and override defaults', () => {
  const plan = irToRenderPlan(layout([gauge({ color: undefined })]), {
    data: { cpu: 10 },
    theme: {
      tokens: {
        color: {
          accent: { $type: 'color', $value: '#123456' },
          track: { $type: 'color', $value: '#222222' },
        },
        gauge: {
          fill: { $type: 'color', $value: '{color.accent}' },
        },
      },
    },
  });
  const arcs = allOps(plan).filter((op) => op.op === 'arc');
  assert.equal(arcs[0].stroke, '#222222');
  assert.equal(arcs[1].stroke, '#123456');
});

test('widgets are clipped to the canvas and fully outside widgets are skipped', () => {
  const plan = irToRenderPlan(layout([
    { type: 'rect', x: -10, y: 10, w: 40, h: 40 },
    { type: 'bar', x: 400, y: 10, w: 40, h: 40 },
  ], { width: 100, height: 100 }));
  assert.equal(plan.ops.length, 1);
  assert.deepEqual(plan.ops[0].clip, { x: 0, y: 10, w: 30, h: 40 });
  assert.ok(plan.warnings.some((warning) => warning.includes('outside')));
});

test('the same layout produces byte-identical plans', () => {
  const input = layout([gauge(), {
    type: 'sparkline',
    x: 10,
    y: 200,
    w: 200,
    h: 30,
    values: [1, 2, 3],
  }]);
  const first = irToRenderPlan(input, { data: { cpu: 42 } });
  const second = irToRenderPlan(input, { data: { cpu: 42 } });
  assert.equal(JSON.stringify(first), JSON.stringify(second));
});

/* ------------------------------------------------------------------------- */
/* applyData reuses geometry                                                  */
/* ------------------------------------------------------------------------- */

test('applyData mutates bound values without rebuilding operations', () => {
  const plan = irToRenderPlan(layout([gauge(), {
    type: 'bar',
    x: 0,
    y: 200,
    w: 200,
    h: 30,
    metric: 'cpu',
    min: 0,
    max: 100,
    warn: 70,
  }]), { data: { cpu: 10 } });
  const group = plan.ops[0];
  const children = group.children.slice();
  const valueArc = children.find((op) => op.role === 'gauge-value');
  const valueText = children.find((op) => op.role === 'value-text');
  const startAngle = valueArc.startAngle;

  applyData(plan, { cpu: 95 });

  assert.equal(plan.ops[0], group);
  assert.equal(group.children[0], children[0]);
  assert.equal(children.find((op) => op.role === 'gauge-value'), valueArc);
  assert.equal(valueArc.startAngle, startAngle);
  assert.equal(valueArc.endAngle, 391.5);
  assert.equal(valueArc.stroke, '#f26464');
  assert.equal(valueText.text, '95%');
});

/* ------------------------------------------------------------------------- */
/* SVG escaping, determinism and rejection                                    */
/* ------------------------------------------------------------------------- */

test('SVG output escapes text and attributes', () => {
  const plan = irToRenderPlan(layout([{
    type: 'text',
    x: 0,
    y: 0,
    w: 300,
    h: 40,
    text: '<script>alert("x")</script>&"\'' ,
  }]));
  const svg = planToSvg(plan);
  assert.ok(svg.startsWith('<svg '));
  assert.ok(svg.includes('&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;&amp;'));
  assert.equal(svg.includes('<script>'), false);
  assert.ok(svg.includes('viewBox="0 0 320 240"'));
});

test('unknown draw operations are rejected by SVG and LVGL adapters', () => {
  const plan = { width: 100, height: 100, ops: [{ op: 'eval', x: 0, y: 0 }] };
  assert.throws(() => planToSvg(plan), (error) => error instanceof DisplayIrError && error.code === 'UNKNOWN_OP');
  assert.throws(() => planToLvgl(plan), (error) => error instanceof DisplayIrError && error.code === 'UNKNOWN_OP');
});

test('large inputs and huge dimensions are rejected or bounded', () => {
  const widgets = Array.from({ length: MAX_WIDGETS + 1 }, () => ({ type: 'text', w: 1, h: 1 }));
  assert.throws(
    () => irToRenderPlan({ widgets }),
    (error) => error instanceof DisplayIrError && error.code === 'TOO_MANY_WIDGETS',
  );
  assert.throws(
    () => irToRenderPlan({ width: MAX_DIMENSION + 1, widgets: [gauge()] }),
    (error) => error instanceof DisplayIrError && error.code === 'INVALID_SIZE',
  );
});

/* ------------------------------------------------------------------------- */
/* LVGL primitives                                                            */
/* ------------------------------------------------------------------------- */

test('planToLvgl emits deterministic primitives for the core widgets', () => {
  const plan = irToRenderPlan(layout([gauge(), {
    type: 'bar',
    x: 0,
    y: 200,
    w: 200,
    h: 30,
    metric: 'cpu',
  }, {
    type: 'sparkline',
    x: 0,
    y: 0,
    w: 100,
    h: 40,
    values: [1, 2, 3],
  }]), { data: { cpu: 50 } });
  const lvgl = planToLvgl(plan);
  assert.equal(lvgl.target, 'lvgl');
  const types = new Set(lvgl.primitives.map((item) => item.type));
  assert.ok(types.has('lv_arc'));
  assert.ok(types.has('lv_bar'));
  assert.ok(types.has('lv_chart'));
  assert.ok(types.has('lv_label'));
  assert.ok(types.has('lv_obj'));
  assert.equal(JSON.stringify(lvgl), JSON.stringify(planToLvgl(plan)));
});
