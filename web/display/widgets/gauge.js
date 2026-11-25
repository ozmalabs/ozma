// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Dependency-free SVG gauge/widget family.
 *
 * The module exposes pure geometry helpers for tests and a `renderGauge`
 * browser helper that writes accessible SVG into a container.  Widgets support
 * arc gauges, horizontal bars, dual bars, threshold/severity bands, ticks,
 * rounded caps, theme tokens, and reduced-motion preferences.
 */

const SVG_NS = 'http://www.w3.org/2000/svg';

const DEFAULT_COLORS = Object.freeze({
  normal: '#5b6fff',
  warn: '#f0b94a',
  crit: '#f26464',
  second: '#42d392',
  track: 'rgba(255, 255, 255, 0.12)',
  text: '#e5e7eb',
  muted: '#8d9bb3',
});

const REFERENCE_RE = /\{([^{}]+)\}/g;

/* ------------------------------------------------------------------------- */
/* Small helpers                                                              */
/* ------------------------------------------------------------------------- */

export function clamp(value, min, max) {
  const lower = Number.isFinite(Number(min)) ? Number(min) : 0;
  const upper = Number.isFinite(Number(max)) ? Number(max) : 100;
  const number = Number(value);
  if (!Number.isFinite(number)) return lower;
  if (upper === lower) return lower;
  const lo = Math.min(lower, upper);
  const hi = Math.max(lower, upper);
  return Math.max(lo, Math.min(hi, number));
}

function clampInteger(value, min, max, fallback) {
  const number = Number(value);
  if (!Number.isFinite(number)) return fallback;
  return Math.max(min, Math.min(max, Math.round(number)));
}

function finite(value, fallback) {
  if (value === undefined || value === null || value === '') return fallback;
  const number = Number(value);
  return Number.isFinite(number) ? number : fallback;
}

function positive(value, fallback) {
  if (value === undefined || value === null || value === '') return fallback;
  const number = Number(value);
  return Number.isFinite(number) && number > 0 ? number : fallback;
}

function firstDefined(...values) {
  for (const value of values) {
    if (value !== undefined && value !== null && value !== '') return value;
  }
  return undefined;
}

function numberText(value) {
  const rounded = Math.round(Number(value) * 1000) / 1000;
  return String(Object.is(rounded, -0) ? 0 : rounded);
}

function escapeXml(value) {
  return String(value ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

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

function tokenSource(tokens) {
  if (!tokens || typeof tokens !== 'object') return {};
  if (
    tokens.tokens
    && typeof tokens.tokens === 'object'
    && (tokens.schema || tokens.canvas || tokens.id)
  ) {
    return tokens.tokens;
  }
  return tokens;
}

function rawToken(tokens, key) {
  const source = tokenSource(tokens);
  if (!source || typeof source !== 'object') return { found: false };
  if (Object.prototype.hasOwnProperty.call(source, key)) {
    return { found: true, value: source[key] };
  }
  let current = source;
  for (const part of String(key).split('.')) {
    if (!isPlainObject(current) || !Object.prototype.hasOwnProperty.call(current, part)) {
      return { found: false };
    }
    current = current[part];
  }
  return { found: true, value: current };
}

function unwrap(value) {
  if (isTokenObject(value)) {
    return Object.prototype.hasOwnProperty.call(value, '$value') ? value.$value : value.value;
  }
  return value;
}

function resolveString(text, tokens, depth = 0) {
  if (typeof text !== 'string') return text;
  if (depth > 12) return text;

  const full = text.match(/^\{([^{}]+)\}$/);
  if (full) {
    const { key, fallback } = parseGaugeReference(full[1]);
    const value = lookupTokenValue(key, tokens, depth + 1);
    if (value !== undefined) return value;
    return fallback ? resolveString(fallback, tokens, depth + 1) : text;
  }

  return text.replace(REFERENCE_RE, (match, body) => {
    const { key, fallback } = parseGaugeReference(body);
    const value = lookupTokenValue(key, tokens, depth + 1);
    if (value !== undefined) {
      return Array.isArray(value) ? value.join(', ') : String(value);
    }
    return fallback ? String(resolveString(fallback, tokens, depth + 1)) : match;
  });
}

function parseGaugeReference(body) {
  const pipe = body.indexOf('|');
  const comma = body.indexOf(',');
  let split = pipe;
  if (split === -1 && comma !== -1 && !body.slice(0, comma).includes('(')) split = comma;
  if (split === -1) return { key: body.trim(), fallback: '' };
  return {
    key: body.slice(0, split).trim(),
    fallback: body.slice(split + 1).trim(),
  };
}

function lookupTokenValue(key, tokens, depth = 0) {
  if (depth > 12) return undefined;
  const found = rawToken(tokens, key);
  if (!found.found) return undefined;
  const value = unwrap(found.value);
  if (value === undefined || value === null) return undefined;
  if (typeof value === 'string') return resolveString(value, tokens, depth);
  return value;
}

function tokenValue(tokens, names, fallback) {
  for (const name of names) {
    if (!name) continue;
    const value = lookupTokenValue(name, tokens);
    if (value !== undefined && value !== '') return value;
  }
  return fallback;
}

function tokenString(tokens, names, fallback) {
  const value = tokenValue(tokens, names, fallback);
  if (Array.isArray(value)) return value.join(', ');
  if (isPlainObject(value)) return fallback;
  return String(value);
}

/* ------------------------------------------------------------------------- */
/* Geometry                                                                   */
/* ------------------------------------------------------------------------- */

/**
 * Convert a clock-style angle (0° = 12 o'clock, clockwise positive) to an
 * SVG coordinate on a circle.
 */
export function polarPoint(cx, cy, radius, angleDeg) {
  const angle = (Number(angleDeg) - 90) * (Math.PI / 180);
  return {
    x: Number(cx) + Number(radius) * Math.cos(angle),
    y: Number(cy) + Number(radius) * Math.sin(angle),
  };
}

export const polarToCartesian = polarPoint;

/**
 * Build an SVG arc path between two clock-style angles.
 *
 * A full 360° span is split into two 180° arcs because a single SVG arc
 * command cannot represent a complete circle.
 */
export function arcPath({ cx, cy, radius, startAngle, endAngle }) {
  const start = polarPoint(cx, cy, radius, startAngle);
  const end = polarPoint(cx, cy, radius, endAngle);
  const startX = numberText(start.x);
  const startY = numberText(start.y);
  const endX = numberText(end.x);
  const endY = numberText(end.y);
  const r = numberText(Math.max(0, Number(radius)));
  let span = Number(endAngle) - Number(startAngle);
  if (!Number.isFinite(span)) span = 0;

  if (Math.abs(span) >= 359.999) {
    const middle = polarPoint(cx, cy, radius, Number(startAngle) + span / 2);
    const sweep = span >= 0 ? 1 : 0;
    return `M ${startX} ${startY} A ${r} ${r} 0 0 ${sweep} ${numberText(middle.x)} ${numberText(middle.y)} A ${r} ${r} 0 0 ${sweep} ${endX} ${endY}`;
  }
  if (span === 0) return `M ${startX} ${startY}`;

  const largeArc = Math.abs(span) > 180 ? 1 : 0;
  const sweep = span >= 0 ? 1 : 0;
  return `M ${startX} ${startY} A ${r} ${r} 0 ${largeArc} ${sweep} ${endX} ${endY}`;
}

export const describeArc = arcPath;

/**
 * Map a value onto an angle within `[startAngle, endAngle]`, clamping values
 * outside `[min, max]`.
 */
export function valueToAngle(value, min, max, startAngle, endAngle) {
  const lower = finite(min, 0);
  const upper = finite(max, 100);
  const start = finite(startAngle, -120);
  const end = finite(endAngle, 120);
  if (upper === lower) return start;
  const ratio = clamp((Number(value) - lower) / (upper - lower), 0, 1);
  return start + (end - start) * ratio;
}

/**
 * Format the numeric reading.  Explicit `decimals` are honoured; otherwise
 * integers stay integers and fractions get up to two trimmed decimals.
 */
export function formatGaugeValue(value, config = {}) {
  const number = Number(value);
  if (!Number.isFinite(number)) return '—';

  if (Number.isInteger(config.decimals)) {
    const decimals = clampInteger(config.decimals, 0, 6, 0);
    return number.toFixed(decimals);
  }
  if (Number.isInteger(number)) return String(number);
  return number.toFixed(2).replace(/0+$/, '').replace(/\.$/, '');
}

/* ------------------------------------------------------------------------- */
/* Configuration and colours                                                  */
/* ------------------------------------------------------------------------- */

function normalizeMode(config = {}) {
  const raw = String(
    config.mode ?? config.style ?? config.chart ?? (config.type === 'bar' ? 'bar' : 'arc'),
  ).toLowerCase();
  if (raw === 'bar' || raw === 'vu_meter' || raw === 'meter' || raw === 'linear') return 'bar';
  if (raw === 'dual-bar' || raw === 'dual_bar' || raw === 'dualbar' || raw === 'dual') {
    return 'dual-bar';
  }
  return 'arc';
}

export function normalizeGaugeConfig(config = {}) {
  const mode = normalizeMode(config);
  const size = positive(config.size, null);
  const width = positive(config.width, mode === 'arc' ? (size ?? 200) : 240);
  const height = positive(
    config.height,
    mode === 'arc' ? (size ?? width) : (mode === 'dual-bar' ? 96 : 64),
  );

  const min = finite(config.min, 0);
  let max = finite(config.max, 100);
  if (max === min) max = min + 1;

  let startAngle = finite(firstDefined(config.startAngle, config.start_angle), -120);
  let endAngle = finite(firstDefined(config.endAngle, config.end_angle), 120);
  if (endAngle === startAngle) endAngle = startAngle + 1;

  const thickness = positive(
    firstDefined(config.thickness, config.strokeWidth, config.stroke_width),
    Math.max(5, Math.min(width, height) * 0.08),
  );

  return {
    ...config,
    mode,
    width,
    height,
    min,
    max,
    startAngle,
    endAngle,
    thickness,
    padding: finite(config.padding, 8),
    decimals: Number.isInteger(config.decimals) ? config.decimals : undefined,
    label: String(firstDefined(config.label, config.title, config.name, '') ?? ''),
    unit: String(firstDefined(config.unit, config.units, '') ?? ''),
    tickCount: clampInteger(
      firstDefined(config.tickCount, config.tick_count),
      0,
      360,
      0,
    ),
    rounded: config.rounded !== false,
    showValue: config.showValue !== false && config.show_value !== false,
    showLabel: config.showLabel !== false && config.show_label !== false,
    warn: finite(firstDefined(config.warn, config.warn_at, config.warnAt, config.thresholds?.warn), null),
    crit: finite(firstDefined(config.crit, config.critical, config.crit_at, config.critAt, config.thresholds?.crit), null),
    animate: Boolean(firstDefined(config.animate, config.animated, false)),
  };
}

function resolveColour(value, tokens) {
  if (value === undefined || value === null || value === '') return undefined;
  if (typeof value === 'string') {
    return String(resolveString(value, tokens));
  }
  return String(value);
}

/**
 * Resolve gauge colours from explicit config, theme tokens, then defaults.
 */
export function resolveGaugeColors(config = {}, tokens = {}) {
  const explicit = config.colors && typeof config.colors === 'object' ? config.colors : {};
  return {
    normal: resolveColour(firstDefined(
      explicit.normal,
      explicit.fill,
      config.fill,
      config.color,
      tokenValue(tokens, ['gauge.fill', 'gauge.normal', 'color.accent'], undefined),
    ), tokens) ?? DEFAULT_COLORS.normal,
    warn: resolveColour(firstDefined(
      explicit.warn,
      config.warnColor,
      config.warn_color,
      tokenValue(tokens, ['gauge.warn', 'color.warn'], undefined),
    ), tokens) ?? DEFAULT_COLORS.warn,
    crit: resolveColour(firstDefined(
      explicit.crit,
      explicit.critical,
      config.critColor,
      config.crit_color,
      tokenValue(tokens, ['gauge.crit', 'color.crit', 'color.error'], undefined),
    ), tokens) ?? DEFAULT_COLORS.crit,
    second: resolveColour(firstDefined(
      explicit.second,
      config.secondColor,
      config.second_color,
      tokenValue(tokens, ['gauge.second', 'color.second', 'color.ok'], undefined),
    ), tokens) ?? DEFAULT_COLORS.second,
    track: resolveColour(firstDefined(
      explicit.track,
      config.track,
      tokenValue(tokens, ['gauge.track', 'color.track'], undefined),
    ), tokens) ?? DEFAULT_COLORS.track,
    text: resolveColour(firstDefined(
      explicit.text,
      tokenValue(tokens, ['gauge.text', 'color.fg', 'color.text'], undefined),
    ), tokens) ?? DEFAULT_COLORS.text,
    muted: resolveColour(firstDefined(
      explicit.muted,
      tokenValue(tokens, ['gauge.muted', 'color.muted', 'color.dim'], undefined),
    ), tokens) ?? DEFAULT_COLORS.muted,
  };
}

export function thresholdColor(value, config = {}, tokens = {}) {
  const colors = resolveGaugeColors(config, tokens);
  return colors[severityFor(value, normalizeGaugeConfig(config))];
}

/**
 * Severity for a value.  `crit` wins over `warn`; both are upper thresholds.
 */
export function severityFor(value, config = {}) {
  const number = Number(value);
  const warn = finite(firstDefined(config.warn, config.warn_at, config.warnAt), null);
  const crit = finite(firstDefined(config.crit, config.critical, config.crit_at, config.critAt), null);
  if (!Number.isFinite(number)) return 'normal';
  if (crit !== null && number >= crit) return 'crit';
  if (warn !== null && number >= warn) return 'warn';
  return 'normal';
}

/* ------------------------------------------------------------------------- */
/* Geometry objects                                                           */
/* ------------------------------------------------------------------------- */

function layoutArc(config) {
  const width = config.width;
  const height = config.height;
  const cx = width / 2;
  const cy = height / 2;
  const radius = Math.max(
    1,
    Math.min(width, height) / 2 - config.thickness / 2 - config.padding,
  );
  return { width, height, cx, cy, radius, thickness: config.thickness };
}

/**
 * Full arc-gauge geometry, including threshold bands and ticks.
 */
export function gaugeGeometry(config = {}, value, tokens = {}) {
  let cfg = normalizeGaugeConfig(config);
  const explicitThickness = firstDefined(
    config.thickness,
    config.strokeWidth,
    config.stroke_width,
  );
  if (explicitThickness === undefined) {
    const tokenThickness = Number.parseFloat(
      String(tokenValue(tokens, ['gauge.thickness', 'gauge.strokeWidth', 'gauge.stroke_width'], '') ?? ''),
    );
    if (Number.isFinite(tokenThickness) && tokenThickness > 0) {
      cfg = { ...cfg, thickness: tokenThickness };
    }
  }

  const layout = layoutArc(cfg);
  const raw = finite(value, finite(config.value, cfg.min));
  const clamped = clamp(raw, cfg.min, cfg.max);
  const percent = cfg.max === cfg.min
    ? 0
    : clamp((clamped - cfg.min) / (cfg.max - cfg.min), 0, 1) * 100;
  const valueAngle = valueToAngle(clamped, cfg.min, cfg.max, cfg.startAngle, cfg.endAngle);
  const severity = severityFor(clamped, cfg);
  const colors = resolveGaugeColors(cfg, tokens);

  return {
    ...cfg,
    ...layout,
    rawValue: raw,
    value: clamped,
    percent,
    valueAngle,
    severity,
    colors,
    bands: thresholdBands(cfg, tokens),
    ticks: tickMarks(cfg, tokens, layout),
  };
}

/**
 * Threshold/severity bands covering the full gauge range.
 */
export function thresholdBands(config = {}, tokens = {}) {
  const cfg = normalizeGaugeConfig(config);
  const colors = resolveGaugeColors(cfg, tokens);
  const points = [cfg.min, cfg.max];
  if (cfg.warn !== null && cfg.warn > cfg.min && cfg.warn < cfg.max) points.push(cfg.warn);
  if (cfg.crit !== null && cfg.crit > cfg.min && cfg.crit < cfg.max) points.push(cfg.crit);

  const sorted = points
    .slice()
    .sort((a, b) => a - b)
    .filter((point, index, array) => index === 0 || Math.abs(point - array[index - 1]) > 1e-9);

  const bands = [];
  for (let index = 0; index < sorted.length - 1; index += 1) {
    const from = sorted[index];
    const to = sorted[index + 1];
    if (to <= from) continue;
    const severity = severityFor((from + to) / 2, cfg);
    bands.push({
      from,
      to,
      severity,
      color: colors[severity],
      startAngle: valueToAngle(from, cfg.min, cfg.max, cfg.startAngle, cfg.endAngle),
      endAngle: valueToAngle(to, cfg.min, cfg.max, cfg.startAngle, cfg.endAngle),
    });
  }
  return bands;
}

/**
 * Tick line coordinates.  The first and last ticks coincide with the arc
 * endpoints when `tickCount` is greater than one.
 */
export function tickMarks(config = {}, tokens = {}, layout = null) {
  const cfg = normalizeGaugeConfig(config);
  if (cfg.tickCount < 2) return [];

  const arc = layout ?? layoutArc(cfg);
  const colors = resolveGaugeColors(cfg, tokens);
  const ticks = [];
  const inner = Math.max(1, arc.radius - cfg.thickness / 2 - 2);
  const outer = arc.radius + cfg.thickness / 2 + 3;

  for (let index = 0; index < cfg.tickCount; index += 1) {
    const ratio = index / (cfg.tickCount - 1);
    const angle = cfg.startAngle + (cfg.endAngle - cfg.startAngle) * ratio;
    const a = polarPoint(arc.cx, arc.cy, inner, angle);
    const b = polarPoint(arc.cx, arc.cy, outer, angle);
    ticks.push({
      index,
      angle,
      value: cfg.min + (cfg.max - cfg.min) * ratio,
      major: index === 0 || index === cfg.tickCount - 1,
      x1: a.x,
      y1: a.y,
      x2: b.x,
      y2: b.y,
      color: colors.muted,
    });
  }
  return ticks;
}

/**
 * Horizontal bar geometry.  Values outside `[min, max]` are clamped.
 */
export function barGeometry(config = {}, value, tokens = {}) {
  const cfg = normalizeGaugeConfig({ ...config, mode: 'bar' });
  const raw = finite(value, finite(config.value, cfg.min));
  const clamped = clamp(raw, cfg.min, cfg.max);
  const percent = cfg.max === cfg.min
    ? 0
    : clamp((clamped - cfg.min) / (cfg.max - cfg.min), 0, 1) * 100;
  const padding = cfg.padding;
  const trackHeight = positive(
    firstDefined(config.trackHeight, config.track_height),
    Math.max(6, cfg.height * 0.28),
  );
  const x = padding;
  const y = cfg.height - padding - trackHeight;
  const width = Math.max(1, cfg.width - padding * 2);
  const severity = severityFor(clamped, cfg);

  return {
    ...cfg,
    ...resolveGaugeColors(cfg, tokens),
    colors: resolveGaugeColors(cfg, tokens),
    rawValue: raw,
    value: clamped,
    percent,
    severity,
    viewWidth: cfg.width,
    viewHeight: cfg.height,
    x,
    y,
    width,
    height: trackHeight,
    trackHeight,
    radius: cfg.rounded ? trackHeight / 2 : 0,
    fillWidth: width * (percent / 100),
    bands: thresholdBands(cfg, tokens),
  };
}

/**
 * Two-row bar geometry for dual-value widgets.
 */
export function dualBarGeometry(config = {}, values, tokens = {}) {
  const cfg = normalizeGaugeConfig({ ...config, mode: 'dual-bar' });
  const width = cfg.width;
  const height = cfg.height;
  const padding = cfg.padding;
  const gap = finite(firstDefined(config.rowGap, config.row_gap), 8);
  const rowHeight = Math.max(4, (height - padding * 2 - gap) / 2);
  const input = Array.isArray(values)
    ? values
    : [
      values?.value ?? values?.first ?? values?.primary ?? values ?? firstDefined(config.value, cfg.min),
      values?.second
        ?? values?.secondary
        ?? values?.secondValue
        ?? firstDefined(config.secondValue, config.value2, config.second_value, cfg.min),
    ];

  const rows = [0, 1].map((index) => {
    const raw = finite(input[index], cfg.min);
    const clamped = clamp(raw, cfg.min, cfg.max);
    const percent = cfg.max === cfg.min
      ? 0
      : clamp((clamped - cfg.min) / (cfg.max - cfg.min), 0, 1) * 100;
    const severity = severityFor(clamped, cfg);
    const colors = resolveGaugeColors(cfg, tokens);
    return {
      index,
      rawValue: raw,
      value: clamped,
      percent,
      severity,
      x: padding,
      y: padding + index * (rowHeight + gap),
      width: Math.max(1, width - padding * 2),
      height: rowHeight,
      radius: cfg.rounded ? rowHeight / 2 : 0,
      fillWidth: Math.max(1, width - padding * 2) * (percent / 100),
      color: severity === 'normal'
        ? (index === 1 ? colors.second : colors.normal)
        : colors[severity],
      label: String(
        firstDefined(
          Array.isArray(config.labels) ? config.labels[index] : undefined,
          index === 1
            ? firstDefined(config.secondLabel, config.second_label, config.label2, cfg.label)
            : cfg.label,
          '',
        ) ?? '',
      ),
      unit: String(
        firstDefined(
          Array.isArray(config.units) ? config.units[index] : undefined,
          index === 1 ? firstDefined(config.secondUnit, config.second_unit) : undefined,
          cfg.unit,
          '',
        ) ?? '',
      ),
    };
  });

  return {
    ...cfg,
    colors: resolveGaugeColors(cfg, tokens),
    width,
    height,
    rows,
    bands: thresholdBands(cfg, tokens),
  };
}

/* ------------------------------------------------------------------------- */
/* SVG rendering                                                              */
/* ------------------------------------------------------------------------- */

function idSafe(value) {
  return String(value || 'gauge').replace(/[^A-Za-z0-9_-]+/g, '-').slice(0, 64) || 'gauge';
}

function hashText(value) {
  let hash = 5381;
  const text = String(value ?? '');
  for (let index = 0; index < text.length; index += 1) {
    hash = ((hash << 5) + hash + text.charCodeAt(index)) >>> 0;
  }
  return hash.toString(36);
}

function detectReducedMotion(config, options) {
  if (typeof options.reducedMotion === 'boolean') return options.reducedMotion;
  if (typeof config.reducedMotion === 'boolean') return config.reducedMotion;
  if (typeof globalThis.matchMedia === 'function') {
    try {
      return Boolean(globalThis.matchMedia('(prefers-reduced-motion: reduce)').matches);
    } catch {
      return false;
    }
  }
  return false;
}

function ariaLabelFor(label, formatted, unit, severity) {
  const base = [label, `${formatted}${unit ? ` ${unit}` : ''}`].filter(Boolean).join(' ').trim()
    || `Gauge ${formatted}${unit ? ` ${unit}` : ''}`;
  if (severity === 'crit') return `${base}, critical`;
  if (severity === 'warn') return `${base}, warning`;
  return base;
}

function severityWord(severity) {
  if (severity === 'crit') return 'critical';
  if (severity === 'warn') return 'warning';
  return 'normal';
}

function valueClass(config, reducedMotion) {
  const classes = ['ozma-gauge'];
  classes.push(`ozma-gauge-${normalizeMode(config)}`);
  if (reducedMotion) classes.push('ozma-gauge-reduced-motion');
  return classes.join(' ');
}

function transitionStyle(config, reducedMotion) {
  return config.animate && !reducedMotion
    ? ' style="transition: stroke 220ms ease, fill 220ms ease"'
    : '';
}

function textAttributes(colors, fontFamily, size, weight = 400) {
  return `fill="${escapeXml(colors.text)}" font-family="${escapeXml(fontFamily)}" font-size="${numberText(size)}" font-weight="${weight}"`;
}

function renderArc(config, value, tokens, options) {
  const geometry = gaugeGeometry(config, value, tokens);
  const reducedMotion = detectReducedMotion(geometry, options);
  const fontFamily = tokenString(tokens, ['font.family', 'fontFamily', 'typography.family'], 'system-ui, sans-serif');
  const cap = geometry.rounded ? 'round' : 'butt';
  const formatted = formatGaugeValue(geometry.value, geometry);
  const unit = geometry.unit;
  const severity = severityWord(geometry.severity);
  const label = geometry.label;
  const title = label || 'Gauge';
  const description = `${title} reading ${formatted}${unit ? ` ${unit}` : ''}; range ${formatGaugeValue(geometry.min, geometry)} to ${formatGaugeValue(geometry.max, geometry)}${unit ? ` ${unit}` : ''}; ${severity}.`;
  const uid = idSafe(options.id ?? geometry.id ?? `gauge-${hashText(`${title}|${geometry.min}|${geometry.max}`)}`);
  const ariaLabel = ariaLabelFor(label, formatted, unit, geometry.severity);
  const fontSize = Math.max(10, Math.min(30, Math.min(geometry.width, geometry.height) * 0.14));
  const labelSize = Math.max(9, fontSize * 0.52);

  const parts = [];
  parts.push(`<svg xmlns="${SVG_NS}" viewBox="0 0 ${numberText(geometry.width)} ${numberText(geometry.height)}" class="${valueClass(geometry, reducedMotion)}" role="img" aria-label="${escapeXml(ariaLabel)}" aria-describedby="${uid}-desc" data-reduced-motion="${reducedMotion ? 'true' : 'false'}"${geometry.animate ? ' data-animate="true"' : ''} focusable="false" preserveAspectRatio="xMidYMid meet">`);
  parts.push(`<title id="${uid}-title">${escapeXml(title)}</title>`);
  parts.push(`<desc id="${uid}-desc">${escapeXml(description)}</desc>`);

  const track = arcPath({
    cx: geometry.cx,
    cy: geometry.cy,
    radius: geometry.radius,
    startAngle: geometry.startAngle,
    endAngle: geometry.endAngle,
  });
  parts.push(`<path d="${track}" fill="none" stroke="${escapeXml(geometry.colors.track)}" stroke-width="${numberText(geometry.thickness)}" stroke-linecap="${cap}"/>`);

  for (const band of geometry.bands) {
    if (band.severity === 'normal') continue;
    const d = arcPath({
      cx: geometry.cx,
      cy: geometry.cy,
      radius: geometry.radius,
      startAngle: band.startAngle,
      endAngle: band.endAngle,
    });
    parts.push(`<path d="${d}" fill="none" stroke="${escapeXml(band.color)}" stroke-width="${numberText(geometry.thickness)}" stroke-opacity="0.26" stroke-linecap="${cap}"/>`);
  }

  for (const tick of geometry.ticks) {
    parts.push(`<line x1="${numberText(tick.x1)}" y1="${numberText(tick.y1)}" x2="${numberText(tick.x2)}" y2="${numberText(tick.y2)}" stroke="${escapeXml(tick.color)}" stroke-width="${numberText(tick.major ? 1.6 : 1)}" stroke-opacity="0.65" stroke-linecap="round"/>`);
  }

  const valueArc = arcPath({
    cx: geometry.cx,
    cy: geometry.cy,
    radius: geometry.radius,
    startAngle: geometry.startAngle,
    endAngle: geometry.valueAngle,
  });
  parts.push(`<path class="ozma-gauge-value" d="${valueArc}" fill="none" stroke="${escapeXml(geometry.colors[geometry.severity])}" stroke-width="${numberText(geometry.thickness)}" stroke-linecap="${cap}" data-severity="${geometry.severity}"${transitionStyle(geometry, reducedMotion)}/>`);

  if (geometry.showLabel && label) {
    parts.push(`<text x="${numberText(geometry.cx)}" y="${numberText(geometry.cy - fontSize * 0.72)}" text-anchor="middle" dominant-baseline="middle" ${textAttributes(geometry.colors, fontFamily, labelSize, 600)}>${escapeXml(label)}</text>`);
  }
  if (geometry.showValue) {
    parts.push(`<text x="${numberText(geometry.cx)}" y="${numberText(geometry.cy + (label ? fontSize * 0.28 : 0))}" text-anchor="middle" dominant-baseline="middle" ${textAttributes(geometry.colors, fontFamily, fontSize, 650)}>${escapeXml(`${formatted}${unit ? ` ${unit}` : ''}`)}</text>`);
  }

  parts.push('</svg>');
  return parts.join('');
}

function renderBar(config, value, tokens, options) {
  const geometry = barGeometry(config, value, tokens);
  const reducedMotion = detectReducedMotion(geometry, options);
  const fontFamily = tokenString(tokens, ['font.family', 'fontFamily', 'typography.family'], 'system-ui, sans-serif');
  const formatted = formatGaugeValue(geometry.value, geometry);
  const unit = geometry.unit;
  const label = geometry.label;
  const title = label || 'Bar';
  const description = `${title} reading ${formatted}${unit ? ` ${unit}` : ''}; range ${formatGaugeValue(geometry.min, geometry)} to ${formatGaugeValue(geometry.max, geometry)}${unit ? ` ${unit}` : ''}; ${severityWord(geometry.severity)}.`;
  const uid = idSafe(options.id ?? geometry.id ?? `bar-${hashText(`${title}|${geometry.min}|${geometry.max}`)}`);
  const ariaLabel = ariaLabelFor(label, formatted, unit, geometry.severity);
  const fontSize = Math.max(9, Math.min(15, geometry.viewHeight * 0.22));
  const textY = Math.max(fontSize, geometry.padding + fontSize);

  const parts = [];
  parts.push(`<svg xmlns="${SVG_NS}" viewBox="0 0 ${numberText(geometry.viewWidth)} ${numberText(geometry.viewHeight)}" class="${valueClass({ ...geometry, mode: 'bar' }, reducedMotion)}" role="img" aria-label="${escapeXml(ariaLabel)}" aria-describedby="${uid}-desc" data-reduced-motion="${reducedMotion ? 'true' : 'false'}"${geometry.animate ? ' data-animate="true"' : ''} focusable="false" preserveAspectRatio="xMidYMid meet">`);
  parts.push(`<title id="${uid}-title">${escapeXml(title)}</title>`);
  parts.push(`<desc id="${uid}-desc">${escapeXml(description)}</desc>`);

  parts.push(`<rect x="${numberText(geometry.x)}" y="${numberText(geometry.y)}" width="${numberText(geometry.width)}" height="${numberText(geometry.trackHeight)}" rx="${numberText(geometry.radius)}" fill="${escapeXml(geometry.track)}"/>`);

  for (const band of geometry.bands) {
    if (band.severity === 'normal') continue;
    const bandStart = geometry.x + geometry.width * ((band.from - geometry.min) / (geometry.max - geometry.min));
    const bandWidth = geometry.width * ((band.to - band.from) / (geometry.max - geometry.min));
    parts.push(`<rect x="${numberText(bandStart)}" y="${numberText(geometry.y)}" width="${numberText(Math.max(0, bandWidth))}" height="${numberText(geometry.trackHeight)}" fill="${escapeXml(band.color)}" fill-opacity="0.24"/>`);
  }

  parts.push(`<rect class="ozma-gauge-value" x="${numberText(geometry.x)}" y="${numberText(geometry.y)}" width="${numberText(geometry.fillWidth)}" height="${numberText(geometry.trackHeight)}" rx="${numberText(geometry.radius)}" fill="${escapeXml(geometry.colors[geometry.severity])}" data-severity="${geometry.severity}"${transitionStyle(geometry, reducedMotion)}/>`);

  if (geometry.showLabel && label) {
    parts.push(`<text x="${numberText(geometry.padding)}" y="${numberText(textY)}" text-anchor="start" ${textAttributes(geometry.colors, fontFamily, fontSize, 600)}>${escapeXml(label)}</text>`);
  }
  if (geometry.showValue) {
    parts.push(`<text x="${numberText(geometry.viewWidth - geometry.padding)}" y="${numberText(textY)}" text-anchor="end" ${textAttributes(geometry.colors, fontFamily, fontSize, 650)}>${escapeXml(`${formatted}${unit ? ` ${unit}` : ''}`)}</text>`);
  }

  parts.push('</svg>');
  return parts.join('');
}

function renderDualBar(config, values, tokens, options) {
  const geometry = dualBarGeometry(config, values, tokens);
  const reducedMotion = detectReducedMotion(geometry, options);
  const fontFamily = tokenString(tokens, ['font.family', 'fontFamily', 'typography.family'], 'system-ui, sans-serif');
  const label = geometry.label || 'Dual bar';
  const description = geometry.rows.map((row) => {
    const text = formatGaugeValue(row.value, geometry);
    return `${row.label || 'value'} ${text}${row.unit ? ` ${row.unit}` : ''} (${severityWord(row.severity)})`;
  }).join('; ');
  const uid = idSafe(options.id ?? geometry.id ?? `dual-bar-${hashText(`${label}|${geometry.min}|${geometry.max}`)}`);
  const ariaLabel = `${label}: ${geometry.rows.map((row) => {
    const text = formatGaugeValue(row.value, geometry);
    return `${row.label ? `${row.label} ` : ''}${text}${row.unit ? ` ${row.unit}` : ''}`;
  }).join(', ')}`;
  const fontSize = Math.max(8, Math.min(13, geometry.height * 0.14));

  const parts = [];
  parts.push(`<svg xmlns="${SVG_NS}" viewBox="0 0 ${numberText(geometry.width)} ${numberText(geometry.height)}" class="${valueClass({ ...geometry, mode: 'dual-bar' }, reducedMotion)}" role="img" aria-label="${escapeXml(ariaLabel)}" aria-describedby="${uid}-desc" data-reduced-motion="${reducedMotion ? 'true' : 'false'}"${geometry.animate ? ' data-animate="true"' : ''} focusable="false" preserveAspectRatio="xMidYMid meet">`);
  parts.push(`<title id="${uid}-title">${escapeXml(label)}</title>`);
  parts.push(`<desc id="${uid}-desc">${escapeXml(description)}</desc>`);

  for (const row of geometry.rows) {
    parts.push(`<rect x="${numberText(row.x)}" y="${numberText(row.y)}" width="${numberText(row.width)}" height="${numberText(row.height)}" rx="${numberText(row.radius)}" fill="${escapeXml(geometry.colors.track)}"/>`);

    for (const band of geometry.bands) {
      if (band.severity === 'normal') continue;
      const bandStart = row.x + row.width * ((band.from - geometry.min) / (geometry.max - geometry.min));
      const bandWidth = row.width * ((band.to - band.from) / (geometry.max - geometry.min));
      parts.push(`<rect x="${numberText(bandStart)}" y="${numberText(row.y)}" width="${numberText(Math.max(0, bandWidth))}" height="${numberText(row.height)}" fill="${escapeXml(band.color)}" fill-opacity="0.24"/>`);
    }

    parts.push(`<rect class="ozma-gauge-value" x="${numberText(row.x)}" y="${numberText(row.y)}" width="${numberText(row.fillWidth)}" height="${numberText(row.height)}" rx="${numberText(row.radius)}" fill="${escapeXml(row.color)}" data-severity="${row.severity}"${transitionStyle(geometry, reducedMotion)}/>`);

    const textY = row.y + Math.min(row.height - 2, fontSize + 2);
    const valueText = `${formatGaugeValue(row.value, geometry)}${row.unit ? ` ${row.unit}` : ''}`;
    if (geometry.showLabel && row.label) {
      parts.push(`<text x="${numberText(row.x + 2)}" y="${numberText(textY)}" text-anchor="start" ${textAttributes(geometry.colors, fontFamily, fontSize, 600)}>${escapeXml(row.label)}</text>`);
    }
    if (geometry.showValue) {
      parts.push(`<text x="${numberText(row.x + row.width - 2)}" y="${numberText(textY)}" text-anchor="end" ${textAttributes(geometry.colors, fontFamily, fontSize, 650)}>${escapeXml(valueText)}</text>`);
    }
  }

  parts.push('</svg>');
  return parts.join('');
}

/**
 * Render a gauge to an SVG string.  Pure: no DOM access, no mutation.
 */
export function renderGaugeSVG(config = {}, value, tokens = {}, options = {}) {
  const cfg = normalizeGaugeConfig(config);
  if (cfg.mode === 'bar') return renderBar(cfg, value, tokens, options);
  if (cfg.mode === 'dual-bar') {
    return renderDualBar(cfg, value ?? cfg.value, tokens, options);
  }
  return renderArc(cfg, value, tokens, options);
}

/**
 * Render accessible SVG into a container.  Returns the created element when
 * the environment exposes one (browsers do), otherwise the SVG string.
 */
export function renderGauge(container, config = {}, value, tokens = {}, options = {}) {
  if (!container || typeof container !== 'object') {
    throw new TypeError('renderGauge requires a container element');
  }

  const markup = renderGaugeSVG(config, value, tokens, options);
  container.innerHTML = markup;
  const element = container.firstElementChild
    ?? (typeof container.querySelector === 'function' ? container.querySelector('svg') : null);
  return element || markup;
}
