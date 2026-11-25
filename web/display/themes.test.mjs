// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Tests for the shipped Ozma theme pack.
 *
 * Run with: node --test web/display/themes.test.mjs
 */

import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, statSync } from 'node:fs';

import {
  MAX_THEME_BYTES,
  THEME_SCHEMA,
  VIBE_TAXONOMY,
  flattenTokens,
  parseTheme,
  resolveThemeTokens,
  themeToCssVariables,
} from './theme.js';

const THEMES_DIR = new URL('./themes/', import.meta.url);
const NEW_THEMES = ['core-light', 'noc', 'audio', 'minimal'];
const ALL_THEMES = ['core-dark', ...NEW_THEMES];

const REQUIRED_TOKENS = [
  'color.bg',
  'color.fg',
  'color.muted',
  'color.accent',
  'color.warn',
  'color.crit',
  'font.family',
  'font.mono',
  'radius',
  'shadow.card',
  'gauge.fill',
  'gauge.track',
  'gauge.warn',
  'gauge.crit',
  'gauge.thickness',
  'sparkline.line',
  'sparkline.fill',
  'sparkline.track',
  'sparkline.grid',
  'sparkline.baseline',
  'sparkline.text',
  'sparkline.warn',
  'sparkline.crit',
  'sparkline.width',
  'sparkline.height',
];

const EXECUTABLE_KEYS = new Set([
  'script',
  'scripts',
  'inline_script',
  'javascript',
  'js',
  'eval',
  'exec',
  'code',
  'module',
  'import',
  'require',
  'function',
  'handler',
  'handlers',
  'callback',
  'callbacks',
]);

function readTheme(name) {
  const url = new URL(`${name}.json`, THEMES_DIR);
  return { url, text: readFileSync(url, 'utf8') };
}

function loadTheme(name) {
  const { url, text } = readTheme(name);
  return { url, text, theme: parseTheme(text), tokens: resolveThemeTokens(parseTheme(text)) };
}

function hexLuminance(hex) {
  let value = String(hex).trim().replace(/^#/, '');
  if (value.length === 3) value = value.split('').map((part) => part + part).join('');
  assert.match(value, /^[0-9a-f]{6}$/i, `expected #rrggbb, got ${hex}`);
  const channels = [0, 2, 4].map((offset) => Number.parseInt(value.slice(offset, offset + 2), 16) / 255);
  const linear = channels.map((channel) => (
    channel <= 0.03928
      ? channel / 12.92
      : ((channel + 0.055) / 1.055) ** 2.4
  ));
  return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
}

function contrastRatio(foreground, background) {
  const first = hexLuminance(foreground);
  const second = hexLuminance(background);
  return (Math.max(first, second) + 0.05) / (Math.min(first, second) + 0.05);
}

function collectKeys(value, out = []) {
  if (!value || typeof value !== 'object') return out;
  if (Array.isArray(value)) {
    for (const item of value) collectKeys(item, out);
    return out;
  }
  for (const [key, child] of Object.entries(value)) {
    out.push(key);
    collectKeys(child, out);
  }
  return out;
}

/* ------------------------------------------------------------------------- */
/* Theme files                                                                */
/* ------------------------------------------------------------------------- */

for (const name of ALL_THEMES) {
  test(`${name}.json validates and carries pack metadata`, () => {
    const { text, theme, tokens } = loadTheme(name);

    assert.equal(theme.schema, THEME_SCHEMA);
    assert.equal(theme.id, `org.ozma.theme.${name}`);
    assert.ok(theme.name);
    assert.match(theme.license, /^AGPL-3\.0-only/);
    assert.equal(theme.provenance.source, 'original');
    assert.ok(theme.canvas.width > 0 && theme.canvas.height > 0);
    assert.ok(theme.canvas.background);
    assert.ok(Array.isArray(theme.widgets) && theme.widgets.length > 0);

    assert.equal(theme.vibe.taxonomy, VIBE_TAXONOMY);
    assert.ok(Array.isArray(theme.vibe.keywords) && theme.vibe.keywords.length > 0);
    if (NEW_THEMES.includes(name)) {
      assert.ok(theme.shape && typeof theme.shape === 'object');
      assert.ok(theme.coherence && typeof theme.coherence === 'object');
      assert.equal(typeof theme.coherence.score, 'number');
      assert.ok(theme.coherence.score >= 0 && theme.coherence.score <= 1);
    }

    assert.ok(text.length < MAX_THEME_BYTES, `${name} must stay under the theme byte limit`);
  });
}

for (const name of NEW_THEMES) {
  test(`${name}.json defines the required colour, type, radius, shadow, gauge and sparkline tokens`, () => {
    const { theme, tokens } = loadTheme(name);
    const flattened = flattenTokens(theme.tokens);

    assert.ok(Object.keys(flattened).length <= 4096);
    for (const key of REQUIRED_TOKENS) {
      assert.notEqual(tokens[key], undefined, `${name} is missing ${key}`);
      assert.notEqual(tokens[key], '', `${name} has an empty ${key}`);
    }

    assert.ok(Array.isArray(tokens['font.family']));
    assert.ok(Array.isArray(tokens['font.mono']));
    assert.notEqual(tokens['typography.body'], undefined);

    const variables = themeToCssVariables(theme);
    assert.equal(variables['--bg'], tokens['color.bg']);
    assert.equal(variables['--accent'], tokens['color.accent']);
    assert.equal(variables['--ozma-canvas-background'], tokens['color.bg']);
    assert.notEqual(variables['--ozma-gauge-fill'], undefined);
    assert.notEqual(variables['--ozma-sparkline-line'], undefined);
  });
}

test('core-light meets WCAG AA contrast on both canvas and panel backgrounds', () => {
  const { tokens } = loadTheme('core-light');
  for (const background of ['color.bg', 'color.panel']) {
    for (const foreground of ['color.fg', 'color.muted', 'color.dim', 'color.accent', 'color.ok', 'color.warn', 'color.crit']) {
      const ratio = contrastRatio(tokens[foreground], tokens[background]);
      assert.ok(
        ratio >= 4.5,
        `${foreground} on ${background} must be >= 4.5:1, got ${ratio.toFixed(2)}:1`,
      );
    }
  }
});

test('theme files and the manifest stay within bounded sizes', () => {
  for (const name of ALL_THEMES) {
    const { url } = readTheme(name);
    assert.ok(statSync(url).size < MAX_THEME_BYTES, `${name}.json must be < ${MAX_THEME_BYTES} bytes`);
  }
  const manifestUrl = new URL('manifest.json', THEMES_DIR);
  assert.ok(statSync(manifestUrl).size < MAX_THEME_BYTES);
});

/* ------------------------------------------------------------------------- */
/* Manifest                                                                    */
/* ------------------------------------------------------------------------- */

test('manifest identifies the first-party core theme pack', () => {
  const manifest = JSON.parse(readFileSync(new URL('manifest.json', THEMES_DIR), 'utf8'));
  assert.equal(manifest.schema, 'org.ozma.theme-pack/1');
  assert.equal(manifest.type, 'theme_pack');
  assert.equal(manifest.id, 'ozma-core-themes');
  assert.match(manifest.version, /^\d+\.\d+\.\d+$/);
  assert.equal(manifest.author, 'ozmalabs');
  assert.equal(manifest.license, 'AGPL-3.0-only');
  assert.equal(manifest.provenance.type, 'original');
  assert.equal(manifest.provenance.author, 'ozmalabs');
  assert.ok(Array.isArray(manifest.tags) && manifest.tags.length > 0);
  assert.equal(manifest.vibe?.taxonomy, VIBE_TAXONOMY);
  assert.ok(manifest.shape && typeof manifest.shape === 'object');
  assert.ok(manifest.coherence && typeof manifest.coherence.score === 'number');
  assert.ok(Array.isArray(manifest.themes) && manifest.themes.length >= NEW_THEMES.length);

  const ids = new Set(manifest.themes.map((entry) => entry.id));
  for (const name of ALL_THEMES) assert.ok(ids.has(`org.ozma.theme.${name}`), `manifest is missing ${name}`);

  const keys = collectKeys(manifest).map((key) => key.toLowerCase());
  for (const key of keys) {
    assert.ok(!EXECUTABLE_KEYS.has(key), `manifest contains executable field ${key}`);
    assert.ok(!/^on[a-z]+$/.test(key), `manifest contains event handler field ${key}`);
  }
});

test('manifest sha256 hashes match the theme files byte-for-byte', () => {
  const manifestUrl = new URL('manifest.json', THEMES_DIR);
  const manifest = JSON.parse(readFileSync(manifestUrl, 'utf8'));

  for (const entry of manifest.themes) {
    assert.ok(entry.id, 'entry.id');
    assert.ok(entry.path, `${entry.id}.path`);
    assert.match(entry.sha256, /^[0-9a-f]{64}$/, `${entry.id}.sha256`);
    assert.equal(entry.vibe?.taxonomy, VIBE_TAXONOMY, `${entry.id}.vibe.taxonomy`);
    assert.ok(entry.shape && typeof entry.shape === 'object', `${entry.id}.shape`);
    assert.ok(entry.coherence && typeof entry.coherence.score === 'number', `${entry.id}.coherence`);

    const url = new URL(entry.path, manifestUrl);
    const bytes = readFileSync(url);
    const hash = createHash('sha256').update(bytes).digest('hex');
    assert.equal(hash, entry.sha256, `${entry.path} hash mismatch`);
    assert.equal(manifest.checksums.files[entry.path], hash, `${entry.path} checksum mismatch`);
    assert.ok(bytes.length < MAX_THEME_BYTES, `${entry.path} must be below the theme byte limit`);

    const theme = parseTheme(bytes.toString('utf8'));
    assert.equal(theme.id, entry.id);
    assert.equal(theme.vibe.taxonomy, entry.vibe.taxonomy);
  }
});
