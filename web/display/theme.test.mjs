// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Tests for the theme loader.
 *
 * Run with: node --test web/display/theme.test.mjs
 */

import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import {
  CSS_ALIASES,
  MAX_THEME_BYTES,
  MAX_TOKEN_DEPTH,
  THEME_SCHEMA,
  ThemeError,
  applyTheme,
  checkTheme,
  loadThemeFromQuery,
  parseTheme,
  resolveThemeTokens,
  resolveThemeUrl,
  resolveTokenValue,
  themeToCssVariables,
  tokenToCssVariable,
  validateTheme,
} from './theme.js';

function baseTheme(overrides = {}) {
  return {
    schema: THEME_SCHEMA,
    id: 'org.ozma.theme.test',
    name: 'Test Theme',
    author: 'Test',
    license: 'AGPL-3.0-only WITH OzmaPluginException',
    canvas: { width: 680, height: 480, background: '{color.bg}' },
    tokens: {
      color: {
        bg: { $type: 'color', $value: '#0a0a0f' },
        fg: { $type: 'color', $value: '#e5e7eb' },
        accent: { $type: 'color', $value: '#5b6fff' },
        warn: { $type: 'color', $value: '#f0b94a' },
        crit: { $type: 'color', $value: '#f26464' },
      },
      'font.family': {
        $type: 'fontFamily',
        $value: ['Inter', 'system-ui', 'sans-serif'],
      },
    },
    widgets: [
      {
        type: 'gauge',
        metric: '@active.TCPU',
        min: 0,
        max: 100,
        warn: 70,
        crit: 90,
        label: 'CPU',
        style: 'arc',
      },
    ],
    ...overrides,
  };
}

function fakeDocument() {
  const properties = new Map();
  const root = {
    dataset: {},
    style: {
      setProperty(name, value) {
        properties.set(name, value);
      },
    },
  };
  return { documentElement: root, properties };
}

/* ------------------------------------------------------------------------- */
/* Validation                                                                 */
/* ------------------------------------------------------------------------- */

test('validates a theme and preserves benign unknown fields', () => {
  const theme = validateTheme({
    ...baseTheme(),
    'x-vendor': { note: 'kept' },
  });

  assert.equal(theme.schema, THEME_SCHEMA);
  assert.equal(theme.id, 'org.ozma.theme.test');
  assert.equal(theme.canvas.width, 680);
  assert.equal(theme.widgets.length, 1);
  assert.deepEqual(theme['x-vendor'], { note: 'kept' });
});

test('rejects a theme with the wrong schema or id', () => {
  assert.throws(
    () => validateTheme(baseTheme({ schema: 'org.ozma.theme/0' })),
    (error) => error instanceof ThemeError && error.code === 'THEME_SCHEMA',
  );
  assert.throws(
    () => validateTheme(baseTheme({ id: '   ' })),
    (error) => error instanceof ThemeError && error.code === 'THEME_ID',
  );
});

test('rejects executable fields and javascript URLs', () => {
  assert.throws(
    () => validateTheme({ ...baseTheme(), script: 'alert(1)' }),
    (error) => error.code === 'THEME_EXECUTABLE',
  );
  assert.throws(
    () => validateTheme({ ...baseTheme(), widgets: [{ type: 'gauge', onclick: 'x' }] }),
    (error) => error.code === 'THEME_EXECUTABLE',
  );
  assert.throws(
    () => validateTheme({ ...baseTheme(), canvas: { background: 'javascript:alert(1)' } }),
    (error) => error.code === 'THEME_EXECUTABLE',
  );
  assert.throws(
    () => validateTheme({ ...baseTheme(), canvas: { background: '<script>alert(1)</script>' } }),
    (error) => error.code === 'THEME_EXECUTABLE',
  );
});

test('rejects object depth above the limit', () => {
  let deep = { value: 'leaf' };
  for (let index = 0; index < 40; index += 1) deep = { nested: deep };
  assert.throws(
    () => validateTheme({ ...baseTheme(), deep }),
    (error) => error.code === 'THEME_DEPTH',
  );
});

test('rejects oversized canvas and invalid widget sizes', () => {
  assert.throws(
    () => validateTheme(baseTheme({ canvas: { width: 20_000, height: 480 } })),
    (error) => /canvas\.width/.test(error.message) || error.path.includes('canvas.width'),
  );
  assert.throws(
    () => validateTheme(baseTheme({
      widgets: [{ type: 'gauge', w: -5, h: 10 }],
    })),
    (error) => error.code === 'THEME_NUMBER',
  );
});

test('rejects invalid metric bindings and accepts adapter-style bindings', () => {
  assert.doesNotThrow(() => validateTheme(baseTheme({
    widgets: [{ type: 'gauge', metric: '@active.TCPU' }],
  })));
  assert.doesNotThrow(() => validateTheme(baseTheme({
    widgets: [{ type: 'gauge', metric: 'device.cpu.temp', min: 0, max: 100 }],
  })));
  assert.throws(
    () => validateTheme(baseTheme({
      widgets: [{ type: 'gauge', metric: 'bad metric with spaces' }],
    })),
    (error) => error.code === 'THEME_WIDGET_BINDING',
  );
});

test('rejects unknown token types and malformed token values', () => {
  assert.throws(
    () => validateTheme(baseTheme({
      tokens: { bad: { $type: 'not-a-token-type', $value: 'x' } },
    })),
    (error) => error.code === 'THEME_TOKEN_TYPE',
  );
  assert.throws(
    () => validateTheme(baseTheme({
      tokens: { bad: { $type: 'number', $value: 'not-a-number' } },
    })),
    (error) => error.code === 'THEME_TOKEN_VALUE',
  );
});

test('checkTheme returns a typed result instead of throwing', () => {
  const result = checkTheme({ schema: 'nope' });
  assert.equal(result.ok, false);
  assert.equal(result.theme, null);
  assert.equal(result.errors[0].code, 'THEME_SCHEMA');
});

/* ------------------------------------------------------------------------- */
/* Token resolution                                                           */
/* ------------------------------------------------------------------------- */

test('resolves nested, flat, interpolated, and fallback token references', () => {
  const tokens = {
    color: {
      bg: { $type: 'color', $value: '#0a0a0f' },
      accent: { $type: 'color', $value: '#5b6fff' },
    },
    'border.strong': { $type: 'color', $value: '{color.accent}' },
  };

  assert.equal(resolveTokenValue('{color.accent}', tokens), '#5b6fff');
  assert.equal(resolveTokenValue('{border.strong}', tokens), '#5b6fff');
  assert.equal(
    resolveTokenValue('1px solid {color.accent}', tokens),
    '1px solid #5b6fff',
  );
  assert.equal(
    resolveTokenValue('1px solid {color.missing | #ffffff}', tokens),
    '1px solid #ffffff',
  );
  assert.equal(
    resolveTokenValue('1px solid {color.missing, #eeeeee}', tokens),
    '1px solid #eeeeee',
  );
  assert.equal(
    resolveTokenValue('{color.missing ?? #123456}', tokens),
    '#123456',
  );
  assert.equal(resolveTokenValue('var(--accent, {color.accent})', tokens), 'var(--accent, #5b6fff)');
});

test('a full-string reference preserves native token values', () => {
  const tokens = {
    'font.family': { $type: 'fontFamily', $value: ['Inter', 'sans-serif'] },
  };
  assert.deepEqual(resolveTokenValue('{font.family}', tokens), ['Inter', 'sans-serif']);
});

test('rejects unknown references without a fallback', () => {
  assert.throws(
    () => resolveTokenValue('{color.missing}', {}),
    (error) => error.code === 'THEME_TOKEN_MISSING',
  );
  assert.throws(
    () => resolveTokenValue('{color}', { color: { accent: '#fff' } }),
    (error) => error.code === 'THEME_TOKEN_VALUE',
  );
});

test('rejects token cycles and excessive token depth', () => {
  assert.throws(
    () => resolveTokenValue('{a}', { a: '{b}', b: '{a}' }),
    (error) => error.code === 'THEME_TOKEN_CYCLE',
  );

  const tokens = { a0: '#fff' };
  for (let index = 1; index <= MAX_TOKEN_DEPTH + 2; index += 1) {
    tokens[`a${index}`] = `{a${index - 1}}`;
  }
  assert.throws(
    () => resolveTokenValue(`{a${MAX_TOKEN_DEPTH + 2}}`, tokens),
    (error) => error.code === 'THEME_TOKEN_DEPTH',
  );
});

test('resolveThemeTokens flattens and resolves all token leaves', () => {
  const theme = validateTheme(baseTheme());
  const resolved = resolveThemeTokens(theme);
  assert.equal(resolved['color.bg'], '#0a0a0f');
  assert.equal(resolved['color.accent'], '#5b6fff');
  assert.deepEqual(resolved['font.family'], ['Inter', 'system-ui', 'sans-serif']);
});

/* ------------------------------------------------------------------------- */
/* CSS variables                                                              */
/* ------------------------------------------------------------------------- */

test('builds namespaced CSS variables and adapter aliases', () => {
  const theme = validateTheme(baseTheme());
  const variables = themeToCssVariables(theme);

  assert.equal(tokenToCssVariable('color.accent'), '--ozma-color-accent');
  assert.equal(tokenToCssVariable('fontFamily'), '--ozma-font-family');
  assert.equal(variables['--ozma-color-bg'], '#0a0a0f');
  assert.equal(variables['--ozma-font-family'], 'Inter, system-ui, sans-serif');
  assert.equal(variables['--bg'], CSS_ALIASES['color.bg'] ? '#0a0a0f' : undefined);
  assert.equal(variables['--accent'], '#5b6fff');
  assert.equal(variables['--ozma-canvas-background'], '#0a0a0f');
});

test('applies a theme to a document root', () => {
  const theme = validateTheme(baseTheme());
  const doc = fakeDocument();
  const variables = applyTheme(theme, doc);

  assert.equal(doc.properties.get('--ozma-color-bg'), '#0a0a0f');
  assert.equal(doc.properties.get('--accent'), '#5b6fff');
  assert.equal(doc.documentElement.dataset.ozmaTheme, 'org.ozma.theme.test');
  assert.equal(doc.documentElement.dataset.ozmaThemeSchema, THEME_SCHEMA);
  assert.equal(variables['--ozma-color-fg'], '#e5e7eb');
});

/* ------------------------------------------------------------------------- */
/* Parsing and loading                                                        */
/* ------------------------------------------------------------------------- */

test('parses theme JSON and rejects oversized or malformed sources', () => {
  const parsed = parseTheme(JSON.stringify(baseTheme()));
  assert.equal(parsed.id, 'org.ozma.theme.test');

  assert.throws(
    () => parseTheme('x'.repeat(MAX_THEME_BYTES + 1)),
    (error) => error.code === 'THEME_SIZE',
  );
  assert.throws(
    () => parseTheme('{not json'),
    (error) => error.code === 'THEME_JSON',
  );
});

test('resolves built-in ids, explicit URLs, and rejects executable URLs', () => {
  const baseUrl = 'https://example.test/display/index.html';
  assert.equal(
    resolveThemeUrl('core-dark', { baseUrl }),
    'https://example.test/display/themes/core-dark.json',
  );
  assert.equal(
    resolveThemeUrl('./themes/custom.json', { baseUrl }),
    'https://example.test/display/themes/custom.json',
  );
  assert.equal(
    resolveThemeUrl('https://cdn.test/theme.json', { baseUrl }),
    'https://cdn.test/theme.json',
  );
  assert.throws(
    () => resolveThemeUrl('javascript:alert(1)', { baseUrl }),
    (error) => error.code === 'THEME_URL',
  );
  assert.throws(
    () => resolveThemeUrl('data:text/json,{}', { baseUrl }),
    (error) => error.code === 'THEME_URL',
  );
});

test('loads ?theme= through an injected fetch implementation', async () => {
  const calls = [];
  const fetchImpl = async (url) => {
    calls.push(url);
    return {
      ok: true,
      status: 200,
      text: async () => JSON.stringify(baseTheme()),
    };
  };

  const result = await loadThemeFromQuery('?theme=core-dark', {
    baseUrl: 'https://example.test/display/index.html',
    fetchImpl,
  });

  assert.equal(calls[0], 'https://example.test/display/themes/core-dark.json');
  assert.equal(result.theme.id, 'org.ozma.theme.test');
  assert.equal(result.source, 'https://example.test/display/themes/core-dark.json');

  assert.equal(await loadThemeFromQuery('', { fetchImpl }), null);
  assert.equal(calls.length, 1);
});

/* ------------------------------------------------------------------------- */
/* Shipped theme                                                              */
/* ------------------------------------------------------------------------- */

test('core-dark.json validates and resolves its token references', () => {
  const text = readFileSync(new URL('./themes/core-dark.json', import.meta.url), 'utf8');
  const theme = parseTheme(text);
  const tokens = resolveThemeTokens(theme);

  assert.equal(theme.id, 'org.ozma.theme.core-dark');
  assert.equal(theme.vibe.taxonomy, 'org.ozma.vibe/1');
  assert.equal(tokens['color.bg'], '#0a0a0f');
  assert.equal(tokens['gauge.fill'], '#5b6fff');
  assert.deepEqual(tokens['font.family'], ['Inter', 'system-ui', 'sans-serif']);

  const variables = themeToCssVariables(theme);
  assert.equal(variables['--ozma-canvas-background'], '#0a0a0f');
  assert.equal(variables['--ozma-gauge-fill'], '#5b6fff');
});
