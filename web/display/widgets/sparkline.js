// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Dependency-free SVG sparkline/history widget.
 *
 * The module exposes pure helpers for tests (`normalizeHistory`,
 * `sparklineGeometry`, `sparklinePath`, `areaPath`, `thresholdSegments`,
 * `severityFor`, `renderSparklineSVG`) and a small DOM helper that writes
 * accessible SVG into a container.  It has no external dependencies, honours
 * reduced-motion preferences, and supports gaps, auto/explicit ranges,
 * threshold colours, area fill, baselines, ticks, and theme tokens.
 */

const SVG_NS = 'http://www.w3.org/2000/svg';

const DEFAULT_WIDTH = 240;
const DEFAULT_HEIGHT = 64;
const DEFAULT_HISTORY_POINTS = 4096;

const DEFAULT_COLORS = Object.freeze({
  line: '#5b6fff',
  normal: '#5b6fff',
  warn: '#f0b94a',
  crit: '#f26464',
  fill: '#5b6fff',
  track: 'rgba(255, 255, 255, 0.12)',
  grid: 'rgba(255, 255, 255, 0.12)',
  baseline: 'rgba(255, 255, 255, 0.28)',
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

function idSafe(value) {
  return String(value || 'sparkline')
    .replace(/[^A-Za-z0-9_-]+/g, '-')
    .slice(0, 64) || 'sparkline';
}

function hashText(value) {
  let hash = 5381;
  const text = String(value ?? '');
  for (let index = 0; index < text.length; index += 1) {
    hash = ((hash << 5) + hash + text.charCodeAt(index)) >>> 0;
  }
  return hash.toString(36);
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

/* ------------------------------------------------------------------------- */
/* Token resolution                                                           */
/* ------------------------------------------------------------------------- */

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

function parseReference(body) {
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
  if (typeof value === 'string') return resolveString(value, tokens, depth + 1);
  return value;
}

function resolveString(text, tokens, depth = 0) {
  if (typeof text !== 'string') return text;
  if (depth > 12) return text;

  const full = text.match(/^\{([^{}]+)\}$/);
  if (full) {
    const { key, fallback } = parseReference(full[1]);
    const value = lookupTokenValue(key, tokens, depth + 1);
    if (value !== undefined) return value;
    return fallback ? resolveString(fallback, tokens, depth + 1) : text;
  }

  return text.replace(REFERENCE_RE, (match, body) => {
    const { key, fallback } = parseReference(body);
    const value = lookupTokenValue(key, tokens, depth + 1);
    if (value !== undefined) {
      return Array.isArray(value) ? value.join(', ') : String(value);
    }
    return fallback ? String(resolveString(fallback, tokens, depth + 1)) : match;
  });
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

function resolveColour(value, tokens) {
  if (value === undefined || value === null || value === '') return undefined;
  if (typeof value === 'string') return String(resolveString(value, tokens));
  return String(value);
}

/* ------------------------------------------------------------------------- */
/* Value formatting and severity                                              */
/* ------------------------------------------------------------------------- */

/**
 * Format a sparkline value using explicit `decimals`, or trimmed decimals.
 */
export function formatSparklineValue(value, config = {}) {
  const number = Number(value);
  if (!Number.isFinite(number)) return '—';
  if (Number.isInteger(config.decimals)) {
    const decimals = clampInteger(config.decimals, 0, 6, 0);
    return number.toFixed(decimals);
  }
  if (Number.isInteger(number)) return String(number);
  return number.toFixed(2).replace(/0+$/, '').replace(/\.$/, '');
}

/**
 * Severity for a value.  `crit` wins over `warn`; both are upper thresholds.
 */
export function severityFor(value, config = {}, critArg) {
  const number = Number(value);
  if (!Number.isFinite(number)) return 'normal';
  const source = typeof config === 'number' ? { warn: config, crit: critArg } : config;
  const warn = finite(firstDefined(source.warn, source.warn_at, source.warnAt), null);
  const crit = finite(
    firstDefined(source.crit, source.critical, source.crit_at, source.critAt),
    null,
  );
  if (crit !== null && number >= crit) return 'crit';
  if (warn !== null && number >= warn) return 'warn';
  return 'normal';
}

function severityWord(severity) {
  if (severity === 'crit') return 'critical';
  if (severity === 'warn') return 'warning';
  return 'normal';
}

function severityColour(severity, colors) {
  if (severity === 'crit') return colors.crit;
  if (severity === 'warn') return colors.warn;
  return colors.line || colors.normal;
}

/* ------------------------------------------------------------------------- */
/* History and geometry                                                       */
/* ------------------------------------------------------------------------- */

function historySamples(history) {
  if (Array.isArray(history)) return history;
  if (ArrayBuffer.isView(history)) return Array.from(history);
  if (isPlainObject(history)) {
    for (const key of ['samples', 'values', 'history', 'data', 'points']) {
      if (history[key] !== undefined) return historySamples(history[key]);
    }
  }
  if (typeof history === 'number' || typeof history === 'bigint') return [history];
  return [];
}

/**
 * Normalise a history sequence into numbers or `null` gaps.
 *
 * Accepts arrays, typed arrays, `{samples|values|history|data|points}` objects,
 * and a single numeric sample. Missing, non-finite, and non-numeric samples
 * become `null` so geometry can render a gap rather than invent a value.
 */
export function normalizeHistory(history, options = {}) {
  const settings = typeof options === 'number' || typeof options === 'string'
    ? { points: options }
    : options;
  let samples = historySamples(history).map((sample) => {
    if (sample === undefined || sample === null || sample === '') return null;
    if (typeof sample === 'boolean') return sample ? 1 : 0;
    const number = Number(sample);
    return Number.isFinite(number) ? number : null;
  });

  const limit = firstDefined(settings.points, settings.window, settings.capacity, settings.limit);
  const count = clampInteger(limit, 1, DEFAULT_HISTORY_POINTS, null);
  if (count !== null && samples.length > count) samples = samples.slice(-count);
  return samples;
}

function normalizePadding(value) {
  const defaultPadding = { top: 6, right: 6, bottom: 6, left: 6 };
  if (typeof value === 'number' && Number.isFinite(value) && value >= 0) {
    return { top: value, right: value, bottom: value, left: value };
  }
  if (!isPlainObject(value)) return defaultPadding;
  return {
    top: Math.max(0, finite(value.top, defaultPadding.top)),
    right: Math.max(0, finite(value.right, defaultPadding.right)),
    bottom: Math.max(0, finite(value.bottom, defaultPadding.bottom)),
    left: Math.max(0, finite(value.left, defaultPadding.left)),
  };
}

/**
 * Normalise widget configuration.  Unknown fields are retained.
 */
export function normalizeSparklineConfig(config = {}) {
  const width = positive(firstDefined(config.width, config.w), DEFAULT_WIDTH);
  const height = positive(firstDefined(config.height, config.h), DEFAULT_HEIGHT);
  const chartMark = String(
    firstDefined(config.mark, config.chartMark, config.chart_mark, 'line'),
  ).toLowerCase();

  return {
    ...config,
    width,
    height,
    min: finite(config.min, null),
    max: finite(config.max, null),
    warn: finite(firstDefined(config.warn, config.warn_at, config.warnAt), null),
    crit: finite(
      firstDefined(config.crit, config.critical, config.crit_at, config.critAt),
      null,
    ),
    auto: config.auto === true,
    padding: normalizePadding(config.padding),
    lineWidth: positive(
      firstDefined(config.lineWidth, config.line_width, config.strokeWidth, config.stroke_width),
      2,
    ),
    decimals: Number.isInteger(config.decimals)
      ? clampInteger(config.decimals, 0, 6, 2)
      : undefined,
    unit: String(firstDefined(config.unit, '') ?? ''),
    label: String(firstDefined(config.label, config.title, config.name, '') ?? ''),
    tickCount: clampInteger(
      firstDefined(config.ticks, config.tickCount, config.tick_count),
      0,
      20,
      0,
    ),
    showTicks: config.showTicks !== false && config.show_ticks !== false,
    showTickLabels: config.showTickLabels === true || config.tickLabels === true,
    showValue: config.showValue === true || config.show_value === true,
    showArea: config.area === true
      || config.areaFill === true
      || config.area_fill === true
      || config.fill === true
      || typeof config.fill === 'string',
    areaOpacity: clamp(
      finite(firstDefined(config.areaOpacity, config.fillOpacity, config.fill_opacity), 0.18),
      0,
      1,
    ),
    showBaseline: config.baseline !== undefined
      && config.baseline !== null
      && config.baseline !== false,
    showThresholds: config.showThresholds !== false && config.show_thresholds !== false,
    baseline: config.baseline,
    chartMark: ['line', 'area', 'step', 'dot', 'bar'].includes(chartMark) ? chartMark : 'line',
    animate: Boolean(firstDefined(config.animate, config.animated, false)),
  };
}

/**
 * Resolve sparkline colours from explicit config, theme tokens, then defaults.
 */
export function resolveSparklineColors(config = {}, tokens = {}) {
  const explicit = config.colors && typeof config.colors === 'object' ? config.colors : {};
  const line = resolveColour(firstDefined(
    explicit.line,
    explicit.stroke,
    config.lineColor,
    config.line_color,
    config.color,
    tokenValue(tokens, ['sparkline.line', 'sparkline.stroke', 'gauge.fill', 'color.accent']),
  ), tokens) ?? DEFAULT_COLORS.line;

  const fill = resolveColour(firstDefined(
    explicit.fill,
    typeof config.fill === 'string' ? config.fill : undefined,
    config.fillColor,
    config.fill_color,
    tokenValue(tokens, ['sparkline.fill', 'sparkline.area', 'color.accent']),
  ), tokens) ?? line;

  return {
    line,
    normal: line,
    warn: resolveColour(firstDefined(
      explicit.warn,
      config.warnColor,
      config.warn_color,
      tokenValue(tokens, ['sparkline.warn', 'gauge.warn', 'color.warn']),
    ), tokens) ?? DEFAULT_COLORS.warn,
    crit: resolveColour(firstDefined(
      explicit.crit,
      explicit.critical,
      config.critColor,
      config.crit_color,
      tokenValue(tokens, ['sparkline.crit', 'gauge.crit', 'color.crit', 'color.error']),
    ), tokens) ?? DEFAULT_COLORS.crit,
    fill,
    track: resolveColour(firstDefined(
      explicit.track,
      config.track,
      tokenValue(tokens, ['sparkline.track', 'gauge.track', 'color.track']),
    ), tokens) ?? DEFAULT_COLORS.track,
    grid: resolveColour(firstDefined(
      explicit.grid,
      explicit.ticks,
      tokenValue(tokens, ['sparkline.grid', 'sparkline.tick', 'color.grid', 'color.track']),
    ), tokens) ?? DEFAULT_COLORS.grid,
    baseline: resolveColour(firstDefined(
      explicit.baseline,
      tokenValue(tokens, ['sparkline.baseline', 'color.dim', 'color.muted']),
    ), tokens) ?? DEFAULT_COLORS.baseline,
    text: resolveColour(firstDefined(
      explicit.text,
      tokenValue(tokens, ['sparkline.text', 'gauge.text', 'color.fg', 'color.text']),
    ), tokens) ?? DEFAULT_COLORS.text,
    muted: resolveColour(firstDefined(
      explicit.muted,
      tokenValue(tokens, ['sparkline.muted', 'gauge.muted', 'color.muted', 'color.dim']),
    ), tokens) ?? DEFAULT_COLORS.muted,
  };
}

function baselineValue(value, min, max) {
  if (value === undefined || value === null || value === false) return null;
  if (value === true || value === 'min') return min;
  if (value === 'max') return max;
  if (value === 'zero' || value === 'origin') return clamp(0, min, max);
  const number = Number(value);
  return Number.isFinite(number) ? clamp(number, min, max) : min;
}

/**
 * Build the full sparkline geometry: viewport, range, points, contiguous
 * segments (gaps split segments), baseline, colours, ticks, and current value.
 */
export function sparklineGeometry(config = {}, history = [], tokens = {}) {
  let cfg = normalizeSparklineConfig(config);
  if (
    config.lineWidth === undefined
    && config.line_width === undefined
    && config.strokeWidth === undefined
    && config.stroke_width === undefined
  ) {
    const tokenWidth = Number.parseFloat(String(tokenValue(
      tokens,
      ['sparkline.width', 'sparkline.lineWidth', 'sparkline.line_width', 'sparkline.strokeWidth'],
      '',
    ) ?? ''));
    if (Number.isFinite(tokenWidth) && tokenWidth > 0) {
      cfg = { ...cfg, lineWidth: tokenWidth };
    }
  }
  if (
    config.areaOpacity === undefined
    && config.fillOpacity === undefined
    && config.fill_opacity === undefined
  ) {
    const tokenOpacity = Number(tokenValue(
      tokens,
      ['sparkline.area-opacity', 'sparkline.areaOpacity', 'sparkline.fill-opacity'],
      Number.NaN,
    ));
    if (Number.isFinite(tokenOpacity)) {
      cfg = { ...cfg, areaOpacity: clamp(tokenOpacity, 0, 1) };
    }
  }
  const limit = firstDefined(
    cfg.historyPoints,
    cfg.history_points,
    cfg.points,
    cfg.window,
    cfg.history && typeof cfg.history === 'object' ? firstDefined(cfg.history.points, cfg.history.window) : undefined,
  );
  const values = normalizeHistory(history, { points: limit });
  const defined = values.filter((value) => value !== null);
  const autoMin = defined.length ? Math.min(...defined) : 0;
  const autoMax = defined.length ? Math.max(...defined) : 1;

  let min = cfg.auto || !Number.isFinite(cfg.min) ? autoMin : cfg.min;
  let max = cfg.auto || !Number.isFinite(cfg.max) ? autoMax : cfg.max;
  if (!Number.isFinite(min)) min = 0;
  if (!Number.isFinite(max)) max = 1;
  if (max <= min) max = min + 1;

  const { width, height, padding } = cfg;
  const left = Math.min(padding.left, width - 1);
  const right = Math.max(left + 1, width - padding.right);
  const top = Math.min(padding.top, height - 1);
  const bottom = Math.max(top + 1, height - padding.bottom);
  const plotWidth = Math.max(1, right - left);
  const plotHeight = Math.max(1, bottom - top);
  const step = values.length > 1 ? plotWidth / (values.length - 1) : 0;

  const points = values.map((value, index) => {
    const x = values.length === 1 ? left + plotWidth / 2 : left + step * index;
    if (value === null) {
      return {
        index,
        value: null,
        clamped: null,
        defined: false,
        x,
        y: null,
        severity: 'normal',
      };
    }
    const clamped = cfg.clampValues === false ? value : clamp(value, min, max);
    const ratio = max === min ? 0 : (clamped - min) / (max - min);
    return {
      index,
      value,
      clamped,
      defined: true,
      x,
      y: bottom - ratio * plotHeight,
      severity: severityFor(clamped, cfg),
    };
  });

  const segments = [];
  let current = [];
  for (const point of points) {
    if (point.defined) {
      current.push(point);
    } else if (current.length) {
      segments.push(current);
      current = [];
    }
  }
  if (current.length) segments.push(current);

  const baseline = baselineValue(cfg.baseline, min, max);
  const baselineY = baseline === null ? null : bottom - ((baseline - min) / (max - min)) * plotHeight;
  const definedPoints = points.filter((point) => point.defined);
  const latest = definedPoints.length ? definedPoints[definedPoints.length - 1] : null;
  const colors = resolveSparklineColors(cfg, tokens);
  const severity = latest ? latest.severity : 'normal';
  const tickValues = [];
  if (cfg.tickCount > 0) {
    for (let index = 0; index < cfg.tickCount; index += 1) {
      const ratio = cfg.tickCount === 1 ? 0 : index / (cfg.tickCount - 1);
      const value = min + (max - min) * ratio;
      tickValues.push({
        index,
        value,
        y: bottom - ratio * plotHeight,
      });
    }
  }

  return {
    ...cfg,
    values,
    points,
    segments,
    definedPoints,
    latest,
    current: latest ? latest.value : null,
    min,
    max,
    autoMin,
    autoMax,
    left,
    right,
    top,
    bottom,
    plotWidth,
    plotHeight,
    step,
    baseline,
    baselineY,
    colors,
    severity,
    tickValues,
    gapCount: values.length - defined.length,
    definedCount: defined.length,
  };
}

function pointsToPath(points, options = {}) {
  const clean = (Array.isArray(points) ? points : []).filter(
    (point) => point && Number.isFinite(point.x) && Number.isFinite(point.y),
  );
  if (!clean.length) return '';
  const parts = [`M ${numberText(clean[0].x)} ${numberText(clean[0].y)}`];
  for (let index = 1; index < clean.length; index += 1) {
    const point = clean[index];
    if (options.step) {
      const previous = clean[index - 1];
      parts.push(`L ${numberText(point.x)} ${numberText(previous.y)}`);
    }
    parts.push(`L ${numberText(point.x)} ${numberText(point.y)}`);
  }
  return parts.join(' ');
}

function segmentsFrom(value) {
  if (Array.isArray(value)) {
    if (!value.length) return [];
    if (Array.isArray(value[0])) return value.filter((segment) => Array.isArray(segment));
    return [value];
  }
  if (value && Array.isArray(value.segments)) {
    return value.segments.filter((segment) => Array.isArray(segment));
  }
  return [];
}

/**
 * Build one SVG path for all contiguous segments.  Gaps remain separate
 * subpaths so the line never connects across missing samples.
 */
export function sparklinePath(geometryOrPoints, options = {}) {
  const settings = typeof options === 'boolean'
    ? { step: options }
    : options === 'step'
      ? { step: true }
      : options;
  const segments = segmentsFrom(geometryOrPoints);
  const step = settings.step ?? geometryOrPoints?.chartMark === 'step';
  return segments
    .map((segment) => pointsToPath(segment, { step }))
    .filter(Boolean)
    .join(' ');
}

/**
 * Build an area-fill path from the sparkline geometry, closing each segment
 * against its baseline (or the plot bottom when no baseline is configured).
 */
export function areaPath(geometryOrPoints, options = {}) {
  const settings = typeof options === 'number' ? { baseline: options } : options;
  const segments = segmentsFrom(geometryOrPoints);
  const baseline = Number.isFinite(settings.baseline)
    ? settings.baseline
    : Number.isFinite(geometryOrPoints?.baselineY)
      ? geometryOrPoints.baselineY
      : Number.isFinite(geometryOrPoints?.bottom)
        ? geometryOrPoints.bottom
        : 0;

  return segments.map((segment) => {
    const clean = segment.filter(
      (point) => point && Number.isFinite(point.x) && Number.isFinite(point.y),
    );
    if (!clean.length) return '';
    const parts = [
      `M ${numberText(clean[0].x)} ${numberText(baseline)}`,
      `L ${numberText(clean[0].x)} ${numberText(clean[0].y)}`,
    ];
    for (let index = 1; index < clean.length; index += 1) {
      parts.push(`L ${numberText(clean[index].x)} ${numberText(clean[index].y)}`);
    }
    parts.push(`L ${numberText(clean[clean.length - 1].x)} ${numberText(baseline)} Z`);
    return parts.join(' ');
  }).filter(Boolean).join(' ');
}

function crossingPoint(first, second, threshold) {
  const span = second.clamped - first.clamped;
  const ratio = span === 0 ? 0 : (threshold - first.clamped) / span;
  return {
    index: first.index,
    value: threshold,
    clamped: threshold,
    defined: true,
    x: first.x + (second.x - first.x) * ratio,
    y: first.y + (second.y - first.y) * ratio,
  };
}

/**
 * Split the line into runs of constant severity, inserting interpolated
 * points where a segment crosses `warn` or `crit`.  Each run is a separate
 * path so threshold colours are exact rather than block-coloured by sample.
 */
export function thresholdSegments(geometry) {
  const segments = geometry?.segments ?? [];
  const warn = finite(firstDefined(geometry?.warn, geometry?.warn_at), null);
  const crit = finite(firstDefined(geometry?.crit, geometry?.critical), null);
  const thresholds = [warn, crit].filter((value) => value !== null).sort((a, b) => a - b);
  const runs = [];

  segments.forEach((segment, segmentIndex) => {
    const clean = segment.filter((point) => point && point.defined);
    if (!clean.length) return;
    if (clean.length === 1) {
      const severity = severityFor(clean[0].clamped, geometry);
      runs.push({
        segmentIndex,
        severity,
        color: severityColour(severity, geometry.colors ?? DEFAULT_COLORS),
        points: clean.slice(),
        path: pointsToPath(clean, { step: geometry?.chartMark === 'step' }),
      });
      return;
    }

    let run = null;
    for (let index = 0; index < clean.length - 1; index += 1) {
      const first = clean[index];
      const second = clean[index + 1];
      const cuts = thresholds
        .filter((threshold) => (
          (first.clamped < threshold && second.clamped > threshold)
          || (first.clamped > threshold && second.clamped < threshold)
        ))
        .map((threshold) => crossingPoint(first, second, threshold))
        .sort((a, b) => (first.x <= second.x ? a.x - b.x : b.x - a.x));
      const points = [first, ...cuts, second];

      for (let part = 0; part < points.length - 1; part += 1) {
        const start = points[part];
        const end = points[part + 1];
        const severity = severityFor((start.clamped + end.clamped) / 2, geometry);
        if (!run || run.severity !== severity) {
          if (run) {
            run.path = pointsToPath(run.points, { step: geometry?.chartMark === 'step' });
            runs.push(run);
          }
          run = {
            segmentIndex,
            severity,
            color: severityColour(severity, geometry.colors ?? DEFAULT_COLORS),
            points: [start],
          };
        } else if (run.points[run.points.length - 1] !== start) {
          run.points.push(start);
        }
        run.points.push(end);
      }
    }
    if (run) {
      run.path = pointsToPath(run.points, { step: geometry?.chartMark === 'step' });
      runs.push(run);
    }
  });

  return runs;
}

/* ------------------------------------------------------------------------- */
/* SVG rendering                                                              */
/* ------------------------------------------------------------------------- */

function detectReducedMotion(config = {}, options = {}) {
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

function ariaLabelFor(label, formatted, unit, severity, hasData) {
  if (!hasData) return `${label || 'Sparkline'}: no data`;
  const base = [label || 'Sparkline', `${formatted}${unit ? ` ${unit}` : ''}`]
    .filter(Boolean)
    .join(' ')
    .trim();
  if (severity === 'crit') return `${base}, critical`;
  if (severity === 'warn') return `${base}, warning`;
  return base;
}

function sparklineClass(geometry, reducedMotion) {
  const classes = ['ozma-sparkline', `ozma-sparkline-${geometry.chartMark || 'line'}`];
  if (reducedMotion) classes.push('ozma-sparkline-reduced-motion');
  return classes.join(' ');
}

function transitionStyle(geometry, reducedMotion) {
  return geometry.animate && !reducedMotion
    ? ' style="transition: stroke 200ms ease, fill 200ms ease, opacity 200ms ease"'
    : '';
}

function thresholdLines(geometry) {
  const lines = [];
  for (const value of [geometry.warn, geometry.crit]) {
    if (!Number.isFinite(value) || value < geometry.min || value > geometry.max) continue;
    const y = geometry.bottom - ((value - geometry.min) / (geometry.max - geometry.min)) * geometry.plotHeight;
    const severity = severityFor(value, geometry);
    lines.push({
      value,
      y,
      severity,
      color: severityColour(severity, geometry.colors),
    });
  }
  return lines;
}

function renderGrid(geometry, fontFamily) {
  if (!geometry.showTicks || geometry.tickCount <= 0) return '';
  const parts = [];
  for (const tick of geometry.tickValues) {
    parts.push(`<line class="ozma-sparkline-tick" x1="${numberText(geometry.left)}" y1="${numberText(tick.y)}" x2="${numberText(geometry.right)}" y2="${numberText(tick.y)}" stroke="${escapeXml(geometry.colors.grid)}" stroke-width="1" stroke-opacity="0.7"/>`);
    if (geometry.showTickLabels) {
      parts.push(`<text class="ozma-sparkline-tick-label" x="${numberText(geometry.left)}" y="${numberText(Math.max(8, tick.y - 2))}" fill="${escapeXml(geometry.colors.muted)}" font-family="${escapeXml(fontFamily)}" font-size="8">${escapeXml(formatSparklineValue(tick.value, geometry))}</text>`);
    }
  }
  return `<g class="ozma-sparkline-grid">${parts.join('')}</g>`;
}

function renderThresholds(geometry) {
  if (!geometry.showThresholds) return '';
  const lines = thresholdLines(geometry);
  if (!lines.length) return '';
  return `<g class="ozma-sparkline-thresholds">${lines.map((line) => `<line class="ozma-sparkline-threshold" x1="${numberText(geometry.left)}" y1="${numberText(line.y)}" x2="${numberText(geometry.right)}" y2="${numberText(line.y)}" stroke="${escapeXml(line.color)}" stroke-width="1" stroke-opacity="0.55" stroke-dasharray="3 4" data-severity="${line.severity}"/>`).join('')}</g>`;
}

/**
 * Render a sparkline to an SVG string.  Pure: no DOM access, no mutation.
 */
export function renderSparklineSVG(config = {}, history = [], tokens = {}, options = {}) {
  const geometry = sparklineGeometry(config, history, tokens);
  const reducedMotion = detectReducedMotion(geometry, options);
  const fontFamily = tokenString(tokens, ['font.family', 'fontFamily', 'typography.family'], 'system-ui, sans-serif');
  const hasData = geometry.definedCount > 0;
  const formatted = hasData ? formatSparklineValue(geometry.current, geometry) : '—';
  const unit = geometry.unit;
  const title = geometry.label || 'Sparkline';
  const uid = idSafe(options.id ?? geometry.id ?? `sparkline-${hashText(`${title}|${geometry.min}|${geometry.max}|${geometry.width}x${geometry.height}`)}`);
  const ariaLabel = ariaLabelFor(geometry.label, formatted, unit, geometry.severity, hasData);
  const description = hasData
    ? `${title}: ${geometry.values.length} samples; current ${formatted}${unit ? ` ${unit}` : ''}; range ${formatSparklineValue(geometry.min, geometry)} to ${formatSparklineValue(geometry.max, geometry)}${unit ? ` ${unit}` : ''}; ${severityWord(geometry.severity)}${geometry.gapCount ? `; ${geometry.gapCount} missing sample${geometry.gapCount === 1 ? '' : 's'}` : ''}.`
    : `${title}: no data.`;
  const valueSize = Math.max(9, Math.min(14, geometry.height * 0.16));

  const parts = [];
  parts.push(`<svg xmlns="${SVG_NS}" viewBox="0 0 ${numberText(geometry.width)} ${numberText(geometry.height)}" class="${sparklineClass(geometry, reducedMotion)}" role="img" aria-label="${escapeXml(ariaLabel)}" aria-describedby="${uid}-desc" data-reduced-motion="${reducedMotion ? 'true' : 'false'}"${geometry.animate ? ' data-animate="true"' : ''} data-severity="${geometry.severity}" focusable="false" preserveAspectRatio="xMidYMid meet"${transitionStyle(geometry, reducedMotion)}>`);
  parts.push(`<title id="${uid}-title">${escapeXml(title)}</title>`);
  parts.push(`<desc id="${uid}-desc">${escapeXml(description)}</desc>`);
  parts.push(renderGrid(geometry, fontFamily));
  parts.push(renderThresholds(geometry));

  if (geometry.showArea && geometry.segments.length) {
    parts.push(`<path class="ozma-sparkline-area" d="${areaPath(geometry)}" fill="${escapeXml(geometry.colors.fill)}" fill-opacity="${numberText(geometry.areaOpacity)}" stroke="none"/>`);
  }

  const runs = thresholdSegments(geometry);
  if (runs.length) {
    parts.push(`<g class="ozma-sparkline-lines">${runs.map((run) => `<path class="ozma-sparkline-line" d="${run.path}" fill="none" stroke="${escapeXml(run.color)}" stroke-width="${numberText(geometry.lineWidth)}" stroke-linecap="round" stroke-linejoin="round" data-severity="${run.severity}" vector-effect="non-scaling-stroke"/>`).join('')}</g>`);
  }

  if (geometry.showBaseline && Number.isFinite(geometry.baselineY)) {
    parts.push(`<line class="ozma-sparkline-baseline" x1="${numberText(geometry.left)}" y1="${numberText(geometry.baselineY)}" x2="${numberText(geometry.right)}" y2="${numberText(geometry.baselineY)}" stroke="${escapeXml(geometry.colors.baseline)}" stroke-width="1" stroke-opacity="0.8" stroke-dasharray="2 4"/>`);
  }

  if (geometry.showValue && hasData) {
    parts.push(`<text class="ozma-sparkline-value" x="${numberText(geometry.right)}" y="${numberText(Math.max(valueSize, geometry.top + valueSize))}" text-anchor="end" fill="${escapeXml(geometry.colors.text)}" font-family="${escapeXml(fontFamily)}" font-size="${numberText(valueSize)}" font-weight="650">${escapeXml(`${formatted}${unit ? ` ${unit}` : ''}`)}</text>`);
  }

  parts.push('</svg>');
  return parts.join('');
}

/* ------------------------------------------------------------------------- */
/* DOM helpers                                                                */
/* ------------------------------------------------------------------------- */

function classList(value) {
  return String(value || '').split(/\s+/).filter(Boolean);
}

function hasClass(element, name) {
  if (!element || typeof element.getAttribute !== 'function') return false;
  return classList(element.getAttribute('class')).includes(name);
}

function byClass(root, name) {
  if (!root) return null;
  if (hasClass(root, name)) return root;
  const children = Array.isArray(root.children)
    ? root.children
    : root.childNodes
      ? Array.from(root.childNodes)
      : [];
  for (const child of children) {
    const found = byClass(child, name);
    if (found) return found;
  }
  return null;
}

function byTag(root, tag) {
  if (!root) return null;
  const name = String(tag || '').toLowerCase();
  if (String(root.tagName || '').toLowerCase() === name) return root;
  const children = Array.isArray(root.children)
    ? root.children
    : root.childNodes
      ? Array.from(root.childNodes)
      : [];
  for (const child of children) {
    const found = byTag(child, name);
    if (found) return found;
  }
  return null;
}

function setAttributes(element, attributes) {
  if (!element || typeof element.setAttribute !== 'function') return;
  for (const [name, value] of Object.entries(attributes)) {
    if (value === undefined || value === null) continue;
    element.setAttribute(name, String(value));
  }
}

function clearElement(element) {
  if (!element) return;
  if (typeof element.replaceChildren === 'function') {
    element.replaceChildren();
    return;
  }
  while (element.firstChild) element.removeChild(element.firstChild);
}

function createSvgElement(doc, tag, className) {
  const element = doc.createElementNS(SVG_NS, tag);
  if (className) element.setAttribute('class', className);
  return element;
}

function ensureGroup(svg, className) {
  const doc = svg.ownerDocument;
  let group = byClass(svg, className);
  if (!group && doc) {
    group = createSvgElement(doc, 'g', className);
    svg.appendChild(group);
  }
  return group;
}

function ensurePath(svg, className) {
  const doc = svg.ownerDocument;
  let path = byClass(svg, className);
  if (!path && doc) {
    path = createSvgElement(doc, 'path', className);
    svg.appendChild(path);
  }
  return path;
}

function ensureLine(svg, className) {
  const doc = svg.ownerDocument;
  let line = byClass(svg, className);
  if (!line && doc) {
    line = createSvgElement(doc, 'line', className);
    svg.appendChild(line);
  }
  return line;
}

function ensureText(svg, className) {
  const doc = svg.ownerDocument;
  let text = byClass(svg, className);
  if (!text && doc) {
    text = createSvgElement(doc, 'text', className);
    svg.appendChild(text);
  }
  return text;
}

function removeElement(element) {
  if (element && element.parentNode && typeof element.parentNode.removeChild === 'function') {
    element.parentNode.removeChild(element);
  }
}

function applySparkline(svg, config, history, tokens, options) {
  const geometry = sparklineGeometry(config, history, tokens);
  const reducedMotion = detectReducedMotion(geometry, options);
  const doc = svg.ownerDocument;
  if (!doc) return;

  const fontFamily = tokenString(tokens, ['font.family', 'fontFamily', 'typography.family'], 'system-ui, sans-serif');
  const hasData = geometry.definedCount > 0;
  const formatted = hasData ? formatSparklineValue(geometry.current, geometry) : '—';
  const unit = geometry.unit;
  const title = geometry.label || 'Sparkline';
  const uid = idSafe(options.id ?? geometry.id ?? `sparkline-${hashText(`${title}|${geometry.min}|${geometry.max}|${geometry.width}x${geometry.height}`)}`);
  const ariaLabel = ariaLabelFor(geometry.label, formatted, unit, geometry.severity, hasData);
  const description = hasData
    ? `${title}: ${geometry.values.length} samples; current ${formatted}${unit ? ` ${unit}` : ''}; range ${formatSparklineValue(geometry.min, geometry)} to ${formatSparklineValue(geometry.max, geometry)}${unit ? ` ${unit}` : ''}; ${severityWord(geometry.severity)}${geometry.gapCount ? `; ${geometry.gapCount} missing sample${geometry.gapCount === 1 ? '' : 's'}` : ''}.`
    : `${title}: no data.`;

  setAttributes(svg, {
    viewBox: `0 0 ${numberText(geometry.width)} ${numberText(geometry.height)}`,
    class: sparklineClass(geometry, reducedMotion),
    role: 'img',
    'aria-label': ariaLabel,
    'aria-describedby': `${uid}-desc`,
    'data-reduced-motion': reducedMotion ? 'true' : 'false',
    'data-animate': geometry.animate ? 'true' : null,
    'data-severity': geometry.severity,
    focusable: 'false',
    preserveAspectRatio: 'xMidYMid meet',
    style: geometry.animate && !reducedMotion
      ? 'transition: stroke 200ms ease, fill 200ms ease, opacity 200ms ease'
      : null,
  });

  const titleElement = byTag(svg, 'title') || createSvgElement(doc, 'title', 'ozma-sparkline-title');
  if (!titleElement.parentNode) svg.appendChild(titleElement);
  titleElement.setAttribute('id', `${uid}-title`);
  titleElement.textContent = title;

  const descElement = byTag(svg, 'desc') || createSvgElement(doc, 'desc', 'ozma-sparkline-desc');
  if (!descElement.parentNode) svg.appendChild(descElement);
  descElement.setAttribute('id', `${uid}-desc`);
  descElement.textContent = description;

  const grid = ensureGroup(svg, 'ozma-sparkline-grid');
  if (grid) {
    clearElement(grid);
    if (geometry.showTicks && geometry.tickCount > 0) {
      for (const tick of geometry.tickValues) {
        const line = createSvgElement(doc, 'line', 'ozma-sparkline-tick');
        setAttributes(line, {
          x1: numberText(geometry.left),
          y1: numberText(tick.y),
          x2: numberText(geometry.right),
          y2: numberText(tick.y),
          stroke: geometry.colors.grid,
          'stroke-width': 1,
          'stroke-opacity': 0.7,
        });
        grid.appendChild(line);
        if (geometry.showTickLabels) {
          const label = createSvgElement(doc, 'text', 'ozma-sparkline-tick-label');
          setAttributes(label, {
            x: numberText(geometry.left),
            y: numberText(Math.max(8, tick.y - 2)),
            fill: geometry.colors.muted,
            'font-family': fontFamily,
            'font-size': 8,
          });
          label.textContent = formatSparklineValue(tick.value, geometry);
          grid.appendChild(label);
        }
      }
    }
  }

  const thresholds = ensureGroup(svg, 'ozma-sparkline-thresholds');
  if (thresholds) {
    clearElement(thresholds);
    if (geometry.showThresholds) {
      for (const line of thresholdLines(geometry)) {
        const element = createSvgElement(doc, 'line', 'ozma-sparkline-threshold');
        setAttributes(element, {
          x1: numberText(geometry.left),
          y1: numberText(line.y),
          x2: numberText(geometry.right),
          y2: numberText(line.y),
          stroke: line.color,
          'stroke-width': 1,
          'stroke-opacity': 0.55,
          'stroke-dasharray': '3 4',
          'data-severity': line.severity,
        });
        thresholds.appendChild(element);
      }
    }
  }

  const area = ensurePath(svg, 'ozma-sparkline-area');
  if (area) {
    if (geometry.showArea && geometry.segments.length) {
      setAttributes(area, {
        d: areaPath(geometry),
        fill: geometry.colors.fill,
        'fill-opacity': numberText(geometry.areaOpacity),
        stroke: 'none',
      });
    } else {
      removeElement(area);
    }
  }

  const lines = ensureGroup(svg, 'ozma-sparkline-lines');
  if (lines) {
    clearElement(lines);
    for (const run of thresholdSegments(geometry)) {
      const path = createSvgElement(doc, 'path', 'ozma-sparkline-line');
      setAttributes(path, {
        d: run.path,
        fill: 'none',
        stroke: run.color,
        'stroke-width': numberText(geometry.lineWidth),
        'stroke-linecap': 'round',
        'stroke-linejoin': 'round',
        'data-severity': run.severity,
        'vector-effect': 'non-scaling-stroke',
      });
      lines.appendChild(path);
    }
  }

  const baseline = ensureLine(svg, 'ozma-sparkline-baseline');
  if (baseline) {
    if (geometry.showBaseline && Number.isFinite(geometry.baselineY)) {
      setAttributes(baseline, {
        x1: numberText(geometry.left),
        y1: numberText(geometry.baselineY),
        x2: numberText(geometry.right),
        y2: numberText(geometry.baselineY),
        stroke: geometry.colors.baseline,
        'stroke-width': 1,
        'stroke-opacity': 0.8,
        'stroke-dasharray': '2 4',
      });
    } else {
      removeElement(baseline);
    }
  }

  const valueText = ensureText(svg, 'ozma-sparkline-value');
  if (valueText) {
    if (geometry.showValue && hasData) {
      const valueSize = Math.max(9, Math.min(14, geometry.height * 0.16));
      setAttributes(valueText, {
        x: numberText(geometry.right),
        y: numberText(Math.max(valueSize, geometry.top + valueSize)),
        'text-anchor': 'end',
        fill: geometry.colors.text,
        'font-family': fontFamily,
        'font-size': numberText(valueSize),
        'font-weight': 650,
      });
      valueText.textContent = `${formatted}${unit ? ` ${unit}` : ''}`;
    } else {
      removeElement(valueText);
    }
  }
}

function findSparklineSvg(container) {
  const first = container.firstElementChild;
  if (first && String(first.tagName || '').toLowerCase() === 'svg' && hasClass(first, 'ozma-sparkline')) {
    return first;
  }
  if (typeof container.querySelector === 'function') {
    return container.querySelector('svg.ozma-sparkline');
  }
  return null;
}

/**
 * Render accessible SVG into a container.  Returns the SVG element in DOM
 * environments, otherwise the SVG string.
 */
export function renderSparkline(container, config = {}, history = [], tokens = {}, options = {}) {
  if (!container || typeof container !== 'object') {
    throw new TypeError('renderSparkline requires a container element');
  }

  const doc = container.ownerDocument
    ?? (typeof globalThis.document !== 'undefined' ? globalThis.document : null);
  if (doc && typeof doc.createElementNS === 'function' && typeof container.appendChild === 'function') {
    clearElement(container);
    const svg = doc.createElementNS(SVG_NS, 'svg');
    container.appendChild(svg);
    applySparkline(svg, config, history, tokens, options);
    return svg;
  }

  const markup = renderSparklineSVG(config, history, tokens, options);
  container.innerHTML = markup;
  return container.firstElementChild ?? markup;
}

/**
 * Update an existing sparkline in place.  The SVG element is reused; only its
 * attributes and child paths/labels are updated.  Falls back to a fresh render
 * when the container does not contain a sparkline yet.
 */
export function updateSparkline(container, history = [], config = {}, tokens = {}, options = {}) {
  if (!container || typeof container !== 'object') {
    throw new TypeError('updateSparkline requires a container element');
  }
  const svg = findSparklineSvg(container);
  if (!svg) return renderSparkline(container, config, history, tokens, options);
  applySparkline(svg, config, history, tokens, options);
  return svg;
}

export const __private__ = Object.freeze({
  DEFAULT_COLORS,
  DEFAULT_WIDTH,
  DEFAULT_HEIGHT,
});
