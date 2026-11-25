// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Golden render-parity tests for `renderer/display_ir.js`.
 *
 * Run with:
 *   node --test renderer/golden.test.mjs
 *
 * Regenerate intentionally (test-only, guarded):
 *   UPDATE_GOLDENS=1 node --test renderer/golden.test.mjs
 *
 * The regenerator rewrites only the files it owns under
 * `tests/fixtures/display_golden/` and merges its entries into `hashes.json`.
 * Never run it to paper over a production behaviour change; inspect the diff
 * first.
 */

import test, { before } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import displayIr from './display_ir.js';

const {
  DisplayIrError,
  applyData,
  irToRenderPlan,
  planToLvgl,
  planToSvg,
} = displayIr;

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '..');
const GOLDEN_DIR = path.join(ROOT, 'tests', 'fixtures', 'display_golden');
const UPDATE_GOLDENS = process.env.UPDATE_GOLDENS === '1';

const HASHES_FILE = path.join(GOLDEN_DIR, 'hashes.json');
const OWNED_FILES = ['render_plan.json', 'render_plan.svg', 'lvgl_primitives.json'];

function readJson(name) {
  return JSON.parse(fs.readFileSync(path.join(GOLDEN_DIR, name), 'utf8'));
}

const layout = readJson('screen_layout.json');
const theme = readJson('theme.json');
const data = readJson('data.json');
const renderOptions = { data, tokens: theme.tokens };

const plan = irToRenderPlan(layout, renderOptions);
const svg = planToSvg(plan);
const primitives = planToLvgl(plan);

function serializeJson(value) {
  return `${JSON.stringify(value, null, 2)}\n`;
}

function serializeText(value) {
  return value.endsWith('\n') ? value : `${value}\n`;
}

function writeGolden(name, text) {
  fs.writeFileSync(path.join(GOLDEN_DIR, name), text, 'utf8');
}

function firstDiff(expected, actual) {
  const expectedLines = expected.split('\n');
  const actualLines = actual.split('\n');
  const limit = Math.max(expectedLines.length, actualLines.length);
  for (let index = 0; index < limit; index += 1) {
    if (expectedLines[index] !== actualLines[index]) {
      const start = Math.max(0, index - 2);
      const context = [];
      for (let line = start; line <= Math.min(limit - 1, index + 2); line += 1) {
        context.push(`  ${line + 1}: expected ${JSON.stringify(expectedLines[line])}`);
        context.push(`  ${line + 1}: actual   ${JSON.stringify(actualLines[line])}`);
      }
      return `first difference at line ${index + 1}\n${context.join('\n')}`;
    }
  }
  return 'no textual difference';
}

function assertGolden(name, actual) {
  if (UPDATE_GOLDENS) {
    writeGolden(name, actual);
    return;
  }
  const expectedPath = path.join(GOLDEN_DIR, name);
  assert.ok(fs.existsSync(expectedPath), `missing golden ${expectedPath}`);
  const expected = fs.readFileSync(expectedPath, 'utf8');
  assert.equal(
    actual,
    expected,
    `golden mismatch for ${name}\n${firstDiff(expected, actual)}\n`
      + 'Run UPDATE_GOLDENS=1 node --test renderer/golden.test.mjs to regenerate intentionally.',
  );
}

function sha256(text) {
  return createHash('sha256').update(text, 'utf8').digest('hex');
}

function readHashes() {
  if (!fs.existsSync(HASHES_FILE)) return {};
  return JSON.parse(fs.readFileSync(HASHES_FILE, 'utf8'));
}

before(() => {
  if (!UPDATE_GOLDENS) return;
  writeGolden('render_plan.json', serializeJson(plan));
  writeGolden('render_plan.svg', serializeText(svg));
  writeGolden('lvgl_primitives.json', serializeJson(primitives));
});

/* ------------------------------------------------------------------------- */
/* Golden outputs                                                            */
/* ------------------------------------------------------------------------- */

test('golden render plan matches exactly', () => {
  assertGolden('render_plan.json', serializeJson(plan));
});

test('golden SVG preview matches exactly', () => {
  assertGolden('render_plan.svg', serializeText(svg));
});

test('golden LVGL primitive list matches exactly', () => {
  assertGolden('lvgl_primitives.json', serializeJson(primitives));
});

/* ------------------------------------------------------------------------- */
/* Browser plan -> LVGL primitive parity                                     */
/* ------------------------------------------------------------------------- */

function flattenOps(ops) {
  const output = [];
  for (const op of ops) {
    if (op.op === 'group') output.push(...flattenOps(op.children));
    else output.push(op);
  }
  return output;
}

test('plan -> LVGL primitives preserves bounds, values, thresholds, and text', () => {
  const byId = new Map(primitives.map((primitive) => [primitive.id, primitive]));
  const topGroups = primitives.filter(
    (primitive) => /^w\d+$/.test(primitive.id) && primitive.type === 'lv_obj',
  );
  assert.equal(topGroups.length, layout.widgets.length);

  layout.widgets.forEach((widget, index) => {
    const group = byId.get(`w${index}`);
    assert.ok(group, `missing group primitive w${index}`);
    assert.deepEqual(
      { x: group.x, y: group.y, w: group.w, h: group.h },
      { x: widget.x, y: widget.y, w: widget.w, h: widget.h },
      `bounds changed for ${widget.id}`,
    );
    assert.deepEqual(group.clip, plan.ops[index].clip);
  });

  const ops = flattenOps(plan.ops);
  let boundTexts = 0;
  for (const op of ops) {
    const primitive = byId.get(op.id);
    assert.ok(primitive, `missing primitive for ${op.id}`);
    if (op.role === 'gauge-value' || op.role === 'bar') {
      assert.equal(primitive.value, op.value, `value changed for ${op.id}`);
      assert.equal(primitive.min, op.binding.min, `min changed for ${op.id}`);
      assert.equal(primitive.max, op.binding.max, `max changed for ${op.id}`);
      assert.deepEqual(primitive.thresholds, op.thresholds, `thresholds changed for ${op.id}`);
      assert.equal(primitive.type, op.role === 'gauge-value' ? 'lv_arc' : 'lv_bar');
    }
    if (op.op === 'text') {
      assert.equal(primitive.type, 'lv_label', `text primitive type changed for ${op.id}`);
      assert.equal(primitive.text, op.text, `text changed for ${op.id}`);
      if (op.role === 'value-text') boundTexts += 1;
    }
  }
  assert.ok(boundTexts >= 4, 'expected gauge/bar/number/card value text primitives');

  const gauge = byId.get('cpu_temp.value');
  assert.equal(gauge.value, 72.5);
  assert.deepEqual(
    gauge.thresholds.map((threshold) => [threshold.at, threshold.severity]),
    [[0, 'ok'], [75, 'warning'], [90, 'critical']],
  );
  const bar = byId.get('memory');
  assert.equal(bar.value, 63);
  assert.equal(byId.get('node_label.text').text, 'Node atlas-01 / load 42.0%');
  assert.equal(byId.get('quick_stats.r1.c0').text, 'CPU');
});

test('applyData updates bound primitives without changing geometry', () => {
  const updated = irToRenderPlan(layout, renderOptions);
  const beforeArc = flattenOps(updated.ops).find((op) => op.id === 'cpu_temp.value');
  const beforeText = flattenOps(updated.ops).find((op) => op.id === 'power_draw.value-text');
  const beforeGeometry = {
    x: beforeArc.cx,
    y: beforeArc.cy,
    r: beforeArc.r,
    text: beforeText.text,
  };

  applyData(updated, { ...data, 'org.ozma/node.cpu_temp': 91, 'power.draw': 460 });
  const afterArc = flattenOps(updated.ops).find((op) => op.id === 'cpu_temp.value');
  const afterText = flattenOps(updated.ops).find((op) => op.id === 'power_draw.value-text');

  assert.equal(afterArc.stroke, '#f26464');
  assert.equal(afterArc.severity, 'critical');
  assert.equal(afterText.text, '460 watt');
  assert.deepEqual(
    { x: afterArc.cx, y: afterArc.cy, r: afterArc.r },
    { x: beforeGeometry.x, y: beforeGeometry.y, r: beforeGeometry.r },
  );
});

/* ------------------------------------------------------------------------- */
/* Fail closed                                                               */
/* ------------------------------------------------------------------------- */

test('unknown operations and invalid values fail closed', () => {
  assert.throws(
    () => planToSvg({ width: 10, height: 10, ops: [{ op: 'bogus' }] }),
    (error) => error instanceof DisplayIrError && error.code === 'UNKNOWN_OP',
  );
  assert.throws(
    () => planToLvgl({ width: 10, height: 10, ops: [{ op: 'bogus' }] }),
    (error) => error instanceof DisplayIrError && error.code === 'UNKNOWN_OP',
  );
  assert.throws(
    () => planToSvg({ ops: 'not-an-array' }),
    (error) => error instanceof DisplayIrError && error.code === 'INVALID_PLAN',
  );
  assert.throws(
    () => applyData({ ops: [] }, null),
    (error) => error instanceof DisplayIrError && error.code === 'INVALID_DATA',
  );
  assert.throws(
    () => irToRenderPlan({}),
    (error) => error instanceof DisplayIrError && error.code === 'INVALID_LAYOUT',
  );
  assert.throws(
    () => planToSvg({
      width: 10,
      height: 10,
      ops: [{ op: 'polyline', id: 'p', points: [[0, 0], ['x', 1]] }],
    }),
    (error) => error instanceof DisplayIrError && error.code === 'INVALID_OP',
  );
});

/* ------------------------------------------------------------------------- */
/* Stable hashes                                                             */
/* ------------------------------------------------------------------------- */

test('golden artifact hashes are stable', () => {
  const hashes = readHashes();
  if (UPDATE_GOLDENS) {
    for (const name of OWNED_FILES) {
      hashes[name] = sha256(fs.readFileSync(path.join(GOLDEN_DIR, name), 'utf8'));
    }
    fs.writeFileSync(
      HASHES_FILE,
      `${JSON.stringify(hashes, Object.keys(hashes).sort(), 2)}\n`,
      'utf8',
    );
    return;
  }
  for (const name of OWNED_FILES) {
    assert.ok(hashes[name], `hashes.json is missing ${name}`);
    const actual = sha256(fs.readFileSync(path.join(GOLDEN_DIR, name), 'utf8'));
    assert.equal(actual, hashes[name], `hash mismatch for ${name}`);
  }
});
