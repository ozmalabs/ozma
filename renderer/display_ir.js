/*
 * SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
 *
 * Semantic display IR adapters for the controller PNG renderer.
 *
 * WGT-043: turn a `ScreenLayout` / `DisplayDefinition` (or a compact Widget IR
 * projection) into a deterministic, target-neutral draw plan.  The same plan
 * feeds the PNG renderer (`renderer/server.js`), a dependency-free SVG preview
 * (`planToSvg`), and LVGL primitives (`planToLvgl`).
 *
 * The module is pure, dependency-free, and never evaluates input.  Sizes,
 * counts, strings, theme data, and operation nesting are bounded so hostile
 * display definitions fail closed instead of exhausting the renderer.
 */

'use strict';

/* ------------------------------------------------------------------------- */
/* Bounds and vocabularies                                                    */
/* ------------------------------------------------------------------------- */

const PLAN_SCHEMA = 'org.ozma.render-plan/1';
const PLAN_VERSION = 1;

const MAX_WIDGETS = 4096;
const MAX_OPS = 100_000;
const MAX_POINTS = 4096;
const MAX_THRESHOLDS = 64;
const MAX_DIMENSION = 16_384;
const MAX_TEXT_LENGTH = 4096;
const MAX_STRING_LENGTH = 8192;
const MAX_THEME_NODES = 20_000;
const MAX_THEME_DEPTH = 16;
const MAX_GROUP_DEPTH = 8;
const MAX_ROWS = 64;

const OP_NAMES = new Set([
  'rect',
  'text',
  'arc',
  'bar',
  'line',
  'polyline',
  'image',
  'icon',
  'group',
]);

const WIDGET_KINDS = new Set([
  'gauge',
  'bar',
  'number',
  'text',
  'sparkline',
  'chart',
  'status',
  'icon',
  'table',
  'card',
  'custom',
]);

const KIND_ALIASES = Object.freeze({
  label: 'text',
  chart: 'sparkline',
  'dual-bar': 'bar',
  vu_meter: 'bar',
  meter: 'bar',
  scenario: 'status',
  clock: 'text',
  image: 'image',
  grid: 'card',
});

const SEVERITY_ALIASES = Object.freeze({
  warn: 'warning',
  warning: 'warning',
  crit: 'critical',
  critical: 'critical',
  error: 'critical',
  danger: 'critical',
  info: 'info',
  nominal: 'info',
  ok: 'ok',
  success: 'ok',
  unknown: 'unknown',
});

const DEFAULT_TOKENS = Object.freeze({
  'color.bg': '#0a0a0f',
  'color.fg': '#e5e7eb',
  'color.text': '#e5e7eb',
  'color.muted': '#8d9bb3',
  'color.dim': '#64728a',
  'color.accent': '#5b6fff',
  'color.ok': '#42d392',
  'color.warn': '#f0b94a',
  'color.crit': '#f26464',
  'color.error': '#f26464',
  'color.track': 'rgba(255, 255, 255, 0.12)',
  'gauge.fill': '{color.accent}',
  'gauge.track': '{color.track}',
  'gauge.warn': '{color.warn}',
  'gauge.crit': '{color.crit}',
  'gauge.text': '{color.fg}',
  'gauge.muted': '{color.muted}',
  'bar.fill': '{color.accent}',
  'bar.track': '{color.track}',
  'radius': '12px',
  'radius.sm': '8px',
});

const DEFAULTS = Object.freeze({
  width: 480,
  height: 480,
  background: '#0a0a0f',
  gaugeStart: 135,
  gaugeSweep: 270,
  tickCount: 5,
  decimals: 1,
  fontSize: 14,
});

/* ------------------------------------------------------------------------- */
/* Errors                                                                     */
/* ------------------------------------------------------------------------- */

class DisplayIrError extends Error {
  constructor(code, message) {
    super(message);
    this.name = 'DisplayIrError';
    this.code = code;
  }
}

function fail(code, message) {
  throw new DisplayIrError(code, message);
}

/* ------------------------------------------------------------------------- */
/* Primitive helpers                                                          */
/* ------------------------------------------------------------------------- */

function isPlainObject(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
  const proto = Object.getPrototypeOf(value);
  return proto === Object.prototype || proto === null;
}

function finiteNumber(value, fallback) {
  const number = typeof value === 'number' ? value : Number(value);
  return Number.isFinite(number) ? number : fallback;
}

function clamp(value, min, max) {
  if (!Number.isFinite(value)) return min;
  if (value < min) return min;
  if (value > max) return max;
  return value;
}

function clampInt(value, min, max, fallback) {
  const number = finiteNumber(value, fallback);
  return Math.round(clamp(number, min, max));
}

/** Stable numeric serialisation: no `-0`, bounded precision. */
function roundNumber(value, places = 3) {
  const number = finiteNumber(value, 0);
  const factor = 10 ** places;
  const rounded = Math.round(number * factor) / factor;
  return Object.is(rounded, -0) ? 0 : rounded;
}

function asString(value, fallback = '', limit = MAX_TEXT_LENGTH) {
  if (typeof value === 'string') {
    return value.length > limit ? value.slice(0, limit) : value;
  }
  if (typeof value === 'number' && Number.isFinite(value)) return String(value);
  if (typeof value === 'boolean') return value ? 'true' : 'false';
  return fallback;
}

function safeString(value, fallback = '', limit = MAX_STRING_LENGTH) {
  if (typeof value !== 'string') return fallback;
  if (value.length > limit) return value.slice(0, limit);
  return value;
}

function escapeXmlText(value) {
  return String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&apos;');
}

function escapeXmlAttr(value) {
  return escapeXmlText(value);
}

function safeResourceUrl(value) {
  const url = safeString(value, '', MAX_STRING_LENGTH).trim();
  if (!url) return '';
  if (/^(?:https?:)?\/\//i.test(url)) return url;
  if (/^data:image\/(?:png|jpeg|jpg|gif|webp);/i.test(url)) return url;
  if (/^[a-z0-9_./-]+$/i.test(url) && !url.includes('..')) return url;
  return '';
}

function safeColor(value, fallback) {
  if (typeof value !== 'string') return fallback;
  const colour = value.trim();
  if (!colour || colour.length > 128) return fallback;
  if (/^#[0-9a-fA-F]{3,8}$/.test(colour)) return colour;
  if (/^(?:rgb|rgba|hsl|hsla)\(\s*[-+0-9.%\s,/]+\)$/i.test(colour)) return colour;
  if (/^[a-zA-Z]{3,20}$/.test(colour)) return colour.toLowerCase();
  if (colour === 'transparent' || colour === 'none') return colour;
  return fallback;
}

function hashString(text) {
  // FNV-1a, only used for deterministic clip identifiers.
  let hash = 0x811c9dc5;
  for (let index = 0; index < text.length; index += 1) {
    hash ^= text.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0).toString(16).padStart(8, '0');
}

/* ------------------------------------------------------------------------- */
/* Theme tokens                                                               */
/* ------------------------------------------------------------------------- */

function tokenEntries(root, prefix, depth, out, state) {
  if (depth > MAX_THEME_DEPTH) {
    fail('THEME_DEPTH', `theme exceeds maximum depth ${MAX_THEME_DEPTH}`);
  }
  for (const key of Object.keys(root)) {
    if (state.nodes >= MAX_THEME_NODES) {
      fail('THEME_NODES', `theme exceeds ${MAX_THEME_NODES} tokens`);
    }
    state.nodes += 1;
    if (key === '__proto__' || key === 'prototype' || key === 'constructor') continue;
    const value = root[key];
    const path = prefix ? `${prefix}.${key}` : key;
    if (isPlainObject(value)) {
      if (Object.prototype.hasOwnProperty.call(value, '$value')) {
        out[path] = value.$value;
      } else if (Object.prototype.hasOwnProperty.call(value, 'value')) {
        out[path] = value.value;
      } else {
        tokenEntries(value, path, depth + 1, out, state);
      }
    } else {
      out[path] = value;
    }
  }
}

function resolveToken(value, tokens, depth = 0) {
  if (typeof value !== 'string') return value;
  const match = /^\{([^{}]{1,128})\}$/.exec(value.trim());
  if (!match) return value;
  if (depth >= 8) return value;
  const key = match[1];
  if (!Object.prototype.hasOwnProperty.call(tokens, key)) return value;
  return resolveToken(tokens[key], tokens, depth + 1);
}

function buildTokens(themeOrOptions) {
  const source = isPlainObject(themeOrOptions) ? themeOrOptions : {};
  const raw = isPlainObject(source.tokens)
    ? source.tokens
    : (isPlainObject(source.theme) && isPlainObject(source.theme.tokens)
      ? source.theme.tokens
      : source);
  const flat = Object.create(null);
  const state = { nodes: 0 };
  tokenEntries(raw, '', 0, flat, state);
  for (const [key, value] of Object.entries(DEFAULT_TOKENS)) {
    if (!Object.prototype.hasOwnProperty.call(flat, key)) flat[key] = value;
  }
  const resolved = Object.create(null);
  for (const key of Object.keys(flat)) {
    resolved[key] = resolveToken(flat[key], flat, 0);
  }
  return resolved;
}

function tokenValue(tokens, keys, fallback) {
  for (const key of keys) {
    if (Object.prototype.hasOwnProperty.call(tokens, key)) {
      const value = tokens[key];
      if (value !== undefined && value !== null && value !== '') return value;
    }
  }
  return fallback;
}

function buildPalette(tokens, widget = {}) {
  const style = isPlainObject(widget.style) ? widget.style : {};
  const styleTokens = isPlainObject(style.tokens) ? style.tokens : {};
  const pick = (explicit, tokenKeys, fallback) => {
    for (const value of explicit) {
      if (typeof value === 'string' && value.trim()) {
        return safeColor(resolveToken(value, tokens), fallback);
      }
    }
    return safeColor(tokenValue(tokens, tokenKeys, fallback), fallback);
  };
  return {
    accent: pick(
      [widget.color, styleTokens.accent, styleTokens.fill],
      ['gauge.fill', 'bar.fill', 'color.accent'],
      '#5b6fff',
    ),
    track: pick(
      [widget.track, widget.track_color, styleTokens.track],
      ['gauge.track', 'bar.track', 'color.track'],
      'rgba(255, 255, 255, 0.12)',
    ),
    text: pick(
      [widget.text_color, styleTokens.text],
      ['gauge.text', 'color.fg', 'color.text'],
      '#e5e7eb',
    ),
    muted: pick(
      [widget.muted_color, styleTokens.muted],
      ['gauge.muted', 'color.muted', 'color.dim'],
      '#8d9bb3',
    ),
    warn: pick(
      [widget.warn_color, styleTokens.warn],
      ['gauge.warn', 'color.warn'],
      '#f0b94a',
    ),
    crit: pick(
      [widget.crit_color, styleTokens.crit],
      ['gauge.crit', 'color.crit', 'color.error'],
      '#f26464',
    ),
    ok: pick(
      [widget.ok_color, styleTokens.ok],
      ['color.ok'],
      '#42d392',
    ),
    surface: pick(
      [widget.background, widget.bg, styleTokens.surface],
      ['color.surface', 'color.panel', 'color.bg'],
      '#111118',
    ),
  };
}

/* ------------------------------------------------------------------------- */
/* Input normalisation                                                        */
/* ------------------------------------------------------------------------- */

function extractLayout(input) {
  if (Array.isArray(input)) {
    return { layout: { widgets: input }, sourceFormat: 'ozma_screen_layout' };
  }
  if (!isPlainObject(input)) {
    fail('INVALID_LAYOUT', 'layout must be an object with a widgets array');
  }
  if (isPlainObject(input.layout) && Array.isArray(input.layout.widgets)) {
    return { layout: input.layout, sourceFormat: 'ozma_display_definition' };
  }
  if (Array.isArray(input.widgets)) {
    return { layout: input, sourceFormat: 'ozma_screen_layout' };
  }
  if (isPlainObject(input.ir)) {
    return { layout: { widgets: [irToScreenWidget(input.ir)] }, sourceFormat: 'org.ozma.widget-ir' };
  }
  if (typeof input.kind === 'string' && isPlainObject(input.binding)) {
    return { layout: { widgets: [irToScreenWidget(input)] }, sourceFormat: 'org.ozma.widget-ir' };
  }
  if (isPlainObject(input.layout) && typeof input.layout.kind === 'string') {
    return { layout: { widgets: [irToScreenWidget(input.layout)] }, sourceFormat: 'org.ozma.widget-ir' };
  }
  fail('INVALID_LAYOUT', 'layout must contain a widgets array');
  return null;
}

function irToScreenWidget(ir) {
  const binding = isPlainObject(ir.binding) ? ir.binding : {};
  const layout = isPlainObject(ir.layout) ? ir.layout : {};
  const widget = {
    type: ir.kind || 'card',
    ir_kind: ir.kind,
    ir_id: ir.id,
    x: layout.x,
    y: layout.y,
    w: layout.width ?? layout.w,
    h: layout.height ?? layout.h,
    metric: binding.metric,
    unit: binding.unit,
    format: binding.format,
    label: ir.label,
  };
  if (isPlainObject(ir.domain)) {
    if (ir.domain.min !== undefined) widget.min = ir.domain.min;
    if (ir.domain.max !== undefined) widget.max = ir.domain.max;
    if (Array.isArray(ir.domain.thresholds)) widget.thresholds = ir.domain.thresholds;
  }
  if (isPlainObject(ir.typography) && ir.typography.size_px !== undefined) {
    widget.font_size = ir.typography.size_px;
  }
  if (isPlainObject(ir.icon)) {
    if (ir.icon.name) widget.icon = ir.icon.name;
    if (ir.icon.asset_id) widget.icon_asset = ir.icon.asset_id;
  }
  if (Array.isArray(ir.history) && ir.history.length > 0) widget.values = ir.history;
  return widget;
}

function normaliseKind(widget) {
  const raw = asString(widget.ir_kind || widget.kind || widget.type, 'card', 64).toLowerCase();
  if (WIDGET_KINDS.has(raw)) return raw;
  if (Object.prototype.hasOwnProperty.call(KIND_ALIASES, raw)) return KIND_ALIASES[raw];
  return 'card';
}

function normaliseWidget(widget, index, layout, warnings) {
  if (!isPlainObject(widget)) {
    fail('INVALID_WIDGET', `widgets[${index}] must be an object`);
  }
  const kind = normaliseKind(widget);
  const rawType = asString(widget.ir_kind || widget.kind || widget.type, kind, 64);
  if (kind === 'card' && !['card', 'custom', 'table', 'grid'].includes(rawType.toLowerCase())
      && !Object.prototype.hasOwnProperty.call(KIND_ALIASES, rawType.toLowerCase())) {
    warnings.push(`widget "${rawType}" rendered as a card fallback`);
  }

  const bounds = {
    x: roundNumber(clamp(finiteNumber(widget.x, 0), -MAX_DIMENSION, MAX_DIMENSION)),
    y: roundNumber(clamp(finiteNumber(widget.y, 0), -MAX_DIMENSION, MAX_DIMENSION)),
    w: roundNumber(clamp(finiteNumber(widget.w ?? widget.width, 120), 1, MAX_DIMENSION)),
    h: roundNumber(clamp(finiteNumber(widget.h ?? widget.height, 120), 1, MAX_DIMENSION)),
  };

  const domain = isPlainObject(widget.domain) ? widget.domain : {};
  const min = finiteNumber(widget.min ?? domain.min, 0);
  const max = finiteNumber(widget.max ?? domain.max, 100);
  if (max < min) {
    // A reversed range is malformed; normalise it instead of rendering
    // nonsensical geometry and record the coercion.
    warnings.push(`widget[${index}] has max < min; range swapped`);
  }
  const domainMin = Math.min(min, max);
  const domainMax = Math.max(min, max);

  return {
    index,
    kind,
    id: asString(widget.ir_id || widget.id, `w${index}`, 128),
    widget,
    bounds,
    min: roundNumber(domainMin),
    max: roundNumber(domainMax),
    label: asString(widget.label, '', MAX_TEXT_LENGTH),
    unit: asString(widget.unit ?? (isPlainObject(widget.binding) ? widget.binding.unit : ''), '', 32),
    decimals: resolveDecimals(widget),
    metric: asString(
      widget.metric ?? widget.field ?? (isPlainObject(widget.binding) ? widget.binding.metric : ''),
      '',
      256,
    ),
    layout,
  };
}

function resolveDecimals(widget) {
  if (widget.decimals !== undefined) {
    return clampInt(widget.decimals, 0, 8, DEFAULTS.decimals);
  }
  const format = widget.format ?? (isPlainObject(widget.binding) ? widget.binding.format : '');
  if (typeof format === 'string') {
    const fixed = /^fixed\((\d{1,2})\)$/.exec(format.trim());
    if (fixed) return clampInt(Number(fixed[1]), 0, 8, DEFAULTS.decimals);
    if (format.trim() === 'integer' || format.trim() === 'int') return 0;
  }
  return DEFAULTS.decimals;
}

/* ------------------------------------------------------------------------- */
/* Thresholds and formatting                                                  */
/* ------------------------------------------------------------------------- */

function normaliseSeverity(value) {
  const key = asString(value, 'info', 32).toLowerCase();
  return SEVERITY_ALIASES[key] || key || 'info';
}

function addThreshold(list, at, severity, color, label, palette) {
  const value = finiteNumber(at, NaN);
  if (!Number.isFinite(value)) return;
  if (list.length >= MAX_THRESHOLDS) return;
  const name = normaliseSeverity(severity);
  let fallback = palette.accent;
  if (name === 'warning' || name === 'warn') fallback = palette.warn;
  if (name === 'critical' || name === 'crit' || name === 'error') fallback = palette.crit;
  if (name === 'ok' || name === 'success') fallback = palette.ok;
  list.push({
    at: roundNumber(value),
    severity: name,
    color: safeColor(color, fallback),
    label: asString(label, '', 128),
  });
}

function collectThresholds(widget, palette) {
  const thresholds = [];
  const domain = isPlainObject(widget.domain) ? widget.domain : {};

  if (Array.isArray(domain.thresholds)) {
    for (const item of domain.thresholds) {
      if (!isPlainObject(item)) continue;
      addThreshold(thresholds, item.at, item.severity, item.color, item.label, palette);
    }
  }
  if (Array.isArray(domain.severity_bands)) {
    for (const band of domain.severity_bands) {
      if (!isPlainObject(band) || band.min === undefined) continue;
      addThreshold(
        thresholds,
        band.min,
        band.severity,
        undefined,
        band.label,
        palette,
      );
    }
  }
  if (Array.isArray(widget.thresholds)) {
    for (const item of widget.thresholds) {
      if (!isPlainObject(item)) continue;
      addThreshold(thresholds, item.at, item.severity, item.color, item.label, palette);
    }
  } else if (isPlainObject(widget.thresholds)) {
    for (const severity of Object.keys(widget.thresholds)) {
      addThreshold(
        thresholds,
        widget.thresholds[severity],
        severity,
        undefined,
        undefined,
        palette,
      );
    }
  }

  // The browser/widget contract uses warn/crit point thresholds.
  for (const [key, severity, color] of [
    ['warn', 'warning', palette.warn],
    ['warn_at', 'warning', palette.warn],
    ['warnAt', 'warning', palette.warn],
    ['crit', 'critical', palette.crit],
    ['critical', 'critical', palette.crit],
    ['crit_at', 'critical', palette.crit],
    ['critAt', 'critical', palette.crit],
  ]) {
    if (widget[key] !== undefined) {
      const explicit = severity === 'critical'
        ? safeColor(widget.crit_color, color)
        : safeColor(widget.warn_color, color);
      addThreshold(thresholds, widget[key], severity, explicit, undefined, palette);
    }
  }

  thresholds.sort((left, right) => (
    left.at - right.at
    || left.severity.localeCompare(right.severity)
    || left.label.localeCompare(right.label)
  ));
  return thresholds;
}

function severityFor(value, thresholds, palette) {
  let severity = 'normal';
  let color = palette.accent;
  if (!Number.isFinite(value)) return { severity, color };
  for (const threshold of thresholds) {
    if (value >= threshold.at) {
      severity = threshold.severity;
      color = threshold.color;
    }
  }
  return { severity, color };
}

function displayUnit(unit) {
  const text = asString(unit, '', 32);
  if (!text) return '';
  const lowered = text.toLowerCase();
  if (lowered === 'celsius' || lowered === 'degc') return '°C';
  if (lowered === 'fahrenheit' || lowered === 'degf') return '°F';
  if (lowered === 'percent') return '%';
  if (['%', '°', '°c', '°f'].includes(lowered)) return text;
  return ` ${text}`;
}

function coerceDisplayValue(value, decimals) {
  if (value === undefined || value === null) return '—';
  if (typeof value === 'boolean') return value ? 'On' : 'Off';
  if (typeof value === 'number') {
    if (!Number.isFinite(value)) return '—';
    return value.toFixed(decimals);
  }
  if (typeof value === 'string') {
    const number = Number(value);
    if (value.trim() !== '' && Number.isFinite(number)) return number.toFixed(decimals);
    return asString(value, '', MAX_TEXT_LENGTH);
  }
  if (Array.isArray(value)) return coerceDisplayValue(value[value.length - 1], decimals);
  if (isPlainObject(value) && value.value !== undefined) {
    return coerceDisplayValue(value.value, decimals);
  }
  return '—';
}

function formatValue(value, descriptor) {
  if (descriptor.mode === 'text' && typeof value !== 'number') {
    return asString(value, '—', MAX_TEXT_LENGTH);
  }
  const decimals = clampInt(descriptor.decimals, 0, 8, DEFAULTS.decimals);
  const body = coerceDisplayValue(value, decimals);
  const unit = displayUnit(descriptor.unit);
  return `${body}${unit}`;
}

function readValue(values, metric) {
  if (!isPlainObject(values) || !metric) return undefined;
  if (Object.prototype.hasOwnProperty.call(values, metric)) return values[metric];

  const candidates = [metric];
  if (metric.startsWith('@')) candidates.push(metric.slice(1));
  const parts = metric.split(/[./]/).filter(Boolean);
  if (parts.length > 1) {
    candidates.push(parts[parts.length - 1]);
    candidates.push(parts.slice(-2).join('.'));
  }

  for (const candidate of candidates) {
    if (Object.prototype.hasOwnProperty.call(values, candidate)) return values[candidate];
  }

  // Bounded dotted-path walk, own properties only.
  const path = metric.replace(/^@/, '').split(/[./]/).filter(Boolean);
  if (path.length > 1) {
    let cursor = values;
    let ok = true;
    for (const part of path.slice(0, 8)) {
      if (part === '__proto__' || part === 'constructor' || part === 'prototype') {
        ok = false;
        break;
      }
      if (!isPlainObject(cursor) || !Object.prototype.hasOwnProperty.call(cursor, part)) {
        ok = false;
        break;
      }
      cursor = cursor[part];
    }
    if (ok) return cursor;
  }
  return undefined;
}

function interpolateTemplate(template, values, fallbackMetric) {
  const text = asString(template, '', MAX_TEXT_LENGTH);
  if (!text) return '';
  let replaced = 0;
  const output = text.replace(/\{([^{}]{1,128})\}/g, (match, key) => {
    if (replaced >= 32) return match;
    replaced += 1;
    const value = readValue(values, key);
    if (value === undefined) {
      if (key === fallbackMetric) return match;
      return '';
    }
    return coerceDisplayValue(value, DEFAULTS.decimals);
  });
  return output;
}

/* ------------------------------------------------------------------------- */
/* Operation factories                                                        */
/* ------------------------------------------------------------------------- */

function baseOp(op, id, bounds) {
  return {
    op,
    id,
    x: bounds.x,
    y: bounds.y,
    w: bounds.w,
    h: bounds.h,
  };
}

function textOp(id, x, y, text, descriptor = {}) {
  const op = {
    op: 'text',
    id,
    x: roundNumber(x),
    y: roundNumber(y),
    text: asString(text, '', MAX_TEXT_LENGTH),
    fill: safeColor(descriptor.fill, '#e5e7eb'),
    fontSize: roundNumber(clampInt(descriptor.fontSize, 4, 512, DEFAULTS.fontSize)),
    fontFamily: asString(descriptor.fontFamily, 'sans-serif', 128),
    fontWeight: descriptor.fontWeight === 'bold' ? 'bold' : 'normal',
    align: ['left', 'center', 'right'].includes(descriptor.align) ? descriptor.align : 'left',
    baseline: ['top', 'middle', 'bottom', 'alphabetic'].includes(descriptor.baseline)
      ? descriptor.baseline
      : 'alphabetic',
  };
  if (descriptor.maxWidth !== undefined) op.maxWidth = roundNumber(Math.max(1, descriptor.maxWidth));
  if (descriptor.binding) op.binding = descriptor.binding;
  if (descriptor.role) op.role = descriptor.role;
  if (descriptor.template) op.template = asString(descriptor.template, '', MAX_TEXT_LENGTH);
  return op;
}

function rectOp(id, x, y, w, h, descriptor = {}) {
  const op = {
    op: 'rect',
    id,
    x: roundNumber(x),
    y: roundNumber(y),
    w: roundNumber(Math.max(0, w)),
    h: roundNumber(Math.max(0, h)),
  };
  if (descriptor.fill !== undefined) op.fill = safeColor(descriptor.fill, '#000000');
  if (descriptor.stroke !== undefined) op.stroke = safeColor(descriptor.stroke, '#000000');
  if (descriptor.lineWidth !== undefined) op.lineWidth = roundNumber(Math.max(0, descriptor.lineWidth));
  if (descriptor.radius !== undefined) op.radius = roundNumber(Math.max(0, descriptor.radius));
  if (descriptor.opacity !== undefined) op.opacity = roundNumber(clamp(descriptor.opacity, 0, 1));
  return op;
}

function lineOp(id, x1, y1, x2, y2, descriptor = {}) {
  return {
    op: 'line',
    id,
    x1: roundNumber(x1),
    y1: roundNumber(y1),
    x2: roundNumber(x2),
    y2: roundNumber(y2),
    stroke: safeColor(descriptor.stroke, '#e5e7eb'),
    lineWidth: roundNumber(clamp(finiteNumber(descriptor.lineWidth, 1), 0.1, 64)),
    cap: descriptor.cap === 'round' ? 'round' : 'butt',
  };
}

function arcOp(id, cx, cy, radius, startAngle, endAngle, descriptor = {}) {
  const op = {
    op: 'arc',
    id,
    cx: roundNumber(cx),
    cy: roundNumber(cy),
    r: roundNumber(Math.max(0.1, radius)),
    startAngle: roundNumber(startAngle),
    endAngle: roundNumber(endAngle),
  };
  if (descriptor.fill !== undefined) op.fill = safeColor(descriptor.fill, '#000000');
  if (descriptor.stroke !== undefined) op.stroke = safeColor(descriptor.stroke, '#000000');
  op.lineWidth = roundNumber(clamp(finiteNumber(descriptor.lineWidth, 2), 0, 512));
  if (descriptor.cap === 'round') op.cap = 'round';
  if (descriptor.binding) op.binding = descriptor.binding;
  if (descriptor.role) op.role = descriptor.role;
  if (descriptor.angleStart !== undefined) op.angleStart = roundNumber(descriptor.angleStart);
  if (descriptor.angleSweep !== undefined) op.angleSweep = roundNumber(descriptor.angleSweep);
  if (descriptor.thresholds) op.thresholds = descriptor.thresholds;
  if (descriptor.colors) op.colors = descriptor.colors;
  if (descriptor.value !== undefined) op.value = roundNumber(descriptor.value);
  if (descriptor.showFill !== undefined) op.showFill = Boolean(descriptor.showFill);
  return op;
}

function polylineOp(id, points, descriptor = {}) {
  const bounded = [];
  for (const point of Array.isArray(points) ? points : []) {
    if (bounded.length >= MAX_POINTS) break;
    if (!Array.isArray(point) || point.length < 2) continue;
    const x = finiteNumber(point[0], NaN);
    const y = finiteNumber(point[1], NaN);
    if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
    bounded.push([roundNumber(x), roundNumber(y)]);
  }
  const op = {
    op: 'polyline',
    id,
    points: bounded,
    stroke: safeColor(descriptor.stroke, '#5b6fff'),
    lineWidth: roundNumber(clamp(finiteNumber(descriptor.lineWidth, 1.5), 0.1, 64)),
    close: Boolean(descriptor.close),
  };
  if (descriptor.fill !== undefined) op.fill = safeColor(descriptor.fill, '#5b6fff');
  if (descriptor.binding) op.binding = descriptor.binding;
  if (descriptor.role) op.role = descriptor.role;
  if (descriptor.geometry) op.geometry = descriptor.geometry;
  return op;
}

/* ------------------------------------------------------------------------- */
/* Widget adapters                                                            */
/* ------------------------------------------------------------------------- */

function clipFor(bounds, width, height) {
  const left = Math.max(0, bounds.x);
  const top = Math.max(0, bounds.y);
  const right = Math.min(width, bounds.x + bounds.w);
  const bottom = Math.min(height, bounds.y + bounds.h);
  if (right <= left || bottom <= top) return null;
  return {
    x: roundNumber(left),
    y: roundNumber(top),
    w: roundNumber(right - left),
    h: roundNumber(bottom - top),
  };
}

function bindingFor(widget, normalized, palette, thresholds, mode = 'number') {
  return {
    metric: normalized.metric,
    decimals: normalized.decimals,
    unit: normalized.unit,
    format: asString(widget.format, '', 64),
    mode,
    min: normalized.min,
    max: normalized.max,
    thresholds,
    colors: {
      accent: palette.accent,
      warn: palette.warn,
      crit: palette.crit,
      ok: palette.ok,
      text: palette.text,
    },
  };
}

function renderGauge(widget, normalized, options) {
  const { bounds, min, max } = normalized;
  const palette = buildPalette(options.tokens, widget);
  const thresholds = collectThresholds(widget, palette);
  const shape = isPlainObject(widget.shape) ? widget.shape : {};
  const startAngle = finiteNumber(
    widget.arc_start_deg ?? widget.start_angle ?? shape.arc_start_deg,
    DEFAULTS.gaugeStart,
  );
  let endAngle = finiteNumber(
    widget.arc_end_deg ?? widget.end_angle ?? shape.arc_end_deg,
    startAngle + DEFAULTS.gaugeSweep,
  );
  let sweep = endAngle - startAngle;
  if (!Number.isFinite(sweep) || sweep <= 0) {
    endAngle = startAngle + DEFAULTS.gaugeSweep;
    sweep = DEFAULTS.gaugeSweep;
  }
  if (sweep > 360) {
    endAngle = startAngle + 360;
    sweep = 360;
  }

  const pad = Math.max(2, Math.min(bounds.w, bounds.h) * 0.06);
  const thickness = clamp(
    finiteNumber(widget.thickness, Math.max(2, Math.min(bounds.w, bounds.h) * 0.09)),
    1,
    Math.max(1, Math.min(bounds.w, bounds.h) / 2),
  );
  const radius = Math.max(1, Math.min(bounds.w, bounds.h) / 2 - pad - thickness / 2);
  const cx = bounds.x + bounds.w / 2;
  const cy = bounds.y + bounds.h / 2;
  const value = clamp(finiteNumber(readValue(options.data, normalized.metric), min), min, max);
  const pct = max > min ? (value - min) / (max - min) : 0;
  const severity = severityFor(value, thresholds, palette);
  const valueEnd = startAngle + sweep * pct;

  const children = [];
  children.push(arcOp(
    `${normalized.id}.track`,
    cx,
    cy,
    radius,
    startAngle,
    endAngle,
    { stroke: palette.track, lineWidth: thickness, cap: 'round' },
  ));
  children.push(arcOp(
    `${normalized.id}.value`,
    cx,
    cy,
    radius,
    startAngle,
    valueEnd,
    {
      stroke: severity.color,
      lineWidth: thickness,
      cap: 'round',
      role: 'gauge-value',
      binding: bindingFor(widget, normalized, palette, thresholds),
      angleStart: startAngle,
      angleSweep: sweep,
      thresholds,
      value,
      colors: { accent: palette.accent, warn: palette.warn, crit: palette.crit, ok: palette.ok },
    },
  ));

  const tickCount = clampInt(
    widget.tick_count ?? widget.tickCount ?? shape.tick_count,
    0,
    24,
    DEFAULTS.tickCount,
  );
  for (let index = 0; index < tickCount; index += 1) {
    const fraction = tickCount <= 1 ? 0.5 : index / (tickCount - 1);
    const angle = ((startAngle + sweep * fraction) * Math.PI) / 180;
    const inner = radius - thickness / 2;
    const outer = radius + thickness / 2;
    children.push(lineOp(
      `${normalized.id}.tick${index}`,
      cx + Math.cos(angle) * inner,
      cy + Math.sin(angle) * inner,
      cx + Math.cos(angle) * outer,
      cy + Math.sin(angle) * outer,
      { stroke: palette.muted, lineWidth: Math.max(1, thickness * 0.08) },
    ));
  }

  const valueText = formatValue(value, {
    decimals: normalized.decimals,
    unit: normalized.unit,
  });
  const fontValue = clamp(
    finiteNumber(widget.font_size, Math.min(bounds.w, bounds.h) * 0.22),
    6,
    256,
  );
  const showValue = widget.showValue !== false && widget.show_value !== false;
  const showLabel = widget.showLabel !== false && widget.show_label !== false;

  if (showValue) {
    children.push(textOp(
      `${normalized.id}.value-text`,
      cx,
      cy + fontValue * 0.12,
      valueText,
      {
        fill: severity.color,
        fontSize: fontValue,
        align: 'center',
        baseline: 'middle',
        fontWeight: 'bold',
        role: 'value-text',
        binding: bindingFor(widget, normalized, palette, thresholds),
      },
    ));
  }
  if (showLabel && normalized.label) {
    children.push(textOp(
      `${normalized.id}.label`,
      cx,
      cy + radius * 0.62,
      normalized.label,
      {
        fill: palette.muted,
        fontSize: Math.max(6, fontValue * 0.34),
        align: 'center',
        baseline: 'middle',
      },
    ));
  }

  return children;
}

function renderBar(widget, normalized, options) {
  const { bounds, min, max } = normalized;
  const palette = buildPalette(options.tokens, widget);
  const thresholds = collectThresholds(widget, palette);
  const value = clamp(finiteNumber(readValue(options.data, normalized.metric), min), min, max);
  const severity = severityFor(value, thresholds, palette);
  const orientation = widget.orientation === 'vertical' ? 'vertical' : 'horizontal';
  const fontSize = clampInt(widget.font_size, 6, 128, 12);

  const op = {
    op: 'bar',
    id: normalized.id,
    x: bounds.x,
    y: bounds.y,
    w: bounds.w,
    h: bounds.h,
    value: roundNumber(value),
    min: roundNumber(min),
    max: roundNumber(max),
    orientation,
    track: palette.track,
    fill: severity.color,
    radius: roundNumber(clamp(finiteNumber(widget.radius, 3), 0, Math.min(bounds.w, bounds.h) / 2)),
    role: 'bar',
    binding: bindingFor(widget, normalized, palette, thresholds),
    thresholds,
    colors: { accent: palette.accent, warn: palette.warn, crit: palette.crit, ok: palette.ok },
  };

  const children = [op];
  const valueText = formatValue(value, {
    decimals: normalized.decimals,
    unit: normalized.unit,
  });
  const textY = bounds.y + Math.min(fontSize * 0.9, bounds.h * 0.5);
  if (normalized.label) {
    children.push(textOp(
      `${normalized.id}.label`,
      bounds.x + 2,
      textY,
      normalized.label,
      {
        fill: palette.muted,
        fontSize,
        align: 'left',
        baseline: 'middle',
      },
    ));
  }
  children.push(textOp(
    `${normalized.id}.value-text`,
    bounds.x + bounds.w - 2,
    textY,
    valueText,
    {
      fill: palette.text,
      fontSize,
      align: 'right',
      baseline: 'middle',
      role: 'value-text',
      binding: bindingFor(widget, normalized, palette, thresholds),
    },
  ));
  return children;
}

function renderNumber(widget, normalized, options) {
  const { bounds } = normalized;
  const palette = buildPalette(options.tokens, widget);
  const thresholds = collectThresholds(widget, palette);
  const raw = readValue(options.data, normalized.metric);
  const value = raw === undefined ? clamp(normalized.min, normalized.min, normalized.max) : raw;
  const numeric = typeof value === 'number' ? clamp(value, normalized.min, normalized.max) : value;
  const severity = severityFor(typeof numeric === 'number' ? numeric : NaN, thresholds, palette);
  const fontSize = clampInt(
    widget.font_size,
    6,
    512,
    Math.max(12, Math.min(bounds.w / 6, bounds.h * 0.45)),
  );
  const binding = bindingFor(widget, normalized, palette, thresholds);
  const display = formatValue(numeric, {
    decimals: normalized.decimals,
    unit: normalized.unit,
  });

  const children = [
    textOp(`${normalized.id}.value-text`, bounds.x + bounds.w / 2, bounds.y + bounds.h * 0.5, display, {
      fill: severity.color,
      fontSize,
      align: 'center',
      baseline: 'middle',
      fontWeight: 'bold',
      role: 'value-text',
      binding,
    }),
  ];
  if (normalized.label) {
    children.push(textOp(
      `${normalized.id}.label`,
      bounds.x + bounds.w / 2,
      bounds.y + bounds.h * 0.82,
      normalized.label,
      {
        fill: palette.muted,
        fontSize: Math.max(6, fontSize * 0.32),
        align: 'center',
        baseline: 'middle',
      },
    ));
  }
  return children;
}

function renderText(widget, normalized, options) {
  const { bounds } = normalized;
  const palette = buildPalette(options.tokens, widget);
  const fontSize = clampInt(widget.font_size, 4, 512, Math.max(8, Math.min(bounds.h * 0.7, 32)));
  const template = asString(widget.text ?? widget.template ?? normalized.label, '', MAX_TEXT_LENGTH);
  const hasTemplate = /\{[^{}]{1,128}\}/.test(template);
  const metric = normalized.metric || (hasTemplate
    ? (/\{([^{}]{1,128})\}/.exec(template) || [])[1]
    : '');
  let text = template;
  if (hasTemplate && options.data) text = interpolateTemplate(template, options.data, metric);
  if (!text && normalized.metric) {
    text = formatValue(readValue(options.data, normalized.metric), {
      decimals: normalized.decimals,
      unit: normalized.unit,
    });
  }
  const descriptor = {
    fill: palette.text,
    fontSize,
    align: ['left', 'center', 'right'].includes(widget.align) ? widget.align : 'left',
    baseline: 'middle',
    maxWidth: bounds.w,
  };
  if (hasTemplate || metric) {
    descriptor.role = 'template-text';
    descriptor.binding = {
      ...bindingFor(widget, { ...normalized, metric }, palette, []),
      mode: 'text',
      template,
      metric,
    };
  }
  return [textOp(`${normalized.id}.text`, bounds.x + 2, bounds.y + bounds.h / 2, text, descriptor)];
}

function sparklineValues(widget, normalized, options) {
  let values = widget.values ?? widget.data ?? widget.points;
  if (values === undefined && isPlainObject(widget.history)) values = widget.history.values;
  if (values === undefined) values = readValue(options.data, normalized.metric);
  if (isPlainObject(values) && Array.isArray(values.values)) values = values.values;
  if (!Array.isArray(values)) return [];
  const budget = options.pointBudget;
  const output = [];
  for (const value of values) {
    if (output.length >= MAX_POINTS) break;
    if (budget) {
      budget.count += 1;
      if (budget.count > MAX_POINTS * 16) {
        fail('TOO_MANY_POINTS', `plan exceeds ${MAX_POINTS * 16} history points`);
      }
    }
    const number = finiteNumber(value, NaN);
    if (Number.isFinite(number)) output.push(number);
  }
  return output;
}

function sparklinePoints(values, bounds, min, max) {
  if (values.length === 0) return [];
  const span = max > min ? max - min : 1;
  const step = values.length > 1 ? bounds.w / (values.length - 1) : 0;
  const points = [];
  for (let index = 0; index < values.length; index += 1) {
    const x = bounds.x + (values.length > 1 ? index * step : bounds.w / 2);
    const y = bounds.y + bounds.h - ((values[index] - min) / span) * bounds.h;
    points.push([roundNumber(x), roundNumber(clamp(y, bounds.y, bounds.y + bounds.h))]);
  }
  return points;
}

function renderSparkline(widget, normalized, options) {
  const { bounds } = normalized;
  const palette = buildPalette(options.tokens, widget);
  const thresholds = collectThresholds(widget, palette);
  const values = sparklineValues(widget, normalized, options);
  let min = finiteNumber(widget.min, NaN);
  let max = finiteNumber(widget.max, NaN);
  if (!Number.isFinite(min) && values.length > 0) min = Math.min(...values);
  if (!Number.isFinite(max) && values.length > 0) max = Math.max(...values);
  min = Number.isFinite(min) ? min : 0;
  max = Number.isFinite(max) && max > min ? max : min + 1;

  const points = sparklinePoints(values, bounds, min, max);
  const last = values.length > 0 ? values[values.length - 1] : undefined;
  const severity = severityFor(last, thresholds, palette);
  const fontSize = clampInt(widget.font_size, 6, 128, 11);
  const children = [
    rectOp(`${normalized.id}.surface`, bounds.x, bounds.y, bounds.w, bounds.h, {
      fill: palette.surface,
      radius: 2,
      opacity: 0.6,
    }),
    polylineOp(`${normalized.id}.line`, points, {
      stroke: severity.color,
      lineWidth: Math.max(1, finiteNumber(widget.line_width, 1.5)),
      fill: widget.fill === true ? severity.color : undefined,
      close: widget.fill === true && points.length > 2,
      role: 'sparkline',
      binding: bindingFor(widget, normalized, palette, thresholds),
      geometry: {
        x: bounds.x,
        y: bounds.y,
        w: bounds.w,
        h: bounds.h,
        min,
        max,
        fill: widget.fill === true,
        close: widget.fill === true && points.length > 2,
      },
    }),
  ];
  const labelY = bounds.y + bounds.h - Math.min(fontSize, bounds.h * 0.3);
  if (normalized.label) {
    children.push(textOp(
      `${normalized.id}.label`,
      bounds.x + 2,
      labelY,
      normalized.label,
      { fill: palette.muted, fontSize, align: 'left', baseline: 'bottom' },
    ));
  }
  if (last !== undefined) {
    children.push(textOp(
      `${normalized.id}.value-text`,
      bounds.x + bounds.w - 2,
      labelY,
      formatValue(last, { decimals: normalized.decimals, unit: normalized.unit }),
      {
        fill: severity.color,
        fontSize,
        align: 'right',
        baseline: 'bottom',
        role: 'value-text',
        binding: bindingFor(widget, normalized, palette, thresholds),
      },
    ));
  }
  return children;
}

function renderStatus(widget, normalized, options) {
  const { bounds } = normalized;
  const palette = buildPalette(options.tokens, widget);
  const thresholds = collectThresholds(widget, palette);
  const raw = readValue(options.data, normalized.metric);
  const offline = raw === false || raw === 0 || raw === 'offline' || raw === 'down';
  const dotColor = offline ? palette.crit : (raw === undefined ? palette.muted : palette.ok);
  const dotRadius = Math.max(2, Math.min(bounds.w, bounds.h) * 0.18);
  const fontSize = clampInt(widget.font_size, 6, 128, Math.max(8, Math.min(bounds.h * 0.4, 18)));
  const text = widget.text
    ? interpolateTemplate(widget.text, options.data || {}, normalized.metric)
    : (offline ? 'Offline' : (raw === undefined ? normalized.label || 'Status' : 'Online'));

  const children = [
    arcOp(`${normalized.id}.dot`, bounds.x + dotRadius + 2, bounds.y + bounds.h / 2, dotRadius, 0, 360, {
      fill: dotColor,
      lineWidth: 0,
      role: 'status-dot',
      binding: bindingFor(widget, normalized, palette, thresholds, 'status'),
      colors: { accent: palette.accent, warn: palette.warn, crit: palette.crit, ok: palette.ok, muted: palette.muted },
    }),
  ];
  if (text) {
    children.push(textOp(
      `${normalized.id}.text`,
      bounds.x + dotRadius * 2 + 8,
      bounds.y + bounds.h / 2,
      text,
      {
        fill: palette.text,
        fontSize,
        align: 'left',
        baseline: 'middle',
        role: 'status-text',
        binding: {
          ...bindingFor(widget, normalized, palette, thresholds, 'status'),
          mode: 'status',
          offlineText: 'Offline',
          onlineText: normalized.label || 'Online',
        },
      },
    ));
  }
  return children;
}

function renderIcon(widget, normalized, options) {
  const { bounds } = normalized;
  const palette = buildPalette(options.tokens, widget);
  const name = asString(
    widget.icon ?? widget.icon_name ?? widget.icon_asset ?? widget.name,
    'icon',
    128,
  );
  const children = [
    rectOp(`${normalized.id}.surface`, bounds.x, bounds.y, bounds.w, bounds.h, {
      fill: palette.surface,
      radius: Math.min(bounds.w, bounds.h) * 0.14,
      stroke: palette.track,
      lineWidth: 1,
    }),
    {
      op: 'icon',
      id: `${normalized.id}.glyph`,
      x: bounds.x,
      y: bounds.y,
      w: bounds.w,
      h: bounds.h,
      name,
      color: palette.accent,
      label: normalized.label,
    },
  ];
  if (normalized.label) {
    children.push(textOp(
      `${normalized.id}.label`,
      bounds.x + bounds.w / 2,
      bounds.y + bounds.h * 0.88,
      normalized.label,
      {
        fill: palette.muted,
        fontSize: Math.max(6, Math.min(bounds.h * 0.14, 14)),
        align: 'center',
        baseline: 'middle',
      },
    ));
  }
  return children;
}

function renderImage(widget, normalized, options) {
  const { bounds } = normalized;
  const palette = buildPalette(options.tokens, widget);
  const href = safeResourceUrl(widget.url ?? widget.image_url ?? widget.src ?? widget.href);
  if (!href) {
    return [rectOp(`${normalized.id}.placeholder`, bounds.x, bounds.y, bounds.w, bounds.h, {
      fill: palette.surface,
      stroke: palette.track,
      lineWidth: 1,
      radius: 2,
    })];
  }
  return [{
    op: 'image',
    id: normalized.id,
    x: bounds.x,
    y: bounds.y,
    w: bounds.w,
    h: bounds.h,
    href,
    alt: normalized.label,
  }];
}

function renderTable(widget, normalized, options) {
  const { bounds } = normalized;
  const palette = buildPalette(options.tokens, widget);
  const rows = Array.isArray(widget.rows)
    ? widget.rows.slice(0, MAX_ROWS)
    : (Array.isArray(widget.data) ? widget.data.slice(0, MAX_ROWS) : []);
  const fontSize = clampInt(widget.font_size, 6, 128, 11);
  const rowHeight = Math.max(fontSize + 2, bounds.h / Math.max(1, rows.length + 1));
  const children = [
    rectOp(`${normalized.id}.surface`, bounds.x, bounds.y, bounds.w, bounds.h, {
      fill: palette.surface,
      radius: 2,
    }),
  ];
  if (normalized.label) {
    children.push(textOp(`${normalized.id}.header`, bounds.x + 4, bounds.y + rowHeight * 0.6, normalized.label, {
      fill: palette.text,
      fontSize,
      align: 'left',
      baseline: 'middle',
      fontWeight: 'bold',
    }));
  }
  if (rows.length === 0 && normalized.metric) {
    children.push(textOp(
      `${normalized.id}.value-text`,
      bounds.x + bounds.w / 2,
      bounds.y + bounds.h / 2,
      formatValue(readValue(options.data, normalized.metric), {
        decimals: normalized.decimals,
        unit: normalized.unit,
      }),
      {
        fill: palette.text,
        fontSize,
        align: 'center',
        baseline: 'middle',
        role: 'value-text',
        binding: bindingFor(widget, normalized, palette, collectThresholds(widget, palette)),
      },
    ));
  }
  rows.forEach((row, rowIndex) => {
    const cells = Array.isArray(row) ? row.slice(0, 8) : [row];
    const width = bounds.w / Math.max(1, cells.length);
    cells.forEach((cell, cellIndex) => {
      children.push(textOp(
        `${normalized.id}.r${rowIndex}.c${cellIndex}`,
        bounds.x + width * cellIndex + 4,
        bounds.y + rowHeight * (rowIndex + (normalized.label ? 1.6 : 0.6)),
        asString(cell, '', 512),
        {
          fill: palette.text,
          fontSize,
          align: 'left',
          baseline: 'middle',
        },
      ));
    });
  });
  return children;
}

function renderCard(widget, normalized, options) {
  const { bounds } = normalized;
  const palette = buildPalette(options.tokens, widget);
  const thresholds = collectThresholds(widget, palette);
  const raw = readValue(options.data, normalized.metric);
  const numeric = typeof raw === 'number' ? clamp(raw, normalized.min, normalized.max) : raw;
  const severity = severityFor(typeof numeric === 'number' ? numeric : NaN, thresholds, palette);
  const titleSize = clampInt(widget.font_size, 6, 128, Math.max(10, Math.min(bounds.h * 0.18, 22)));
  const children = [
    rectOp(`${normalized.id}.surface`, bounds.x, bounds.y, bounds.w, bounds.h, {
      fill: palette.surface,
      stroke: palette.track,
      lineWidth: 1,
      radius: clamp(finiteNumber(widget.radius, 6), 0, Math.min(bounds.w, bounds.h) / 2),
    }),
  ];
  if (normalized.label) {
    children.push(textOp(`${normalized.id}.label`, bounds.x + 8, bounds.y + titleSize + 4, normalized.label, {
      fill: palette.muted,
      fontSize: titleSize * 0.55,
      align: 'left',
      baseline: 'middle',
    }));
  }
  if (normalized.metric || raw !== undefined) {
    children.push(textOp(
      `${normalized.id}.value-text`,
      bounds.x + bounds.w / 2,
      bounds.y + bounds.h * 0.58,
      formatValue(numeric, { decimals: normalized.decimals, unit: normalized.unit }),
      {
        fill: severity.color,
        fontSize: Math.max(titleSize, Math.min(bounds.w / 5, bounds.h * 0.34)),
        align: 'center',
        baseline: 'middle',
        fontWeight: 'bold',
        role: 'value-text',
        binding: bindingFor(widget, normalized, palette, thresholds),
      },
    ));
  } else {
    const text = widget.text ? interpolateTemplate(widget.text, options.data || {}, '') : '';
    if (text) {
      children.push(textOp(
        `${normalized.id}.text`,
        bounds.x + bounds.w / 2,
        bounds.y + bounds.h * 0.58,
        text,
        {
          fill: palette.text,
          fontSize: titleSize,
          align: 'center',
          baseline: 'middle',
          role: 'template-text',
          binding: {
            ...bindingFor(widget, normalized, palette, [], 'text'),
            template: text,
          },
        },
      ));
    }
  }
  return children;
}

function renderWidgetChildren(widget, normalized, options, depth) {
  if (options.widgetBudget) {
    options.widgetBudget.count += 1;
    if (options.widgetBudget.count > MAX_WIDGETS) {
      fail('TOO_MANY_WIDGETS', `layout exceeds ${MAX_WIDGETS} widgets including nested children`);
    }
  }
  switch (normalized.kind) {
    case 'gauge':
      return renderGauge(widget, normalized, options);
    case 'bar':
      return renderBar(widget, normalized, options);
    case 'number':
      return renderNumber(widget, normalized, options);
    case 'text':
      return renderText(widget, normalized, options);
    case 'sparkline':
    case 'chart':
      return renderSparkline(widget, normalized, options);
    case 'status':
      return renderStatus(widget, normalized, options);
    case 'icon':
      return renderIcon(widget, normalized, options);
    case 'image':
      return renderImage(widget, normalized, options);
    case 'table':
      return renderTable(widget, normalized, options);
    case 'card':
    case 'custom':
    default: {
      if (Array.isArray(widget.children) && widget.children.length > 0) {
        if (depth >= MAX_GROUP_DEPTH) {
          fail('GROUP_DEPTH', `widget groups exceed maximum depth ${MAX_GROUP_DEPTH}`);
        }
        const children = [];
        widget.children.slice(0, MAX_WIDGETS).forEach((child, childIndex) => {
          if (!isPlainObject(child)) return;
          const childWidget = {
            ...child,
            x: finiteNumber(child.x, 0) + normalized.bounds.x,
            y: finiteNumber(child.y, 0) + normalized.bounds.y,
          };
          const childNormalized = normaliseWidget(
            childWidget,
            `${normalized.index}.${childIndex}`,
            normalized.layout,
            options.warnings,
          );
          const rendered = renderWidgetChildren(childWidget, childNormalized, options, depth + 1);
          const clip = clipFor(childNormalized.bounds, options.width, options.height);
          children.push({
            op: 'group',
            id: `${normalized.id}.g${childIndex}`,
            x: childNormalized.bounds.x,
            y: childNormalized.bounds.y,
            w: childNormalized.bounds.w,
            h: childNormalized.bounds.h,
            clip,
            children: rendered,
          });
        });
        return [{
          op: 'group',
          id: `${normalized.id}.group`,
          x: normalized.bounds.x,
          y: normalized.bounds.y,
          w: normalized.bounds.w,
          h: normalized.bounds.h,
          clip: clipFor(normalized.bounds, options.width, options.height),
          children,
        }];
      }
      return renderCard(widget, normalized, options);
    }
  }
}

/* ------------------------------------------------------------------------- */
/* Public: IR -> render plan                                                  */
/* ------------------------------------------------------------------------- */

/**
 * Build a deterministic draw plan from a ScreenLayout/DisplayDefinition.
 *
 * @param {object} layout ScreenLayout or definition (`layout.widgets`) shape.
 * @param {object} [options]
 * @param {number} [options.width] target width; defaults to layout width.
 * @param {number} [options.height] target height; defaults to layout height.
 * @param {object} [options.data] metric values used to seed bound operations.
 * @param {object} [options.theme] theme document or token map.
 * @param {object} [options.tokens] explicit token map.
 * @param {string} [options.background] canvas background override.
 * @returns {object} render plan.
 */
function irToRenderPlan(layout, options = {}) {
  if (!isPlainObject(options)) fail('INVALID_OPTIONS', 'options must be an object');
  const { layout: screen, sourceFormat } = extractLayout(layout);
  const widgets = screen.widgets;
  if (!Array.isArray(widgets)) fail('INVALID_LAYOUT', 'layout.widgets must be an array');
  if (widgets.length > MAX_WIDGETS) {
    fail('TOO_MANY_WIDGETS', `layout exceeds ${MAX_WIDGETS} widgets`);
  }

  const boundsDimension = (value, fallback, name) => {
    const number = finiteNumber(value, fallback);
    if (number > MAX_DIMENSION) {
      fail('INVALID_SIZE', `${name} exceeds maximum ${MAX_DIMENSION}`);
    }
    return clampInt(number, 1, MAX_DIMENSION, fallback);
  };
  const width = boundsDimension(options.width ?? screen.width, DEFAULTS.width, 'width');
  const height = boundsDimension(options.height ?? screen.height, DEFAULTS.height, 'height');
  const tokens = buildTokens(options.tokens || options.theme || {});
  const background = safeColor(
    options.background ?? screen.background ?? tokenValue(tokens, ['color.bg'], DEFAULTS.background),
    DEFAULTS.background,
  );

  const warnings = [];
  const ops = [];
  const widgetBudget = { count: 0 };
  const pointBudget = { count: 0 };
  widgets.forEach((widget, index) => {
    const normalized = normaliseWidget(widget, index, screen, warnings);
    const clip = clipFor(normalized.bounds, width, height);
    if (!clip) {
      warnings.push(`widget[${index}] is outside the ${width}x${height} canvas`);
      return;
    }
    const children = renderWidgetChildren(widget, normalized, {
      width,
      height,
      data: isPlainObject(options.data) ? options.data : {},
      tokens,
      warnings,
      widgetBudget,
      pointBudget,
    }, 0);
    ops.push({
      op: 'group',
      id: `w${index}`,
      x: normalized.bounds.x,
      y: normalized.bounds.y,
      w: normalized.bounds.w,
      h: normalized.bounds.h,
      clip,
      children,
    });
  });

  const plan = {
    schema: PLAN_SCHEMA,
    version: PLAN_VERSION,
    width,
    height,
    background,
    sourceFormat,
    ops,
    warnings,
  };
  validateOps(plan.ops);
  return plan;
}

/* ------------------------------------------------------------------------- */
/* Operation validation                                                       */
/* ------------------------------------------------------------------------- */

function validateOps(ops, depth = 0, counter = { count: 0 }) {
  if (!Array.isArray(ops)) fail('INVALID_PLAN', 'ops must be an array');
  if (depth > MAX_GROUP_DEPTH + 2) fail('GROUP_DEPTH', 'operations are nested too deeply');
  for (const op of ops) {
    if (!isPlainObject(op)) fail('INVALID_OP', 'draw operation must be an object');
    counter.count += 1;
    if (counter.count > MAX_OPS) fail('TOO_MANY_OPS', `plan exceeds ${MAX_OPS} operations`);
    const name = op.op;
    if (typeof name !== 'string' || !OP_NAMES.has(name)) {
      fail('UNKNOWN_OP', `unknown draw operation "${String(name)}"`);
    }
    if (name === 'group') {
      validateOps(op.children, depth + 1, counter);
      continue;
    }
    const requireFinite = (keys) => {
      for (const key of keys) {
        if (!Number.isFinite(Number(op[key]))) {
          fail('INVALID_OP', `operation "${name}" has non-finite "${key}"`);
        }
      }
    };
    if (name === 'rect' || name === 'bar' || name === 'image' || name === 'icon') {
      requireFinite(['x', 'y', 'w', 'h']);
    } else if (name === 'text') {
      requireFinite(['x', 'y']);
      if (typeof op.text !== 'string') fail('INVALID_OP', 'text operation requires a string text value');
      if (op.text.length > MAX_TEXT_LENGTH) {
        fail('INVALID_OP', `text exceeds ${MAX_TEXT_LENGTH} characters`);
      }
    } else if (name === 'arc') {
      requireFinite(['cx', 'cy', 'r', 'startAngle', 'endAngle']);
    } else if (name === 'line') {
      requireFinite(['x1', 'y1', 'x2', 'y2']);
    } else if (name === 'polyline') {
      if (!Array.isArray(op.points)) fail('INVALID_OP', 'polyline points must be an array');
      if (op.points.length > MAX_POINTS) fail('TOO_MANY_POINTS', `polyline exceeds ${MAX_POINTS} points`);
      for (const point of op.points) {
        if (!Array.isArray(point) || !Number.isFinite(Number(point[0])) || !Number.isFinite(Number(point[1]))) {
          fail('INVALID_OP', 'polyline point must be a finite pair');
        }
      }
    }
  }
  return counter.count;
}

/* ------------------------------------------------------------------------- */
/* Public: apply data without rebuilding the plan                             */
/* ------------------------------------------------------------------------- */

function updateSparkline(op, value) {
  if (!Array.isArray(value)) return;
  const geometry = isPlainObject(op.geometry) ? op.geometry : null;
  if (!geometry) return;
  const values = [];
  for (const item of value) {
    if (values.length >= MAX_POINTS) break;
    const number = finiteNumber(item, NaN);
    if (Number.isFinite(number)) values.push(number);
  }
  const min = finiteNumber(geometry.min, 0);
  const max = finiteNumber(geometry.max, min + 1);
  const points = sparklinePoints(values, geometry, min, max > min ? max : min + 1);
  if (Array.isArray(op.points)) {
    op.points.length = 0;
    for (const point of points) op.points.push(point);
  } else {
    op.points = points;
  }
  if (geometry.close !== undefined) op.close = Boolean(geometry.close);
  op.value = values.length > 0 ? values[values.length - 1] : undefined;
}

function applyDataToOps(ops, values) {
  for (const op of ops) {
    if (op.op === 'group') {
      applyDataToOps(op.children, values);
      continue;
    }
    const binding = isPlainObject(op.binding) ? op.binding : null;
    if (!binding || !binding.metric) continue;
    const raw = readValue(values, binding.metric);

    if (op.role === 'gauge-value') {
      const min = finiteNumber(binding.min, 0);
      const max = finiteNumber(binding.max, min + 1);
      const value = clamp(finiteNumber(raw, min), min, max);
      const pct = max > min ? (value - min) / (max - min) : 0;
      const start = finiteNumber(op.angleStart, finiteNumber(op.startAngle, 0));
      const sweep = finiteNumber(op.angleSweep, 270);
      op.endAngle = roundNumber(start + sweep * pct);
      op.value = roundNumber(value);
      const severity = severityFor(value, op.thresholds || [], binding.colors || {});
      op.stroke = severity.color;
      op.severity = severity.severity;
    } else if (op.role === 'bar') {
      const min = finiteNumber(binding.min, 0);
      const max = finiteNumber(binding.max, min + 1);
      const value = clamp(finiteNumber(raw, min), min, max);
      op.value = roundNumber(value);
      const severity = severityFor(value, op.thresholds || [], binding.colors || {});
      op.fill = severity.color;
      op.severity = severity.severity;
    } else if (op.role === 'value-text') {
      let value = raw;
      const min = finiteNumber(binding.min, -Infinity);
      const max = finiteNumber(binding.max, Infinity);
      if (typeof value === 'number') value = clamp(value, min, max);
      op.text = formatValue(value, binding);
      const severity = severityFor(typeof value === 'number' ? value : NaN, binding.thresholds || [], binding.colors || {});
      if (!binding.mode || binding.mode !== 'status') op.fill = severity.color;
    } else if (op.role === 'sparkline') {
      updateSparkline(op, raw);
    } else if (op.role === 'status-dot') {
      const offline = raw === false || raw === 0 || raw === 'offline' || raw === 'down';
      op.fill = offline ? (binding.colors?.crit || '#f26464') : (raw === undefined ? (binding.colors?.muted || '#8d9bb3') : (binding.colors?.ok || '#42d392'));
    } else if (op.role === 'status-text') {
      const offline = raw === false || raw === 0 || raw === 'offline' || raw === 'down';
      op.text = offline
        ? asString(binding.offlineText, 'Offline', MAX_TEXT_LENGTH)
        : (raw === undefined ? asString(binding.onlineText, 'Status', MAX_TEXT_LENGTH) : binding.onlineText);
    } else if (op.role === 'template-text') {
      const template = asString(op.template ?? binding.template, '', MAX_TEXT_LENGTH);
      op.text = interpolateTemplate(template, values, binding.metric);
    }
  }
}

/**
 * Update data-bound operations in place.
 *
 * Geometry remains the same object graph; only values, threshold colours,
 * formatted text, sparkline points, and arc sweeps change.
 */
function applyData(plan, values) {
  if (!isPlainObject(plan) || !Array.isArray(plan.ops)) {
    fail('INVALID_PLAN', 'plan must be an object with an ops array');
  }
  if (!isPlainObject(values)) fail('INVALID_DATA', 'values must be an object');
  validateOps(plan.ops);
  applyDataToOps(plan.ops, values);
  return plan;
}

/* ------------------------------------------------------------------------- */
/* Public: plan -> SVG preview                                                */
/* ------------------------------------------------------------------------- */

function svgArcPath(cx, cy, radius, startAngle, endAngle) {
  const sweep = endAngle - startAngle;
  if (Math.abs(sweep) >= 359.999) {
    const mid = startAngle + 180;
    const first = svgArcPath(cx, cy, radius, startAngle, mid);
    const second = svgArcPath(cx, cy, radius, mid, endAngle);
    return `${first} ${second.replace(/^M [^ ]+ [^ ]+ /, '')}`;
  }
  const startRad = (startAngle * Math.PI) / 180;
  const endRad = (endAngle * Math.PI) / 180;
  const x1 = cx + Math.cos(startRad) * radius;
  const y1 = cy + Math.sin(startRad) * radius;
  const x2 = cx + Math.cos(endRad) * radius;
  const y2 = cy + Math.sin(endRad) * radius;
  const large = Math.abs(sweep) > 180 ? 1 : 0;
  const direction = sweep >= 0 ? 1 : 0;
  return `M ${roundNumber(x1)} ${roundNumber(y1)} A ${roundNumber(radius)} ${roundNumber(radius)} 0 ${large} ${direction} ${roundNumber(x2)} ${roundNumber(y2)}`;
}

function svgStyle(op, fillKey = 'fill', strokeKey = 'stroke') {
  const parts = [];
  if (op[fillKey] !== undefined && op[fillKey] !== 'none' && op[fillKey] !== 'transparent') {
    parts.push(`fill="${escapeXmlAttr(op[fillKey])}"`);
  } else if (op[fillKey] === 'none' || op[fillKey] === 'transparent') {
    parts.push('fill="none"');
  } else {
    parts.push('fill="none"');
  }
  if (op[strokeKey] !== undefined) {
    parts.push(`stroke="${escapeXmlAttr(op[strokeKey])}"`);
    if (op.lineWidth !== undefined) parts.push(`stroke-width="${roundNumber(op.lineWidth)}"`);
    if (op.cap === 'round') parts.push('stroke-linecap="round"');
  }
  return parts.join(' ');
}

function renderSvgOp(op, state) {
  switch (op.op) {
    case 'group': {
      const children = op.children.map((child) => renderSvgOp(child, state)).join('');
      if (!isPlainObject(op.clip)) return `<g data-id="${escapeXmlAttr(op.id)}">${children}</g>`;
      const clipId = `clip${state.clip}`;
      state.clip += 1;
      state.defs.push(
        `<clipPath id="${clipId}"><rect x="${roundNumber(op.clip.x)}" y="${roundNumber(op.clip.y)}" width="${roundNumber(op.clip.w)}" height="${roundNumber(op.clip.h)}"/></clipPath>`,
      );
      return `<g data-id="${escapeXmlAttr(op.id)}" clip-path="url(#${clipId})">${children}</g>`;
    }
    case 'rect': {
      const radius = op.radius ? ` rx="${roundNumber(op.radius)}"` : '';
      return `<rect x="${roundNumber(op.x)}" y="${roundNumber(op.y)}" width="${roundNumber(op.w)}" height="${roundNumber(op.h)}"${radius} ${svgStyle(op)}${op.opacity !== undefined ? ` opacity="${roundNumber(op.opacity)}"` : ''}/>`;
    }
    case 'text': {
      const anchor = op.align === 'center' ? 'middle' : (op.align === 'right' ? 'end' : 'start');
      const baseline = op.baseline === 'middle' ? 'central' : (op.baseline === 'top' ? 'hanging' : op.baseline);
      const weight = op.fontWeight === 'bold' ? ' font-weight="bold"' : '';
      return `<text x="${roundNumber(op.x)}" y="${roundNumber(op.y)}" fill="${escapeXmlAttr(op.fill)}" font-size="${roundNumber(op.fontSize)}" font-family="${escapeXmlAttr(op.fontFamily)}"${weight} text-anchor="${anchor}" dominant-baseline="${baseline}">${escapeXmlText(op.text)}</text>`;
    }
    case 'arc': {
      const path = svgArcPath(op.cx, op.cy, op.r, op.startAngle, op.endAngle);
      if (op.fill !== undefined && op.lineWidth === 0) {
        return `<path d="${path}" fill="${escapeXmlAttr(op.fill)}" stroke="none"/>`;
      }
      return `<path d="${path}" ${svgStyle(op)}/>`;
    }
    case 'bar': {
      const radius = op.radius ? ` rx="${roundNumber(op.radius)}"` : '';
      const span = op.max > op.min ? op.max - op.min : 1;
      const pct = clamp((op.value - op.min) / span, 0, 1);
      const track = `<rect x="${roundNumber(op.x)}" y="${roundNumber(op.y)}" width="${roundNumber(op.w)}" height="${roundNumber(op.h)}"${radius} fill="${escapeXmlAttr(op.track)}"/>`;
      const fill = op.orientation === 'vertical'
        ? `<rect x="${roundNumber(op.x)}" y="${roundNumber(op.y + op.h * (1 - pct))}" width="${roundNumber(op.w)}" height="${roundNumber(op.h * pct)}"${radius} fill="${escapeXmlAttr(op.fill)}"/>`
        : `<rect x="${roundNumber(op.x)}" y="${roundNumber(op.y)}" width="${roundNumber(op.w * pct)}" height="${roundNumber(op.h)}"${radius} fill="${escapeXmlAttr(op.fill)}"/>`;
      return `<g data-id="${escapeXmlAttr(op.id)}">${track}${fill}</g>`;
    }
    case 'line':
      return `<line x1="${roundNumber(op.x1)}" y1="${roundNumber(op.y1)}" x2="${roundNumber(op.x2)}" y2="${roundNumber(op.y2)}" ${svgStyle(op, 'none', 'stroke')}/>`;
    case 'polyline': {
      const points = op.points.map((point) => `${roundNumber(point[0])},${roundNumber(point[1])}`).join(' ');
      const tag = op.close ? 'polygon' : 'polyline';
      return `<${tag} points="${points}" ${svgStyle(op)}/>`;
    }
    case 'image': {
      const href = safeResourceUrl(op.href);
      if (!href) {
        return `<rect x="${roundNumber(op.x)}" y="${roundNumber(op.y)}" width="${roundNumber(op.w)}" height="${roundNumber(op.h)}" fill="#111118" stroke="#444"/>`;
      }
      return `<image x="${roundNumber(op.x)}" y="${roundNumber(op.y)}" width="${roundNumber(op.w)}" height="${roundNumber(op.h)}" href="${escapeXmlAttr(href)}" preserveAspectRatio="xMidYMid meet"/>`;
    }
    case 'icon': {
      const size = Math.min(op.w, op.h) * 0.42;
      const cx = op.x + op.w / 2;
      const cy = op.y + op.h / 2;
      const initial = asString(op.name, '?', 1).toUpperCase() || '?';
      return `<g data-id="${escapeXmlAttr(op.id)}"><circle cx="${roundNumber(cx)}" cy="${roundNumber(cy)}" r="${roundNumber(size)}" fill="none" stroke="${escapeXmlAttr(op.color)}" stroke-width="2"/><text x="${roundNumber(cx)}" y="${roundNumber(cy)}" fill="${escapeXmlAttr(op.color)}" font-size="${roundNumber(size)}" text-anchor="middle" dominant-baseline="central">${escapeXmlText(initial)}</text></g>`;
    }
    default:
      fail('UNKNOWN_OP', `unknown draw operation "${String(op.op)}"`);
      return '';
  }
}

/**
 * Render a plan to dependency-free SVG (tests, previews, and thumbnails).
 */
function planToSvg(plan, options = {}) {
  if (!isPlainObject(plan) || !Array.isArray(plan.ops)) {
    fail('INVALID_PLAN', 'plan must be an object with an ops array');
  }
  validateOps(plan.ops);
  const width = clampInt(options.width ?? plan.width, 1, MAX_DIMENSION, DEFAULTS.width);
  const height = clampInt(options.height ?? plan.height, 1, MAX_DIMENSION, DEFAULTS.height);
  const state = { clip: 0, defs: [] };
  const body = plan.ops.map((op) => renderSvgOp(op, state)).join('');
  const background = safeColor(plan.background, DEFAULTS.background);
  const defs = state.defs.length > 0 ? `<defs>${state.defs.join('')}</defs>` : '';
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" role="img">${defs}<rect x="0" y="0" width="${width}" height="${height}" fill="${escapeXmlAttr(background)}"/>${body}</svg>`;
}

/* ------------------------------------------------------------------------- */
/* Public: plan -> LVGL primitives                                            */
/* ------------------------------------------------------------------------- */

function lvglPrimitiveForOp(op, parent) {
  const base = {
    id: op.id,
    x: op.x,
    y: op.y,
    w: op.w,
    h: op.h,
  };
  if (parent !== undefined) base.parent = parent;
  switch (op.op) {
    case 'rect':
      return {
        ...base,
        type: 'lv_obj',
        style: {
          bgColor: op.fill,
          borderColor: op.stroke,
          borderWidth: op.lineWidth || 0,
          radius: op.radius || 0,
          opacity: op.opacity,
        },
      };
    case 'text':
      return {
        ...base,
        type: 'lv_label',
        text: op.text,
        style: {
          textColor: op.fill,
          fontSize: op.fontSize,
          align: op.align,
        },
      };
    case 'arc':
      return {
        ...base,
        type: 'lv_arc',
        x: op.cx - op.r,
        y: op.cy - op.r,
        w: op.r * 2,
        h: op.r * 2,
        min: op.binding?.min,
        max: op.binding?.max,
        value: op.value ?? op.binding?.min,
        startAngle: op.startAngle,
        endAngle: op.endAngle,
        thickness: op.lineWidth,
        color: op.stroke || op.fill,
        trackColor: undefined,
        severity: op.severity,
        thresholds: op.thresholds || [],
        binding: op.binding,
      };
    case 'bar':
      return {
        ...base,
        type: 'lv_bar',
        min: op.min,
        max: op.max,
        value: op.value,
        orientation: op.orientation,
        color: op.fill,
        trackColor: op.track,
        radius: op.radius,
        severity: op.severity,
        thresholds: op.thresholds || [],
        binding: op.binding,
      };
    case 'line':
      return {
        ...base,
        type: 'lv_line',
        points: [[op.x1, op.y1], [op.x2, op.y2]],
        color: op.stroke,
        width: op.lineWidth,
      };
    case 'polyline':
      return {
        ...base,
        type: op.points.length > 2 ? 'lv_chart' : 'lv_line',
        points: op.points,
        color: op.stroke,
        fill: op.fill,
        width: op.lineWidth,
      };
    case 'image':
      return {
        ...base,
        type: 'lv_img',
        src: op.href,
        alt: op.alt,
      };
    case 'icon':
      return {
        ...base,
        type: 'lv_label',
        text: op.label || op.name,
        symbol: op.name,
        style: { textColor: op.color },
      };
    case 'group':
      return {
        ...base,
        type: 'lv_obj',
        clip: op.clip,
        style: { bgOpacity: 0 },
      };
    default:
      fail('UNKNOWN_OP', `unknown draw operation "${String(op.op)}"`);
      return null;
  }
}

/**
 * Convert a plan to a flat, target-neutral LVGL primitive list.
 *
 * Groups become transparent containers; children carry a `parent` identifier.
 */
function planToLvgl(plan) {
  if (!isPlainObject(plan) || !Array.isArray(plan.ops)) {
    fail('INVALID_PLAN', 'plan must be an object with an ops array');
  }
  validateOps(plan.ops);
  const primitives = [];
  const visit = (ops, parent) => {
    for (const op of ops) {
      if (primitives.length >= MAX_OPS) fail('TOO_MANY_OPS', `plan exceeds ${MAX_OPS} operations`);
      if (op.op === 'group') {
        const primitive = lvglPrimitiveForOp(op, parent);
        primitive.children = op.children.length;
        primitives.push(primitive);
        visit(op.children, op.id);
      } else {
        primitives.push(lvglPrimitiveForOp(op, parent));
      }
    }
  };
  visit(plan.ops, undefined);
  // Return the primitive list directly (the common LVGL-consumer shape) while
  // exposing metadata and a `.primitives` alias for plan-style callers.
  const result = primitives;
  Object.defineProperties(result, {
    schema: { value: 'org.ozma.lvgl-plan/1', enumerable: false },
    target: { value: 'lvgl', enumerable: false },
    width: { value: plan.width, enumerable: false },
    height: { value: plan.height, enumerable: false },
    background: { value: plan.background, enumerable: false },
    primitives: { value: result, enumerable: false },
  });
  return result;
}

/* ------------------------------------------------------------------------- */
/* Exports                                                                    */
/* ------------------------------------------------------------------------- */

module.exports = {
  DisplayIrError,
  PLAN_SCHEMA,
  PLAN_VERSION,
  OP_NAMES,
  MAX_WIDGETS,
  MAX_OPS,
  MAX_POINTS,
  MAX_DIMENSION,
  applyData,
  irToRenderPlan,
  planToLvgl,
  planToSvg,
};
