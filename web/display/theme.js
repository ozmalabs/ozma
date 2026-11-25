// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Data-only Ozma display theme loader.
 *
 * Themes use the `org.ozma.theme/1` JSON schema described in
 * `docs/display-widget-library-research.md`.  They are untrusted input:
 * this module validates structure, size, depth, token types, widget bindings,
 * and rejects executable fields.  It never evaluates theme content.
 *
 * The module is dependency-free and exposes pure functions so it can be
 * tested in Node without a DOM.  When loaded directly by a browser page it
 * also applies `?theme=<id-or-url>` to the document.
 */

export const THEME_SCHEMA = 'org.ozma.theme/1';
export const VIBE_TAXONOMY = 'org.ozma.vibe/1';

export const MAX_THEME_BYTES = 512 * 1024;
export const MAX_THEME_DEPTH = 32;
export const MAX_TOKEN_DEPTH = 16;
export const MAX_TOKENS = 4096;
export const MAX_WIDGETS = 2048;
export const MAX_THEME_NODES = 100_000;
export const MAX_CANVAS_SIZE = 16_384;
export const MAX_STRING_LENGTH = 16_384;

const DEFAULT_CANVAS = Object.freeze({ width: 680, height: 480 });

/**
 * DTCG-compatible token types accepted by the theme loader.
 * Unknown types fail closed so a new token type is a deliberate change.
 */
export const TOKEN_TYPES = new Set([
  'color',
  'dimension',
  'fontFamily',
  'fontWeight',
  'duration',
  'cubicBezier',
  'number',
  'strokeStyle',
  'border',
  'transition',
  'shadow',
  'gradient',
  'typography',
  'string',
  'boolean',
]);

const BINDING_KEYS = [
  'metric',
  'binding',
  'bind',
  'secondMetric',
  'second_metric',
  'secondaryMetric',
  'secondary_metric',
  'data_key',
  'dataKey',
  'metric_key',
  'metricKey',
  'field',
  'field_id',
  'fieldId',
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
  'source_code',
  'module',
  'import',
  'require',
  'function',
  'handler',
  'handlers',
  'callback',
  'callbacks',
]);

const FORBIDDEN_KEYS = new Set(['__proto__', 'prototype', 'constructor']);

const EXECUTABLE_VALUE_PATTERNS = [
  /(?:^|[\s("'=;])javascript\s*:/i,
  /(?:^|[\s("'=;])vbscript\s*:/i,
  /<\s*script(?:\s|>)/i,
  /data\s*:\s*text\/javascript/i,
  /\bon[a-z]+\s*=/i,
  /\beval\s*\(/i,
  /\bnew\s+Function\s*\(/i,
];

/**
 * Map theme tokens onto the browser adapter's existing root variables.  This
 * keeps existing `style.css` rules working while themes supply their values.
 */
export const CSS_ALIASES = Object.freeze({
  'color.bg': '--bg',
  'color.background': '--bg',
  'color.fg': '--text',
  'color.text': '--text',
  'color.muted': '--text-muted',
  'color.dim': '--text-dim',
  'color.accent': '--accent',
  'color.ok': '--ok',
  'color.warn': '--warn',
  'color.crit': '--error',
  'font.family': '--font',
  'font.mono': '--mono',
  radius: '--radius',
  'radius.sm': '--radius-sm',
});

const BUILTIN_THEMES = Object.freeze({
  'core-dark': 'themes/core-dark.json',
});

/* ------------------------------------------------------------------------- */
/* Errors                                                                     */
/* ------------------------------------------------------------------------- */

export class ThemeError extends Error {
  constructor(code, message, path = '') {
    super(message);
    this.name = 'ThemeError';
    this.code = code;
    this.path = path;
  }
}

export class ThemeValidationError extends ThemeError {
  constructor(code, message, path = '') {
    super(code, message, path);
    this.name = 'ThemeValidationError';
  }
}

export class ThemeTokenError extends ThemeError {
  constructor(code, message, path = '') {
    super(code, message, path);
    this.name = 'ThemeTokenError';
  }
}

function fail(code, message, path = '') {
  throw new ThemeValidationError(code, message, path);
}

function tokenFail(code, message, path = '') {
  throw new ThemeTokenError(code, message, path);
}

/* ------------------------------------------------------------------------- */
/* Structural validation                                                      */
/* ------------------------------------------------------------------------- */

function isPlainObject(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function isTokenObject(value) {
  return isPlainObject(value) && (
    Object.prototype.hasOwnProperty.call(value, '$value')
    || Object.prototype.hasOwnProperty.call(value, 'value')
  );
}

/**
 * Iterative depth/node/executable scan.  Iterative traversal avoids a stack
 * overflow on a hostile deeply nested JSON document.
 */
function scanStructure(root, options = {}) {
  const maxDepth = options.maxDepth ?? MAX_THEME_DEPTH;
  const maxNodes = options.maxNodes ?? MAX_THEME_NODES;
  const stack = [{ value: root, path: 'theme', depth: 1 }];
  let nodes = 0;

  while (stack.length > 0) {
    const { value, path, depth } = stack.pop();
    nodes += 1;

    if (nodes > maxNodes) {
      fail('THEME_NODES', `Theme exceeds ${maxNodes} values`, path);
    }
    if (depth > maxDepth) {
      fail('THEME_DEPTH', `Theme exceeds maximum depth ${maxDepth}`, path);
    }

    if (typeof value === 'function') {
      fail('THEME_EXECUTABLE', 'Functions are not allowed in themes', path);
    }
    if (typeof value === 'string') {
      if (value.length > MAX_STRING_LENGTH) {
        fail('THEME_STRING', `String exceeds ${MAX_STRING_LENGTH} characters`, path);
      }
      for (const pattern of EXECUTABLE_VALUE_PATTERNS) {
        if (pattern.test(value)) {
          fail('THEME_EXECUTABLE', 'Executable content is not allowed in themes', path);
        }
      }
      continue;
    }
    if (!value || typeof value !== 'object') continue;

    if (Array.isArray(value)) {
      for (let index = 0; index < value.length; index += 1) {
        stack.push({ value: value[index], path: `${path}[${index}]`, depth: depth + 1 });
      }
      continue;
    }

    for (const [key, child] of Object.entries(value)) {
      const childPath = path ? `${path}.${key}` : key;
      if (FORBIDDEN_KEYS.has(key)) {
        fail('THEME_KEY', `Unsafe key "${key}" is not allowed`, childPath);
      }
      if (EXECUTABLE_KEYS.has(key.toLowerCase()) || /^on[a-z]+$/i.test(key)) {
        fail('THEME_EXECUTABLE', `Executable field "${key}" is not allowed`, childPath);
      }
      stack.push({ value: child, path: childPath, depth: depth + 1 });
    }
  }
}

/* ------------------------------------------------------------------------- */
/* Theme validation                                                           */
/* ------------------------------------------------------------------------- */

function expectString(value, path, { optional = true, max = MAX_STRING_LENGTH } = {}) {
  if (value === undefined || value === null) {
    if (optional) return;
    fail('THEME_STRING', 'Expected a string', path);
  }
  if (typeof value !== 'string') {
    fail('THEME_STRING', 'Expected a string', path);
  }
  if (value.length > max) {
    fail('THEME_STRING', `String exceeds ${max} characters`, path);
  }
}

function expectFiniteNumber(value, path, options = {}) {
  if (value === undefined || value === null) {
    if (options.optional) return null;
    fail('THEME_NUMBER', 'Expected a number', path);
  }
  if (typeof value !== 'number' || !Number.isFinite(value)) {
    fail('THEME_NUMBER', 'Expected a finite number', path);
  }
  if (options.min !== undefined && value < options.min) {
    fail('THEME_NUMBER', `Expected a number >= ${options.min}`, path);
  }
  if (options.max !== undefined && value > options.max) {
    fail('THEME_NUMBER', `Expected a number <= ${options.max}`, path);
  }
  if (options.integer && !Number.isInteger(value)) {
    fail('THEME_NUMBER', 'Expected an integer', path);
  }
  return value;
}

function validateVibe(vibe) {
  if (vibe === undefined || vibe === null) return;
  if (!isPlainObject(vibe)) fail('THEME_VIBE', 'vibe must be an object', 'theme.vibe');

  expectString(vibe.taxonomy, 'theme.vibe.taxonomy');
  expectString(vibe.era, 'theme.vibe.era');
  expectString(vibe.density, 'theme.vibe.density');

  const arrayFields = [
    'keywords',
    'mood',
    'form',
    'material',
    'color',
    'typography',
    'motion',
    'references',
    'ornament',
  ];
  for (const field of arrayFields) {
    const value = vibe[field];
    if (value === undefined) continue;
    if (!Array.isArray(value)) {
      fail('THEME_VIBE', `vibe.${field} must be an array`, `theme.vibe.${field}`);
    }
    if (value.length > 256) {
      fail('THEME_VIBE', `vibe.${field} has too many values`, `theme.vibe.${field}`);
    }
    for (let index = 0; index < value.length; index += 1) {
      expectString(value[index], `theme.vibe.${field}[${index}]`, { optional: false, max: 512 });
    }
  }

  if (vibe.confidence !== undefined) {
    expectFiniteNumber(vibe.confidence, 'theme.vibe.confidence', { min: 0, max: 1 });
  }

  if (vibe.provenance !== undefined) {
    if (!isPlainObject(vibe.provenance)) {
      fail('THEME_VIBE', 'vibe.provenance must be an object', 'theme.vibe.provenance');
    }
    for (const field of ['source_keywords', 'derived_keywords', 'user_keywords', 'methods']) {
      const value = vibe.provenance[field];
      if (value === undefined) continue;
      if (!Array.isArray(value)) {
        fail('THEME_VIBE', `${field} must be an array`, `theme.vibe.provenance.${field}`);
      }
      for (let index = 0; index < value.length; index += 1) {
        expectString(value[index], `theme.vibe.provenance.${field}[${index}]`, {
          optional: false,
          max: 512,
        });
      }
    }
  }
}

function validateCanvas(canvas) {
  if (canvas === undefined || canvas === null) return { ...DEFAULT_CANVAS };
  if (!isPlainObject(canvas)) fail('THEME_CANVAS', 'canvas must be an object', 'theme.canvas');

  const width = canvas.width === undefined
    ? DEFAULT_CANVAS.width
    : expectFiniteNumber(canvas.width, 'theme.canvas.width', {
      min: 1,
      max: MAX_CANVAS_SIZE,
    });
  const height = canvas.height === undefined
    ? DEFAULT_CANVAS.height
    : expectFiniteNumber(canvas.height, 'theme.canvas.height', {
      min: 1,
      max: MAX_CANVAS_SIZE,
    });

  if (canvas.background !== undefined && canvas.background !== null) {
    if (typeof canvas.background !== 'string' && !isPlainObject(canvas.background)) {
      fail('THEME_CANVAS', 'canvas.background must be a token or string', 'theme.canvas.background');
    }
  }
  if (canvas.pixelRatio !== undefined) {
    expectFiniteNumber(canvas.pixelRatio, 'theme.canvas.pixelRatio', {
      optional: true,
      min: 0.1,
      max: 16,
    });
  }

  return { ...canvas, width, height };
}

function validateTokenValueType(type, value, path) {
  if (!TOKEN_TYPES.has(type)) {
    fail('THEME_TOKEN_TYPE', `Unknown token type "${type}"`, path);
  }
  if (value === undefined || value === null) {
    fail('THEME_TOKEN_VALUE', 'Token value is missing', path);
  }

  // A reference, CSS variable, or calc() is resolved later; defer its type
  // check rather than rejecting a valid indirection.
  if (typeof value === 'string' && (
    /\{[^{}]+\}/.test(value)
    || /^\s*var\(/i.test(value)
    || /^\s*calc\(/i.test(value)
  )) {
    return;
  }

  switch (type) {
    case 'color':
      if (typeof value !== 'string' || !value.trim()) {
        fail('THEME_TOKEN_VALUE', 'color token must be a non-empty string', path);
      }
      break;
    case 'dimension':
      if (!((typeof value === 'number' && Number.isFinite(value)) || typeof value === 'string')) {
        fail('THEME_TOKEN_VALUE', 'dimension token must be a number or string', path);
      }
      break;
    case 'fontFamily':
      if (typeof value === 'string') break;
      if (Array.isArray(value) && value.every((item) => typeof item === 'string')) break;
      fail('THEME_TOKEN_VALUE', 'fontFamily token must be a string or string array', path);
      break;
    case 'fontWeight':
      if (typeof value === 'string') break;
      if (typeof value === 'number' && Number.isFinite(value)) break;
      fail('THEME_TOKEN_VALUE', 'fontWeight token must be a number or string', path);
      break;
    case 'duration':
      if (typeof value === 'number' && Number.isFinite(value) && value >= 0) break;
      if (typeof value === 'string') break;
      fail('THEME_TOKEN_VALUE', 'duration token must be a non-negative number or string', path);
      break;
    case 'cubicBezier':
      if (Array.isArray(value) && value.length === 4 && value.every(
        (item) => typeof item === 'number' && Number.isFinite(item),
      )) break;
      fail('THEME_TOKEN_VALUE', 'cubicBezier token must be four numbers', path);
      break;
    case 'number':
      if (typeof value !== 'number' || !Number.isFinite(value)) {
        fail('THEME_TOKEN_VALUE', 'number token must be a finite number', path);
      }
      break;
    case 'boolean':
      if (typeof value !== 'boolean') {
        fail('THEME_TOKEN_VALUE', 'boolean token must be true or false', path);
      }
      break;
    case 'string':
      if (typeof value !== 'string') {
        fail('THEME_TOKEN_VALUE', 'string token must be a string', path);
      }
      break;
    case 'strokeStyle':
    case 'border':
    case 'transition':
    case 'shadow':
    case 'gradient':
    case 'typography':
      if (!(typeof value === 'string' || Array.isArray(value) || isPlainObject(value))) {
        fail('THEME_TOKEN_VALUE', `${type} token has an unsupported value`, path);
      }
      break;
    default:
      break;
  }
}

function walkTokens(node, path, inheritedType, state) {
  if (isTokenObject(node)) {
    state.count += 1;
    if (state.count > state.max) {
      fail('THEME_TOKENS', `Theme exceeds ${state.max} tokens`, path);
    }
    const rawValue = Object.prototype.hasOwnProperty.call(node, '$value')
      ? node.$value
      : node.value;
    if (rawValue === undefined || rawValue === null) {
      fail('THEME_TOKEN_VALUE', 'Token value is missing', `${path}.$value`);
    }
    const type = node.$type ?? node.type ?? inheritedType;
    if (type !== undefined) {
      if (typeof type !== 'string') {
        fail('THEME_TOKEN_TYPE', 'Token type must be a string', `${path}.$type`);
      }
      validateTokenValueType(type, rawValue, `${path}.$value`);
    }
    return;
  }

  if (!isPlainObject(node)) {
    state.count += 1;
    if (state.count > state.max) {
      fail('THEME_TOKENS', `Theme exceeds ${state.max} tokens`, path);
    }
    if (typeof node === 'function' || node === undefined || node === null) {
      fail('THEME_TOKEN_VALUE', 'Token value must be a primitive', path);
    }
    if (inheritedType !== undefined) {
      validateTokenValueType(inheritedType, node, path);
    }
    return;
  }

  const groupType = typeof node.$type === 'string' ? node.$type : inheritedType;
  if (node.$type !== undefined && typeof node.$type !== 'string') {
    fail('THEME_TOKEN_TYPE', 'Group token type must be a string', `${path}.$type`);
  }
  if (typeof node.$type === 'string' && !TOKEN_TYPES.has(node.$type)) {
    fail('THEME_TOKEN_TYPE', `Unknown token type "${node.$type}"`, `${path}.$type`);
  }
  for (const [key, child] of Object.entries(node)) {
    if (key.startsWith('$')) continue;
    walkTokens(child, `${path}.${key}`, groupType, state);
  }
}

function validateTokens(tokens) {
  if (tokens === undefined || tokens === null) return {};
  if (!isPlainObject(tokens)) fail('THEME_TOKENS', 'tokens must be an object', 'theme.tokens');
  walkTokens(tokens, 'theme.tokens', undefined, { count: 0, max: MAX_TOKENS });
  return tokens;
}

function validateBindingValue(value, path) {
  if (typeof value === 'number') {
    if (!Number.isInteger(value) || value < 0) {
      fail('THEME_WIDGET_BINDING', 'Numeric field bindings must be non-negative integers', path);
    }
    return;
  }
  if (typeof value !== 'string') {
    fail('THEME_WIDGET_BINDING', 'Widget metric binding must be a string or field id', path);
  }
  const text = value.trim();
  if (!text) fail('THEME_WIDGET_BINDING', 'Widget metric binding cannot be empty', path);
  if (text.length > 256) {
    fail('THEME_WIDGET_BINDING', 'Widget metric binding is too long', path);
  }
  if (!/^@?[A-Za-z0-9_][A-Za-z0-9_.:@/-]*$/.test(text)) {
    fail('THEME_WIDGET_BINDING', `Invalid widget metric binding "${text}"`, path);
  }
}

function validateWidget(widget, index) {
  const base = `theme.widgets[${index}]`;
  if (!isPlainObject(widget)) fail('THEME_WIDGET', 'Widget must be an object', base);

  const type = widget.type;
  if (typeof type !== 'string' || !type.trim()) {
    fail('THEME_WIDGET', 'Widget type must be a non-empty string', `${base}.type`);
  }
  if (type.length > 64 || !/^[A-Za-z][A-Za-z0-9_-]*$/.test(type)) {
    fail('THEME_WIDGET', `Invalid widget type "${type}"`, `${base}.type`);
  }

  for (const key of BINDING_KEYS) {
    if (widget[key] !== undefined && widget[key] !== null && widget[key] !== '') {
      validateBindingValue(widget[key], `${base}.${key}`);
    }
  }

  const numericFields = ['x', 'y', 'w', 'h', 'min', 'max', 'warn', 'crit', 'value'];
  for (const key of numericFields) {
    if (widget[key] !== undefined) {
      expectFiniteNumber(widget[key], `${base}.${key}`);
    }
  }

  for (const key of ['w', 'h']) {
    if (widget[key] !== undefined) {
      const size = expectFiniteNumber(widget[key], `${base}.${key}`, {
        min: 0,
        max: MAX_CANVAS_SIZE,
      });
      if (size <= 0) {
        fail('THEME_WIDGET_SIZE', 'Widget width and height must be positive', `${base}.${key}`);
      }
    }
  }

  if (widget.min !== undefined && widget.max !== undefined && widget.max <= widget.min) {
    fail('THEME_WIDGET_RANGE', 'Widget max must be greater than min', `${base}.max`);
  }

  for (const key of ['startAngle', 'start_angle', 'endAngle', 'end_angle']) {
    if (widget[key] !== undefined) {
      expectFiniteNumber(widget[key], `${base}.${key}`, { min: -720, max: 720 });
    }
  }

  if (widget.decimals !== undefined) {
    expectFiniteNumber(widget.decimals, `${base}.decimals`, { min: 0, max: 10, integer: true });
  }

  for (const key of ['tickCount', 'tick_count']) {
    if (widget[key] !== undefined) {
      expectFiniteNumber(widget[key], `${base}.${key}`, { min: 0, max: 360, integer: true });
    }
  }

  for (const key of ['label', 'unit', 'title', 'name', 'style', 'mode', 'ariaLabel', 'aria_label']) {
    if (widget[key] !== undefined) {
      expectString(widget[key], `${base}.${key}`, { optional: true, max: 512 });
    }
  }
}

function validateWidgets(widgets) {
  if (widgets === undefined || widgets === null) return [];
  if (!Array.isArray(widgets)) fail('THEME_WIDGETS', 'widgets must be an array', 'theme.widgets');
  if (widgets.length > MAX_WIDGETS) {
    fail('THEME_WIDGETS', `Theme exceeds ${MAX_WIDGETS} widgets`, 'theme.widgets');
  }
  widgets.forEach((widget, index) => validateWidget(widget, index));
  return widgets;
}

/**
 * Validate and normalise a theme object.  Unknown non-executable fields are
 * preserved for forward compatibility; executable fields always fail.
 *
 * @param {object} input
 * @param {object} [options]
 * @returns {object} normalised theme
 * @throws {ThemeValidationError}
 */
export function validateTheme(input, options = {}) {
  if (!isPlainObject(input)) {
    fail('THEME_OBJECT', 'Theme must be a JSON object', 'theme');
  }

  scanStructure(input, options);

  if (input.schema !== THEME_SCHEMA) {
    fail('THEME_SCHEMA', `Expected schema "${THEME_SCHEMA}"`, 'theme.schema');
  }

  expectString(input.id, 'theme.id', { optional: true, max: 256 });
  expectString(input.name, 'theme.name', { optional: true, max: 512 });
  expectString(input.author, 'theme.author', { optional: true, max: 512 });
  expectString(input.license, 'theme.license', { optional: true, max: 512 });
  if (input.id !== undefined && input.id !== null && !input.id.trim()) {
    fail('THEME_ID', 'Theme id cannot be empty', 'theme.id');
  }

  if (input.provenance !== undefined && input.provenance !== null) {
    if (!isPlainObject(input.provenance)) {
      fail('THEME_PROVENANCE', 'provenance must be an object', 'theme.provenance');
    }
    expectString(input.provenance.source, 'theme.provenance.source');
    if (input.provenance.inspired_by !== undefined) {
      if (!Array.isArray(input.provenance.inspired_by)) {
        fail('THEME_PROVENANCE', 'inspired_by must be an array', 'theme.provenance.inspired_by');
      }
      for (let index = 0; index < input.provenance.inspired_by.length; index += 1) {
        expectString(input.provenance.inspired_by[index], `theme.provenance.inspired_by[${index}]`, {
          optional: false,
          max: 512,
        });
      }
    }
  }

  validateVibe(input.vibe);

  if (input.background !== undefined && input.background !== null) {
    if (typeof input.background !== 'string' && !isPlainObject(input.background)) {
      fail('THEME_BACKGROUND', 'background must be a token or string', 'theme.background');
    }
  }

  const canvas = validateCanvas(input.canvas);
  const tokens = validateTokens(input.tokens);
  const widgets = validateWidgets(input.widgets);

  return {
    ...input,
    schema: THEME_SCHEMA,
    canvas,
    tokens,
    widgets,
  };
}

/**
 * Non-throwing validation result.
 *
 * @param {object} input
 * @param {object} [options]
 * @returns {{ok: boolean, theme: object|null, errors: Array, warnings: Array}}
 */
export function checkTheme(input, options = {}) {
  try {
    return { ok: true, theme: validateTheme(input, options), errors: [], warnings: [] };
  } catch (error) {
    return {
      ok: false,
      theme: null,
      errors: [{
        code: error.code || 'THEME_ERROR',
        message: error.message,
        path: error.path || '',
      }],
      warnings: [],
    };
  }
}

/* ------------------------------------------------------------------------- */
/* Token resolution                                                           */
/* ------------------------------------------------------------------------- */

const REFERENCE_RE = /\{([^{}]+)\}/g;

function parseReference(body) {
  let key = body;
  let fallback;
  const pipe = body.indexOf('|');
  const coalesce = body.indexOf('??');
  const comma = body.indexOf(',');
  let split = -1;
  if (pipe !== -1 && (coalesce === -1 || pipe < coalesce)) split = pipe;
  else if (coalesce !== -1) split = coalesce;
  // Support CSS-style `{token, fallback}` without treating commas inside
  // functions such as rgb(0, 0, 0) as separators.
  if (split === -1 && comma !== -1 && !body.slice(0, comma).includes('(')) {
    split = comma;
  }

  if (split !== -1) {
    key = body.slice(0, split);
    const separatorLength = body.startsWith('??', split) ? 2 : 1;
    fallback = body.slice(split + separatorLength).trim();
  }

  key = key.trim();
  return { key, fallback };
}

function lookupToken(tokens, key) {
  if (!isPlainObject(tokens)) return { found: false };
  if (Object.prototype.hasOwnProperty.call(tokens, key)) {
    return { found: true, value: tokens[key] };
  }

  let current = tokens;
  for (const part of key.split('.')) {
    if (!isPlainObject(current) || !Object.prototype.hasOwnProperty.call(current, part)) {
      return { found: false };
    }
    current = current[part];
  }
  return { found: true, value: current };
}

function unwrapTokenValue(value, path) {
  if (isTokenObject(value)) {
    if (Object.prototype.hasOwnProperty.call(value, '$value')) return value.$value;
    return value.value;
  }
  return value;
}

function cssValue(value) {
  if (value === undefined || value === null) return '';
  if (Array.isArray(value)) return value.map((item) => cssValue(item)).join(', ');
  if (isPlainObject(value)) {
    try {
      return JSON.stringify(value);
    } catch {
      return String(value);
    }
  }
  return String(value);
}

function resolveAny(value, tokens, stack, options) {
  if (typeof value === 'string') {
    return resolveString(value, tokens, stack, options);
  }
  if (isTokenObject(value)) {
    return resolveAny(unwrapTokenValue(value), tokens, stack, options);
  }
  if (Array.isArray(value)) {
    return value.map((item) => resolveAny(item, tokens, stack, options));
  }
  if (isPlainObject(value)) {
    const out = {};
    for (const [key, child] of Object.entries(value)) {
      out[key] = resolveAny(child, tokens, stack, options);
    }
    return out;
  }
  return value;
}

function resolveTokenKey(key, tokens, stack, options) {
  const maxDepth = options.maxTokenDepth ?? MAX_TOKEN_DEPTH;
  if (!key) tokenFail('THEME_TOKEN_REF', 'Empty token reference');
  if (key.length > 256) tokenFail('THEME_TOKEN_REF', 'Token reference is too long');
  if (stack.includes(key)) {
    tokenFail('THEME_TOKEN_CYCLE', `Token cycle: ${[...stack, key].join(' -> ')}`, key);
  }
  if (stack.length >= maxDepth) {
    tokenFail('THEME_TOKEN_DEPTH', `Token reference depth exceeds ${maxDepth}`, key);
  }

  const found = lookupToken(tokens, key);
  if (!found.found) {
    tokenFail('THEME_TOKEN_MISSING', `Unknown token "${key}"`, key);
  }
  if (isPlainObject(found.value) && !isTokenObject(found.value)) {
    tokenFail('THEME_TOKEN_VALUE', `Token reference "${key}" points to a group`, key);
  }

  const raw = unwrapTokenValue(found.value, key);
  if (raw === undefined) {
    tokenFail('THEME_TOKEN_VALUE', `Token "${key}" has no value`, key);
  }
  return resolveAny(raw, tokens, [...stack, key], options);
}

function resolveString(text, tokens, stack, options) {
  if (typeof text !== 'string') return text;

  const full = text.match(/^\{([^{}]+)\}$/);
  if (full) {
    const { key, fallback } = parseReference(full[1]);
    try {
      return resolveTokenKey(key, tokens, stack, options);
    } catch (error) {
      if (fallback !== undefined) {
        return resolveString(fallback, tokens, stack, options);
      }
      if (options.strict === false) return text;
      throw error;
    }
  }

  return text.replace(REFERENCE_RE, (match, body) => {
    const { key, fallback } = parseReference(body);
    try {
      return cssValue(resolveTokenKey(key, tokens, stack, options));
    } catch (error) {
      if (fallback !== undefined) {
        return cssValue(resolveString(fallback, tokens, stack, options));
      }
      if (options.strict === false) return match;
      throw error;
    }
  });
}

/**
 * Resolve a token reference or an interpolated string.
 *
 * Supports `{color.accent}` and fallbacks using `{color.accent | #5b6fff}`
 * or `{color.accent ?? #5b6fff}`.  A full-string reference preserves the
 * token's native value (for example a font-family array).
 *
 * @param {*} value
 * @param {object} tokens
 * @param {object} [options] `{ strict, maxTokenDepth }`
 */
export function resolveTokenValue(value, tokens = {}, options = {}) {
  return resolveAny(value, tokens, [], options);
}

/**
 * Flatten a (possibly nested) DTCG token object into `"group.name"` keys.
 * Values are left unresolved.
 */
export function flattenTokens(tokens, prefix = '', out = {}) {
  if (!isPlainObject(tokens)) return out;
  for (const [key, value] of Object.entries(tokens)) {
    if (key.startsWith('$')) continue;
    const path = prefix ? `${prefix}.${key}` : key;
    if (isTokenObject(value)) {
      out[path] = unwrapTokenValue(value, path);
    } else if (isPlainObject(value)) {
      flattenTokens(value, path, out);
    } else {
      out[path] = value;
    }
  }
  return out;
}

/**
 * Resolve every token in a theme into a flat `"group.name"` map.
 */
export function resolveThemeTokens(theme, options = {}) {
  const source = theme && typeof theme === 'object' ? theme : {};
  const looksLikeTheme = Object.prototype.hasOwnProperty.call(source, 'tokens')
    || Object.prototype.hasOwnProperty.call(source, 'schema')
    || Object.prototype.hasOwnProperty.call(source, 'canvas');
  const tokens = looksLikeTheme ? (source.tokens ?? {}) : source;
  const flat = flattenTokens(tokens);
  const resolved = {};
  for (const [key, value] of Object.entries(flat)) {
    resolved[key] = resolveTokenValue(value, tokens, options);
  }
  return resolved;
}

/* ------------------------------------------------------------------------- */
/* CSS variables                                                              */
/* ------------------------------------------------------------------------- */

/**
 * Convert a token key such as `color.accent` or `fontFamily` into a CSS
 * custom-property name.
 */
export function tokenToCssVariable(key) {
  const name = String(key)
    .replace(/([a-z0-9])([A-Z])/g, '$1-$2')
    .replace(/[^A-Za-z0-9_-]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .toLowerCase();
  return `--ozma-${name || 'token'}`;
}

/**
 * Build CSS custom properties for a theme, including aliases that let the
 * existing adapter stylesheet pick up theme colours and fonts.
 */
export function themeToCssVariables(theme, options = {}) {
  const validated = validateTheme(theme, options);
  const tokens = validated.tokens;
  const resolved = resolveThemeTokens(validated, options);
  const variables = {};

  for (const [key, value] of Object.entries(resolved)) {
    variables[tokenToCssVariable(key)] = cssValue(value);
  }

  for (const [key, alias] of Object.entries(CSS_ALIASES)) {
    if (resolved[key] !== undefined) {
      variables[alias] = cssValue(resolved[key]);
    }
  }

  const background = validated.background ?? validated.canvas?.background;
  if (background !== undefined && background !== null) {
    variables['--ozma-canvas-background'] = cssValue(resolveTokenValue(background, tokens, options));
  }
  if (validated.canvas?.width !== undefined) {
    variables['--ozma-canvas-width'] = String(validated.canvas.width);
  }
  if (validated.canvas?.height !== undefined) {
    variables['--ozma-canvas-height'] = String(validated.canvas.height);
  }

  return variables;
}

/**
 * Apply a theme to a document element.  No global mutable state is used; the
 * caller can pass a `Document` (or a compatible test double).
 */
export function applyTheme(theme, doc, options = {}) {
  const documentRef = doc ?? (typeof document !== 'undefined' ? document : null);
  if (!documentRef || !documentRef.documentElement) {
    throw new ThemeError('THEME_DOM', 'A document with documentElement is required');
  }

  const root = documentRef.documentElement;
  const variables = themeToCssVariables(theme, options);

  for (const [name, value] of Object.entries(variables)) {
    if (root.style && typeof root.style.setProperty === 'function') {
      root.style.setProperty(name, value);
    }
  }

  if (root.dataset) {
    root.dataset.ozmaTheme = theme.id || 'custom';
    root.dataset.ozmaThemeSchema = theme.schema;
    if (theme.name) root.dataset.ozmaThemeName = theme.name;
  }

  return variables;
}

/* ------------------------------------------------------------------------- */
/* Loading                                                                    */
/* ------------------------------------------------------------------------- */

/**
 * Parse and validate theme JSON, enforcing the byte limit before parsing.
 */
export function parseTheme(text, options = {}) {
  if (typeof text !== 'string') {
    throw new ThemeError('THEME_JSON', 'Theme source must be a string');
  }
  const maxBytes = options.maxBytes ?? MAX_THEME_BYTES;
  const bytes = new TextEncoder().encode(text).length;
  if (bytes > maxBytes) {
    throw new ThemeValidationError(
      'THEME_SIZE',
      `Theme exceeds ${maxBytes} bytes (${bytes} bytes)`,
    );
  }

  let data;
  try {
    data = JSON.parse(text);
  } catch (error) {
    throw new ThemeError('THEME_JSON', `Invalid theme JSON: ${error.message}`);
  }
  return validateTheme(data, options);
}

/**
 * Fetch and validate a theme URL.
 */
export async function fetchTheme(url, options = {}) {
  const fetchImpl = options.fetchImpl
    ?? (typeof fetch === 'function' ? fetch : null);
  if (typeof fetchImpl !== 'function') {
    throw new ThemeError('THEME_FETCH', 'No fetch implementation available', url);
  }

  let response;
  try {
    response = await fetchImpl(url);
  } catch (error) {
    throw new ThemeError('THEME_FETCH', `Failed to load theme: ${error.message}`, url);
  }
  if (!response || response.ok === false) {
    const status = response?.status ? ` (HTTP ${response.status})` : '';
    throw new ThemeError('THEME_FETCH', `Failed to load theme${status}`, url);
  }

  const text = await response.text();
  try {
    return parseTheme(text, options);
  } catch (error) {
    if (error && !error.path && url) error.path = url;
    throw error;
  }
}

/**
 * Load a theme from an object, JSON string, or URL.
 */
export async function loadTheme(source, options = {}) {
  if (source && typeof source === 'object') {
    return validateTheme(source, options);
  }
  if (typeof source !== 'string') {
    throw new ThemeError('THEME_SOURCE', 'Theme source must be an object, JSON string, or URL');
  }
  const trimmed = source.trim();
  if (trimmed.startsWith('{')) return parseTheme(trimmed, options);
  return fetchTheme(trimmed, options);
}

/**
 * Resolve `?theme=core-dark` or `?theme=<url>` into a fetchable URL.
 * Only HTTP(S) theme URLs are accepted; `javascript:` and `data:` sources
 * are rejected.
 */
export function resolveThemeUrl(value, options = {}) {
  const raw = String(value ?? '').trim();
  if (!raw) throw new ThemeError('THEME_URL', 'Theme id or URL cannot be empty');

  if (/^(?:javascript|vbscript|data):/i.test(raw)) {
    throw new ThemeError('THEME_URL', 'Executable or inline theme URLs are not allowed', raw);
  }

  const baseUrl = options.baseUrl
    ?? (typeof document !== 'undefined' ? document.baseURI : undefined);
  if (!baseUrl) {
    throw new ThemeError('THEME_URL', 'A base URL is required to resolve a theme', raw);
  }

  let url;
  try {
    const builtins = options.builtins ?? BUILTIN_THEMES;
    if (Object.prototype.hasOwnProperty.call(builtins, raw)) {
      url = new URL(builtins[raw], baseUrl);
    } else {
      const looksLikeUrl = /[/\\?#:]/.test(raw) || /\.json$/i.test(raw) || raw.startsWith('.');
      if (!looksLikeUrl) {
        if (!/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(raw)) {
          throw new ThemeError('THEME_URL', `Invalid theme id "${raw}"`, raw);
        }
        const themesPath = options.themesPath ?? 'themes';
        url = new URL(`${themesPath}/${raw}.json`, baseUrl);
      } else {
        url = new URL(raw, baseUrl);
      }
    }
  } catch (error) {
    if (error instanceof ThemeError) throw error;
    throw new ThemeError('THEME_URL', `Invalid theme URL: ${error.message}`, raw);
  }

  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    throw new ThemeError('THEME_URL', 'Only HTTP(S) theme URLs are allowed', url.href);
  }
  return url.href;
}

/**
 * Load the theme requested by a query string, if any.
 *
 * @returns {Promise<null|{theme: object, source: string, id: string}>}
 */
export async function loadThemeFromQuery(search, options = {}) {
  const params = new URLSearchParams(
    typeof search === 'string' ? search.replace(/^\?/, '') : search,
  );
  const requested = params.get('theme');
  if (!requested) return null;

  const source = resolveThemeUrl(requested, options);
  const theme = await fetchTheme(source, options);
  return { theme, source, id: theme.id || requested };
}

/**
 * Browser entry point used by `index.html`.  With no `?theme` parameter it
 * leaves the page untouched, preserving the adapter's default appearance.
 */
export async function bootstrapThemeFromLocation(options = {}) {
  const win = options.window ?? (typeof window !== 'undefined' ? window : null);
  const doc = options.document ?? (typeof document !== 'undefined' ? document : null);
  if (!win || !doc || !win.location) return null;

  const result = await loadThemeFromQuery(win.location.search, {
    ...options,
    baseUrl: options.baseUrl ?? doc.baseURI ?? win.location.href,
    fetchImpl: options.fetchImpl
      ?? (typeof win.fetch === 'function' ? win.fetch.bind(win) : undefined),
  });
  if (!result) return null;

  applyTheme(result.theme, doc, options);
  return result;
}

/* ------------------------------------------------------------------------- */
/* Automatic browser bootstrap                                                */
/* ------------------------------------------------------------------------- */

if (typeof window !== 'undefined' && typeof document !== 'undefined') {
  const bootKey = '__ozmaThemeBooted';
  if (!window[bootKey]) {
    window[bootKey] = true;
    if (new URLSearchParams(window.location.search).get('theme')) {
      bootstrapThemeFromLocation().catch((error) => {
        const code = error?.code || 'THEME_BOOT';
        document.documentElement.dataset.ozmaThemeError = code;
        if (typeof console !== 'undefined' && typeof console.warn === 'function') {
          console.warn(`Ozma theme not applied (${code}): ${error?.message || error}`);
        }
      });
    }
  }
}
